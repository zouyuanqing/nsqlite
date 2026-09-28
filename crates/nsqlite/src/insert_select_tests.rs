//! Tests for `INSERT ... SELECT`.
//!
//! Every expected value in this file was produced by running the same SQL
//! through the real `sqlite3` 3.53.4 at
//! `C:/Users/zyq/scoop/apps/msys2/bin/sqlite3.exe` and is quoted in the test
//! that checks it, rather than asserted bare. Where sqlite3's answer is
//! surprising, the reason is in the comment; the point of a test here is to pin
//! the behaviour, not to assert what seemed reasonable.
//!
//! The tests drive [`super::insert_select`] through a fake [`InsertTarget`]
//! rather than a real connection, because the connection's row-writing half is
//! private to `connection.rs`. The fake is deliberately a *recording* one: it
//! captures the exact rows and rowids the module decided to write, which is
//! what makes the affinity, default, rowid and ordering rules observable. The
//! reading side is real — the fake evaluates the source query with the
//! connection's own executor over a real table, so a mis-evaluated SELECT fails
//! here too.

use super::{count_error, insert_select, InsertTarget, Source, UniqueOutcome, ATOMICITY};
use crate::affinity::Affinity;
use crate::catalog::{Catalog, Table};
use crate::connection::{Connection, Outcome};
use crate::error::{Error, Result, ResultCode};
use crate::parser::{parse_one, Stmt};
use crate::value::Value;

/// A table built the way the catalog builds one, so the tests do not have to
/// restate how affinity and the rowid alias are derived.
fn table(sql: &str) -> Table {
    let Stmt::CreateTable {
        name,
        columns,
        constraints,
        without_rowid,
        ..
    } = parse_one(sql).expect("test SQL should parse")
    else {
        panic!("expected CREATE TABLE, got {sql:?}")
    };
    let mut t = Catalog::new().table_from_create(&name, &columns, &constraints);
    t.without_rowid = without_rowid;
    t
}

/// A destination that records what it was asked to write.
///
/// The source query is run by a real [`Connection`], so the rows handed to this
/// fake are the ones the engine actually produced. The write side is recorded
/// rather than performed, which is what the tests below need to look at: the
/// rowid assigned, the affinity applied, the default filled in.
#[derive(Debug, Default)]
struct Recorder {
    tables: Vec<Table>,
    written: Vec<(i64, Vec<Value>)>,
    finished: Option<(usize, Option<i64>)>,
    /// Extra statements run against the source connection before the SELECT, so
    /// a test can populate a source table -- including `t` itself, which is what
    /// a self-insert needs.
    seed: Vec<String>,
}

impl Recorder {
    fn with(sqls: &[&str]) -> Recorder {
        let mut catalog = Catalog::new();
        for sql in sqls {
            catalog.put(table(sql));
        }
        Recorder {
            tables: catalog.all_tables(),
            ..Default::default()
        }
    }

    /// A recorder that also seeds the source connection, so the SELECT reads
    /// real rows rather than an empty table.
    fn with_seed(sqls: &[&str], seed: &[&str]) -> Recorder {
        let mut r = Recorder::with(sqls);
        r.seed = seed.iter().map(|s| (*s).to_owned()).collect();
        r
    }

    /// The stored rows, as `(rowid, values)`.
    fn rows(&self) -> &[(i64, Vec<Value>)] {
        &self.written
    }

    /// The stored row at an index.
    fn row(&self, i: usize) -> &[Value] {
        &self.written[i].1
    }

    fn col(&self, i: usize, j: usize) -> Value {
        self.written[i].1[j].clone()
    }

    /// The stored value of a column, as its rendered form, for comparisons
    /// against what sqlite3 printed.
    fn text(&self, i: usize, j: usize) -> String {
        self.col(i, j).to_string()
    }
}

impl InsertTarget for Recorder {
    fn target_table(&self, written_name: &str) -> Result<Table> {
        let base = crate::join::strip_schema_qualifier(written_name).unwrap_or(written_name);
        self.tables
            .iter()
            .find(|t| t.name.eq_ignore_ascii_case(base))
            .cloned()
            .ok_or_else(|| Error::new(ResultCode::Error, format!("no such table: {written_name}")))
    }

    /// Runs the query against a throwaway in-memory database, so the engine's
    /// own SELECT path produces the rows rather than this file inventing them.
    ///
    /// The width is taken from the query's own column names rather than from
    /// the rows, which is what makes it right for a query that matched nothing
    /// — the case sqlite3 still reports a count error for.
    fn run_source(&mut self, select: &crate::parser::Select) -> Result<Source> {
        let mut conn = Connection::open_memory().expect("in-memory database");
        for sql in [
            "CREATE TABLE a(x)",
            "CREATE TABLE b(x,y)",
            "CREATE TABLE d(x,y,z)",
            "CREATE TABLE t(a)",
            "CREATE TABLE s(x)",
        ] {
            let _ = conn.execute_script(sql);
        }
        let _ = conn.execute_script("INSERT INTO a VALUES(1),(2),(3)");
        let _ = conn.execute_script("INSERT INTO b VALUES(1,2),(3,4)");
        let _ = conn.execute_script("INSERT INTO d VALUES(1,2,3)");
        // A test's own seed runs last, so it can populate `s` -- which is empty
        // by default -- and can add columns to it if it needs more than one.
        for sql in &self.seed {
            conn.execute_script(sql)
                .unwrap_or_else(|e| panic!("seeding {sql:?} failed: {e}"));
        }
        let out = conn.execute(&Stmt::Select(select.clone()))?;
        match out {
            Outcome::Query { columns, rows } => Ok(Source {
                width: columns.len(),
                rows,
            }),
            other => panic!("expected a query, got {other:?}"),
        }
    }

    /// One past the largest rowid written so far, or 1 when there are none,
    /// which is how the connection assigns one.
    fn next_rowid(&mut self, table: &Table, values: &[Value]) -> Result<i64> {
        if let Some(i) = table.rowid_alias {
            if let Some(Value::Integer(v)) = values.get(i) {
                return Ok(*v);
            }
        }
        let next = self
            .written
            .iter()
            .map(|(r, _)| *r)
            .filter(|r| *r >= 0)
            .max()
            .map_or(1, |m| m + 1);
        Ok(next)
    }

