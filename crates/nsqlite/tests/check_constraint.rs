//! A CHECK is a constraint, not a comment.
//!
//! The DDL was always parsed and always round-tripped: reading `sqlite_schema`
//! back gave the `CREATE TABLE` text verbatim, and `msg.rs` already had
//! `CHECK constraint failed: CLAUSE` with a corpus of cases measured against
//! sqlite3 3.53.4. Nothing ever raised it, because nothing ever evaluated the
//! predicate -- `Constraint::Check` was built by the parser and dropped on the
//! floor by the catalog.
//!
//! So this file is about the gap between those two halves. A CHECK that is
//! parsed and never enforced writes rows the schema forbids, and unlike a wrong
//! answer that is a row in the file that should not be there.
//!
//! Every expectation here was measured against sqlite3 3.53.4 first, including
//! the ones that are easy to get wrong: a NULL result *passes*, the first
//! failing constraint is the one named, OR IGNORE skips while OR REPLACE
//! refuses, and a multi-row statement that fails at its second row leaves
//! nothing behind.
//!
//! The last test is a differential rather than a transcription: both engines are
//! handed the *same* starting file and asked the same statement, so it compares
//! verdicts rather than two sequences that drifted apart before the first case.

use nsqlite::connection::{Connection, Outcome};
use nsqlite::value::Value;
use std::path::PathBuf;
use std::process::Command;

fn temp(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("nsqlite-check-{}-{tag}.db", std::process::id()));
    let _ = std::fs::remove_file(&p);
    let _ = std::fs::remove_file(format!("{}-journal", p.display()));
    p
}

/// A value whose `length` is 19, against a check that wants at most 5.
const TOO_LONG: &str = "-9223372036854775808";
const TOO_LONG2: &str = "9223372036854775807";

const DDL: &str = "CREATE TABLE t(a INT, b TEXT, c INT CHECK(length(c) <= 5));";

/// Applies a script and reports what its last statement did.
fn run(sql: &str) -> Result<Outcome, String> {
    let mut c = Connection::open_memory().expect("the database opens");
    let mut out = c.execute_script(sql).map_err(|e| e.message)?;
    out.pop().ok_or_else(|| "the script produced no outcome".to_string())
}

/// The refusal, or a panic saying the statement was accepted.
fn err(sql: &str) -> String {
    run(sql).expect_err("the statement is refused")
}

/// How many rows `t` holds after `sql` has been applied.
///
/// A refusal part way through is the point rather than a failure of the
/// fixture, so the script's own error is ignored: the question is what it left
/// behind.
fn count(sql: &str) -> i64 {
    let mut c = Connection::open_memory().expect("the database opens");
    let _ = c.execute_script(sql);
    count_in(&mut c)
}

/// The same, through a connection the caller already has open.
fn count_in(conn: &mut Connection) -> i64 {
    let mut out = conn
        .execute_script("SELECT count(*) FROM t;")
        .expect("the count runs");
    match out.pop().expect("one outcome") {
        Outcome::Query { rows, .. } => match &rows[0].values[0] {
            Value::Integer(n) => *n,
            other => panic!("expected an integer, got {other:?}"),
        },
        other => panic!("expected a result set, got {other:?}"),
    }
}

#[test]
fn a_row_that_fails_a_check_is_refused() {
    // The engine's message is the reference's, exactly. `nsqlited` prefixes every
    // constraint error with its code when it prints -- `Error: CONSTRAINT: CHECK
    // constraint failed: ...` -- and so it does for UNIQUE and NOT NULL too;
    // that is the CLI's convention rather than the engine's, and it is a separate
    // question from whether the predicate is enforced.
    assert_eq!(
        err(&format!("{DDL} INSERT INTO t VALUES(1,'x',{TOO_LONG});")),
        "CHECK constraint failed: length(c) <= 5"
    );
    assert_eq!(count(&format!("{DDL} INSERT INTO t VALUES(1,'x',{TOO_LONG});")), 0);
    assert_eq!(count(&format!("{DDL} INSERT INTO t VALUES(1,'x',9);")), 1);
}

#[test]
fn the_refusal_quotes_the_constraint_as_written() {
    // Not a rendering of the expression tree. The schema is quoted, so the
    // whitespace and the capitalisation come back as they were typed.
    assert_eq!(
        err("CREATE TABLE t(a CHECK(a>10)); INSERT INTO t VALUES(1);"),
        "CHECK constraint failed: a>10"
    );
    assert_eq!(
        err("CREATE TABLE t(a CHECK(a > 10)); INSERT INTO t VALUES(1);"),
        "CHECK constraint failed: a > 10"
    );
    assert_eq!(
        err("CREATE TABLE t(a CHECK(A>0)); INSERT INTO t VALUES(-1);"),
        "CHECK constraint failed: A>0"
    );
}

