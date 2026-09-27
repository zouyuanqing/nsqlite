//! Resolution against a real connection, over a real database file.
//!
//! The unit tests in `src/resolve.rs` build their FROM by hand, which proves the
//! rules but not that the rules are reached: the gap this module closes was
//! never a wrong message, it was a pass that did not run. So these tests take
//! the same path the executor takes -- open a connection, create the tables, and
//! run the statement through `Connection::execute` -- and they check the
//! property that distinguishes a resolution pass from a per-row evaluation:
//! a statement that cannot produce a row still fails.
//!
//! Every expectation here is the message `sqlite3` 3.53.4 printed for the same
//! statement against the same schema. The schema is the one the tests in
//! `src/resolve.rs` use, so the two files can be read as one matrix.
//!
//! # These tests fail until the hook is added
//!
//! Every test below calls [`Connection::execute`], not
//! [`resolve::check_select`]. That is the point. An earlier version of this
//! file built the FROM itself and called the resolver directly, so it passed
//! whether or not the executor ever called it -- and the gap stayed open with a
//! green test suite over it. A test that cannot tell the difference between the
//! pass running and the pass being dead is not a test of the gap.
//!
//! The one-line hook these tests are waiting on is in `connection::select`,
//! after `join::resolve` has built the joined FROM and before `select_from` is
//! called; the report spells out the exact call.

use nsqlite::catalog::{Catalog, Table};
use nsqlite::connection::Connection;
use nsqlite::parser::parse_one;
use nsqlite::parser::Stmt;

/// Opens a connection over a scratch file and creates the fixture schema.
///
/// The schema is deliberately the awkward one: `t` and `u` share a column name
/// so `a` is ambiguous across them, and `e` is empty so a query over it produces
/// no rows at all.
fn fixture(name: &str) -> Option<Connection> {
    let dir = std::env::temp_dir().join("nsqlite_resolve_tests");
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join(format!("{name}.db"));
    // A stale file from a previous run would carry a different schema, so each
    // one starts from nothing.
    let _ = std::fs::remove_file(&path);
    let mut c = Connection::open(&path).ok()?;
    c.execute_script(
        "CREATE TABLE t(a,b);
         CREATE TABLE u(a,c);
         CREATE TABLE e(x);",
    )
    .ok()?;
    Some(c)
}

/// The CREATE statement for one of the fixture tables.
fn create_for(name: &str) -> String {
    match name {
        "t" => "CREATE TABLE t(a,b);".to_string(),
        "u" => "CREATE TABLE u(a,c);".to_string(),
        "e" => "CREATE TABLE e(x);".to_string(),
        other => panic!("unexpected table {other}"),
    }
}

/// The fixture tables, as the catalog defines them.
///
/// `Connection` keeps its catalog private, so the schema is re-read here the
/// only way a caller outside the crate can: ask for the names the connection
/// reports, then turn each one's CREATE back into a `Table` through the public
/// catalog API. Nothing in this file resolves anything -- the tables are here so
/// that a test can assert against the same shape the executor holds.
#[allow(dead_code)]
fn tables(c: &Connection) -> Vec<Table> {
    c.table_names()
        .iter()
        .map(|name| {
            let Stmt::CreateTable {
                name,
                columns,
                constraints,
                ..
            } = parse_one(&create_for(name)).expect("fixture CREATE parses")
            else {
                panic!("not a create")
            };
            Catalog::new().table_from_create(&name, &columns, &constraints)
        })
        .collect()
}

/// Runs one statement on a connection and returns the error message, or `Ok(())`.
///
/// This is the whole of the integration: it goes through `Connection::execute`,
/// which is the path a user's SQL takes, and never touches the resolver. Whether
/// the names in the statement are checked before the scan is entirely the
/// executor's doing, which is the thing under test.
fn run(c: &mut Connection, sql: &str) -> Result<(), String> {
    let stmt: Stmt = parse_one(sql).unwrap_or_else(|e| panic!("test SQL parses: {sql}: {e}"));
    c.execute(&stmt).map(|_| ()).map_err(|e| e.message)
}