    fn write_row(&mut self, _table: &Table, rowid: i64, values: Vec<Value>) -> Result<()> {
        self.written.push((rowid, values));
        Ok(())
    }

    fn finish(&mut self, changed: usize, last_rowid: Option<i64>) -> Result<()> {
        self.finished = Some((changed, last_rowid));
        Ok(())
    }

    /// This fake has no b-tree, so it never reports a conflict: uniqueness is
    /// the connection's job, and the engine's own cases are in
    /// `unique_constraint_tests`. Saying so here is what keeps the two from
    /// drifting -- a fake that invented its own verdict would test itself.
    fn write_unique(
        &mut self,
        _table: &Table,
        _values: &[Value],
        _conflict: crate::parser::ConflictAction,
        _pending: &[(Vec<usize>, Vec<Value>)],
    ) -> Result<UniqueOutcome> {
        Ok(UniqueOutcome::Write)
    }

    fn written_keys(&self, _table: &Table, _values: &[Value]) -> Vec<Vec<usize>> {
        Vec::new()
    }

    fn unique_keys(&self, _table: &Table) -> Vec<Vec<usize>> {
        Vec::new()
    }

    fn stored_unique_conflict(
        &mut self,
        _table: &Table,
        _keys: &[Vec<usize>],
        _values: &[Value],
        _exclude: Option<i64>,
    ) -> Result<Option<usize>> {
        Ok(None)
    }
}

/// Runs one INSERT ... SELECT through the module under test.
fn run(sql: &str) -> Result<(Recorder, Outcome)> {
    let Stmt::Insert {
        table,
        columns,
        source,
        conflict,
    } = parse_one(sql).expect("test SQL should parse")
    else {
        panic!("expected INSERT, got {sql:?}")
    };
    let crate::parser::InsertSource::Select(select) = source else {
        panic!("expected INSERT ... SELECT, got {sql:?}")
    };
    let mut rec = Recorder::with(&[
        "CREATE TABLE t(a)",
        "CREATE TABLE t2(a,b)",
        "CREATE TABLE t3(a,b,c)",
    ]);
    let out = insert_select(&mut rec, &table, columns.as_deref(), &select, conflict)?;
    Ok((rec, out))
}

/// The single value an INSERT produced, or the error it produced.
fn one(sql: &str) -> Result<Value> {
    let (rec, out) = run(sql)?;
    assert_eq!(
        out,
        Outcome::Changed(1),
        "expected one inserted row in {sql:?}"
    );
    assert_eq!(rec.rows().len(), 1);
    Ok(rec.row(0)[0].clone())
}

/// The number of rows an INSERT wrote, or the error it raised.
fn changed(sql: &str) -> Result<usize> {
    let (_, out) = run(sql)?;
    match out {
        Outcome::Changed(n) => Ok(n),
        other => panic!("expected Changed, got {other:?}"),
    }
}

// --- the shape of the result set -------------------------------------------

#[test]
fn a_select_whose_columns_match_the_table_inserts_every_row() {
    // sqlite3: CREATE TABLE a(x); INSERT INTO a VALUES(1),(2),(3);
    //          CREATE TABLE t(a); INSERT INTO t SELECT x FROM a;
    //          SELECT a FROM t --> 1 2 3, changes() --> 3
    let (rec, out) = run("INSERT INTO t SELECT x FROM a").unwrap();
    assert_eq!(out, Outcome::Changed(3));
    assert_eq!(rec.rows().len(), 3);
    assert_eq!(rec.col(0, 0), Value::Integer(1));
    assert_eq!(rec.col(1, 0), Value::Integer(2));
    assert_eq!(rec.col(2, 0), Value::Integer(3));
}

#[test]
fn a_select_with_no_from_inserts_its_single_row() {
    // sqlite3: CREATE TABLE t(a); INSERT INTO t SELECT 1;  SELECT a FROM t --> 1
    let value = one("INSERT INTO t SELECT 1").unwrap();
    assert_eq!(value, Value::Integer(1));
}

#[test]
fn a_select_that_matches_nothing_inserts_nothing() {
    // sqlite3: CREATE TABLE t(a); CREATE TABLE s(x);   -- s is empty
    //          INSERT INTO t SELECT x FROM s;  changes() --> 0
    //          INSERT INTO t VALUES(42); INSERT INTO t SELECT x FROM s;
    //          last_insert_rowid() --> 1
    //
    // The width still has to be right when there are no rows, which is why
    // `run_source` reports the projection's width rather than a row's.
    let (rec, out) = run("INSERT INTO t SELECT x FROM a WHERE 0").unwrap();
    assert_eq!(out, Outcome::Changed(0));
    assert_eq!(rec.rows().len(), 0);
    // The last rowid is left alone rather than set to a fresh value, which is
    // what lets `last_insert_rowid()` survive a zero-row insert.
    assert_eq!(rec.finished, Some((0, None)));
}

#[test]
fn changes_counts_the_rows_the_query_produced() {
    // sqlite3: changes() after INSERT INTO t SELECT x FROM a --> 3
    let (rec, _) = run("INSERT INTO t SELECT x FROM a").unwrap();
    assert_eq!(rec.finished, Some((3, Some(3))));
}

#[test]
fn the_rowid_continues_from_what_the_table_already_holds() {
    // sqlite3: INSERT INTO t VALUES(7); INSERT INTO t SELECT x FROM a;
    //          SELECT rowid,a FROM t --> 1|7  2|1  3|2  4|3
    let Stmt::Insert {
        table,
        columns,
        source,
        conflict,
    } = parse_one("INSERT INTO t SELECT x FROM a").unwrap()
    else {
        unreachable!()
    };
    let crate::parser::InsertSource::Select(select) = source else {
        unreachable!()
    };
    let mut rec = Recorder::with(&["CREATE TABLE t(a)"]);
    rec.written.push((1, vec![Value::Integer(7)]));
    insert_select(&mut rec, &table, columns.as_deref(), &select, conflict).unwrap();
    let rowids: Vec<i64> = rec.rows().iter().map(|(r, _)| *r).collect();
    assert_eq!(rowids, vec![1, 2, 3, 4]);
}

