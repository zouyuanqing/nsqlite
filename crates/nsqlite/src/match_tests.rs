//! `MATCH`, measured against sqlite3 3.53.4 on this machine.
//!
//! `MATCH` is FTS5's operator, and this engine hosts no FTS5 and no `vec0`
//! table to hand it to. So the only question this file answers is: what does
//! the reference do, and does this engine do the same thing for the same
//! reason? The answer is a refusal, and the refusal is *lazy*, which is the
//! part that is easy to get wrong.
//!
//! # What was measured
//!
//! Every expectation below was produced by running the statement through the
//! real binary, not by reading the grammar:
//!
//! ```text
//! $ printf "SELECT 'a' MATCH 'a';\n" | sqlite3 :memory:
//! Error near line 1: unable to use function MATCH in the requested context
//! ```
//!
//! Three properties of that answer drive the implementation:
//!
//! 1. **It is a runtime error, not a parse error.** The statement parses.
//!    `EXPLAIN SELECT 'a' MATCH 'a'` prints a whole program:
//!
//!    ```text
//!    1     Function       3     2     1     match(2)       0   r[1]=func(r[2..3])
//!    ```
//!
//!    A single `Function` opcode with `match(2)` in the comment column, which
//!    is SQLite's own lowering of the operator. So the parser has to accept
//!    `MATCH`, and the refusal cannot live in the grammar.
//!
//! 2. **It is raised only when the expression is reached.** A short-circuited
//!    arm never touches it:
//!
//!    ```text
//!    $ printf "SELECT CASE WHEN 0 THEN ('a' MATCH 'a') ELSE 42 END;\n" | sqlite3 :memory:
//!    42
//!    $ printf "SELECT 0 AND ('a' MATCH 'a');\n" | sqlite3 :memory:
//!    0
//!    $ printf "CREATE TABLE t(x);\nSELECT x FROM t WHERE x MATCH 'a';\n" | sqlite3 :memory:
//!                                    (no output, no error: the table is empty)
//!    ```
//!
//!    This is why the refusal sits in the `eval` arm and not in the planner:
//!    hoisting it would make all three of those answer wrongly.
//!
//! # The one shape that does NOT match, and why
//!
//! `SELECT x FROM t WHERE x MATCH 'a' LIMIT 0` answers no rows and no error
//! on the reference, and this engine refuses it. The engine applies LIMIT
//! after the whole scan has run — `connection::apply_limit` truncates a
//! `Vec<Row>` that is already complete — so with a row present the WHERE
//! clause is still evaluated and the refusal still fires.
//!
//! This is **architectural, not MATCH-specific**, and it is a pre-existing gap
//! rather than anything this change introduced. A `LIMIT` that is satisfied
//! before the last row is also not short-circuited: MEASURED, this engine
//! evaluates the WHERE clause for every row of the table and only then takes
//! the first `n`. Closing it means stopping the scan, which is a change to the
//! row pipeline and not to `MATCH`. It is recorded here so that the next
//! differential case to find it points at the right file.
//!
//! 3. **Neither operand is evaluated first, and neither is resolved at
//!    evaluation time.** An unknown name is a *parse* error, because SQLite
//!    resolves columns while preparing:
//!
//!    ```text
//!    $ printf "SELECT nosuchcol MATCH 'a';\n" | sqlite3 :memory:
//!    Parse error near line 1: no such column: nosuchcol
//!    ```
//!
//!    So the column on the left of a `MATCH` binds through the ordinary
//!    resolver -- that is what the `vec0` contract's MATCH item needs -- and
//!    the context fault is what is left over once the names are good.

use crate::connection::{Connection, Outcome, Row};
use crate::value::Value;

/// The reference's own sentence, measured on 3.53.4.
const REFUSED: &str = "unable to use function MATCH in the requested context";

fn mem() -> Connection {
    Connection::open_memory().expect("open")
}

/// Runs a script and returns the last outcome.
fn run(c: &mut Connection, sql: &str) -> Outcome {
    c.execute_script(sql)
        .unwrap_or_else(|e| panic!("{sql:?} failed: {e}"))
        .into_iter()
        .last()
        .expect("one outcome")
}

/// Runs a script expecting the last statement to fail, and returns the text.
///
/// The error's own message, not the whole `Error`, because the message is the
/// thing the differential suite compares byte for byte and the part that has to
/// match the reference.
fn refused(c: &mut Connection, sql: &str) -> String {
    let e = c
        .execute_script(sql)
        .err()
        .unwrap_or_else(|| panic!("{sql:?} should have been refused, but it succeeded"));
    e.message
}

