//! `EXPLAIN` as a *statement* rather than as a function called by a test.
//!
//! The rest of this module's tests build a catalog by hand and call
//! [`nsqlite::explain::execute`] directly, which exercises the planner and the
//! listing but proves nothing about whether the word reaches it. Before this
//! file the parser had no branch for `EXPLAIN` at all, so every case in the
//! suite answered `near "EXPLAIN": syntax error` -- a 4000-line module that
//! nothing could reach.
//!
//! Every expectation below was read off the real `sqlite3` 3.53.4, and the
//! schema and the statement are the same two values both engines were given.
//! Where the two answers are allowed to differ, the difference is named in the
//! test rather than papered over.
//!
//! The setup runs in one connection because an index written by one connection
//! and read by the next is a different track's question (index pages surviving
//! a reopen) and would make every case here fail for a reason that is not
//! about `EXPLAIN`.

use nsqlite::connection::{Connection, Outcome};
use nsqlite::explain::QUERY_PLAN_COLUMNS;
use nsqlite::parser::{parse_one, Stmt};

/// The schema every case here shares, applied to a fresh connection.
///
/// `t1` has two indexes over a common leading column, `t3` has one composite
/// and `t2` has none, so the plain scan, the index search, the covering
/// search, the join, the compound, the sorter, the grouping note, the plain
/// sub-select and the correlated sub-select are all reachable from one
/// database -- which is the list the plan text has to be measured on.
fn db() -> Connection {
    let mut c = Connection::open_memory().expect("a memory connection");
    c.execute_script(
        "CREATE TABLE t1(a, b, c);
         CREATE TABLE t2(x, y, z);
         CREATE TABLE t3(p, q);
         CREATE INDEX i1 ON t1(b);
         CREATE INDEX i2 ON t1(b, c);
         CREATE INDEX i3 ON t3(p, q);",
    )
    .expect("the schema applies");
    c
}

/// The `detail` column of every plan row, joined with ` ~ `.
///
/// Which is a plan as a reader sees it: the words and their order. The `id` and
/// `parent` columns are SQLite's own bookkeeping and are not comparable.
fn details(sql: &str, c: &mut Connection) -> Result<String, String> {
    let out = c.execute_script(sql).map_err(|e| e.message)?;
    let Some(Outcome::Query { columns, rows }) = out.into_iter().next() else {
        return Err("EXPLAIN QUERY PLAN did not produce rows".into());
    };
    assert_eq!(columns, QUERY_PLAN_COLUMNS, "column names for {sql}");
    Ok(rows
        .iter()
        .map(|r| match &r.values[3] {
            nsqlite::Value::Text(s) => s.clone(),
            other => panic!("plan detail is not text: {other:?}"),
        })
        .collect::<Vec<_>>()
        .join(" ~ "))
}

/// Asserts a plan matches the text `sqlite3` prints for the same statement.
///
/// The plan is run through a connection rather than through the explain
/// module, so a case covers the parser arm and the dispatch as well.
#[track_caller]
fn eqp(sql: &str, expected: &str) {
    let mut c = db();
    match details(sql, &mut c) {
        Ok(got) => assert_eq!(got, expected, "plan for {sql}"),
        Err(e) => panic!("plan for {sql} failed: {e}"),
    }
}

// --- the statement is parsed at all ---------------------------------------

/// `EXPLAIN` and `EXPLAIN QUERY PLAN` are two statements, and the parser
/// reaches both.
///
/// This is the case that failed before: the parser had no branch for the
/// keyword, so both of these were `near "EXPLAIN": syntax error`. The mode
/// lives in the parsed node rather than in the executor, which is why the
/// assertion can be made on the parse tree alone.
#[test]
fn both_forms_parse_to_different_statements() {
    let eqp_stmt = parse_one("EXPLAIN QUERY PLAN SELECT 1").expect("EXPLAIN QUERY PLAN parses");
    let Stmt::Explain(e) = &eqp_stmt else {
        panic!("EXPLAIN QUERY PLAN did not parse to an Explain: {eqp_stmt:?}");
    };
    assert_eq!(e.mode, nsqlite::explain::Mode::QueryPlan);
    assert_eq!(e.inner, parse_one("SELECT 1").expect("the inner parses"));

    let stmt = parse_one("EXPLAIN SELECT 1").expect("EXPLAIN parses");
    let Stmt::Explain(e) = &stmt else {
        panic!("EXPLAIN did not parse to an Explain: {stmt:?}");
    };
    assert_eq!(e.mode, nsqlite::explain::Mode::Opcodes);
}

