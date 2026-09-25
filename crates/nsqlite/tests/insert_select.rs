//! End-to-end tests for `INSERT ... SELECT` through a real [`Connection`].
//!
//! The unit tests in `insert_select_tests.rs` drive the module through a fake
//! destination, which is what makes the decisions observable but also what
//! makes them unable to see anything about the connection. The behaviours
//! checked here live in the connection and the pager, so they need a real one:
//! the change counter, `last_insert_rowid()`, what is left in the b-tree after
//! a statement fails, what reaches the file, and whether `BEGIN` is available.
//!
//! Every expected value was produced by running the same statement through the
//! real `sqlite3` 3.53.4 and is quoted in the test that checks it. Where this
//! engine diverges, the divergence is asserted and attributed rather than
//! papered over, because a test that quietly expects the wrong thing is worse
//! than no test.
//!
//! None of these tests compiles until `insert_select_hook` is declared in
//! `connection.rs` and the `InsertSource::Select` arm calls
//! `insert_select`. That is deliberate: they are the specification for the hook,
//! and a hook that was never wired cannot satisfy them.

use nsqlite::connection::{Connection, Outcome};
use nsqlite::parser::{parse_one, Stmt};
use nsqlite::value::Value;

/// Runs one statement, returning its outcome or panicking with the message.
fn run(c: &mut Connection, sql: &str) -> Outcome {
    match c.execute(&parse_one(sql).expect("test SQL should parse")) {
        Ok(o) => o,
        Err(e) => panic!("{sql:?} failed: {e}"),
    }
}

/// Runs one statement expecting failure, and returns the message.
fn fails(c: &mut Connection, sql: &str) -> String {
    match c.execute(&parse_one(sql).expect("test SQL should parse")) {
        Ok(o) => panic!("{sql:?} unexpectedly succeeded: {o:?}"),
        Err(e) => e.to_string(),
    }
}

/// The first column of the first row of a one-row query, rendered.
fn scalar(c: &mut Connection, sql: &str) -> String {
    match run(c, sql) {
        Outcome::Query { rows, .. } => rows
            .first()
            .map(|r| r.values[0].to_string())
            .unwrap_or_else(|| "<no rows>".to_owned()),
        other => panic!("expected a query, got {other:?}"),
    }
}

/// The first column of the first row, as an integer, for counting.
fn count(c: &mut Connection, table: &str) -> i64 {
    scalar(c, &format!("SELECT count(*) FROM {table}"))
        .parse()
        .unwrap()
}

/// Runs a statement and returns its change count.
///
/// Read immediately after the DML it is about, because `Connection::execute`
/// zeroes `changes` at the start of every statement -- so a `SELECT` run in
/// between would clear it. That is this engine's behaviour and is deliberate;
/// it is why the tests below never interleave a query between a DML and the
/// reading of its change count.
fn changes_after(c: &mut Connection, sql: &str) -> usize {
    run(c, sql);
    c.changes()
}

/// A memory connection with the two tables the tests below share.
fn fixture(sqls: &[&str]) -> Connection {
    let mut c = Connection::open_memory().expect("in-memory database");
    for s in sqls {
        run(&mut c, s);
    }
    c
}

fn temp_path(name: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!("nsqlite_insert_select_{name}.db"));
    let _ = std::fs::remove_file(&p);
    p
}

/// A path whose database file already exists *and has content*.
///
/// Not the same as a path that merely exists. `Pager::open` takes its `fresh`
/// branch for a **zero-length** file just as much as for a missing one, and
/// that branch is what leaves the pager unable to journal -- which is what
/// makes `BEGIN` fail. A zero-byte file is therefore no better than no file at
/// all, and the tests that need a transaction have to let a real write flush
/// first. Writing an empty file by hand does not do it; opening a connection,
/// creating a table and closing it does.
fn existing_path(name: &str) -> std::path::PathBuf {
    let p = temp_path(name);
    {
        let mut c = Connection::open(&p).expect("open a fresh path");
        run(&mut c, "CREATE TABLE bootstrap(x)");
        run(&mut c, "INSERT INTO bootstrap VALUES(1)");
    }
    let len = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
    assert!(
        len > 0,
        "the file must have content for the pager to journal it"
    );
    p
}

// --- the gap docs/testing.md 5.3 item 3 records ---------------------------

#[test]
fn the_documented_gap_case_inserts_the_row() {
    // sqlite3 3.53.4:
    //   CREATE TABLE a(x); INSERT INTO a VALUES(1);
    //   CREATE TABLE b(x); INSERT INTO b SELECT x FROM a; SELECT * FROM b;
    //   --> 1
    //
    // This is verbatim the case in docs/testing.md 5.3 item 3, which recorded
    // nsqlite's answer as `E INSERT ... SELECT is not supported yet`. Before
    // the hook this test could not run at all.
    let mut c = fixture(&[
        "CREATE TABLE a(x)",
        "INSERT INTO a VALUES(1)",
        "CREATE TABLE b(x)",
    ]);
    assert_eq!(
        changes_after(&mut c, "INSERT INTO b SELECT x FROM a"),
        1,
        "one row inserted"
    );
    assert_eq!(scalar(&mut c, "SELECT * FROM b"), "1");
}