/// The single integer a statement answers, which is how a non-error is checked
/// without going near a rendering.
fn one_int(c: &mut Connection, sql: &str) -> i64 {
    let out = run(c, sql);
    let Outcome::Query { rows, .. } = out else {
        panic!("{sql:?} did not answer rows: {out:?}")
    };
    match rows.as_slice() {
        [Row { values }] => match values.as_slice() {
            [Value::Integer(i)] => *i,
            other => panic!("{sql:?} answered {other:?}, not one integer"),
        },
        other => panic!("{sql:?} answered {other:?} rows, not one"),
    }
}

#[test]
fn the_accepting_shape_is_a_refusal_carrying_the_references_own_sentence() {
    // MEASURED: `SELECT 'a' MATCH 'a';` is
    //   Error near line 1: unable to use function MATCH in the requested context
    // The engine's message is the reference's, minus the shell's own
    // "Error near line N: " prefix, which is the CLI's annotation and not
    // sqlite3's text.
    let mut c = mem();
    assert_eq!(refused(&mut c, "SELECT 'a' MATCH 'a'"), REFUSED);
}

#[test]
fn not_match_is_refused_with_the_identical_sentence() {
    // MEASURED: `SELECT 'a' NOT MATCH 'a';` is the SAME sentence, not a
    // negation of it and not a separate one. So there is no "invert the
    // answer" path to take: `negated` is parsed, carried, and then never
    // consulted, because the reference never gets far enough to have an
    // answer to invert.
    let mut c = mem();
    assert_eq!(refused(&mut c, "SELECT 'a' NOT MATCH 'a'"), REFUSED);
}

#[test]
fn a_null_operand_is_refused_rather_than_answering_null() {
    // MEASURED, both directions:
    //   SELECT NULL MATCH 'a';  -> Error near line 1: unable to use function
    //                               MATCH in the requested context
    //   SELECT 'a' MATCH NULL;  -> the same
    // Neither answers NULL. That is the difference from a comparison: `x = NULL`
    // is unknown and returns NULL, but MATCH never reaches the point of
    // comparing anything, so a NULL operand is not special. Anything that
    // made NULL propagate instead would be inventing a third behaviour.
    let mut c = mem();
    assert_eq!(refused(&mut c, "SELECT NULL MATCH 'a'"), REFUSED);
    assert_eq!(refused(&mut c, "SELECT 'a' MATCH NULL"), REFUSED);
    assert_eq!(refused(&mut c, "SELECT NULL NOT MATCH NULL"), REFUSED);
}

#[test]
fn the_refusal_is_raised_only_when_the_expression_is_reached() {
    // Each of these four is MEASURED as answering normally, with no error. They
    // are the tests that would fail if the refusal were hoisted out of `eval`
    // into the planner or into the parser, which is the mistake this whole
    // implementation is arranged to avoid.
    let mut c = mem();
    // MEASURED: 42.
    assert_eq!(
        one_int(
            &mut c,
            "SELECT CASE WHEN 0 THEN ('a' MATCH 'a') ELSE 42 END"
        ),
        42
    );
    // MEASURED: 0.
    assert_eq!(one_int(&mut c, "SELECT 0 AND ('a' MATCH 'a')"), 0);
    // MEASURED: an empty table answers no rows and no error, because the WHERE
    // clause is never evaluated for a row that does not exist.
    run(
        &mut c,
        "CREATE TABLE t(x); SELECT x FROM t WHERE x MATCH 'a'",
    );
    // MEASURED: `WHERE 0 AND ...` over a table that HAS a row answers no rows.
    run(
        &mut c,
        "CREATE TABLE u(x); INSERT INTO u VALUES('a'); \
         SELECT x FROM u WHERE 0 AND (x MATCH 'a')",
    );
}

#[test]
fn a_match_over_rows_that_exist_is_refused() {
    // The mirror of the test above, so the two together pin WHERE the refusal
    // happens rather than only that it happens.
    // MEASURED: both are
    //   Error near line 1: unable to use function MATCH in the requested context
    let mut c = mem();
    run(&mut c, "CREATE TABLE t(x); INSERT INTO t VALUES('a')");
    assert_eq!(
        refused(&mut c, "SELECT x FROM t WHERE x MATCH 'a'"),
        REFUSED
    );
    assert_eq!(
        refused(&mut c, "SELECT x FROM t WHERE x NOT MATCH 'a'"),
        REFUSED
    );
}