/// The inner statement is the *parsed* statement, not its text, so a syntax
/// error inside it is the bare statement's error and is raised at the same
/// point.
///
/// The messages below were read off `sqlite3`:
///
/// ```text
/// EXPLAIN SELECT FROM;   ->  near "FROM": syntax error
/// EXPLAIN SELECT;        ->  near "SELECT": syntax error
/// ```
#[test]
fn the_wrapped_statements_syntax_error_surfaces() {
    for sql in ["EXPLAIN SELECT FROM", "EXPLAIN SELECT"] {
        let err = parse_one(sql).expect_err(&format!("{sql} is not a statement"));
        assert!(
            err.message.contains("syntax error"),
            "{sql}: got {}",
            err.message
        );
    }
}

/// An `EXPLAIN` is one statement, so a script keeps going after it.
///
/// The plan is a result set, and a script that produces one still runs what
/// follows. The real engine agrees: `EXPLAIN QUERY PLAN SELECT 1; SELECT 2`
/// prints the plan and then `2` (measured), and the same is true of
/// `EXPLAIN SELECT 1; SELECT 2`, which prints the opcode listing and then the
/// row.
#[test]
fn a_script_continues_after_an_explain() {
    let mut c = db();
    let out = c
        .execute_script("EXPLAIN QUERY PLAN SELECT * FROM t1; SELECT 2;")
        .expect("both statements run");
    assert_eq!(out.len(), 2, "the EXPLAIN and the SELECT are both run");
    let Outcome::Query { rows, .. } = &out[0] else {
        panic!("the EXPLAIN produced no query");
    };
    assert_eq!(rows.len(), 1, "one line for one table");
    let Outcome::Query { rows, .. } = &out[1] else {
        panic!("the SELECT produced no query");
    };
    assert_eq!(rows[0].values[0], nsqlite::Value::Integer(2));
}

// --- EXPLAIN QUERY PLAN: the wording, measured ----------------------------

/// The plan text for a plain scan, an index search and a covering search.
///
/// These three are the whole vocabulary, and each was read off the real
/// engine's own rendering of the same statement over the same schema:
///
/// ```text
/// SELECT * FROM t1                  ->  SCAN t1
/// SELECT * FROM t1 WHERE b=1        ->  SEARCH t1 USING INDEX i2 (b=?)
/// SELECT count(*) FROM t1           ->  SCAN t1 USING COVERING INDEX i1
/// ```
///
/// The first is the one that says the table has no usable index, the second
/// that a seek was chosen over a scan, and the third that the index answered
/// the query on its own.
///
/// `i2` rather than `i1` for the equality is a tie, and the tie is the engine's
/// to break, not this one's to break by re-declaring: over this schema the
/// real engine names `i2` for `b=1` and this one does too, while declaring
/// `i1` first makes the real engine name `i1`. The declaration order is the
/// only thing that changed, so the winner is decided by a row-count estimate
/// this engine does not collect -- the module's [`choose_index`] names the same
/// gap and a test there pins both sides of it.
#[test]
fn a_scan_a_search_and_a_covering_search() {
    eqp("EXPLAIN QUERY PLAN SELECT * FROM t1", "SCAN t1");
    eqp(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE b=1",
        "SEARCH t1 USING INDEX i2 (b=?)",
    );
    eqp(
        "EXPLAIN QUERY PLAN SELECT count(*) FROM t1",
        "SCAN t1 USING COVERING INDEX i1",
    );
}