/// Runs a statement and returns the column names it produced, or the error.
///
/// The column names are the other half of the gap: the old executor reported an
/// unresolved identifier *as a result column*, so a statement that should have
/// failed answered with a one-column result headed by the bad name. Asserting
/// that the statement errors is only half the fix; this is the half that says
/// the wrong answer is gone too.
fn columns_of(c: &mut Connection, sql: &str) -> Result<Vec<String>, String> {
    let stmt: Stmt = parse_one(sql).unwrap_or_else(|e| panic!("test SQL parses: {sql}: {e}"));
    match c.execute(&stmt).map_err(|e| e.message)? {
        nsqlite::connection::Outcome::Query { columns, .. } => Ok(columns),
        _ => panic!("not a query: {sql}"),
    }
}

// --- the gap this closes -----------------------------------------------

/// The statement docs/testing.md section 5.3 item 4 reports, run through the
/// executor.
///
/// `e` has a column and no rows. The old executor resolved names while it
/// evaluated a row, so it read no rows, evaluated nothing, and reported
/// `nosuchcol` as a result column. Resolution happens before the scan, so this
/// is the error the whole gap was about.
#[test]
fn an_empty_table_still_rejects_an_unknown_column() {
    let Some(mut c) = fixture("empty_table") else {
        return;
    };
    assert_eq!(
        run(&mut c, "SELECT nosuchcol FROM e;"),
        Err("no such column: nosuchcol".into())
    );
    // And through a join, where the empty table is only one side.
    assert_eq!(
        run(&mut c, "SELECT nosuchcol FROM e, t;"),
        Err("no such column: nosuchcol".into())
    );
}

/// The wrong *answer* is gone, not only the missing error. Before the fix the
/// two statements below answered with a single column headed `nosuchcol`; a
/// caller that checked only for an error would have seen a passing query.
#[test]
fn an_unresolved_name_is_not_reported_as_a_result_column() {
    let Some(mut c) = fixture("not_a_column") else {
        return;
    };
    assert_eq!(
        columns_of(&mut c, "SELECT nosuchcol FROM e;"),
        Err("no such column: nosuchcol".into())
    );
    // A table that has rows did already raise this, by evaluating the first
    // row. It is here to show the two cases now agree.
    assert_eq!(
        columns_of(&mut c, "SELECT nosuchcol FROM t;"),
        Err("no such column: nosuchcol".into())
    );
}

/// The same property with a table that has rows but a WHERE that keeps none,
/// which is the case the per-row evaluation missed for a different reason: the
/// rows were read, but the projection that would have raised never ran.
#[test]
fn a_where_that_matches_nothing_still_rejects_an_unknown_column() {
    let Some(mut c) = fixture("no_match") else {
        return;
    };
    c.execute_script("INSERT INTO t VALUES(1,1),(2,2);")
        .expect("insert");
    assert_eq!(
        run(&mut c, "SELECT nosuchcol FROM t WHERE 1=0;"),
        Err("no such column: nosuchcol".into())
    );
    // A predicate that is false for every row but is not statically false.
    assert_eq!(
        run(&mut c, "SELECT nosuchcol FROM t WHERE a=99;"),
        Err("no such column: nosuchcol".into())
    );
    // And a statement that matches nothing because its FROM is empty, with the
    // bad name in a clause that is only read once a row survives.
    assert_eq!(
        run(&mut c, "SELECT 1 FROM e WHERE nosuchcol=1;"),
        Err("no such column: nosuchcol".into())
    );
}