#[test]
fn rows_are_written_in_query_order_whatever_the_values_sort_like() {
    // sqlite3 3.53.4:
    //   CREATE TABLE s(x); INSERT INTO s VALUES(3),(1),(2);
    //   CREATE TABLE t(a); INSERT INTO t SELECT x FROM s;
    //   SELECT rowid,a FROM t --> 1|3  2|1  3|2
    //
    // The rowids are assigned in the order the query produced the rows, and
    // the values are not sorted on the way in: the table b-tree reads in rowid
    // order, so this is the order a later SELECT sees them in.
    //
    // This runs the statement rather than hand-looping over a literal vector,
    // so it is the module's ordering that is under test.
    let mut rec = Recorder::with_seed(
        &["CREATE TABLE t(a)", "CREATE TABLE s(x)"],
        &["INSERT INTO s VALUES(3),(1),(2)"],
    );
    let Stmt::Insert {
        table,
        columns,
        source,
        conflict,
    } = parse_one("INSERT INTO t SELECT x FROM s").unwrap()
    else {
        unreachable!()
    };
    let crate::parser::InsertSource::Select(select) = source else {
        unreachable!()
    };
    insert_select(&mut rec, &table, columns.as_deref(), &select, conflict).unwrap();

    let rowids: Vec<i64> = rec.rows().iter().map(|(r, _)| *r).collect();
    assert_eq!(
        rowids,
        vec![1, 2, 3],
        "rowids are assigned in query order, which is insertion order"
    );
    let vals: Vec<Value> = rec.rows().iter().map(|(_, v)| v[0].clone()).collect();
    assert_eq!(
        vals,
        vec![Value::Integer(3), Value::Integer(1), Value::Integer(2)],
        "the values are NOT sorted on the way in"
    );
}

// --- INSERT INTO t SELECT ... FROM t ---------------------------------------

#[test]
fn inserting_a_table_into_itself_terminates_and_sees_only_the_originals() {
    // The case a streaming implementation loops on. sqlite3 3.53.4:
    //
    //   CREATE TABLE t(a); INSERT INTO t VALUES(1),(2),(3);
    //   INSERT INTO t SELECT a+10 FROM t;  SELECT a FROM t;
    //   --> 1 2 3 11 12 13
    //
    // This executes a statement whose FROM *is* the target, against a source
    // table `t` that really holds three rows, and asserts the three values it
    // wrote. It therefore fails if the snapshot is removed: a streaming
    // implementation would either not terminate or produce a frontier longer
    // than three rows.
    //
    // What the fake records is what the module *wrote*, so the pre-existing
    // three are not in `rec.written` -- they live in the source connection,
    // which is what the SELECT read. The end-to-end test
    // `a_self_insert_terminates_and_doubles_the_table` in
    // `tests/insert_select.rs` checks the same thing on a real connection and
    // does see all six rows, including the rowids.
    let mut rec = Recorder::with_seed(&["CREATE TABLE t(a)"], &["INSERT INTO t VALUES(1),(2),(3)"]);
    let Stmt::Insert {
        table,
        columns,
        source,
        conflict,
    } = parse_one("INSERT INTO t SELECT a+10 FROM t").unwrap()
    else {
        unreachable!()
    };
    let crate::parser::InsertSource::Select(select) = source else {
        unreachable!()
    };
    insert_select(&mut rec, &table, columns.as_deref(), &select, conflict).unwrap();

    let got: Vec<String> = (0..rec.rows().len()).map(|i| rec.text(i, 0)).collect();
    assert_eq!(
        got,
        vec!["11", "12", "13"],
        "the shifted originals -- not a frontier that keeps growing"
    );
    assert_eq!(
        rec.rows().len(),
        3,
        "exactly three rows written, then it stops"
    );
    assert_eq!(rec.finished, Some((3, Some(3))), "three rows were inserted");
}

#[test]
fn a_self_insert_reads_the_pre_insert_rows_and_not_its_own_output() {
    // The snapshot, stated as a value rather than as a mechanism: the rows the
    // query saw are the three that were there, so the three it wrote are their
    // sum with ten. If the module re-read the table on each iteration the
    // second row it produced would already be feeding the third.
    let mut rec = Recorder::with_seed(&["CREATE TABLE t(a)"], &["INSERT INTO t VALUES(1),(2),(3)"]);
    let Stmt::Insert {
        table,
        columns,
        source,
        conflict,
    } = parse_one("INSERT INTO t SELECT a+10 FROM t").unwrap()
    else {
        unreachable!()
    };
    let crate::parser::InsertSource::Select(select) = source else {
        unreachable!()
    };
    insert_select(&mut rec, &table, columns.as_deref(), &select, conflict).unwrap();
    // 1+10, 2+10, 3+10 -- each derived from a *pre-existing* row.  If the
    // query had chased its own writes the later values would be 11+10=21 and
    // so on.
    let got: Vec<i64> = (0..rec.rows().len())
        .map(|i| match rec.col(i, 0) {
            Value::Integer(v) => v,
            other => panic!("expected an integer, got {other:?}"),
        })
        .collect();
    assert_eq!(got, vec![11, 12, 13]);
}

#[test]
fn a_self_insert_narrowed_by_a_where_clause_inserts_only_the_matching_originals() {
    // sqlite3 3.53.4: t holds 1,2,3; INSERT INTO t SELECT a+10 FROM t WHERE a>1;
    // then SELECT a FROM t --> 1 2 3 12 13, so two rows were written.
    //
    // Same statement shape, narrowed. A frontier implementation that re-read
    // the table would keep finding a>1 among the rows it just wrote and would
    // not stop at two.
    let mut rec = Recorder::with_seed(&["CREATE TABLE t(a)"], &["INSERT INTO t VALUES(1),(2),(3)"]);
    let Stmt::Insert {
        table,
        columns,
        source,
        conflict,
    } = parse_one("INSERT INTO t SELECT a+10 FROM t WHERE a>1").unwrap()
    else {
        unreachable!()
    };
    let crate::parser::InsertSource::Select(select) = source else {
        unreachable!()
    };
    insert_select(&mut rec, &table, columns.as_deref(), &select, conflict).unwrap();
    let got: Vec<String> = (0..rec.rows().len()).map(|i| rec.text(i, 0)).collect();
    assert_eq!(got, vec!["12", "13"], "only the rows that matched a>1");
}