/// Two equality terms, and the composite index that serves both.
///
/// ```text
/// SELECT * FROM t1 WHERE b=1 AND c=2  ->  SEARCH t1 USING INDEX i2 (b=? AND c=?)
/// SELECT * FROM t1 WHERE b>1 AND b<5  ->  SEARCH t1 USING INDEX i1 (b>? AND b<?)
/// ```
///
/// The first is a seek on two pinned columns and the second a range on one,
/// which are the two shapes a plan's constraint text has to be able to say.
#[test]
fn two_terms_of_a_constraint_text() {
    eqp(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE b=1 AND c=2",
        "SEARCH t1 USING INDEX i2 (b=? AND c=?)",
    );
    eqp(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE b>1 AND b<5",
        "SEARCH t1 USING INDEX i1 (b>? AND b<?)",
    );
}

/// A join, a compound select, a sorter, a grouping note and a DISTINCT note.
///
/// Five shapes, one run against the real engine, and the wording is the point
/// of each case:
///
/// ```text
/// SELECT * FROM t1, t2                    ->  SCAN t1
///                                            BLOOM FILTER ON t2 (z=?)
///                                            SEARCH t2 USING AUTOMATIC COVERING INDEX (z=?)
/// SELECT * FROM t1 UNION ALL SELECT * FROM t1
///                                         ->  COMPOUND QUERY
///                                             LEFT-MOST SUBQUERY / SCAN t1
///                                             UNION ALL / SCAN t1
/// SELECT * FROM t1 ORDER BY c             ->  SCAN t1
///                                            USE TEMP B-TREE FOR ORDER BY
/// SELECT a, count(*) FROM t1 GROUP BY c   ->  SCAN t1
///                                            USE TEMP B-TREE FOR GROUP BY
/// SELECT DISTINCT c FROM t1               ->  SCAN t1 USING COVERING INDEX i2
///                                            USE TEMP B-TREE FOR DISTINCT
/// ```
///
/// The join is the one place these expectations do *not* match, and the
/// mismatch is the module's documented one: this engine builds no automatic
/// index, so it reports one line per table -- `SCAN t1 ~ SCAN t2` -- which is
/// the work it does. The test pins this engine's own text and the reason is in
/// the comment, so the difference from the measurement above is a decision on
/// record rather than a gap nobody mentioned. The other four are SQLite's own
/// words, character for character.
#[test]
fn the_compound_the_sorter_the_grouping_and_the_join() {
    eqp(
        "EXPLAIN QUERY PLAN SELECT * FROM t1, t2 WHERE t1.c=t2.z",
        "SCAN t1 ~ SCAN t2",
    );
    eqp(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 UNION ALL SELECT * FROM t1",
        "COMPOUND QUERY ~ LEFT-MOST SUBQUERY ~ SCAN t1 ~ UNION ALL ~ SCAN t1",
    );
    eqp(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 ORDER BY c",
        "SCAN t1 ~ USE TEMP B-TREE FOR ORDER BY",
    );
    eqp(
        "EXPLAIN QUERY PLAN SELECT a, count(*) FROM t1 GROUP BY c",
        "SCAN t1 ~ USE TEMP B-TREE FOR GROUP BY",
    );
    eqp(
        "EXPLAIN QUERY PLAN SELECT DISTINCT c FROM t1",
        "SCAN t1 USING COVERING INDEX i2 ~ USE TEMP B-TREE FOR DISTINCT",
    );
}

/// A plain sub-select and a correlated one, which are marked differently.
///
/// ```text
/// SELECT * FROM t1 WHERE b IN (SELECT b FROM t1)
///   ->  SEARCH t1 USING INDEX i2 (b=?)
///       LIST SUBQUERY 1
///       SCAN t1 USING COVERING INDEX i1
/// SELECT (SELECT count(*) FROM t1 AS q WHERE q.b=t1.b) FROM t1
///   ->  SCAN t1 USING COVERING INDEX i1
///       CORRELATED SCALAR SUBQUERY 1
///       SEARCH q USING COVERING INDEX i1 (b=?)
/// ```
///
/// The difference is the *value* of the outer reference: in the first it is
/// the list the sub-select is testing against, in the second it is read from
/// the outer row, so the inner table is seeked rather than scanned. That is
/// the whole reason the two markers are different words.
#[test]
fn a_list_subquery_and_a_correlated_one() {
    eqp(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE b IN (SELECT b FROM t1)",
        "SEARCH t1 USING INDEX i2 (b=?) ~ LIST SUBQUERY 1 ~ SCAN t1 USING COVERING INDEX i1",
    );
    eqp(
        "EXPLAIN QUERY PLAN SELECT (SELECT count(*) FROM t1 AS q WHERE q.b=t1.b) FROM t1",
        "SCAN t1 USING COVERING INDEX i1 ~ CORRELATED SCALAR SUBQUERY 1 ~ SEARCH q USING COVERING INDEX i1 (b=?)",
    );
}