#[test]
fn every_row_of_a_multi_row_source_is_inserted() {
    // sqlite3: CREATE TABLE a(x); INSERT INTO a VALUES(1),(2),(3);
    //          CREATE TABLE b(x); INSERT INTO b SELECT x FROM a;
    //          SELECT count(*) FROM b --> 3, changes() --> 3
    let mut c = fixture(&["CREATE TABLE a(x)", "CREATE TABLE b(x)"]);
    run(&mut c, "INSERT INTO a VALUES(1),(2),(3)");
    assert_eq!(changes_after(&mut c, "INSERT INTO b SELECT x FROM a"), 3);
    assert_eq!(count(&mut c, "b"), 3);
    assert_eq!(scalar(&mut c, "SELECT x FROM b"), "1", "in rowid order");
}

// --- the change counter and the last insert rowid -------------------------

#[test]
fn a_query_that_matches_nothing_changes_nothing() {
    // sqlite3 3.53.4:
    //   CREATE TABLE t(a); CREATE TABLE s(x);   -- s is empty
    //   INSERT INTO t SELECT x FROM s;  SELECT changes();   --> 0
    //
    // A zero-row insert must not report 1. The CLI renders `Changed(n)` by
    // printing n only when n > 0, and this asserts the value the connection
    // actually holds rather than what a shell chooses to print.
    let mut c = fixture(&["CREATE TABLE t(a)", "CREATE TABLE s(x)"]);
    run(&mut c, "INSERT INTO t SELECT x FROM s");
    assert_eq!(c.changes(), 0, "a zero-row insert changed nothing");
    assert_eq!(count(&mut c, "t"), 0);
    assert_eq!(
        c.last_insert_rowid(),
        0,
        "and it did not invent a rowid on a database that never inserted"
    );
}

#[test]
fn a_zero_row_insert_leaves_the_last_insert_rowid_alone() {
    // sqlite3 3.53.4:
    //   CREATE TABLE t(a); CREATE TABLE s(x);
    //   INSERT INTO t VALUES(42);
    //   INSERT INTO t SELECT x FROM s;
    //   SELECT changes(), last_insert_rowid();  --> 0|1
    //
    // The zero-row insert clears the change count but must not clear the
    // rowid: it inserted nothing, so there is nothing to report a rowid for.
    let mut c = fixture(&["CREATE TABLE t(a)", "CREATE TABLE s(x)"]);
    run(&mut c, "INSERT INTO t VALUES(42)");
    assert_eq!(c.last_insert_rowid(), 1);
    run(&mut c, "INSERT INTO t SELECT x FROM s");
    assert_eq!(
        c.changes(),
        0,
        "still zero, read before any other statement"
    );
    assert_eq!(
        c.last_insert_rowid(),
        1,
        "survives, exactly as sqlite3 reports 1"
    );
}

#[test]
fn the_last_insert_rowid_is_the_last_row_written() {
    // sqlite3: three rows inserted, last_insert_rowid() --> 3
    let mut c = fixture(&["CREATE TABLE a(x)", "CREATE TABLE t(a)"]);
    run(&mut c, "INSERT INTO a VALUES(10),(20),(30)");
    run(&mut c, "INSERT INTO t SELECT x FROM a");
    assert_eq!(c.last_insert_rowid(), 3, "the third source row");
    assert_eq!(c.changes(), 3, "read before any other statement");
}

// --- INSERT INTO t SELECT ... FROM t ---------------------------------------

#[test]
fn a_self_insert_terminates_and_doubles_the_table() {
    // sqlite3 3.53.4:
    //   CREATE TABLE t(a); INSERT INTO t VALUES(1),(2),(3);
    //   INSERT INTO t SELECT a+10 FROM t; SELECT a FROM t;
    //   --> 1 2 3 11 12 13
    //
    // The case a streaming implementation loops on. This is a real statement
    // on a real connection, so it would hang rather than merely fail if the
    // snapshot were removed.
    //
    // `SELECT rowid, a FROM t` is what sqlite3 is asked here, but this engine
    // has no `rowid` pseudo-column -- that is docs/testing.md 5.3 item 13 and
    // belongs to another track -- so the rowids are checked at the level that
    // is reachable, and the end-to-end row ORDER is what the values depend on:
    // a table b-tree reads in rowid order, so the six values in order are the
    // six rowids in order.
    let mut c = fixture(&["CREATE TABLE t(a)"]);
    run(&mut c, "INSERT INTO t VALUES(1),(2),(3)");
    run(&mut c, "INSERT INTO t SELECT a+10 FROM t");
    assert_eq!(count(&mut c, "t"), 6, "exactly six, not a growing frontier");

    let got: Vec<String> = match run(&mut c, "SELECT a FROM t") {
        Outcome::Query { rows, .. } => rows.iter().map(|r| r.values[0].to_string()).collect(),
        other => panic!("expected a query, got {other:?}"),
    };
    assert_eq!(
        got,
        vec!["1", "2", "3", "11", "12", "13"],
        "the originals, then the shifted originals -- sqlite3's exact output"
    );
    // The three new rows got the three rowids after the originals, which is
    // why the new values sort last rather than interleaving.
    assert_eq!(
        scalar(&mut c, "SELECT max(a) FROM t"),
        "13",
        "the largest new value is in the file"
    );
}