#[test]
fn a_count_aggregate_over_the_target_sees_the_row_count_before_the_insert() {
    // sqlite3 3.53.4: CREATE TABLE t(a); INSERT INTO t VALUES(1),(2),(3);
    // INSERT INTO t SELECT count(*) FROM t; SELECT count(*) FROM t;  --> 4
    //
    // The query produced 3, not a number that grew as the rows went in, and the
    // table ends with four. This runs against a source table `t` that really
    // holds three rows and whose FROM is the insert target.
    let mut rec = Recorder::with_seed(&["CREATE TABLE t(a)"], &["INSERT INTO t VALUES(1),(2),(3)"]);
    let Stmt::Insert {
        table,
        columns,
        source,
        conflict,
    } = parse_one("INSERT INTO t SELECT count(*) FROM t").unwrap()
    else {
        unreachable!()
    };
    let crate::parser::InsertSource::Select(select) = source else {
        unreachable!()
    };
    insert_select(&mut rec, &table, columns.as_deref(), &select, conflict).unwrap();
    assert_eq!(rec.rows().len(), 1, "one row went in");
    assert_eq!(
        rec.col(0, 0),
        Value::Integer(3),
        "the aggregate saw the three pre-existing rows, not a number that grew"
    );
}

// --- the column list decides the targets ------------------------------------

#[test]
fn a_column_list_reorders_the_values() {
    // sqlite3: INSERT INTO t2(b,a) SELECT x,y FROM b;  SELECT a,b --> 2|1 4|3
    let (rec, _) = run("INSERT INTO t2(b,a) SELECT x,y FROM b").unwrap();
    assert_eq!(rec.col(0, 0), Value::Integer(2), "b's value went to a");
    assert_eq!(rec.col(0, 1), Value::Integer(1), "a's value went to b");
    assert_eq!(rec.col(1, 0), Value::Integer(4));
    assert_eq!(rec.col(1, 1), Value::Integer(3));
}

#[test]
fn a_partial_column_list_leaves_the_rest_to_their_defaults() {
    // sqlite3: CREATE TABLE t3(a,b DEFAULT 7); INSERT INTO t3(a) SELECT x FROM a;
    //          SELECT a,b --> 1|7 2|7 3|7
    let mut rec = Recorder::with(&["CREATE TABLE t3(a,b DEFAULT 7)"]);
    let Stmt::Insert {
        table,
        columns,
        source,
        conflict,
    } = parse_one("INSERT INTO t3(a) SELECT x FROM a").unwrap()
    else {
        unreachable!()
    };
    let crate::parser::InsertSource::Select(select) = source else {
        unreachable!()
    };
    insert_select(&mut rec, &table, columns.as_deref(), &select, conflict).unwrap();
    for (i, _) in rec.rows().iter().enumerate() {
        assert_eq!(rec.col(i, 1), Value::Integer(7), "row {i} took the default");
    }
}

#[test]
fn a_default_expression_is_evaluated_and_the_result_stored() {
    // sqlite3: CREATE TABLE t3(a,b DEFAULT abs(-2));
    //          INSERT INTO t3(a) SELECT x FROM a;  SELECT b --> 2
    //
    // The default is an expression, not a literal, and it is evaluated with no
    // row in scope -- so it produces the same value on every row rather than
    // picking up a column that happens to be nearby.
    //
    // `abs(-2)` is the expression, not the *wording* of the declaration. In
    // SQLite a DEFAULT may be a literal, a bare signed number, or a
    // parenthesised expression; it may NOT be a bare function call. Checked
    // against sqlite3 3.53.4:
    //
    //   CREATE TABLE t(a,b DEFAULT abs(-2));   -> near "(": syntax error
    //   CREATE TABLE t(a,b DEFAULT 2+1);        -> near "+": syntax error
    //   CREATE TABLE t(a,b DEFAULT (1+1));     -> accepted, stores 2
    //   CREATE TABLE t(a,b DEFAULT (abs(-2)));  -> accepted, stores 2
    //
    // so the quoting above is the declaration that both engines accept, and
    // what this test checks is that the module evaluates an expression default
    // rather than storing a bare NULL. See
    // `a_parenthesised_default_expression_is_evaluated_not_dropped` for the
    // shape of the expression the parser hands on.
    let mut rec = Recorder::with(&["CREATE TABLE t3(a,b DEFAULT (abs(-2)))"]);
    let Stmt::Insert {
        table,
        columns,
        source,
        conflict,
    } = parse_one("INSERT INTO t3(a) SELECT x FROM a").unwrap()
    else {
        unreachable!()
    };
    let crate::parser::InsertSource::Select(select) = source else {
        unreachable!()
    };
    insert_select(&mut rec, &table, columns.as_deref(), &select, conflict).unwrap();
    for (i, _) in rec.rows().iter().enumerate() {
        // The default is the expression, not a NULL standing in for it: the
        // parser keeps what is inside the parentheses, so `abs(-2)` arrives
        // here as an expression and this module evaluates it. sqlite3 stores
        // 2 on every row; see
        // `a_parenthesised_default_expression_is_evaluated_not_dropped`.
        assert_eq!(
            rec.col(i, 0),
            Value::Integer(i as i64 + 1),
            "a was supplied"
        );
        assert_eq!(
            rec.col(i, 1),
            Value::Integer(2),
            "row {i} took the default the parser produced"
        );
    }
}