/// A FROM with no table is one constant row, and a sub-select in FROM is a
/// co-routine *only to the planner*.
///
/// ```text
/// SELECT 1                          ->  SCAN CONSTANT ROW
/// SELECT * FROM (SELECT * FROM t1)  ->  SCAN t1
/// ```
///
/// The second is the module's own answer and it is not SQLite's: on the real
/// engine the sub-select is materialised as a co-routine and reported as
/// `CO-ROUTINE v / SCAN t1`, and the module knows how to say that -- the
/// `CO-ROUTINE` row is in its own vocabulary and a test there pins it. What
/// stops it being reached here is the executor, not the planner: a sub-select
/// in FROM is a statement this engine cannot run, and the bare statement says
/// so too:
///
/// ```text
/// SELECT * FROM (SELECT * FROM t1) s  ->  a subquery in FROM is not supported yet
/// ```
///
/// A query that cannot run is refused whether or not it is being explained, so
/// the plan is unreachable from a connection and the module's is only reachable
/// from its own tests. That is a real gap and it closes when the executor grows
/// the sub-select, not before.
#[test]
fn a_coroutine_and_the_constant_row() {
    eqp("EXPLAIN QUERY PLAN SELECT 1", "SCAN CONSTANT ROW");
    let mut c = db();
    let err = c
        .execute_script("EXPLAIN QUERY PLAN SELECT * FROM (SELECT * FROM t1) s")
        .expect_err("a sub-select in FROM cannot be run, so it cannot be planned")
        .message;
    assert_eq!(err, "a subquery in FROM is not supported yet");
    // ... and the bare statement's refusal is the same one, so the EXPLAIN has
    // not introduced an error of its own.
    let bare = c
        .execute_script("SELECT * FROM (SELECT * FROM t1) s")
        .expect_err("the bare statement is refused too")
        .message;
    assert_eq!(err, bare);
}

/// `DELETE` and `UPDATE` are planned too, and the no-WHERE case is where they
/// differ from each other.
///
/// ```text
/// DELETE FROM t1              ->  (nothing)
/// DELETE FROM t1 WHERE b=1    ->  SEARCH t1 USING INDEX i1 (b=?)
/// UPDATE t1 SET a=1           ->  SCAN t1
/// UPDATE t1 SET a=1 WHERE b=1 ->  SEARCH t1 USING INDEX i1 (b=?)
/// ```
///
/// An empty `DELETE` rewrites rows it removes and can be a truncate of the
/// b-tree, so SQLite reports no access line at all. An `UPDATE` has to read
/// every row to compute the new value, so it is a scan. Both are the real
/// answers, taken from the real engine rather than reasoned to.
#[test]
fn delete_and_update_are_planned() {
    eqp("EXPLAIN QUERY PLAN DELETE FROM t1", "");
    eqp(
        "EXPLAIN QUERY PLAN DELETE FROM t1 WHERE b=1",
        "SEARCH t1 USING INDEX i1 (b=?)",
    );
    eqp("EXPLAIN QUERY PLAN UPDATE t1 SET a=1", "SCAN t1");
    eqp(
        "EXPLAIN QUERY PLAN UPDATE t1 SET a=1 WHERE b=1",
        "SEARCH t1 USING INDEX i1 (b=?)",
    );
}

// --- EXPLAIN QUERY PLAN: DELETE and UPDATE never run the statement ---------