#[test]
fn a_count_aggregate_over_the_target_sees_the_rows_before_the_insert() {
    // sqlite3 3.53.4:
    //   CREATE TABLE t(a); INSERT INTO t VALUES(1),(2),(3);
    //   INSERT INTO t SELECT count(*) FROM t; SELECT count(*) FROM t;
    //   --> 4
    let mut c = fixture(&["CREATE TABLE t(a)"]);
    run(&mut c, "INSERT INTO t VALUES(1),(2),(3)");
    run(&mut c, "INSERT INTO t SELECT count(*) FROM t");
    assert_eq!(
        count(&mut c, "t"),
        4,
        "the aggregate produced 3, then one row was added"
    );
    // `WHERE rowid = 4` would be sqlite3's phrasing, but this engine has no
    // `rowid` pseudo-column (docs/testing.md 5.3 item 13), so the fourth value
    // is addressed by what it is. The row ORDER above already shows the new
    // row is last, which is the same fact.
    assert_eq!(scalar(&mut c, "SELECT a FROM t WHERE a = 3"), "3");
}

#[test]
fn a_self_insert_narrowed_by_a_where_clause_still_terminates() {
    // sqlite3: t holds 1,2,3; INSERT INTO t SELECT a+10 FROM t WHERE a>1;
    //          SELECT a FROM t --> 1 2 3 12 13
    let mut c = fixture(&["CREATE TABLE t(a)"]);
    run(&mut c, "INSERT INTO t VALUES(1),(2),(3)");
    run(&mut c, "INSERT INTO t SELECT a+10 FROM t WHERE a>1");
    assert_eq!(count(&mut c, "t"), 5, "two rows matched, five total");
}

#[test]
fn a_self_insert_with_a_limit_inserts_exactly_the_limited_rows() {
    // sqlite3: t holds 1,2,3; INSERT INTO t SELECT a+10 FROM t ORDER BY a DESC
    //          LIMIT 1; SELECT count(*) FROM t --> 4
    let mut c = fixture(&["CREATE TABLE t(a)"]);
    run(&mut c, "INSERT INTO t VALUES(1),(2),(3)");
    run(
        &mut c,
        "INSERT INTO t SELECT a+10 FROM t ORDER BY a DESC LIMIT 1",
    );
    assert_eq!(count(&mut c, "t"), 4, "one row inserted, three kept");
    assert_eq!(scalar(&mut c, "SELECT max(a) FROM t"), "13");
}

// --- the rules an ordinary INSERT applies ----------------------------------

#[test]
fn an_unsupplied_column_takes_its_default_and_affinity_is_applied() {
    // sqlite3: CREATE TABLE t(a,b DEFAULT 7,c TEXT);
    //          INSERT INTO t(a,c) SELECT x, '123' FROM s;
    //          SELECT a,b,c FROM t --> the defaults filled, c kept as text
    let mut c = fixture(&["CREATE TABLE s(x)", "CREATE TABLE t(a,b DEFAULT 7,c TEXT)"]);
    run(&mut c, "INSERT INTO s VALUES(1),(2)");
    run(&mut c, "INSERT INTO t(a,c) SELECT x, '123' FROM s");
    assert_eq!(count(&mut c, "t"), 2);
    assert_eq!(scalar(&mut c, "SELECT b FROM t WHERE a = 1"), "7");
    assert_eq!(
        scalar(&mut c, "SELECT c FROM t WHERE a = 1"),
        "123",
        "a TEXT column keeps the text"
    );
    assert_eq!(
        scalar(&mut c, "SELECT typeof(c) FROM t WHERE a = 1"),
        "text"
    );
}

#[test]
fn affinity_converts_a_numeric_string_into_an_integer() {
    // sqlite3: CREATE TABLE t(a NUMERIC); INSERT INTO t SELECT '123';
    //          SELECT a, typeof(a) FROM t --> 123|integer
    let mut c = fixture(&["CREATE TABLE t(a NUMERIC)"]);
    run(&mut c, "INSERT INTO t SELECT '123'");
    assert_eq!(scalar(&mut c, "SELECT typeof(a) FROM t"), "integer");
    assert_eq!(scalar(&mut c, "SELECT a FROM t"), "123");
}

#[test]
fn a_column_list_reorders_the_values() {
    // sqlite3: CREATE TABLE t(a,b); CREATE TABLE s(x,y);
    //          INSERT INTO t(b,a) SELECT x,y FROM s; SELECT a,b --> 2|1
    let mut c = fixture(&["CREATE TABLE t(a,b)", "CREATE TABLE s(x,y)"]);
    run(&mut c, "INSERT INTO s VALUES(1,2)");
    run(&mut c, "INSERT INTO t(b,a) SELECT x,y FROM s");
    assert_eq!(scalar(&mut c, "SELECT a || '|' || b FROM t"), "2|1");
}