#[test]
fn a_parenthesised_default_expression_is_evaluated_not_dropped() {
    // sqlite3 3.53.4: CREATE TABLE t3(a,b DEFAULT (1+1));
    //               INSERT INTO t3(a) VALUES(5); SELECT a,b --> 5|2
    //
    // This used to record a gap: `DEFAULT (expr)` was parsed as
    // `Constraint::Default(Literal(Null))` -- the parentheses were consumed
    // and the expression inside them dropped, so the catalog stored a NULL
    // where sqlite3 stores the expression's value. That is no longer what
    // the parser does, and this test now pins the closed shape rather than
    // the gap, because a test that only says "this is still broken" cannot
    // tell a fixed engine from an unchanged one.
    let t = table("CREATE TABLE t3(a,b DEFAULT (1+1))");
    assert_eq!(
        t.columns[1].default,
        Some(crate::parser::Expr::Binary {
            op: crate::parser::BinOp::Add,
            left: Box::new(crate::parser::Expr::Literal(crate::parser::Literal::Integer(1))),
            right: Box::new(crate::parser::Expr::Literal(crate::parser::Literal::Integer(1))),
        }),
        "the parser kept the expression instead of dropping it for a NULL"
    );
    // And so the value that reaches the stored row is the expression's,
    // which is what the test above observes end to end.
    let (full, key) = super::build_row(&t, &[Some(0)], &[Value::Integer(5)]).unwrap();
    assert_eq!(full[0], Value::Integer(5));
    assert_eq!(full[1], Value::Integer(2), "sqlite3 stores 2 here");
    // A column-list target that is an ordinary column names no key, so the
    // engine still hands one out.
    assert_eq!(key, None);
}
#[test]
fn a_column_named_twice_keeps_the_first_value() {
    // sqlite3: INSERT INTO t2(a,a) SELECT x,y FROM b;  SELECT a --> 1
    // The second value is dropped, not an error and not the last one.
    let (rec, _) = run("INSERT INTO t2(a,a) SELECT x,y FROM b").unwrap();
    assert_eq!(rec.col(0, 0), Value::Integer(1));
    assert_eq!(rec.col(0, 1), Value::Null, "b was never named");
}

#[test]
fn a_schema_qualifier_on_the_target_is_accepted() {
    // sqlite3: INSERT INTO main.t2(a,b) SELECT x,y FROM b;  succeeds
    let (rec, out) = run("INSERT INTO main.t2(a,b) SELECT x,y FROM b").unwrap();
    assert_eq!(out, Outcome::Changed(2));
    assert_eq!(rec.rows().len(), 2);
}

// --- the count check --------------------------------------------------------

#[test]
fn too_few_values_with_a_column_list_says_one_values_for_n_columns() {
    // sqlite3: CREATE TABLE t2(a,b); INSERT INTO t2(a,b) SELECT x FROM b;
    //          Parse error: 1 values for 2 columns
    let e = run("INSERT INTO t2(a,b) SELECT x FROM b").unwrap_err();
    assert_eq!(e.message, "1 values for 2 columns");
    assert_eq!(e.code, ResultCode::Error);
}

#[test]
fn too_many_values_with_a_column_list_counts_the_written_columns() {
    // sqlite3: INSERT INTO t2(a,b) SELECT x,y,x FROM b;
    //          Parse error: 3 values for 2 columns
    let e = run("INSERT INTO t2(a,b) SELECT x,y,x FROM b").unwrap_err();
    assert_eq!(e.message, "3 values for 2 columns");
}

#[test]
fn too_few_values_without_a_column_list_names_the_table() {
    // sqlite3: CREATE TABLE t2(a,b); INSERT INTO t2 SELECT x FROM b;
    //          Parse error: table t2 has 2 columns but 1 values were supplied
    let e = run("INSERT INTO t2 SELECT x FROM b").unwrap_err();
    assert_eq!(
        e.message,
        "table t2 has 2 columns but 1 values were supplied"
    );
    assert_eq!(e.code, ResultCode::Error);
}

#[test]
fn too_many_values_without_a_column_list_names_the_table() {
    // sqlite3: CREATE TABLE t(a); INSERT INTO t SELECT x,y FROM b;
    //          Parse error: table t has 1 columns but 2 values were supplied
    let e = run("INSERT INTO t SELECT x,y FROM b").unwrap_err();
    assert_eq!(
        e.message,
        "table t has 1 columns but 2 values were supplied"
    );
}

#[test]
fn a_mismatch_is_reported_even_when_the_query_matches_no_rows() {
    // sqlite3: CREATE TABLE t2(a,b); CREATE TABLE s(x);   -- s is empty
    //          INSERT INTO t2(a,b) SELECT x FROM s;
    //          Parse error: 1 values for 2 columns
    //
    // This is the case that requires materialising the SELECT before the
    // first insert: with no rows there is nothing to measure unless the
    // projection's width is known, and sqlite3 knows it because it ran the
    // SELECT once already.
    let e = run("INSERT INTO t2(a,b) SELECT x FROM b WHERE 0").unwrap_err();
    assert_eq!(e.message, "1 values for 2 columns");
}

#[test]
fn a_star_counts_by_the_columns_it_expanded_to() {
    // sqlite3: CREATE TABLE t(a); CREATE TABLE d(x,y,z);
    //          INSERT INTO t SELECT * FROM d;
    //          Parse error: table t has 1 columns but 3 values were supplied
    let e = run("INSERT INTO t SELECT * FROM d").unwrap_err();
    assert_eq!(
        e.message,
        "table t has 1 columns but 3 values were supplied"
    );
}

#[test]
fn a_star_with_a_column_list_takes_the_named_wording() {
    // sqlite3: CREATE TABLE t(a); CREATE TABLE d(x,y,z);
    //          INSERT INTO t(a) SELECT * FROM d;
    //          Parse error: 3 values for 1 columns
    let e = run("INSERT INTO t(a) SELECT * FROM d").unwrap_err();
    assert_eq!(e.message, "3 values for 1 columns");
}

#[test]
fn the_count_check_accepts_a_matching_shape() {
    // The positive case, so the check cannot pass by rejecting everything.
    //
    // Driven through real statements rather than by calling the helper, so
    // this is not merely asserting that the function returns Ok for equal
    // integers: a statement whose shape matches has to go all the way to the
    // write loop.
    let (rec, out) = run("INSERT INTO t2(a,b) SELECT x,y FROM b").unwrap();
    assert_eq!(out, Outcome::Changed(2), "two rows written, not an error");
    assert_eq!(rec.rows().len(), 2);
}

// --- resolution order -------------------------------------------------------