/// An `EXPLAIN` describes a statement; it does not run it.
///
/// Both of these *do* run in the real engine when the `EXPLAIN` is dropped,
/// so if the dispatch fell through to the executor the table would not be
/// empty afterwards. It is empty, which is the assertion.
#[test]
fn a_wrapped_delete_and_insert_do_not_run() {
    let mut c = db();
    c.execute_script(
        "EXPLAIN QUERY PLAN DELETE FROM t1;
         EXPLAIN QUERY PLAN INSERT INTO t1 VALUES(1,2,3);",
    )
    .expect("both plan");
    let out = c
        .execute_script("SELECT count(*) FROM t1")
        .expect("the table reads");
    let Outcome::Query { rows, .. } = &out[0] else {
        panic!("the SELECT produced no query");
    };
    assert_eq!(
        rows[0].values[0],
        nsqlite::Value::Integer(0),
        "neither the DELETE nor the INSERT happened"
    );
}

// --- EXPLAIN: the opcode listing ------------------------------------------

/// Plain `EXPLAIN` returns the eight column names and no rows.
///
/// This is the module's documented choice and it is reachable now, which it
/// was not before. `EXPLAIN SELECT 1` against the real engine prints
///
/// ```text
/// addr  opcode  p1  p2  p3  p4  p5  comment
/// 0     Init    0   4   0        0   Start at 4
/// ```
///
/// and so on to `Halt`. The header is reproduced below because it is the part
/// a caller can check; the rows are not, because this engine has no VDBE and a
/// list of them would be a fabrication. The row count is zero, so a caller
/// asking what the columns are gets a real answer and a caller asking what the
/// opcodes are gets an empty list.
#[test]
fn plain_explain_is_reachable_and_empty() {
    let mut c = db();
    let out = c.execute_script("EXPLAIN SELECT 1").expect("EXPLAIN runs");
    let Outcome::Query { columns, rows } = &out[0] else {
        panic!("EXPLAIN produced no query");
    };
    assert_eq!(
        columns,
        ["addr", "opcode", "p1", "p2", "p3", "p4", "p5", "comment"].as_slice()
    );
    assert!(
        rows.is_empty(),
        "there is no VDBE, so there is nothing to dump"
    );
}

/// `EXPLAIN` over something that is not a query is still an empty listing with
/// the same header.
///
/// The real engine prints opcodes for these -- `EXPLAIN BEGIN` is five of them
/// -- and this prints none. The *shape* is what has to match and it does; the
/// argument for why the contents cannot is at the module docs.
#[test]
fn plain_explain_over_a_non_select() {
    let mut c = db();
    for sql in [
        "EXPLAIN BEGIN",
        "EXPLAIN PRAGMA user_version",
        "EXPLAIN CREATE TABLE zzz(q)",
    ] {
        let out = c
            .execute_script(sql)
            .unwrap_or_else(|e| panic!("{sql}: {}", e.message));
        let Outcome::Query { columns, rows } = &out[0] else {
            panic!("{sql} produced no query");
        };
        assert_eq!(columns.len(), 8, "{sql} should report eight columns");
        assert!(rows.is_empty(), "{sql} should have no opcode rows");
    }
}

// --- prepare-time errors, which the EXPLAIN dispatch has to raise ----------

