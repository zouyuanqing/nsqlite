//! The schema must survive growing past a single b-tree page.
//!
//! `sqlite_schema`'s root is page 1, fixed by the file format. Every other
//! table's root may move when the root leaf splits, and the new root is recorded
//! in the catalog; the schema has no catalog row to record one in, so its root
//! has to stay put and page 1 becomes an interior node in place.
//!
//! When it did not, `CREATE TABLE` past roughly the twenty-first table silently
//! truncated the schema: `CREATE TABLE` returned success, exit code 0 and no
//! message, while every table it and the statements before it had created was
//! dropped from the file. Forty-four tables left twenty-three. The differential
//! corpus hit this at its statement 203 and lost 121 of its 150 tables;
//! `integrity_check` afterwards reported `Page NN: never used`.
//!
//! Nothing in the unit tests saw it, because every one of them uses a handful of
//! rows, and the schema does not outgrow a leaf page until it holds dozens of
//! objects.

use nsqlite::connection::{Connection, Outcome};
use nsqlite::value::Value;

fn temp(tag: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!(
        "nsqlite-schema-growth-{}-{tag}.db",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&p);
    let _ = std::fs::remove_file(format!("{}-journal", p.display()));
    p
}

/// The column list from the corpus that reproduced it. The `DEFAULT` clauses are
/// what make each schema row wide enough for the b-tree to outgrow a leaf page
/// after a few dozen tables rather than a few hundred.
const SHAPE: &str = "a INTEGER DEFAULT 0, b REAL DEFAULT 1.5, c TEXT DEFAULT 'x'";

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

#[test]
fn the_schema_survives_more_tables_than_fit_on_one_page() {
    let path = temp("growth");
    let mut conn = Connection::open(&path).unwrap();
    const TABLES: usize = 60;
    for i in 0..TABLES {
        conn.execute_script(&format!("CREATE TABLE t{i}({SHAPE});"))
            .unwrap_or_else(|e| panic!("CREATE TABLE t{i} failed: {e}"));
        for row in 0..3i64 {
            conn.execute_script(&format!(
                "INSERT INTO t{i}(a, b, c) VALUES({row}, 1.5, 'payload-{row}');"
            ))
            .unwrap_or_else(|e| panic!("INSERT into t{i} failed: {e}"));
        }
    }

    assert_eq!(
        one(&mut conn, "SELECT count(*) FROM sqlite_schema WHERE type='table';"),
        TABLES as i64,
        "every table created must still be in sqlite_schema"
    );

    // Each must also still answer, which is what the differential corpus
    // actually observed failing.
    for i in 0..TABLES {
        let c = one(&mut conn, &format!("SELECT count(*) FROM t{i};"));
        assert_eq!(c, 3, "table t{i} became unreadable or lost rows");
    }
    drop(conn);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_schema_past_one_page_survives_a_reopen() {
    let path = temp("reopen");
    {
        let mut conn = Connection::open(&path).unwrap();
        for i in 0..60i64 {
            conn.execute_script(&format!("CREATE TABLE r{i}({SHAPE});"))
                .unwrap();
        }
    }
    let mut conn = Connection::open(&path).unwrap();
    assert_eq!(
        one(&mut conn, "SELECT count(*) FROM sqlite_schema WHERE type='table';"),
        60,
        "a reopened database must still find every table it was closed with"
    );
    // And the engine must be able to add more on top of a schema that already
    // has an interior node at page 1.
    conn.execute_script("CREATE TABLE one_more(x);").unwrap();
    assert_eq!(
        one(&mut conn, "SELECT count(*) FROM sqlite_schema WHERE type='table';"),
        61
    );
    drop(conn);
    let _ = std::fs::remove_file(&path);
}

/// The same thing judged by the real `sqlite3`, which is the only opinion that
/// matters about whether the file is a database.
#[test]
fn the_real_sqlite3_agrees_the_file_is_sound() {
    let Some(sqlite3) = find_sqlite3() else {
        eprintln!("skipping: the real sqlite3 is not on PATH");
        return;
    };
    let path = temp("interop");
    {
        let mut conn = Connection::open(&path).unwrap();
        for i in 0..60i64 {
            conn.execute_script(&format!("CREATE TABLE r{i}({SHAPE});"))
                .unwrap();
        }
    }
    let out = std::process::Command::new(&sqlite3)
        .arg("-batch")
        .arg(&path)
        .arg("PRAGMA integrity_check; SELECT count(*) FROM sqlite_schema WHERE type='table';")
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
        count, "60",
        "the real sqlite3 sees a different number of tables:\n{text}"
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