#[test]
fn an_unknown_column_in_the_column_list_is_reported_before_the_query_runs() {
    // sqlite3: CREATE TABLE t(a); CREATE TABLE s(x);
    //          INSERT INTO t(nosuch) SELECT nosuchcol FROM s;
    //          Parse error: table t has no column named nosuch
    //
    // The target wins over the source, so the target is resolved first and the
    // query is never run.
    let e = run("INSERT INTO t(nosuch) SELECT x FROM b").unwrap_err();
    assert_eq!(e.message, "table t has no column named nosuch");
}

#[test]
fn an_unknown_target_table_is_reported() {
    // sqlite3: INSERT INTO nosucht(x) SELECT x FROM s;
    //          Parse error: no such table: nosucht
    let e = run("INSERT INTO nosucht(x) SELECT x FROM b").unwrap_err();
    assert_eq!(e.message, "no such table: nosucht");
}

#[test]
fn the_error_quotes_the_name_the_statement_wrote() {
    // sqlite3: INSERT INTO main.t(nosuch) SELECT x FROM s;
    //          Parse error: table main.t has no column named nosuch
    let e = run("INSERT INTO main.t(nosuch) SELECT x FROM b").unwrap_err();
    assert_eq!(e.message, "table main.t has no column named nosuch");
}

// --- NOT NULL ---------------------------------------------------------------

#[test]
fn a_null_in_a_not_null_column_fails_with_sqlites_wording() {
    // sqlite3 3.53.4: CREATE TABLE t(a NOT NULL); CREATE TABLE s(x);
    //          INSERT INTO s VALUES(1),(NULL),(3);
    //          INSERT INTO t SELECT x FROM s;
    //          Error: NOT NULL constraint failed: t.a
    //
    // Driven through a statement, so what is checked is the error the caller
    // sees rather than the error one internal call produces. The extended code
    // is 1299 (SQLITE_CONSTRAINT_NOTNULL), read off the C API rather than
    // recalled.
    let mut rec = Recorder::with_seed(
        &["CREATE TABLE t(a NOT NULL)"],
        &["INSERT INTO s VALUES(1),(NULL),(3)"],
    );
    let Stmt::Insert {
        table,
        columns,
        source,
        conflict,
    } = parse_one("INSERT INTO t SELECT x FROM s").unwrap()
    else {
        unreachable!()
    };
    let crate::parser::InsertSource::Select(select) = source else {
        unreachable!()
    };
    let err = insert_select(&mut rec, &table, columns.as_deref(), &select, conflict).unwrap_err();
    assert_eq!(err.message, "NOT NULL constraint failed: t.a");
    assert_eq!(err.code, ResultCode::Constraint);
    assert_eq!(err.extended_code(), 1299);
}

#[test]
fn a_not_null_column_the_statement_never_named_is_still_checked() {
    // sqlite3 3.53.4: CREATE TABLE t(a NOT NULL, b); CREATE TABLE s(x);
    //          INSERT INTO t(b) SELECT x FROM s;
    //          Error: NOT NULL constraint failed: t.a
    //
    // The check runs on the finished row, after the default has been filled in,
    // so a column that was never supplied is still held to its NOT NULL. A
    // source that matched no rows would be the vacuous case; this one supplies
    // a real row, so the failure is real.
    let mut rec = Recorder::with_seed(
        &["CREATE TABLE t(a NOT NULL, b)"],
        &["INSERT INTO s VALUES(1)"],
    );
    let Stmt::Insert {
        table,
        columns,
        source,
        conflict,
    } = parse_one("INSERT INTO t(b) SELECT x FROM s").unwrap()
    else {
        unreachable!()
    };
    let crate::parser::InsertSource::Select(select) = source else {
        unreachable!()
    };
    let err = insert_select(&mut rec, &table, columns.as_deref(), &select, conflict).unwrap_err();
    assert_eq!(
        err.message, "NOT NULL constraint failed: t.a",
        "the unnamed NOT NULL column is the one named"
    );
}

#[test]
fn a_not_null_column_with_a_default_does_not_fail() {
    // sqlite3 3.53.4: CREATE TABLE t(a NOT NULL DEFAULT 9, b);
    //          INSERT INTO t(b) SELECT x FROM s;  succeeds, and a is 9
    //
    // The default is filled in before the check runs, so a NOT NULL column
    // with a default is satisfied by its own default rather than failing.
    let mut rec = Recorder::with_seed(
        &["CREATE TABLE t(a NOT NULL DEFAULT 9, b)"],
        &["INSERT INTO s VALUES(1),(2)"],
    );
    let Stmt::Insert {
        table,
        columns,
        source,
        conflict,
    } = parse_one("INSERT INTO t(b) SELECT x FROM s").unwrap()
    else {
        unreachable!()
    };
    let crate::parser::InsertSource::Select(select) = source else {
        unreachable!()
    };
    insert_select(&mut rec, &table, columns.as_deref(), &select, conflict).unwrap();
    assert_eq!(rec.rows().len(), 2, "both rows went in");
    for i in 0..2 {
        assert_eq!(rec.col(i, 0), Value::Integer(9), "row {i} took the default");
        assert_eq!(rec.col(i, 1), Value::Integer(i as i64 + 1));
    }
}

#[test]
fn a_not_null_column_that_is_always_null_still_fails_on_every_row() {
    // The failure is raised for the row that is actually NULL, not only the
    // first. A source of (NULL),(NULL) has no valid row at all, so the
    // statement fails on its first row and writes nothing.
    let mut rec = Recorder::with_seed(
        &["CREATE TABLE t(a NOT NULL)"],
        &["INSERT INTO s VALUES(NULL),(NULL)"],
    );
    let Stmt::Insert {
        table,
        columns,
        source,
        conflict,
    } = parse_one("INSERT INTO t SELECT x FROM s").unwrap()
    else {
        unreachable!()
    };
    let crate::parser::InsertSource::Select(select) = source else {
        unreachable!()
    };
    let err = insert_select(&mut rec, &table, columns.as_deref(), &select, conflict).unwrap_err();
    assert_eq!(err.code, ResultCode::Constraint);
    assert!(
        rec.rows().is_empty(),
        "the very first row was NULL, so nothing was written"
    );
}

// --- affinity ---------------------------------------------------------------