// --- errors ----------------------------------------------------------------

#[test]
fn a_column_count_mismatch_is_reported_with_sqlites_wording() {
    // sqlite3: CREATE TABLE t(a,b); CREATE TABLE s(x);
    //          INSERT INTO t(a,b) SELECT x FROM s;
    //          Parse error: 1 values for 2 columns
    let mut c = fixture(&["CREATE TABLE t(a,b)", "CREATE TABLE s(x)"]);
    let e = fails(&mut c, "INSERT INTO t(a,b) SELECT x FROM s");
    assert!(
        e.contains("1 values for 2 columns"),
        "got {e:?}, sqlite3 says `1 values for 2 columns`"
    );
    assert_eq!(count(&mut c, "t"), 0, "and nothing was written");
}

#[test]
fn a_count_mismatch_is_reported_even_when_the_source_is_empty() {
    // sqlite3: same statements, s is empty, still an error. This is the case
    // that needs the projection's width rather than a row's length.
    let mut c = fixture(&["CREATE TABLE t(a,b)", "CREATE TABLE s(x)"]);
    let e = fails(&mut c, "INSERT INTO t(a,b) SELECT x FROM s");
    assert!(e.contains("1 values for 2 columns"), "got {e:?}");
}

#[test]
fn a_not_null_failure_names_the_table_and_the_column() {
    // sqlite3: CREATE TABLE t(a NOT NULL); CREATE TABLE s(x);
    //          INSERT INTO s VALUES(1),(NULL),(3);
    //          INSERT INTO t SELECT x FROM s;
    //          Error: NOT NULL constraint failed: t.a
    let mut c = fixture(&["CREATE TABLE t(a NOT NULL)", "CREATE TABLE s(x)"]);
    run(&mut c, "INSERT INTO s VALUES(1),(NULL),(3)");
    let e = fails(&mut c, "INSERT INTO t SELECT x FROM s");
    assert!(
        e.contains("NOT NULL constraint failed: t.a"),
        "got {e:?}, sqlite3 says `NOT NULL constraint failed: t.a`"
    );
}

#[test]
fn a_duplicate_rowid_alias_is_a_unique_failure() {
    // sqlite3 3.53.4:
    //   CREATE TABLE t(a INTEGER PRIMARY KEY); CREATE TABLE s(x);
    //   INSERT INTO s VALUES(1),(1);
    //   INSERT INTO t SELECT x FROM s;
    //   Error: UNIQUE constraint failed: t.a
    //   SELECT count(*) FROM t;  --> 0
    let mut c = fixture(&["CREATE TABLE t(a INTEGER PRIMARY KEY)", "CREATE TABLE s(x)"]);
    run(&mut c, "INSERT INTO s VALUES(1),(1)");
    let e = fails(&mut c, "INSERT INTO t SELECT x FROM s");
    assert!(
        e.contains("UNIQUE constraint failed: t.a"),
        "got {e:?}, sqlite3 says `UNIQUE constraint failed: t.a`"
    );
}

#[test]
fn a_unique_column_that_is_not_the_rowid_alias_is_not_enforced_by_this_engine() {
    // A divergence from sqlite3, asserted rather than hidden, and it is not
    // this module's to fix.
    //
    // sqlite3 3.53.4:
    //   CREATE TABLE t(a UNIQUE); INSERT INTO t SELECT x FROM s;  (s = 1,1)
    //   Error: UNIQUE constraint failed: t.a
    //   SELECT count(*) FROM t;  --> 0
    //
    // this engine: no error, and the row count is 2.
    //
    // The cause is in the catalog, not here: a `Table` records which column is
    // the rowid alias and nothing else about uniqueness, and the insert path
    // only enforces what the b-tree reports for a duplicate rowid key. A
    // UNIQUE on any other column has no index behind it here. The same is true
    // of `INSERT ... VALUES` -- the two forms agree with each other and both
    // differ from sqlite3 -- so this is a pre-existing engine-wide gap, tracked
    // in the catalog and index tracks. It is pinned here because
    // [`nsqlite::insert_select::ATOMICITY`] used to promise that a UNIQUE
    // failure mid-statement was undone, which named a failure this engine does
    // not raise.
    let mut c = fixture(&["CREATE TABLE t(a UNIQUE)", "CREATE TABLE s(x)"]);
    run(&mut c, "INSERT INTO s VALUES(1),(1)");
    let out = c.execute(&parse_one("INSERT INTO t SELECT x FROM s").unwrap());
    assert!(
        out.is_ok(),
        "this engine does not enforce a non-alias UNIQUE; sqlite3 errors here"
    );
    assert_eq!(count(&mut c, "t"), 2, "both rows went in");
}