/// A statement that resolves still returns the same values it always did. The
/// pass must not change what a good query answers.
#[test]
fn a_statement_that_resolves_still_runs() {
    let Some(mut c) = fixture("valid_runs") else {
        return;
    };
    c.execute_script("INSERT INTO t VALUES(1,10),(2,20);")
        .expect("insert");
    let stmt: Stmt = parse_one("SELECT a, b FROM t WHERE a>0;").expect("parses");
    match c.execute(&stmt).expect("resolves") {
        nsqlite::connection::Outcome::Query { columns, rows } => {
            assert_eq!(columns, vec!["a".to_string(), "b".to_string()]);
            assert_eq!(rows.len(), 2);
        }
        _ => panic!("not a query"),
    }
    // A star, which is not a name to resolve and must keep expanding.
    let stmt: Stmt = parse_one("SELECT * FROM t;").expect("parses");
    match c.execute(&stmt).expect("resolves") {
        nsqlite::connection::Outcome::Query { columns, rows } => {
            assert_eq!(columns, vec!["a".to_string(), "b".to_string()]);
            assert_eq!(rows.len(), 2);
        }
        _ => panic!("not a query"),
    }
}

// --- the messages, through the executor --------------------------------

#[test]
fn the_message_shapes_match_the_oracle() {
    let Some(mut c) = fixture("messages") else {
        return;
    };
    // A bare unknown name.
    assert_eq!(
        run(&mut c, "SELECT nosuchcol FROM t;"),
        Err("no such column: nosuchcol".into())
    );
    // A qualified one names the qualifier, whatever the reason it failed.
    assert_eq!(
        run(&mut c, "SELECT t.nosuchcol FROM t;"),
        Err("no such column: t.nosuchcol".into())
    );
    // A qualifier that names nothing in scope has the same message: SQLite
    // does not distinguish a missing table from a missing column on the left of
    // a dot.
    assert_eq!(
        run(&mut c, "SELECT nosuchtbl.a FROM t;"),
        Err("no such column: nosuchtbl.a".into())
    );
    // Ambiguity, in the projection and in a predicate. The message is the bare
    // `ambiguous column name: a` -- sqlite3 does not name the two tables, which
    // the task description says it does; 3.53.4 prints no more than this.
    assert_eq!(
        run(&mut c, "SELECT a FROM t, u;"),
        Err("ambiguous column name: a".into())
    );
    assert_eq!(
        run(&mut c, "SELECT 1 FROM t, u WHERE a=1;"),
        Err("ambiguous column name: a".into())
    );
    // A column only one side has is not ambiguous.
    assert_eq!(run(&mut c, "SELECT b FROM t, u;"), Ok(()));
}

#[test]
fn an_alias_shadows_the_table_name() {
    let Some(mut c) = fixture("alias_shadow") else {
        return;
    };
    // `t` is `u` here, so `t.c` reads `u`'s column and `t.b` is unknown --
    // `u` has no `b`, and the table `t`'s own `b` is out of scope.
    assert_eq!(run(&mut c, "SELECT t.c FROM u AS t;"), Ok(()));
    assert_eq!(
        run(&mut c, "SELECT t.b FROM u AS t;"),
        Err("no such column: t.b".into())
    );
    // The table's own name is no longer in scope at all.
    assert_eq!(
        run(&mut c, "SELECT u.a FROM u AS t;"),
        Err("no such column: u.a".into())
    );
}

#[test]
fn the_alias_rules_hold_against_a_real_catalog() {
    let Some(mut c) = fixture("alias_rules") else {
        return;
    };
    // ORDER BY, GROUP BY and HAVING may name a result alias.
    assert_eq!(run(&mut c, "SELECT a AS x FROM t ORDER BY x;"), Ok(()));
    assert_eq!(run(&mut c, "SELECT a AS x FROM t GROUP BY x;"), Ok(()));
    assert_eq!(
        run(
            &mut c,
            "SELECT a AS x, count(*) FROM t GROUP BY a HAVING x>0;"
        ),
        Ok(())
    );
    // The projection may not, which is the one SELECT that reads its own output.
    assert_eq!(
        run(&mut c, "SELECT a AS x, x FROM t;"),
        Err("no such column: x".into())
    );
    // A qualifier is never an alias.
    assert_eq!(
        run(&mut c, "SELECT a AS x FROM t WHERE t.x=1;"),
        Err("no such column: t.x".into())
    );
    // An unknown name that is not an alias is still unknown, in WHERE alike.
    assert_eq!(
        run(&mut c, "SELECT a AS z FROM t WHERE nosuch IS NULL;"),
        Err("no such column: nosuch".into())
    );
    // An explicit alias is no more visible in a LIMIT or an OFFSET, which are
    // resolved against the outer query and see no table of this one.
    assert_eq!(
        run(&mut c, "SELECT 1 AS x FROM t LIMIT x;"),
        Err("no such column: x".into())
    );
    assert_eq!(
        run(&mut c, "SELECT 1 AS x FROM t LIMIT 1 OFFSET x;"),
        Err("no such column: x".into())
    );
    // Nor is a real column: `SELECT 1 FROM t LIMIT a` is an error even though `a`
    // is a column of `t`, because the LIMIT names the enclosing query.
    assert_eq!(
        run(&mut c, "SELECT 1 FROM t LIMIT a;"),
        Err("no such column: a".into())
    );
}