#[test]
fn affinity_is_applied_to_every_column_it_can_reach() {
    // sqlite3 3.53.4: CREATE TABLE t(a NUMERIC, b TEXT, c REAL, d BLOB);
    //          CREATE TABLE s(x,y,z,w);
    //          INSERT INTO s VALUES('123','456.0',789,x'414243');
    //          INSERT INTO t SELECT x,y,z,w FROM s;
    //          SELECT typeof(a),typeof(b),typeof(c),typeof(d)
    //            --> integer|text|real|blob
    //
    // All four affinities in one statement, read back out of the rows the
    // module actually wrote.
    let mut rec = Recorder::with_seed(
        &["CREATE TABLE t(a NUMERIC, b TEXT, c REAL, d BLOB)"],
        &[
            // The four-column source. The shared fixture's widest table is
            // three columns, so this one is made here.
            "CREATE TABLE s4(x,y,z,w)",
            "INSERT INTO s4 VALUES('123','456.0',789,x'414243')",
        ],
    );
    let Stmt::Insert {
        table,
        columns,
        source,
        conflict,
    } = parse_one("INSERT INTO t SELECT x,y,z,w FROM s4").unwrap()
    else {
        unreachable!()
    };
    let crate::parser::InsertSource::Select(select) = source else {
        unreachable!()
    };
    insert_select(&mut rec, &table, columns.as_deref(), &select, conflict).unwrap();
    assert_eq!(rec.rows().len(), 1);
    assert_eq!(
        rec.col(0, 0),
        Value::Integer(123),
        "NUMERIC promotes a numeric string"
    );
    assert_eq!(
        rec.col(0, 1),
        Value::Text("456.0".into()),
        "TEXT keeps the text"
    );
    assert_eq!(
        rec.col(0, 2),
        Value::Real(789.0),
        "REAL always stores a real"
    );
    assert_eq!(
        rec.col(0, 3),
        Value::Blob(b"ABC".to_vec()),
        "BLOB keeps the bytes"
    );
}

#[test]
fn a_text_value_that_is_not_a_number_keeps_its_text_under_numeric_affinity() {
    // sqlite3 3.53.4: NUMERIC column, 'abc' in --> typeof(a) --> text
    let mut rec = Recorder::with_seed(
        &["CREATE TABLE t(a NUMERIC)"],
        &["INSERT INTO s VALUES('abc')"],
    );
    let Stmt::Insert {
        table,
        columns,
        source,
        conflict,
    } = parse_one("INSERT INTO t SELECT x FROM s").unwrap()
    else {
        unreachable!()
    };
    let crate::parser::InsertSource::Select(select) = source else {
        unreachable!()
    };
    insert_select(&mut rec, &table, columns.as_deref(), &select, conflict).unwrap();
    assert_eq!(rec.col(0, 0), Value::Text("abc".into()));
}

#[test]
fn affinity_agrees_with_what_the_ordinary_insert_path_produces() {
    // The suite leans on INSERT ... SELECT and on VALUES interchangeably, so
    // the two have to agree. Each value goes in as a single-column SELECT into
    // a NUMERIC column, and the stored result is compared against what the
    // shared `apply_affinity` call produces for the same input -- which is the
    // call the connection's own VALUES path makes.
    let inputs = ["'123'", "'456.0'", "789", "1.5", "'abc'", "x'414243'", "''"];
    for literal in inputs {
        let rec = {
            let mut r = Recorder::with_seed(&["CREATE TABLE t(a NUMERIC)"], &[]);
            let sql = format!("INSERT INTO t SELECT {literal}");
            let Stmt::Insert {
                table,
                columns,
                source,
                conflict,
            } = parse_one(&sql).unwrap()
            else {
                unreachable!()
            };
            let crate::parser::InsertSource::Select(select) = source else {
                unreachable!()
            };
            insert_select(&mut r, &table, columns.as_deref(), &select, conflict)
                .unwrap_or_else(|e| panic!("{sql} failed: {e}"));
            r
        };
        let stored = rec.col(0, 0);
        let expected = crate::affinity::apply(&stored, Affinity::Numeric);
        assert_eq!(
            stored, expected,
            "NUMERIC affinity was not idempotent for {literal}"
        );
    }
}

// --- the atomicity note -----------------------------------------------------

#[test]
fn the_atomicity_note_states_what_the_engine_actually_does() {
    // The note is the deliverable for statement-level atomicity, so what it
    // claims is checked rather than left as prose. Each phrase below is
    // something this engine was *measured* to do, and each has a test in
    // `tests/insert_select.rs` that would fail if it stopped being true:
    //
    //   - the refusals that leave nothing behind               ->
    //     `a_not_null_failure_part_way_leaves_nothing_behind`
    //   - the duplicate rowid that still does leave rows     ->
    //     `a_duplicate_rowid_still_leaves_the_earlier_rows_behind`
    //   - the 2499-row claim not reproducing                   ->
    //     `a_failed_large_insert_leaves_nothing_on_disk`
    //   - BEGIN failing on a fresh file                   ->
    //     `a_transaction_cannot_be_opened_on_a_database_file_that_does_not_exist_yet`
    //   - a source error landing before any write          ->
    //     `an_error_in_the_source_query_is_reported_and_nothing_is_written`
    //
    // The note is wrapped for readability, so the phrases are matched after
    // collapsing the whitespace -- otherwise a reflow would silently break the
    // test rather than the promise.
    let flat = ATOMICITY.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(
        flat.contains("BEGIN;"),
        "the caller is told what the transaction route is"
    );
    assert!(flat.contains("no savepoints"), "the limitation is named");
    // The two halves of the current behaviour, both of which have to be stated:
    // the failures it now refuses, and the one it still cannot undo.
    assert!(
        flat.contains("REFUSED, leaving 0 rows"),
        "the refusals are stated, so a reader does not assume every failure leaks"
    );
    assert!(
        flat.contains("NOT UNDONE, leaving the earlier rows behind"),
        "the case that still leaks is stated plainly, with its own cause"
    );
    assert!(
        flat.contains("duplicate rowid cannot be found without writing"),
        "and the reason it cannot be fixed by rearranging the same code is named"
    );
    // The note must not promise a guarantee the engine cannot make. These are
    // the two that the old wording asserted and the measurements refute.
    assert!(
        !flat.contains("A constraint failure part way through IS undone"),
        "this engine does NOT undo a constraint failure; the note must not say it does"
    );
    assert!(
        !flat.contains("A runtime error part way through is NOT undone"),
        "the source query runs to completion, so no error arrives mid-insert; \
         the note must not attribute a partial write to one"
    );
    // The two limitations that are real, and that the old note hid.
    assert!(
        flat.contains("an in-memory database cannot be journalled"),
        "the fresh-file BEGIN failure is named, because it is what makes the advice unusable"
    );
    assert!(
        flat.contains("442"),
        "the count that does not reproduce is quoted, so a reader can see it was checked"
    );
}

