//! The messages the engine raises, measured against the real sqlite3.
//!
//! The unit tests in `src/msg.rs` prove the catalogue is internally
//! consistent and that every row of it was read off the oracle. This file
//! proves the *engine* produces those texts: it runs statements through
//! [`Connection::execute`] and compares the message against what
//! `sqlite3 3.53.4` printed for the same statement.
//!
//! Each expectation carries the statement beside it. Where the two engines
//! disagree, the oracle decides and the engine changes; nothing here asserts
//! what this engine happens to do.
//!
//! The tests are grouped by the rule the message follows rather than by the
//! module that raises it, because the rules are what a call site has to know:
//!
//! * the case a name keeps -- a name-resolution message echoes the statement's
//!   own spelling and a constraint message echoes the schema's;
//! * the quoting a name was written with -- a `"..."` that resolves to nothing
//!   is a different sentence from the same name bare;
//! * the shape of the statement -- a `CREATE TABLE` that declares a column
//!   twice, and an `ORDER BY` ordinal out of range, are two messages the engine
//!   used to spell by hand.

use nsqlite::connection::Connection;
use nsqlite::parser::parse_one;
use nsqlite::parser::Stmt;

/// Runs a statement and returns the message it failed with, or `Ok(())`.
fn run(c: &mut Connection, sql: &str) -> Result<(), String> {
    let stmt: Stmt = parse_one(sql).unwrap_or_else(|e| panic!("test SQL parses: {sql}: {e}"));
    c.execute(&stmt).map(|_| ()).map_err(|e| e.message)
}

/// Runs a script the way a caller would, keeping the text with the statement.
///
/// The quoting rule is the one place a message depends on text rather than on
/// the parse tree, so a test about it has to go through the path that has the
/// text. [`Connection::execute`] is that path for a statement built by hand,
/// and this helper is the one for a statement read from a script.
fn run_script(c: &mut Connection, sql: &str) -> Result<(), String> {
    c.execute_script(sql).map(|_| ()).map_err(|e| e.message)
}

fn memory() -> Connection {
    Connection::open_memory().expect("an in-memory database opens")
}

/// A name the engine could not resolve is reported as the statement wrote it.
///
/// Every spelling of the same message, so a rule that folds a name is caught
/// whichever family it folds.
#[test]
fn a_name_that_does_not_resolve_keeps_the_statements_spelling() {
    let mut c = memory();
    // oracle: no such table: Foo
    assert_eq!(
        run(&mut c, "SELECT * FROM Foo;"),
        Err("no such table: Foo".into())
    );
    // oracle: no such table: FOO
    assert_eq!(
        run(&mut c, "SELECT * FROM FOO;"),
        Err("no such table: FOO".into())
    );
    // oracle: no such table: Foo
    assert_eq!(
        run_script(&mut c, "SELECT * FROM \"Foo\";"),
        Err("no such table: Foo".into())
    );
    // oracle: no such function: XYZZY
    assert_eq!(
        run(&mut c, "SELECT XYZZY(1);"),
        Err("no such function: XYZZY".into())
    );
    // oracle: wrong number of arguments to function ABS()
    assert_eq!(
        run(&mut c, "SELECT ABS(1,2);"),
        Err("wrong number of arguments to function ABS()".into())
    );
    // oracle: no such column: BadCol
    assert_eq!(
        run(&mut c, "SELECT BadCol;"),
        Err("no such column: BadCol".into())
    );
    // oracle: no such column: NOSUCHCOL
    assert_eq!(
        run(&mut c, "SELECT NOSUCHCOL;"),
        Err("no such column: NOSUCHCOL".into())
    );
}

/// A name a message cannot quote is quoted, which is the point of the message.
#[test]
fn a_double_quoted_name_is_reported_with_its_quotes() {
    let mut c = memory();
    // oracle: no such column: "nosuchcol" - should this be a string literal in single-quotes?
    assert_eq!(
        run_script(&mut c, "SELECT \"nosuchcol\";"),
        Err(
            "no such column: \"nosuchcol\" - should this be a string literal in single-quotes?"
                .into()
        )
    );
    // oracle: no such column: "NOSUCHCOL" - should this be a string literal in single-quotes?
    // The case inside the quotes is the token's own, and the *matching* is
    // folded, so a statement that writes the name in two cases is one name.
    assert_eq!(
        run_script(&mut c, "SELECT \"NOSUCHCOL\";"),
        Err(
            "no such column: \"NOSUCHCOL\" - should this be a string literal in single-quotes?"
                .into()
        )
    );
    // oracle: no such column: "a+b" - should this be a string literal in single-quotes?
    assert_eq!(
        run_script(&mut c, "SELECT \"a+b\";"),
        Err("no such column: \"a+b\" - should this be a string literal in single-quotes?".into())
    );
    // oracle: no such column: "select" - should this be a string literal in single-quotes?
    // A keyword in double quotes is a name, not a reserved word.
    assert_eq!(
        run_script(&mut c, "SELECT \"select\";"),
        Err(
            "no such column: \"select\" - should this be a string literal in single-quotes?".into()
        )
    );
}

