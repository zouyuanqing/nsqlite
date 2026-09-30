//! A multi-row INSERT must not lose rows when the table outgrows its first leaf.
//!
//! A statement that writes enough rows to split the root leaf moves the table's
//! b-tree onto a new page part way through itself. The writer has to follow the
//! tree there for the rest of the statement, or the remaining rows land on the
//! page the tree just left behind.
//!
//! It did not, and the failure was silent in every observable that matters. A
//! single `INSERT` with 800 tuples reported 800 rows changed, exited zero,
//! printed nothing but the count -- and left 323 rows in the file. They were
//! rows 1..158 and 637..800: a contiguous block in the middle had gone, whole
//! leaf pages with it. The real `sqlite3` counting the same file also said 323,
//! so they were absent rather than unreadable, and `PRAGMA integrity_check`
//! reported `Rowid 164 out of order` alongside three pages never used.
//!
//! The cliff is where the write crosses a page boundary, so this is about the
//! tree growing and not about the value list: 470 tuples were fine and 480 were
//! not. Separate statements were always correct, 500 of them inserting 500
//! rows, and so was `INSERT .. SELECT` under the threshold -- which is why a
//! corpus of single-row inserts never saw it.

use nsqlite::connection::{Connection, Outcome};
use nsqlite::value::Value;

fn temp(tag: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!(
        "nsqlite-bulk-{}-{tag}.db",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&p);
    let _ = std::fs::remove_file(format!("{}-journal", p.display()));
    p
}

/// The single integer a one-column `SELECT` produced.
fn one(conn: &mut Connection, sql: &str) -> i64 {
    let mut out = conn.execute_script(sql).expect("querying");
    let outcome = out.pop().expect("one outcome per statement");
    let rows = match outcome {
        Outcome::Query { rows, .. } => rows,
        other => panic!("expected a result set, got {other:?}"),
    };
    assert_eq!(rows.len(), 1, "expected exactly one row from {sql}");
    match &rows[0].values[0] {
        Value::Integer(n) => *n,
        other => panic!("expected an integer from {sql}, got {other:?}"),
    }
}

/// The row count the statement itself reported.
fn reported(conn: &mut Connection, sql: &str) -> usize {
    let mut out = conn.execute_script(sql).expect("inserting");
    match out.pop().expect("one outcome per statement") {
        Outcome::Changed(n) => n,
        other => panic!("expected a change count, got {other:?}"),
    }
}

fn tuples(n: usize) -> String {
    tuples_range(1, n as u32)
}

/// §n§ tuples whose keys start at §first§, so several statements in one test
/// cover distinct key ranges rather than colliding.
fn tuples_range(first: u32, n: u32) -> String {
    (first..first + n)
        .map(|i| format!("({i},'r{i}')"))
        .collect::<Vec<_>>()
        .join(",")
}

/// The number at which the table stops fitting on one leaf page. 470 was the
/// last size measured whole and 480 the first that lost rows; the middle is
/// deliberately not pinned, because the boundary moves with the row width and
/// the point of the test is that the tree is followed wherever it goes.
const OVER: usize = 1200;

#[test]
fn a_multi_row_insert_keeps_every_row_when_the_table_grows_mid_statement() {
    let path = temp("values");
    let mut conn = Connection::open(&path).unwrap();
    conn.execute_script("CREATE TABLE t(a INTEGER, b TEXT);")
        .unwrap();

    let said = reported(
        &mut conn,
        &format!("INSERT INTO t(a, b) VALUES {};", tuples(OVER)),
    );
    // The statement's own count is the first half of the claim, and it is the
    // half that used to be true while the other was not.
    assert_eq!(said, OVER, "the statement must report every row it wrote");
    assert_eq!(
        one(&mut conn, "SELECT count(*) FROM t;"),
        OVER as i64,
        "every row must be readable through this connection"
    );

    // Reopening is the stronger claim: a row in the cache but not in the file
    // would satisfy the count above and fail here.
    drop(conn);
    let mut conn = Connection::open(&path).unwrap();
    assert_eq!(
        one(&mut conn, "SELECT count(*) FROM t;"),
        OVER as i64,
        "every row must survive a reopen"
    );
    assert_eq!(
        one(&mut conn, "SELECT count(DISTINCT a) FROM t;"),
        OVER as i64,
        "the keys must be distinct: a repeated key means a lost row"
    );
    assert_eq!(
        one(&mut conn, "SELECT max(a) FROM t;"),
        OVER as i64,
        "the last key written must be the largest"
    );
    drop(conn);
    let _ = std::fs::remove_file(&path);
}