#[test]
fn a_constraint_failure_names_the_table_and_the_column() {
    // The wording every constraint failure in this path shares.
    let t = table("CREATE TABLE t(a NOT NULL)");
    let err = super::build_row(&t, &[Some(0)], &[Value::Null]).unwrap_err();
    assert_eq!(err.message, "NOT NULL constraint failed: t.a");
    assert_eq!(err.extended, 1299);
}

// --- the shape of the statement ---------------------------------------------

#[test]
fn a_where_clause_narrows_what_is_inserted() {
    // sqlite3: INSERT INTO t SELECT x FROM a WHERE x>1;  changes() --> 2
    let n = changed("INSERT INTO t SELECT x FROM a WHERE x>1").unwrap();
    assert_eq!(n, 2);
}

#[test]
fn an_order_by_and_a_limit_decide_what_is_inserted() {
    // sqlite3: source (1,2,3);
    //          INSERT INTO t SELECT x FROM a ORDER BY x DESC LIMIT 2;
    //          SELECT a FROM t --> 3 2 (descending, two rows)
    let (rec, _) = run("INSERT INTO t SELECT x FROM a ORDER BY x DESC LIMIT 2").unwrap();
    assert_eq!(rec.rows().len(), 2);
    assert_eq!(rec.col(0, 0), Value::Integer(3));
    assert_eq!(rec.col(1, 0), Value::Integer(2));
}

#[test]
fn the_limit_is_applied_before_the_rows_are_inserted() {
    // sqlite3: ... ORDER BY x DESC LIMIT 2 on a 3-row source --> 2 rows
    // A LIMIT applied after the insert would have inserted all three first,
    // which is what makes this an INSERT of two rows and not a delete.
    let n = changed("INSERT INTO t SELECT x FROM a ORDER BY x DESC LIMIT 2").unwrap();
    assert_eq!(n, 2);
}

#[test]
fn an_ordinary_select_of_one_column_inserts_one_column() {
    // sqlite3: CREATE TABLE t2(a,b); INSERT INTO t2(a) SELECT x FROM a;
    //          changes() --> 3, and b is NULL for all three
    let (rec, out) = run("INSERT INTO t2(a) SELECT x FROM a").unwrap();
    assert_eq!(out, Outcome::Changed(3));
    for i in 0..3 {
        assert_eq!(rec.col(i, 1), Value::Null, "row {i} left b alone");
    }
}

#[test]
fn a_projection_that_changes_a_value_does_not_change_its_type_by_accident() {
    // sqlite3: INSERT INTO t SELECT x*2 FROM a;  SELECT a FROM t --> 2 4 6
    let (rec, _) = run("INSERT INTO t SELECT x*2 FROM a").unwrap();
    assert_eq!(rec.col(0, 0), Value::Integer(2));
    assert_eq!(rec.col(1, 0), Value::Integer(4));
    assert_eq!(rec.col(2, 0), Value::Integer(6));
}

#[test]
fn a_null_from_the_source_is_stored_as_null() {
    // sqlite3: INSERT INTO t SELECT NULL;  typeof(a), a IS NULL --> null|1
    let (rec, _) = run("INSERT INTO t SELECT NULL").unwrap();
    assert_eq!(rec.col(0, 0), Value::Null);
}

// --- the shared count error --------------------------------------------------

#[test]
fn the_count_error_is_sqlite_error_in_both_wordings() {
    // The result code is the part that had drifted, so it is pinned for both
    // wordings rather than only for the one the SELECT path happened to use.
    // Read off Python's `sqlite3`, which reports SQLITE_ERROR for every count
    // mismatch in both INSERT forms:
    //   INSERT INTO q(a,b) VALUES(1)     -> 1 values for 2 columns            (SQLITE_ERROR)
    //   INSERT INTO q VALUES(1)          -> table q has 2 columns but 1 ...   (SQLITE_ERROR)
    let named = count_error("q", true, 1, 2);
    assert_eq!(named.message, "1 values for 2 columns");
    assert_eq!(named.code, ResultCode::Error);

    let bare = count_error("q", false, 1, 2);
    assert_eq!(
        bare.message,
        "table q has 2 columns but 1 values were supplied"
    );
    assert_eq!(bare.code, ResultCode::Error);

    // Not SQLITE_MISMATCH (20), which is what the VALUES path used to raise and
    // what makes the two forms of one logical error report different codes.
    assert_ne!(named.code, ResultCode::Mismatch);
    assert_ne!(bare.code, ResultCode::Mismatch);
}

#[test]
fn the_count_error_counts_the_targets_not_the_table() {
    // What selects the wording is whether a column list was *written*, not how
    // many columns the table has. A table of three written as `q(a,b)` against
    // three values is `3 values for 2 columns`, not a message about three
    // columns. Checked against 3.53.4:
    //   INSERT INTO q(a,b) VALUES(1,2,3)  -> 3 values for 2 columns
    let e = count_error("q", true, 3, 2);
    assert_eq!(e.message, "3 values for 2 columns");
    // And with no list the same shape names the table and its true width.
    let e = count_error("q", false, 3, 3);
    assert_eq!(
        e.message,
        "table q has 3 columns but 3 values were supplied"
    );
}

#[test]
fn the_count_error_quotes_the_name_the_statement_wrote() {
    // A schema qualifier is stripped to find the table but kept in the message,
    // because that is what the statement wrote. Matches the `target_table` half
    // of the trait, which is measured the same way in
    // `a_schema_qualifier_on_the_target_is_accepted`.
    let e = count_error("main.q", false, 1, 2);
    assert_eq!(
        e.message,
        "table main.q has 2 columns but 1 values were supplied"
    );
}