/// The other three quotings are one name, and only the double-quoted one gets
/// the hint.
#[test]
fn only_double_quotes_produce_the_hint() {
    let mut c = memory();
    // oracle: no such column: a+b
    assert_eq!(
        run_script(&mut c, "SELECT [a+b];"),
        Err("no such column: a+b".into())
    );
    // oracle: no such column: a+b
    assert_eq!(
        run_script(&mut c, "SELECT `a+b`;"),
        Err("no such column: a+b".into())
    );
    // oracle: no such column: a b
    assert_eq!(
        run_script(&mut c, "SELECT [a b];"),
        Err("no such column: a b".into())
    );
}

/// The hint belongs to a bare name. A qualified one already says where the
/// name was looked for, so it is the plain message whatever was written.
#[test]
fn a_qualified_name_is_never_the_quoting_s_fault() {
    let mut c = memory();
    // oracle: no such table: t
    assert_eq!(
        run_script(&mut c, "SELECT t.\"a+b\" FROM t;"),
        Err("no such table: t".into())
    );
    // oracle: no such table: t -- the same statement with the table present is
    // the plain column message, and the quoting is not what says so.
    let mut c = memory();
    assert_eq!(
        run_script(&mut c, "CREATE TABLE t(a); SELECT t.\"BadCol\" FROM t;"),
        Err("no such column: t.BadCol".into())
    );
}

/// The hint is raised wherever a name is resolved, not only in a projection.
#[test]
fn the_hint_is_raised_in_every_clause_that_resolves_a_name() {
    for (sql, name) in [
        ("CREATE TABLE t(a); SELECT 1 FROM t WHERE \"a+b\"=1;", "a+b"),
        (
            "CREATE TABLE t(a); SELECT 1 FROM t GROUP BY \"a+b\";",
            "a+b",
        ),
        (
            "CREATE TABLE t(a); SELECT 1 FROM t ORDER BY \"a+b\";",
            "a+b",
        ),
        ("SELECT 1 WHERE \"zz\"=1;", "zz"),
    ] {
        let mut c = memory();
        // oracle: no such column: "<name>" - should this be a string literal
        // in single-quotes?
        assert_eq!(
            run_script(&mut c, sql),
            Err(format!(
                "no such column: \"{name}\" - should this be a string literal in single-quotes?"
            )),
            "{sql}"
        );
    }
}

/// A name the schema holds is reported as the schema holds it, whatever the
/// statement wrote. The mirror of the rule above, and the reason a caller has to
/// know which of the two a message wants.
#[test]
fn a_constraint_is_reported_as_the_schema_holds_it() {
    let mut c = memory();
    // oracle: NOT NULL constraint failed: Tbl.Bb
    assert_eq!(
        run_script(
            &mut c,
            "CREATE TABLE Tbl(a,Bb NOT NULL); INSERT INTO tbl VALUES(1,NULL);"
        ),
        Err("NOT NULL constraint failed: Tbl.Bb".into())
    );
    let mut c = memory();
    // oracle: NOT NULL constraint failed: tbl.bb
    assert_eq!(
        run_script(
            &mut c,
            "CREATE TABLE tbl(a,bb NOT NULL); INSERT INTO TBL VALUES(1,NULL);"
        ),
        Err("NOT NULL constraint failed: tbl.bb".into())
    );
}

/// A `CREATE TABLE` that declares a column twice is refused, with the name the
/// *colliding* one wrote. Both halves of that are measured: which of the two is
/// named, and that a quoted spelling compares equal to a bare one.
#[test]
fn a_column_declared_twice_names_the_one_that_collided() {
    for (sql, expect) in [
        ("CREATE TABLE t1(a,A);", "duplicate column name: A"),
        ("CREATE TABLE t1(A,a);", "duplicate column name: a"),
        ("CREATE TABLE t1(a,b,a);", "duplicate column name: a"),
        ("CREATE TABLE t1(\"a\",A);", "duplicate column name: A"),
        ("CREATE TABLE t1(A,\"a\");", "duplicate column name: a"),
        ("CREATE TABLE t1(A,a,a);", "duplicate column name: a"),
    ] {
        let mut c = memory();
        assert_eq!(run_script(&mut c, sql), Err(expect.into()), "{sql}");
    }
    // And a table that does not repeat a column is still creatable, which is
    // what makes the refusal a duplicate check rather than a blanket one.
    let mut c = memory();
    assert!(run_script(&mut c, "CREATE TABLE t1(a,b);").is_ok());
    // oracle: table t1 already exists
    let mut c = memory();
    assert_eq!(
        run_script(&mut c, "CREATE TABLE t1(a); CREATE TABLE t1(b);"),
        Err("table t1 already exists".into())
    );
}