/// The same crossing, reached through the `SELECT` form, which writes its rows
/// one at a time through the same helper.
#[test]
fn an_insert_select_keeps_every_row_when_the_table_grows_mid_statement() {
    let path = temp("select");
    let mut conn = Connection::open(&path).unwrap();
    conn.execute_script("CREATE TABLE src(a INTEGER, b TEXT);")
        .unwrap();
    reported(
        &mut conn,
        &format!("INSERT INTO src(a, b) VALUES {};", tuples(OVER)),
    );
    assert_eq!(one(&mut conn, "SELECT count(*) FROM src;"), OVER as i64);

    conn.execute_script("CREATE TABLE t(a INTEGER, b TEXT);").unwrap();
    let said = reported(&mut conn, "INSERT INTO t SELECT a, b FROM src;");
    assert_eq!(said, OVER);
    assert_eq!(one(&mut conn, "SELECT count(*) FROM t;"), OVER as i64);

    drop(conn);
    let mut conn = Connection::open(&path).unwrap();
    assert_eq!(one(&mut conn, "SELECT count(*) FROM t;"), OVER as i64);
    drop(conn);
    let _ = std::fs::remove_file(&path);
}

/// Several statements that each stay under the boundary and together cross it.
/// The tree grows between statements, where the catalog is re-read, so this is
/// the shape that was already correct -- and it is here to stop the fix from
/// being one that only holds within a single statement.
#[test]
fn growing_across_several_statements_keeps_every_row() {
    let path = temp("batches");
    let mut conn = Connection::open(&path).unwrap();
    conn.execute_script("CREATE TABLE t(a INTEGER, b TEXT);").unwrap();
    let mut total = 0usize;
    for batch in 0..12u32 {
        let said = reported(
            &mut conn,
            &format!(
                "INSERT INTO t(a, b) VALUES {};",
                tuples_range(batch * 100 + 1, 100)
            ),
        );
        assert_eq!(said, 100);
        total += said;
    }
    assert_eq!(total, 1200);
    assert_eq!(one(&mut conn, "SELECT count(*) FROM t;"), 1200);
    drop(conn);
    let _ = std::fs::remove_file(&path);
}

/// Judged by the real `sqlite3`, which is the only opinion that matters about
/// whether the file is a database rather than a plausible arrangement of bytes.
#[test]
fn the_real_sqlite3_finds_the_file_sound_after_a_crossing_insert() {
    let Some(sqlite3) = find_sqlite3() else {
        eprintln!("skipping: the real sqlite3 is not on PATH");
        return;
    };
    let path = temp("interop");
    {
        let mut conn = Connection::open(&path).unwrap();
        conn.execute_script("CREATE TABLE t(a INTEGER, b TEXT);").unwrap();
        reported(
            &mut conn,
            &format!("INSERT INTO t(a, b) VALUES {};", tuples(OVER)),
        );
    }
    let out = std::process::Command::new(&sqlite3)
        .arg("-batch")
        .arg(&path)
        .arg("PRAGMA integrity_check; SELECT count(*) FROM t;")
        .output()
        .expect("running sqlite3");
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let _ = std::fs::remove_file(&path);
    let mut lines = text.lines().filter(|l| !l.trim().is_empty());
    let integrity = lines.next().unwrap_or("").trim().to_string();
    let count = lines.next().unwrap_or("").trim().to_string();
    assert_eq!(
        integrity, "ok",
        "the real sqlite3 rejected the file the engine wrote:\n{text}"
    );
    assert_eq!(
        count,
        OVER.to_string(),
        "the real sqlite3 counts a different number of rows:\n{text}"
    );
}

fn find_sqlite3() -> Option<std::path::PathBuf> {
    for dir in std::env::split_paths(&std::env::var_os("PATH")?) {
        for name in ["sqlite3.exe", "sqlite3"] {
            let cand = dir.join(name);
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    None
}