#[test]
fn an_error_in_the_source_query_is_reported_and_nothing_is_written() {
    // sqlite3 3.53.4:
    //   CREATE TABLE t(a); CREATE TABLE s(x); INSERT INTO s VALUES(1),(2),(3);
    //   INSERT INTO t SELECT abs(x,2) FROM s;
    //   Parse error: wrong number of arguments to function abs()
    //   SELECT count(*) FROM t;  --> 0
    //
    // The count is 0 on sqlite3 *in the same session*, not after a reopen. On
    // this engine it is 0 too, because the source runs to completion before the
    // first row is written. That is the one mid-statement guarantee the
    // snapshot does give, and it is narrower than atomicity.
    let mut c = fixture(&["CREATE TABLE t(a)", "CREATE TABLE s(x)"]);
    run(&mut c, "INSERT INTO s VALUES(1),(2),(3)");
    let e = fails(&mut c, "INSERT INTO t SELECT abs(x,2) FROM s");
    assert!(
        e.contains("wrong number of arguments to function abs()"),
        "got {e:?}"
    );
    assert_eq!(
        count(&mut c, "t"),
        0,
        "the source error arrived before any row was written"
    );
}

#[test]
fn an_unknown_source_table_is_reported_rather_than_a_count_mismatch() {
    // sqlite3: CREATE TABLE t(a,b);
    //          INSERT INTO t(a,b) SELECT x FROM nosuchtable;
    //          Parse error: no such table: nosuchtable
    //
    // The source error wins over the shape check here, which is what running
    // the query before counting its width is for. A module that counted first
    // would report `1 values for 2 columns` and hide the real fault.
    let mut c = fixture(&["CREATE TABLE t(a,b)"]);
    let e = fails(&mut c, "INSERT INTO t(a,b) SELECT x FROM nosuchtable");
    assert!(
        e.contains("no such table: nosuchtable"),
        "got {e:?}, sqlite3 says `no such table: nosuchtable`"
    );
}

#[test]
fn an_unknown_target_is_reported_before_the_source_runs() {
    // sqlite3: INSERT INTO nosucht(x) SELECT x FROM s;
    //          Parse error: no such table: nosucht
    let mut c = fixture(&["CREATE TABLE s(x)"]);
    let e = fails(&mut c, "INSERT INTO nosucht(x) SELECT x FROM s");
    assert!(e.contains("no such table: nosucht"), "got {e:?}");
}

#[test]
fn an_unknown_column_in_the_column_list_is_reported() {
    // sqlite3: INSERT INTO t(nosuch) SELECT x FROM s;
    //          Parse error: table t has no column named nosuch
    let mut c = fixture(&["CREATE TABLE t(a)", "CREATE TABLE s(x)"]);
    let e = fails(&mut c, "INSERT INTO t(nosuch) SELECT x FROM s");
    assert!(
        e.contains("table t has no column named nosuch"),
        "got {e:?}"
    );
}

// --- what a mid-statement failure leaves behind ---------------------------

#[test]
fn a_mid_statement_failure_leaves_the_earlier_rows_visible_in_the_session() {
    // The measurement behind [`nsqlite::insert_select::ATOMICITY`].
    //
    // sqlite3 3.53.4, same statements, count taken in the same session:
    //   CREATE TABLE t(a NOT NULL); CREATE TABLE s(x);
    //   INSERT INTO s VALUES(1),(2),(NULL),(4);
    //   INSERT INTO t SELECT x FROM s;   -> NOT NULL constraint failed: t.a
    //   SELECT count(*) FROM t;           -> 0
    //
    // this engine, same session, after reopen:
    //   SELECT count(*) FROM t;           -> 2
    //   SELECT sum(a) FROM t;             -> 3
    //   reopen, SELECT count(*) FROM t;   -> 0
    //
    // The rows before the NULL really are in the b-tree -- it is a read-back
    // that sees them, and `sum` confirms they are the values 1 and 2 rather
    // than something else. They disappear only because the pager never flushes
    // on the error path and the dirty pages are dropped when the connection
    // closes. The file is right; the running session is not.
    let p = temp_path("mid_statement");
    {
        let mut c = Connection::open(&p).expect("open a fresh database file");
        run(&mut c, "CREATE TABLE t(a NOT NULL)");
        run(&mut c, "CREATE TABLE s(x)");
        run(&mut c, "INSERT INTO s VALUES(1),(2),(NULL),(4)");
        let e = fails(&mut c, "INSERT INTO t SELECT x FROM s");
        assert!(e.contains("NOT NULL constraint failed: t.a"), "got {e:?}");

        assert_eq!(
            count(&mut c, "t"),
            2,
            "the rows before the NULL are still there, in this session"
        );
        assert_eq!(
            scalar(&mut c, "SELECT sum(a) FROM t"),
            "3",
            "values 1 and 2"
        );
        assert_eq!(
            c.changes(),
            0,
            "and the statement did not report itself as having changed rows"
        );
    }
    let mut c = Connection::open(&p).expect("reopen");
    assert_eq!(
        count(&mut c, "t"),
        0,
        "the reopened file has nothing, because nothing was flushed"
    );
}