/// An `EXPLAIN` over a statement that cannot be prepared is an error, and the
/// error is the bare statement's.
///
/// SQLite resolves names while it prepares, and it prepares the wrapped
/// statement, so both forms raise. Measured on 3.53.4, every row below:
///
/// ```text
/// EXPLAIN QUERY PLAN SELECT * FROM nosuch       ->  no such table: nosuch
/// EXPLAIN QUERY PLAN SELECT nosuchcol FROM t1   ->  no such column: nosuchcol
/// EXPLAIN QUERY PLAN SELECT count(a,b) FROM t1  ->  wrong number of arguments to function count()
/// EXPLAIN QUERY PLAN SELECT 1 FROM t1 LIMIT count(a) ->  no such column: a
/// EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE nosuchcol=1 ->  no such column: nosuchcol
/// EXPLAIN QUERY PLAN INSERT INTO nosuch VALUES(1)     ->  no such table: nosuch
/// EXPLAIN QUERY PLAN DELETE FROM nosuch               ->  no such table: nosuch
/// EXPLAIN QUERY PLAN UPDATE nosuch SET a=1            ->  no such table: nosuch
/// ```
///
/// The assertion is against the *bare* statement, so the test asks both
/// engines the identical question rather than against a transcript that can
/// drift from what the code does.
///
/// One comparison is absent and is named here: `UPDATE t1 SET a=nosuchcol` is
/// `no such column: nosuchcol` on the real engine and is caught here too, but
/// the *bare* statement is not -- an `UPDATE` against a table with no rows never
/// evaluates its SET clause, and the real engine catches it while preparing,
/// which this engine does not. So the `EXPLAIN` is stricter than the bare
/// statement in exactly that one case, and the transcript rather than the
/// comparison is where that is pinned.
#[test]
fn both_modes_raise_the_bare_statements_error() {
    let mut c = db();
    for sql in [
        "EXPLAIN QUERY PLAN SELECT * FROM nosuch",
        "EXPLAIN QUERY PLAN SELECT nosuchcol FROM t1",
        "EXPLAIN QUERY PLAN SELECT count(a,b) FROM t1",
        "EXPLAIN QUERY PLAN SELECT 1 FROM t1 LIMIT count(a)",
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE nosuchcol=1",
        "EXPLAIN QUERY PLAN INSERT INTO nosuch VALUES(1)",
        "EXPLAIN QUERY PLAN DELETE FROM nosuch",
        "EXPLAIN QUERY PLAN UPDATE nosuch SET a=1",
        "EXPLAIN QUERY PLAN INSERT INTO t1(nosuchcol) VALUES(1)",
    ] {
        let err = c
            .execute_script(sql)
            .expect_err(&format!("{sql} must not run"))
            .message;
        // The same statement without the EXPLAIN is the reference, so the two
        // engines are asked the identical question.
        let bare = sql.trim_start_matches("EXPLAIN QUERY PLAN ");
        let bare_err = c
            .execute_script(bare)
            .expect_err(&format!("{bare} must not run either"))
            .message;
        assert_eq!(err, bare_err, "for {sql}");
    }
}

/// Plain `EXPLAIN` raises the same errors, because it prepares the same
/// statement.
///
/// The two forms are separate statements and either could have prepared a
/// different thing, so this is its own case rather than a second half of the
/// one above.
#[test]
fn the_opcode_listing_raises_them_too() {
    let mut c = db();
    for sql in [
        "EXPLAIN SELECT nosuchcol FROM t1",
        "EXPLAIN SELECT * FROM nosuch",
    ] {
        let err = c
            .execute_script(sql)
            .expect_err(&format!("{sql} must not run"))
            .message;
        let bare = sql.trim_start_matches("EXPLAIN ");
        let bare_err = c
            .execute_script(bare)
            .expect_err(&format!("{bare} must not run either"))
            .message;
        assert_eq!(err, bare_err, "for {sql}");
    }
}