#[test]
fn one_constraint_quotes_all_of_itself() {
    // The whole predicate, not the conjunct that happened to fail.
    assert_eq!(
        err("CREATE TABLE t(a CHECK(a > 0 AND a < 10)); INSERT INTO t VALUES(50);"),
        "CHECK constraint failed: a > 0 AND a < 10"
    );
}

#[test]
fn the_first_failing_constraint_is_the_one_named() {
    assert_eq!(
        err("CREATE TABLE t(a INT, b INT CHECK(b > 0), c INT CHECK(c < 100)); \
             INSERT INTO t VALUES(1,-5,500);"),
        "CHECK constraint failed: b > 0"
    );
    assert_eq!(
        err("CREATE TABLE t(a INT, b INT CHECK(b > 0), c INT CHECK(c < 100)); \
             INSERT INTO t VALUES(1,5,500);"),
        "CHECK constraint failed: c < 100"
    );
}

#[test]
fn a_null_result_passes() {
    // A CHECK is satisfied unless it is false, and unknown is not false.
    assert_eq!(count(&format!("{DDL} INSERT INTO t VALUES(3,'z',NULL);")), 1);
}

#[test]
fn a_multi_row_statement_that_fails_at_its_second_row_leaves_nothing() {
    assert_eq!(
        count(&format!(
            "{DDL} INSERT INTO t VALUES(1,'x',1),(2,'y',{TOO_LONG});"
        )),
        0,
        "the first row must not survive the refusal of the second"
    );
}

#[test]
fn or_ignore_skips_the_row_and_or_replace_refuses() {
    // There is no existing row for a CHECK to conflict *with*, so OR REPLACE has
    // nothing to do and refuses like the default. Measured on sqlite3 3.53.4.
    assert_eq!(
        count(&format!(
            "{DDL} INSERT OR IGNORE INTO t VALUES(1,'x',9),(2,'y',{TOO_LONG});"
        )),
        1
    );
    assert_eq!(
        count(&format!("{DDL} INSERT OR REPLACE INTO t VALUES(2,'y',{TOO_LONG});")),
        0
    );
}