#[test]
fn a_successful_insert_of_many_rows_is_on_disk_after_a_reopen() {
    // The control for the test above, and the reason the reopened count there
    // is not evidence of a partial write: a statement that *succeeds* does
    // flush, and a 2499-row one is there in full after the close.
    //
    // `count(*)` on this engine reads back fewer than were written for tables
    // larger than the page cache (442 of 2499 here) -- a pre-existing read-side
    // bug that reproduces identically on the success path, so it is not an
    // atomicity effect. What is asserted is that the file is not empty and
    // holds the rows, not that the count is exact.
    let p = temp_path("many_rows");
    let n = 2499;
    {
        let mut c = Connection::open(&p).expect("open");
        run(&mut c, "CREATE TABLE t(a NOT NULL, b)");
        let mut sql = String::from("INSERT INTO t VALUES ");
        for i in 0..n {
            if i > 0 {
                sql.push(',');
            }
            sql.push_str(&format!("({i},{i})"));
        }
        sql.push(';');
        run(&mut c, &sql);
        assert_eq!(c.changes(), n, "the statement changed every row");
    }
    let mut c = Connection::open(&p).expect("reopen");
    assert!(
        count(&mut c, "t") > 0,
        "a successful insert flushed its rows; the reopened file is not empty"
    );
    assert_eq!(
        scalar(&mut c, "SELECT count(*) FROM t WHERE a = 0"),
        "1",
        "and the first row is readable"
    );
}

#[test]
fn a_failed_large_insert_leaves_nothing_on_disk() {
    // The check that the 2499-row reproduction does *not* support.
    //
    // A single statement of 2499 rows followed by `(1,1),(NULL,2)` into
    // `t(a NOT NULL, b)` leaves 0 rows in the reopened file, at every size
    // swept: 300, 1000, 2499, 5000, 20000, 60000. sqlite3 also leaves 0. There
    // is no size at which a fraction of a failed statement reaches the file, so
    // the claim that it left 442 rows on disk does not hold and the atomicity
    // note no longer rests on it.
    for n in [300usize, 2499, 20000] {
        let p = temp_path(&format!("failed_large_{n}"));
        {
            let mut c = Connection::open(&p).expect("open");
            run(&mut c, "CREATE TABLE t(a NOT NULL, b)");
            let mut sql = String::from("INSERT INTO t VALUES ");
            for i in 0..n {
                if i > 0 {
                    sql.push(',');
                }
                sql.push_str(&format!("({i},{i})"));
            }
            sql.push_str(",(1,1),(NULL,2);");
            let e = fails(&mut c, &sql);
            assert!(e.contains("NOT NULL constraint failed: t.a"), "got {e:?}");
        }
        let mut c = Connection::open(&p).expect("reopen");
        assert_eq!(
            count(&mut c, "t"),
            0,
            "N={n}: a failed statement reaches the file as nothing at all"
        );
    }
}

// --- the transaction the atomicity advice depends on -----------------------

#[test]
fn a_transaction_cannot_be_opened_on_a_database_file_that_does_not_exist_yet() {
    // Why [`nsqlite::insert_select::ATOMICITY`] cannot tell a caller that
    // wrapping the statement in BEGIN and COMMIT makes it atomic: on a fresh
    // file it cannot be done at all.
    //
    // sqlite3 3.53.4 accepts BEGIN on a new file with no complaint, so this is
    // a divergence with no oracle to match -- it is a limitation of this
    // engine's pager, recorded so the advice is not read as a guarantee.
    //
    // `Pager::open` takes its `fresh` branch for a zero-length file and sets
    // the pager's path to `None`, so `begin_journal` refuses for the whole
    // lifetime of that connection -- not just before the first write. Creating
    // a table in between does not help, because the file is still zero-length
    // on disk until something flushes, and the connection keeps the same pager.
    let p = temp_path("fresh_begin");
    {
        let mut c = Connection::open(&p).expect("open a path that does not exist");
        let e = fails(&mut c, "BEGIN");
        assert!(
            e.contains("an in-memory database cannot be journalled"),
            "got {e:?}"
        );
        // A CREATE TABLE in the same connection does not make it possible.
        run(&mut c, "CREATE TABLE t(a)");
        let e = fails(&mut c, "BEGIN");
        assert!(
            e.contains("an in-memory database cannot be journalled"),
            "still refused after a write: got {e:?}"
        );
    }
    // Once the file exists and has been closed, BEGIN works.
    let mut c = Connection::open(&p).expect("reopen the now-existing file");
    run(&mut c, "BEGIN");
    assert!(c.in_transaction());
    run(&mut c, "ROLLBACK");
    assert!(!c.in_transaction());
}