/// A DML statement's SET clause and column list are name-checked, because
/// SQLite checks them while it prepares.
///
/// These are all errors on the real engine (measured), and the two spellings of
/// `no such column` are not interchangeable: a name in an INSERT's column *list*
/// is a column of the table and says so, while the same name in a VALUES list
/// or an UPDATE's SET is an expression and says the ordinary thing.
///
/// ```text
/// UPDATE t1 SET a=nosuchcol           ->  no such column: nosuchcol
/// INSERT INTO t1 VALUES(nosuchcol)    ->  no such column: nosuchcol
/// INSERT INTO t1(nosuchcol) VALUES(1) ->  table t1 has no column named nosuchcol
/// UPDATE nosuch SET a=1               ->  no such table: nosuch
/// DELETE FROM nosuch                  ->  no such table: nosuch
/// ```
///
/// The last is the case a plan cannot catch on its own: a `DELETE` with no
/// WHERE plans to no lines, so the planner's own `no such table` never runs and
/// the target has to be looked up separately. A `DELETE` with a WHERE would
/// have been caught by the planner.
#[test]
fn a_dml_prepare_error_is_reported() {
    let mut c = db();
    for (sql, expected) in [
        (
            "EXPLAIN QUERY PLAN UPDATE t1 SET a=nosuchcol",
            "no such column: nosuchcol",
        ),
        (
            "EXPLAIN QUERY PLAN INSERT INTO t1 VALUES(nosuchcol)",
            "no such column: nosuchcol",
        ),
        (
            "EXPLAIN QUERY PLAN INSERT INTO t1(nosuchcol) VALUES(1)",
            "table t1 has no column named nosuchcol",
        ),
        (
            "EXPLAIN QUERY PLAN UPDATE nosuch SET a=1",
            "no such table: nosuch",
        ),
        (
            "EXPLAIN QUERY PLAN DELETE FROM nosuch",
            "no such table: nosuch",
        ),
    ] {
        let err = c
            .execute_script(sql)
            .expect_err(&format!("{sql} must not run"))
            .message;
        assert_eq!(err, expected, "for {sql}");
    }
}

/// `no such table` outranks `no such column`, because SQLite refuses the FROM
/// before it resolves a name in it.
///
/// This is the bare statement's order and the EXPLAIN has to keep it, which is
/// the whole reason the two checks run in this order rather than the other way
/// round. Measured:
///
/// ```text
/// SELECT nosuchcol FROM nosuchtable              ->  no such table: nosuchtable
/// EXPLAIN QUERY PLAN SELECT nosuchcol FROM nosuchtable -> no such table: nosuchtable
/// ```
#[test]
fn a_missing_table_outranks_a_missing_column() {
    let mut c = db();
    for sql in [
        "EXPLAIN QUERY PLAN SELECT nosuchcol FROM nosuchtable",
        "EXPLAIN QUERY PLAN SELECT * FROM t1, nosuchtable",
    ] {
        let err = c
            .execute_script(sql)
            .expect_err(&format!("{sql} must not run"))
            .message;
        assert_eq!(err, "no such table: nosuchtable", "for {sql}");
    }
}

/// A plan is a result set, so a statement that plans cleanly returns rows
/// rather than a change count.
#[test]
fn a_plan_is_a_query_outcome() {
    let mut c = db();
    let out = c
        .execute_script("EXPLAIN QUERY PLAN SELECT * FROM t1")
        .expect("the plan runs");
    assert!(
        matches!(out[0], Outcome::Query { .. }),
        "a plan is a query, not a change count"
    );
}

// --- the terminator is part of the slice -----------------------------------

/// The statement's own semicolon has to reach the module, because the module
/// decides between two different messages on the strength of it.
///
/// This is the case that regressed. The parser cut the slice at the *start* of
/// the semicolon rather than its end, so a written `;` never arrived, and every
/// statement whose whole content is the terminator was reported as an input
/// that had run out. Both are SQLite's messages and they are different:
///
/// ```text
/// EXPLAIN;              near ";": syntax error
/// EXPLAIN QUERY;        near ";": syntax error
/// EXPLAIN QUERY PLAN;   near ";": syntax error
/// EXPLAIN               incomplete input
/// EXPLAIN QUERY         incomplete input
/// EXPLAIN QUERY PLAN    incomplete input
/// ```
///
/// Measured on 3.53.4, and the eight rows below are read off it in that order.
///
/// The bug was invisible to the module's own unit tests because they call
/// `explain::parse(";")` directly, which is exactly the slice the parser was
/// supposed to hand over and was not. These cases go through
/// `parse_one`, so they fail if the parser stops handing it.
#[test]
fn a_written_semicolon_is_a_token_and_input_that_ran_out_is_not() {
    for (sql, expected) in [
        ("EXPLAIN;", "near \";\": syntax error"),
        ("EXPLAIN QUERY;", "near \";\": syntax error"),
        ("EXPLAIN QUERY PLAN;", "near \";\": syntax error"),
        ("EXPLAIN blah;", "near \"blah\": syntax error"),
        ("EXPLAIN QUERY FOO;", "near \"FOO\": syntax error"),
        ("EXPLAIN", "incomplete input"),
        ("EXPLAIN QUERY", "incomplete input"),
        ("EXPLAIN QUERY PLAN", "incomplete input"),
    ] {
        let err = parse_one(sql).expect_err(&format!("{sql:?} is not a statement"));
        assert_eq!(err.message, expected, "for {sql}");
    }
}