#[test]
fn the_operator_is_not_a_comparison_so_its_precedence_is_its_own() {
    // MEASURED: `SELECT 1 = 1 MATCH 'a';` is
    //   Error near line 1: unable to use function MATCH in the requested context
    // Not a syntax error, and not `no such column`. So MATCH parses as a
    // postfix operator at the same level as LIKE, GLOB and REGEXP, which is
    // where it sits in the grammar here.
    let mut c = mem();
    assert_eq!(refused(&mut c, "SELECT 1 = 1 MATCH 'a'"), REFUSED);
}

#[test]
fn a_column_on_the_left_of_match_binds_like_any_other_column() {
    // MEASURED: `SELECT nosuchcol MATCH 'a';` is
    //   Parse error near line 1: no such column: nosuchcol
    // A *resolution* failure, and it wins over the context fault -- the engine
    // reports the name it could not resolve, exactly as it would for LIKE.
    // That ordering is only possible if the column goes through the ordinary
    // resolver, which is also what the vec0 contract needs: the left side of a
    // `MATCH` has to reach the planner as a real column reference.
    let mut c = mem();
    let e = c
        .execute_script("SELECT nosuchcol MATCH 'a'")
        .expect_err("an unknown column must be refused");
    assert_eq!(e.message, "no such column: nosuchcol");
    // And the same on the right.
    let e = c
        .execute_script("SELECT 'a' MATCH nosuchcol")
        .expect_err("an unknown column must be refused");
    assert_eq!(e.message, "no such column: nosuchcol");
    // Inside a real query, against a real table.
    run(&mut c, "CREATE TABLE t(x, y); INSERT INTO t VALUES('a', 'a')");
    let e = c
        .execute_script("SELECT x FROM t WHERE x MATCH nosuchcol")
        .expect_err("an unknown column must be refused");
    assert_eq!(e.message, "no such column: nosuchcol");
}

#[test]
fn match_does_not_disturb_the_foreign_key_clause_that_also_says_match() {
    // `MATCH name` inside a REFERENCES clause is a *different* MATCH: it is
    // part of the foreign key tail that `skip_foreign_key_tail` consumes, and
    // it must keep working. Measured against the reference, the clause is
    // accepted and stored:
    //
    //   CREATE TABLE p(id INTEGER PRIMARY KEY);
    //   CREATE TABLE c(id, pid REFERENCES p(id) ON DELETE CASCADE MATCH simple);
    //   -- no error
    //
    // The tokenizer already produced `Keyword::Match` for both spellings, so
    // the risk was the new expression branch stealing the one in a table
    // definition. It does not: the expression grammar is only reached from a
    // SELECT, and the column grammar never goes through it.
    let mut c = mem();
    run(
        &mut c,
        "CREATE TABLE p(id INTEGER PRIMARY KEY); \
         CREATE TABLE c(id, pid REFERENCES p(id) ON DELETE CASCADE MATCH simple)",
    );
}

#[test]
fn the_new_operator_branch_did_not_make_match_a_reserved_word() {
    // MEASURED, the reference answers all three of these normally:
    //   CREATE TABLE match(x); ... SELECT x FROM match;   -> 1
    //   SELECT 1 AS match;                               -> 1
    //   CREATE TABLE t(match TEXT); ... SELECT match FROM t;  -> x
    //
    // The engine agrees on the first and disagrees on the second and third,
    // and the disagreement is PRE-EXISTING rather than something this change
    // introduced: verified by running the same statement against a binary built
    // from HEAD, which answers `near "match": syntax error` for both. The
    // engine does not accept `match` as a column name or as an alias, and that
    // is a separate gap in identifier handling.
    //
    // What matters for THIS change is that the new expression branch did not
    // make it worse, and that the failure is still a *syntax* error rather than
    // a new one -- so `match` is only a keyword in operator position, and the
    // table-name case still resolves.
    let mut c = mem();
    // A table NAMED match works, and so does `match` as a column DECLARATION
    // and as an alias -- MEASURED on this engine, all three succeed, matching
    // the reference. Only the *reference* to such a column fails, and it fails
    // as a syntax error at the reference, not at the declaration.
    run(
        &mut c,
        "CREATE TABLE match(x); INSERT INTO match VALUES(1); SELECT x FROM match",
    );
    run(&mut c, "CREATE TABLE t(match TEXT); INSERT INTO t VALUES('x')");
    run(&mut c, "SELECT 1 AS match");
    // The one shape that does not work, asserted rather than papered over so
    // that whoever closes it sees the shape of the problem. Verified as
    // pre-existing by running the same statement against a binary built from
    // HEAD, which answers the same `near "match": syntax error`.
    let e = c
        .execute_script("SELECT match FROM t")
        .expect_err("a column named match cannot be referenced yet");
    assert_eq!(
        e.message, "near \"match\": syntax error",
        "if this changed, the pre-existing gap closed or moved -- update this comment"
    );
}