#[test]
fn an_update_into_a_violation_refuses_and_leaves_the_row_alone() {
    let path = temp("update");
    let mut c = Connection::open(&path).expect("the database opens");
    c.execute_script(DDL).unwrap();
    c.execute_script("INSERT INTO t VALUES(1,'x',9);").unwrap();

    let e = c
        .execute_script(&format!("UPDATE t SET c = {TOO_LONG};"))
        .expect_err("the update is refused");
    assert_eq!(e.message, "CHECK constraint failed: length(c) <= 5");

    // The row is still exactly as it was: the refusal happens before the
    // remove, because once the old row is gone there is nothing to put back.
    let mut out = c.execute_script("SELECT c FROM t;").expect("the row reads");
    match out.pop().expect("one outcome") {
        Outcome::Query { rows, .. } => assert_eq!(rows[0].values[0], Value::Integer(9)),
        other => panic!("expected a result set, got {other:?}"),
    }

    c.execute_script("UPDATE OR IGNORE t SET c = 3;").unwrap();
    drop(c);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn insert_select_refuses_the_whole_statement() {
    assert_eq!(
        err("CREATE TABLE t(a INT, c INT CHECK(c < 100)); \
             CREATE TABLE s(x); \
             INSERT INTO s VALUES(1),(2),(500); \
             INSERT INTO t SELECT x, x FROM s;"),
        "CHECK constraint failed: c < 100"
    );
}

#[test]
fn insert_select_or_ignore_skips_only_the_failing_row() {
    assert_eq!(
        count("CREATE TABLE t(a INT, c INT CHECK(c < 100)); \
               CREATE TABLE s(x); \
               INSERT INTO s VALUES(1),(2),(500); \
               INSERT OR IGNORE INTO t SELECT x, x FROM s;"),
        2
    );
}

#[test]
fn the_check_survives_a_reopen() {
    // The catalog is rebuilt by re-parsing the stored DDL, so a constraint held
    // only in memory would stop being enforced the next time the file is opened
    // -- the failure mode a test that never closes the connection cannot see.
    let path = temp("reopen");
    {
        let mut c = Connection::open(&path).expect("the database opens");
        c.execute_script(DDL).unwrap();
        c.execute_script("INSERT INTO t VALUES(1,'x',9);").unwrap();
    }
    let mut c = Connection::open(&path).expect("the database reopens");
    let e = c
        .execute_script(&format!("INSERT INTO t VALUES(2,'y',{TOO_LONG});"))
        .expect_err("the reopened database still enforces it");
    assert_eq!(e.message, "CHECK constraint failed: length(c) <= 5");
    drop(c);
    let _ = std::fs::remove_file(&path);
}

/// Both engines, the same starting file, the same statement.
///
/// Each case is run twice on two copies of one seed, so the comparison is of
/// verdicts on identical input rather than of two sequences that had already
/// diverged by the time the interesting case arrived.
#[test]
fn the_real_sqlite3_gives_the_same_verdict_on_the_same_file() {
    let Some(bin) = find_sqlite3() else {
        eprintln!("skipping: the real sqlite3 is not on PATH");
        return;
    };

    let seed = temp("seed");
    {
        let mut c = Connection::open(&seed).expect("the database opens");
        c.execute_script(DDL).unwrap();
        c.execute_script("INSERT INTO t VALUES(1,'x',9);").unwrap();
    }

    // The statement, the refusal both engines must give, and the row count both
    // must leave. The seed holds one row.
    let cases: [(&str, &str, i64); 7] = [
        (
            &format!("INSERT INTO t VALUES(2,'y',{TOO_LONG});"),
            "CHECK constraint failed: length(c) <= 5",
            1,
        ),
        (
            &format!("INSERT OR IGNORE INTO t VALUES(3,'z',{TOO_LONG2});"),
            "",
            1,
        ),
        (
            &format!("INSERT OR REPLACE INTO t VALUES(4,'w',{TOO_LONG2});"),
            "CHECK constraint failed: length(c) <= 5",
            1,
        ),
        (
            &format!("INSERT INTO t VALUES(5,'v',1),(6,'u',{TOO_LONG2});"),
            "CHECK constraint failed: length(c) <= 5",
            1,
        ),
        ("INSERT INTO t VALUES(7,'t',NULL);", "", 2),
        (
            &format!("UPDATE t SET c = {TOO_LONG2};"),
            "CHECK constraint failed: length(c) <= 5",
            1,
        ),
        (
            "CREATE TABLE u(a INT CHECK(a > 0), b INT); INSERT INTO u VALUES(-1, 1);",
            "CHECK constraint failed: a > 0",
            1,
        )
    ];

    for (i, (stmt, want_err, want_rows)) in cases.iter().enumerate() {
        let mine = temp(&format!("mine{i}"));
        let theirs = temp(&format!("theirs{i}"));
        std::fs::copy(&seed, &mine).expect("the first copy is made");
        std::fs::copy(&seed, &theirs).expect("the second copy is made");

        let mut conn = Connection::open(&mine).expect("the copy opens");
        let got_err = conn.execute_script(stmt).err().map(|e| e.message);
        let want = if want_err.is_empty() {
            None
        } else {
            Some(want_err.to_string())
        };
        assert_eq!(got_err.as_deref(), want.as_deref(), "case {i}: {stmt}");
        let mine_rows = count_in(&mut conn);
        drop(conn);

        let out = Command::new(&bin)
            .arg("-batch")
            .arg(&theirs)
            .arg(stmt)
            .output()
            .expect("the reference runs");
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        // Where the shell read the statement from decides how it names the
        // failure: from stdin it says `Error: <message>`, from an argument it
        // says `Error in 3rd command line argument: <message>`. This runs it
        // as an argument, so the second form is the one to strip -- and the
        // message itself is the engine's either way, which is what is compared.
        let theirs_err = if out.status.success() {
            None
        } else {
            let text = stderr.trim();
            text.strip_prefix("Error: ")
                .or_else(|| {
                    text.split_once("command line argument: ").map(|(_, rest)| rest)
                })
                .map(str::trim)
                .map(str::to_string)
        };
        assert_eq!(
            got_err, theirs_err,
            "case {i}: the two engines gave different refusals for: {stmt}"
        );

        let theirs_rows = count_in(&mut Connection::open(&theirs).expect("the copy opens"));
        assert_eq!(
            mine_rows, theirs_rows,
            "case {i}: the two engines left different row counts for: {stmt}"
        );
        assert_eq!(mine_rows, *want_rows, "case {i}: {stmt}");

        let _ = std::fs::remove_file(&mine);
        let _ = std::fs::remove_file(&theirs);
    }
    let _ = std::fs::remove_file(&seed);
}

fn find_sqlite3() -> Option<PathBuf> {
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