/// The same split is SQLite's for `PRAGMA`, and it comes from the same shared
/// slice.
///
/// `PRAGMA;` is `near ";": syntax error` while a bare `PRAGMA` is `incomplete
/// input`, and the two are drawn by the same one-line rule in the parser that
/// fixes the three EXPLAIN cases. It is here so the fix cannot be narrowed to
/// the EXPLAIN arm later without failing something.
///
/// `PRAGMA main.` is the same split one token further in, and it is the second
/// half of the claim: the message is chosen by whether a token is there to
/// name, not by where the statement stopped. `PRAGMA main;` is not an error at
/// all in either engine -- a schema qualifier is optional, so `main` reads as
/// the pragma's own name -- which is why the two rows that differ are the ones
/// with a dot.
#[test]
fn a_pragma_terminator_is_a_token_too() {
    for (sql, expected) in [
        ("PRAGMA;", "near \";\": syntax error"),
        ("PRAGMA", "incomplete input"),
        ("PRAGMA main.", "incomplete input"),
    ] {
        let err = parse_one(sql).expect_err(&format!("{sql:?} is not a statement"));
        assert_eq!(err.message, expected, "for {sql}");
    }
}

/// Including the terminator in the slice must not eat the statement after it.
///
/// The slice grew by one character when the parser was fixed, and the loop
/// that consumes this statement's tokens is bounded by the same offset. A
/// `<=` bound would swallow the token that opens the next statement as well,
/// and `EXPLAIN SELECT 1; SELECT 2` would explain the 2 rather than the 1 --
/// or stop the script outright. The real engine splits it the same way, which
/// was measured before the bound was chosen.
///
/// Measured on 3.53.4, and asserted here through the outcome list, which is
/// what actually distinguishes the two splits: three statements in, three
/// outcomes out, and the middle one is the plan for `SELECT 1`.
#[test]
fn including_the_terminator_does_not_swallow_the_next_statement() {
    let mut c = db();
    let out = c
        .execute_script("SELECT 0; EXPLAIN QUERY PLAN SELECT * FROM t1; SELECT 2;")
        .expect("all three statements run");
    assert_eq!(out.len(), 3, "three statements, three outcomes");
    let Outcome::Query { rows, .. } = &out[1] else {
        panic!("the EXPLAIN produced no query");
    };
    assert_eq!(rows.len(), 1, "one plan line for one table");
    assert_eq!(rows[0].values[3], nsqlite::Value::Text("SCAN t1".into()));
    let Outcome::Query { rows, .. } = &out[2] else {
        panic!("the trailing SELECT produced no query");
    };
    assert_eq!(rows[0].values[0], nsqlite::Value::Integer(2));
}

/// `EXPLAIN` alone returns the eight opcode column names and no rows.
///
/// The column list is the part that can still be right, and this is the
/// statement that carries it through the parser to a caller. `sqlite3` 3.53.4
/// reports exactly these eight names in this order (read from
/// `cursor.description`), so the assertion is on the names and the order, not
/// on the absence of rows alone.
#[test]
fn a_bare_explain_reports_the_opcode_columns_and_no_rows() {
    let mut c = db();
    let out = c.execute_script("EXPLAIN SELECT 1").expect("EXPLAIN runs");
    let Outcome::Query { columns, rows } = &out[0] else {
        panic!("EXPLAIN produced no query");
    };
    assert_eq!(
        columns,
        &nsqlite::explain::OPCODE_COLUMNS,
        "the 8 opcode columns"
    );
    assert!(rows.is_empty(), "this engine has no opcodes to list");
}