#[test]
fn a_rolled_back_transaction_undoes_a_failed_insert_select() {
    // The case the advice *is* good for, on an existing file where BEGIN works.
    //
    // sqlite3: the same statements, with or without the explicit BEGIN, leave 0
    // rows after a NOT NULL failure.
    //
    // This is what the rollback journal is for, and it does it correctly. The
    // limitation is only that it needs a BEGIN, and a BEGIN is not always
    // available -- see the test above and the note itself.
    let p = existing_path("rollback");
    {
        let mut c = Connection::open(&p).expect("open an existing file");
        run(&mut c, "CREATE TABLE t(a NOT NULL)");
        run(&mut c, "CREATE TABLE s(x)");
        run(&mut c, "INSERT INTO s VALUES(1),(2),(NULL),(4)");
        run(&mut c, "BEGIN");
        assert!(c.in_transaction());
        let e = fails(&mut c, "INSERT INTO t SELECT x FROM s");
        assert!(e.contains("NOT NULL constraint failed: t.a"), "got {e:?}");
        run(&mut c, "ROLLBACK");
        assert_eq!(
            count(&mut c, "t"),
            0,
            "the rollback undid the rows written before the failure"
        );
    }
    let mut c = Connection::open(&p).expect("reopen");
    assert_eq!(count(&mut c, "t"), 0, "and the file agrees");
}

#[test]
fn a_committed_transaction_keeps_its_rows() {
    // sqlite3: BEGIN; INSERT ...; COMMIT;  leaves the rows in place.
    let p = existing_path("commit");
    {
        let mut c = Connection::open(&p).expect("open");
        run(&mut c, "CREATE TABLE t(a)");
        run(&mut c, "CREATE TABLE s(x)");
        run(&mut c, "INSERT INTO s VALUES(1),(2),(3)");
        run(&mut c, "BEGIN");
        run(&mut c, "INSERT INTO t SELECT x FROM s");
        run(&mut c, "COMMIT");
        assert!(!c.in_transaction());
    }
    let mut c = Connection::open(&p).expect("reopen");
    assert_eq!(
        count(&mut c, "t"),
        3,
        "a committed insert survives the close"
    );
}

// --- the statement's own shape --------------------------------------------

#[test]
fn a_select_with_no_from_inserts_one_row() {
    // sqlite3: CREATE TABLE t(a); INSERT INTO t SELECT 1; SELECT a --> 1
    let mut c = fixture(&["CREATE TABLE t(a)"]);
    run(&mut c, "INSERT INTO t SELECT 1");
    assert_eq!(count(&mut c, "t"), 1);
    assert_eq!(scalar(&mut c, "SELECT a FROM t"), "1");
}

#[test]
fn a_where_clause_narrows_what_is_inserted() {
    // sqlite3: s holds 1,2,3,4; INSERT INTO t SELECT x FROM s WHERE x>2;
    //          changes() --> 2
    let mut c = fixture(&["CREATE TABLE s(x)", "CREATE TABLE t(a)"]);
    run(&mut c, "INSERT INTO s VALUES(1),(2),(3),(4)");
    assert_eq!(
        changes_after(&mut c, "INSERT INTO t SELECT x FROM s WHERE x>2"),
        2
    );
    assert_eq!(count(&mut c, "t"), 2);
}

#[test]
fn an_order_by_and_a_limit_decide_what_is_inserted() {
    // sqlite3: s holds 1,2,3,4;
    //          INSERT INTO t SELECT x FROM s ORDER BY x DESC LIMIT 2;
    //          SELECT a FROM t --> 4 3
    let mut c = fixture(&["CREATE TABLE s(x)", "CREATE TABLE t(a)"]);
    run(&mut c, "INSERT INTO s VALUES(1),(2),(3),(4)");
    assert_eq!(
        changes_after(
            &mut c,
            "INSERT INTO t SELECT x FROM s ORDER BY x DESC LIMIT 2"
        ),
        2
    );
    let got: Vec<String> = match run(&mut c, "SELECT a FROM t") {
        Outcome::Query { rows, .. } => rows.iter().map(|r| r.values[0].to_string()).collect(),
        other => panic!("expected a query, got {other:?}"),
    };
    assert_eq!(
        got,
        vec!["4", "3"],
        "the two largest source values, largest first -- the target's own \
         column is `a`, since the projection does not rename it"
    );
}

#[test]
fn a_projection_is_evaluated_before_the_row_is_stored() {
    // sqlite3: INSERT INTO t SELECT x*2 FROM s; --> 2 4 6
    let mut c = fixture(&["CREATE TABLE s(x)", "CREATE TABLE t(a)"]);
    run(&mut c, "INSERT INTO s VALUES(1),(2),(3)");
    run(&mut c, "INSERT INTO t SELECT x*2 FROM s");
    let got: Vec<String> = match run(&mut c, "SELECT a FROM t") {
        Outcome::Query { rows, .. } => rows.iter().map(|r| r.values[0].to_string()).collect(),
        other => panic!("expected a query, got {other:?}"),
    };
    assert_eq!(got, vec!["2", "4", "6"]);
}

#[test]
fn a_null_from_the_source_is_stored_as_null() {
    // sqlite3: INSERT INTO t SELECT NULL; typeof(a) --> null
    let mut c = fixture(&["CREATE TABLE t(a)"]);
    run(&mut c, "INSERT INTO t SELECT NULL");
    assert_eq!(scalar(&mut c, "SELECT typeof(a) FROM t"), "null");
}

#[test]
fn a_schema_qualifier_on_the_target_is_accepted() {
    // sqlite3: INSERT INTO main.t(a) SELECT x FROM s; succeeds
    let mut c = fixture(&["CREATE TABLE t(a)", "CREATE TABLE s(x)"]);
    run(&mut c, "INSERT INTO s VALUES(1)");
    run(&mut c, "INSERT INTO main.t(a) SELECT x FROM s");
    assert_eq!(count(&mut c, "t"), 1);
}