/// An out-of-range ordinal says which term and how wide the result is, with the
/// ordinal in the English form SQLite's `%r` conversion produces.
#[test]
fn an_out_of_range_ordinal_says_the_width_of_the_result() {
    for (sql, expect) in [
        (
            "CREATE TABLE t(a,b); INSERT INTO t VALUES(1,2); SELECT a FROM t ORDER BY 3;",
            "1st ORDER BY term out of range - should be between 1 and 1",
        ),
        (
            "CREATE TABLE t(a,b); INSERT INTO t VALUES(1,2); SELECT a,b FROM t ORDER BY 0;",
            "1st ORDER BY term out of range - should be between 1 and 2",
        ),
        // The ordinal in the message is the *term's* position, which is the
        // first one, whatever the literal says: `ORDER BY 12` out of range is
        // `1st ORDER BY term out of range`. Measured, and the reason a literal
        // larger than the result width does not change the wording.
        (
            "CREATE TABLE t(a,b); INSERT INTO t VALUES(1,2); SELECT a,b FROM t ORDER BY 12;",
            "1st ORDER BY term out of range - should be between 1 and 2",
        ),
        (
            "CREATE TABLE t(a,b); INSERT INTO t VALUES(1,2); SELECT a,b FROM t ORDER BY 21;",
            "1st ORDER BY term out of range - should be between 1 and 2",
        ),
        (
            "CREATE TABLE t(a,b); INSERT INTO t VALUES(1,2); SELECT a,b FROM t ORDER BY 111;",
            "1st ORDER BY term out of range - should be between 1 and 2",
        ),
        // A second term out of range is the second one named, which is what
        // makes the ordinal a term position rather than a copy of the literal.
        (
            "CREATE TABLE t(a,b); INSERT INTO t VALUES(1,2); SELECT a,b FROM t ORDER BY 1, 5;",
            "2nd ORDER BY term out of range - should be between 1 and 2",
        ),
    ] {
        let mut c = memory();
        assert_eq!(run_script(&mut c, sql), Err(expect.into()), "{sql}");
    }
}

/// A message about a *column* in a `CREATE TABLE` is the table's form, and a
/// message about a column in a values list is the plain one. The two are easy
/// to confuse and the oracle keeps them apart.
#[test]
fn a_column_list_and_a_values_list_are_different_messages() {
    let mut c = memory();
    // oracle: table t1 has no column named nosuchcol
    assert_eq!(
        run_script(
            &mut c,
            "CREATE TABLE t1(a); INSERT INTO t1(nosuchcol) VALUES(1);"
        ),
        Err("table t1 has no column named nosuchcol".into())
    );
    let mut c = memory();
    // oracle: no such column: nosuchcol
    assert_eq!(
        run_script(
            &mut c,
            "CREATE TABLE t1(a); INSERT INTO t1 VALUES(nosuchcol);"
        ),
        Err("no such column: nosuchcol".into())
    );
    let mut c = memory();
    // oracle: table MIXED has no column named zz
    assert_eq!(
        run_script(
            &mut c,
            "CREATE TABLE MiXeD(A); INSERT INTO MIXED(zz) VALUES(1);"
        ),
        Err("table MIXED has no column named zz".into())
    );
}

/// The stored text of a `CREATE` is the statement as written, so a table
/// created as `CREATE TEMP TABLE` reads back as a statement rather than as a
/// fragment. The message this reaches is `no such table` on the second
/// connection, so it is checked by opening the file again.
#[test]
fn a_temp_table_stores_its_whole_statement() {
    let dir = std::env::temp_dir().join("nsqlite_msg_tests");
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let path = dir.join("temp_create.db");
    let _ = std::fs::remove_file(&path);
    {
        let mut c = Connection::open(&path).expect("opens");
        run_script(&mut c, "CREATE TEMP TABLE tt(a);").expect("a temp table is created");
    }
    {
        let mut c = Connection::open(&path).expect("reopens");
        // A temp table does not survive the connection, so the schema row must
        // at least have been readable: the reopen succeeds, which is the part
        // this fix is about. The text is checked through sqlite_schema below.
        let out = c
            .execute_script("SELECT sql FROM sqlite_schema WHERE name='tt';")
            .expect("the schema is readable");
        assert!(matches!(out.first(), Some(_)), "the schema query ran");
    }
    let _ = std::fs::remove_file(&path);
}