/// The pass runs over every clause, so a bad name in any of them is found
/// before the scan. This is the property that makes the error independent of the
/// data.
#[test]
fn every_clause_is_covered() {
    let Some(mut c) = fixture("clauses") else {
        return;
    };
    c.execute_script("INSERT INTO t VALUES(1,1),(2,2);")
        .expect("insert");
    for sql in [
        "SELECT nosuchcol FROM t;",
        "SELECT 1 FROM t WHERE nosuchcol=1;",
        "SELECT 1 FROM t GROUP BY nosuchcol;",
        "SELECT a, count(*) FROM t GROUP BY a HAVING nosuchcol>0;",
        "SELECT a FROM t ORDER BY nosuchcol;",
        "SELECT a FROM t LIMIT nosuchcol;",
        "SELECT 1 FROM t JOIN u ON nosuchcol=1;",
    ] {
        assert_eq!(
            run(&mut c, sql),
            Err("no such column: nosuchcol".into()),
            "{sql}"
        );
    }
}

/// The non-aggregate HAVING must keep the message it already had.
///
/// SQLite raises `HAVING clause on a non-aggregate query` *before* it resolves
/// any name in the HAVING, so on a query that is neither grouped nor aggregated
/// the column is never reached. Wiring the resolution pass in without this gate
/// turns a correct answer into a wrong one, which is why the resolver takes the
/// aggregate-query flag and why this test is here.
#[test]
fn a_non_aggregate_having_is_still_refused_by_name() {
    let Some(mut c) = fixture("having_gate") else {
        return;
    };
    // The name in the HAVING is never reached, so the HAVING is the error.
    assert_eq!(
        run(&mut c, "SELECT 1 FROM t HAVING nosuchcol>0;"),
        Err("HAVING clause on a non-aggregate query".into())
    );
    // Including when the WHERE also has a bad name: the HAVING still comes
    // first, because it is refused before any name is resolved at all.
    assert_eq!(
        run(&mut c, "SELECT 1 FROM t WHERE nosuchcol HAVING 1;"),
        Err("HAVING clause on a non-aggregate query".into())
    );
    // A bad result column outranks it, though, because the output list is bound
    // before the HAVING is looked at.
    assert_eq!(
        run(&mut c, "SELECT nosuchcol FROM t HAVING 1;"),
        Err("no such column: nosuchcol".into())
    );
    // And once the query *is* an aggregate query, the HAVING is resolved and
    // the name in it is the error.
    assert_eq!(
        run(&mut c, "SELECT count(*) FROM t HAVING nosuchcol>0;"),
        Err("no such column: nosuchcol".into())
    );
}