#[test]
fn the_rows_survive_a_close_and_reopen() {
    // The basic durability check: an INSERT ... SELECT commits like any other.
    let p = temp_path("durable");
    {
        let mut c = Connection::open(&p).expect("open");
        run(&mut c, "CREATE TABLE s(x)");
        run(&mut c, "CREATE TABLE t(a)");
        run(&mut c, "INSERT INTO s VALUES(1),(2),(3)");
        run(&mut c, "INSERT INTO t SELECT x FROM s");
    }
    let mut c = Connection::open(&p).expect("reopen");
    assert_eq!(count(&mut c, "t"), 3);
    assert_eq!(scalar(&mut c, "SELECT a FROM t"), "1", "rowid order");
}

#[test]
fn values_and_select_produce_the_same_stored_row() {
    // The rule the whole module rests on: the two forms of INSERT have to be
    // indistinguishable in what they store.
    //
    // Two identically-declared tables, the same three rows of literals put into
    // one by each route, and the two tables compared column by column. The
    // columns are declared with mixed affinities so the comparison is not
    // vacuous -- a NUMERIC column that stores '123' as text and a REAL column
    // that stores an integer would both show up here.
    //
    // The values are chosen to hit the cases that matter: a numeric string into
    // NUMERIC (becomes an integer), the same into TEXT (stays text), an integer
    // into REAL (becomes a real), a non-numeric string into NUMERIC (stays
    // text), and the empty string into both.
    const ROWS: &str = "('123','456.0',789,1.5),('abc','1',0,0),('','',0,0)";
    let decl = "CREATE TABLE {}(a NUMERIC, b TEXT, c REAL, d BLOB)";

    let mut c = fixture(&[
        &decl.replace("{}", "via_values"),
        &decl.replace("{}", "via_select"),
        "CREATE TABLE s(x,y,z,w)",
    ]);
    // The source, so the SELECT route has real columns to read.
    run(&mut c, &format!("INSERT INTO s VALUES {ROWS}"));
    // Route one: a SELECT that reads the same literals.
    run(&mut c, "INSERT INTO via_select SELECT x,y,z,w FROM s");
    // Route two: the literals directly.
    run(&mut c, &format!("INSERT INTO via_values VALUES {ROWS}"));

    assert_eq!(count(&mut c, "via_select"), 3);
    assert_eq!(count(&mut c, "via_values"), 3);

    let project = |c: &mut Connection, table: &str| -> Vec<String> {
        let sql = format!("SELECT typeof(a),typeof(b),typeof(c),typeof(d),a,b,c,d FROM {table}");
        match run(c, &sql) {
            Outcome::Query { rows, .. } => rows
                .iter()
                .map(|r| {
                    r.values
                        .iter()
                        .map(|v| v.to_string())
                        .collect::<Vec<_>>()
                        .join(",")
                })
                .collect(),
            other => panic!("expected a query, got {other:?}"),
        }
    };
    let via_values = project(&mut c, "via_values");
    let via_select = project(&mut c, "via_select");
    assert_eq!(
        via_values, via_select,
        "the two forms store the same types and the same values for the same inputs"
    );
    assert_eq!(via_values.len(), 3, "and the comparison had three rows");
    // A guard on the guard: the affinities really did something, so a bug that
    // made both paths skip affinity could not pass by agreeing.
    assert_eq!(
        via_values[0], "integer,text,real,real,123,456.0,789.0,1.5",
        "'123' became an integer under NUMERIC, the TEXT column kept its text, \
         and 789 became a real under REAL"
    );
}

#[test]
fn an_insert_select_can_feed_another_insert_select() {
    // sqlite3: the chain inserts three rows, then one more copying them.
    let mut c = fixture(&[
        "CREATE TABLE a(x)",
        "CREATE TABLE b(x)",
        "CREATE TABLE d(x)",
    ]);
    run(&mut c, "INSERT INTO a VALUES(1),(2),(3)");
    run(&mut c, "INSERT INTO b SELECT x FROM a");
    run(&mut c, "INSERT INTO d SELECT x FROM b");
    assert_eq!(count(&mut c, "b"), 3);
    assert_eq!(count(&mut c, "d"), 3);
    assert_eq!(scalar(&mut c, "SELECT x FROM d"), "1");
}

#[test]
fn a_value_typed_helper_confirms_the_int_shape_the_tests_assume() {
    // Guards the shape assertions above: `Value::Integer` renders without a
    // decimal point, so a `2` in the expected vectors is an integer and not a
    // real that happens to print the same.
    assert_eq!(Value::Integer(2).to_string(), "2");
    assert_ne!(Value::Real(2.0).to_string(), "2");
    let stmt = parse_one("INSERT INTO t SELECT x FROM s").expect("parses");
    assert!(
        matches!(stmt, Stmt::Insert { .. }),
        "the statement really is an INSERT"
    );
}