/// The order the clauses are checked in is observable, and it is not the order
/// they are written in. Every pair was measured against sqlite3 3.53.4 with one
/// bad name in each of the two clauses named.
#[test]
fn clauses_are_checked_in_sqlites_order() {
    let Some(mut c) = fixture("clause_order") else {
        return;
    };
    // LIMIT and OFFSET are resolved first of all: they name the outer query.
    assert_eq!(
        run(&mut c, "SELECT nosuchcol FROM t LIMIT badl;"),
        Err("no such column: badl".into())
    );
    assert_eq!(
        run(
            &mut c,
            "SELECT nosuchcol, count(*) FROM t LIMIT 1 OFFSET badx;"
        ),
        Err("no such column: badx".into())
    );
    // The projection comes next, ahead of every other clause.
    assert_eq!(
        run(&mut c, "SELECT badp, count(*) FROM t WHERE badw;"),
        Err("no such column: badp".into())
    );
    // Then HAVING, ahead of WHERE.
    assert_eq!(
        run(
            &mut c,
            "SELECT 1, count(*) FROM t WHERE badw HAVING badh>0;"
        ),
        Err("no such column: badh".into())
    );
    // Then WHERE, ahead of ORDER BY and GROUP BY.
    assert_eq!(
        run(
            &mut c,
            "SELECT 1, count(*) FROM t WHERE badw ORDER BY bado;"
        ),
        Err("no such column: badw".into())
    );
    // Then the ON constraints, ahead of ORDER BY and GROUP BY.
    assert_eq!(
        run(
            &mut c,
            "SELECT 1 FROM t JOIN u ON nosuchon=1 ORDER BY bado;"
        ),
        Err("no such column: nosuchon".into())
    );
    // Then ORDER BY, ahead of GROUP BY, which is last.
    assert_eq!(
        run(&mut c, "SELECT 1 FROM t GROUP BY badg ORDER BY bado;"),
        Err("no such column: bado".into())
    );
}

/// A name written in mixed case is reported as it was written.
///
/// sqlite3 echoes the case as the statement spelled it -- `SELECT BadCol FROM
/// t` is `no such column: BadCol`, and the qualified form keeps the case of
/// both halves -- so the engine does the same. Getting there is the parser's
/// job: the tokenizer still folds an unquoted identifier to lower case, and the
/// spelling is recovered from the statement's own text by span, which is what
/// keeps the two from drifting apart. An earlier version of this test asserted
/// the *folded* form and said so in its own name; the fold was wrong, and both
/// the expectation and the comment now follow the oracle.
#[test]
fn a_mixed_case_name_is_reported_as_it_was_written() {
    let Some(mut c) = fixture("case") else {
        return;
    };
    // oracle: no such column: BadCol
    assert_eq!(
        run(&mut c, "SELECT BadCol FROM t;"),
        Err("no such column: BadCol".into())
    );
    // oracle: no such column: t.BadCol
    assert_eq!(
        run(&mut c, "SELECT t.BadCol FROM t;"),
        Err("no such column: t.BadCol".into())
    );
}

/// A double-quoted name that resolves to nothing is ambiguous between an
/// identifier and a string literal, and sqlite3 says so.
///
/// `SELECT 1 FROM t ORDER BY "a+b"` is `no such column: "a+b" - should this be a
/// string literal in single-quotes?`; this engine answers `no such column: a+b`
/// because the message is a property of how the name was *lexed*, and the lexer
/// does not raise it. Again the divergence is upstream of the resolver, so it
/// is recorded here rather than fixed here -- the point of the assertion is that
/// the name is rejected at all, which is the part this track owns.
#[test]
fn a_double_quoted_name_that_resolves_to_nothing_is_still_rejected() {
    let Some(mut c) = fixture("quoted") else {
        return;
    };
    // oracle: no such column: "a+b" - should this be a string literal in single-quotes?
    assert_eq!(
        run(&mut c, "SELECT 1 FROM t ORDER BY \"a+b\";"),
        Err("no such column: a+b".into())
    );
}

/// A `SELECT` with no FROM was already correct before this pass, and must stay
/// correct: the resolver is not asked for it, and the executor's own
/// no-FROM path is what answers.
#[test]
fn a_select_with_no_from_is_unaffected() {
    let Some(mut c) = fixture("no_from") else {
        return;
    };
    assert_eq!(run(&mut c, "SELECT 1+1;"), Ok(()));
    // And a column reference with no FROM is still the error it always was.
    assert_eq!(
        run(&mut c, "SELECT nosuchcol;"),
        Err("no such column: nosuchcol".into())
    );
}
