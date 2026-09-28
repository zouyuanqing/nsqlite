//! UNIQUE enforcement, measured against sqlite3 3.53.4 on this machine.
//!
//! The engine did not enforce UNIQUE at all: the duplicate row was *written*,
//! which is data corruption rather than a wrong answer, because a caller that
//! relies on the constraint gets a row it was promised could not be there. The
//! three ways a constraint can be written had disagreed, and only one of them
//! was enforced:
//!
//! | declaration                            | was it enforced |
//! | -------------------------------------- | --------------- |
//! | `a INTEGER PRIMARY KEY`               | yes -- the b-tree's own key check |
//! | `a UNIQUE`                             | no              |
//! | `CREATE UNIQUE INDEX i ON u(a)`       | no              |
//!
//! The first row is not evidence that uniqueness works. A single
//! `INTEGER PRIMARY KEY` is the *rowid alias*, so the duplicate is caught by the
//! b-tree key and no constraint check is involved at all -- which is exactly why
//! a UNIQUE column in the same table behaved differently.
//!
//! Every expectation below was measured against the real sqlite3, and each test
//! asserts the ROW COUNT after the duplicate, not merely that an error came
//! out. An INSERT refused for an unrelated reason -- a parse error, an
//! unsupported feature -- also produces an error and also leaves the count
//! right, so the count is what actually distinguishes enforcement from a
//! failure. Where a refusal must leave the table untouched, the tests compare
//! the rows themselves, not just how many there are.

use crate::connection::{Connection, Outcome, Row};
use crate::value::Value;

// -- helpers -------------------------------------------------------------

fn mem() -> Connection {
    Connection::open_memory().expect("open")
}

/// Runs a statement, expecting it to succeed.
fn run(c: &mut Connection, sql: &str) -> Outcome {
    c.execute_script(sql)
        .unwrap_or_else(|e| panic!("{sql:?} failed: {e}"))
        .into_iter()
        .last()
        .expect("one outcome")
}

/// Runs a statement, expecting it to be refused, and returns the message.
///
/// The message and not the `Display` form: `Display` prefixes the result code,
/// so it reads `CONSTRAINT: UNIQUE constraint failed: u.a` where sqlite3's text
/// is `UNIQUE constraint failed: u.a`. The suite compares the message.
fn err(c: &mut Connection, sql: &str) -> String {
    match c.execute_script(sql) {
        Ok(_) => panic!("{sql:?} was accepted; sqlite3 refuses it"),
        Err(e) => e.message,
    }
}

/// How many rows a one-column table holds.
fn count(c: &mut Connection, table: &str) -> i64 {
    match run(c, &format!("SELECT count(*) FROM {table}")) {
        Outcome::Query { rows, .. } => rows[0].values[0].as_i64().expect("an integer count"),
        other => panic!("expected a query, got {other:?}"),
    }
}

/// The rows of a table, as text, so a test can assert the table is unchanged
/// rather than merely that it did not grow.
fn contents(c: &mut Connection, table: &str) -> Vec<String> {
    match run(c, &format!("SELECT * FROM {table} ORDER BY rowid")) {
        Outcome::Query { rows, .. } => rows
            .into_iter()
            .map(|Row { values, .. }| {
                values
                    .iter()
                    .map(|v| match v {
                        Value::Null => "NULL".to_string(),
                        other => other.to_string(),
                    })
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect(),
        other => panic!("expected a query, got {other:?}"),
    }
}

// -- the three paths that disagreed --------------------------------------

#[test]
fn a_unique_column_refuses_the_duplicate() {
    // sqlite3: Error near line 3: UNIQUE constraint failed: uq1.a, count 1.
    let mut c = mem();
    run(&mut c, "CREATE TABLE uq1(a UNIQUE)");
    run(&mut c, "INSERT INTO uq1 VALUES(1)");
    assert_eq!(
        err(&mut c, "INSERT INTO uq1 VALUES(1)"),
        "UNIQUE constraint failed: uq1.a"
    );
    assert_eq!(count(&mut c, "uq1"), 1);
}

#[test]
fn a_create_unique_index_refuses_the_duplicate_with_the_same_message() {
    // The reference does not tell a column constraint from a UNIQUE index over
    // the same column: both report `UNIQUE constraint failed: u.a`. The suite
    // compares this text verbatim, so an enforcement that only covered the
    // declared constraint is observably incomplete.
    let mut c = mem();
    run(&mut c, "CREATE TABLE u(a)");
    run(&mut c, "CREATE UNIQUE INDEX i ON u(a)");
    run(&mut c, "INSERT INTO u VALUES(1)");
    assert_eq!(
        err(&mut c, "INSERT INTO u VALUES(1)"),
        "UNIQUE constraint failed: u.a"
    );
    assert_eq!(count(&mut c, "u"), 1);
}

#[test]
fn the_rowid_alias_still_refuses_its_duplicate() {
    // Unchanged, and for the reason the other two lacked: a single INTEGER
    // PRIMARY KEY is the rowid alias, so the b-tree's own key check catches it.
    let mut c = mem();
    run(&mut c, "CREATE TABLE p(a INTEGER PRIMARY KEY)");
    run(&mut c, "INSERT INTO p VALUES(1)");
    assert_eq!(
        err(&mut c, "INSERT INTO p VALUES(1)"),
        "UNIQUE constraint failed: p.a"
    );
    assert_eq!(count(&mut c, "p"), 1);
}

// -- NULL is exempt -------------------------------------------------------

#[test]
fn any_number_of_nulls_is_legal() {
    // The single most likely thing to get wrong. NULL is not equal to itself
    // for this purpose, so a "have I seen this value before" test that treats
    // two NULLs as equal refuses a row sqlite3 accepts.
    let mut c = mem();
    run(&mut c, "CREATE TABLE u(a UNIQUE)");
    run(&mut c, "INSERT INTO u VALUES(NULL)");
    run(&mut c, "INSERT INTO u VALUES(NULL)");
    assert_eq!(count(&mut c, "u"), 2);
}

#[test]
fn nulls_interleave_and_only_the_real_duplicate_is_refused() {
    // Measured: count 3.
    let mut c = mem();
    run(&mut c, "CREATE TABLE u(a UNIQUE)");
    run(&mut c, "INSERT INTO u VALUES(NULL)");
    run(&mut c, "INSERT INTO u VALUES(1)");
    run(&mut c, "INSERT INTO u VALUES(NULL)");
    assert_eq!(
        err(&mut c, "INSERT INTO u VALUES(1)"),
        "UNIQUE constraint failed: u.a"
    );
    assert_eq!(count(&mut c, "u"), 3);
}

#[test]
fn the_exemption_keys_on_any_null_column_not_on_an_all_null_row() {
    // A composite key is exempt when ANY of its columns is NULL, so (1,NULL)
    // twice is accepted. Checking "the row is all NULLs" would refuse it.
    let mut c = mem();
    run(&mut c, "CREATE TABLE u(a,b,UNIQUE(a,b))");
    run(&mut c, "INSERT INTO u VALUES(1,NULL)");
    run(&mut c, "INSERT INTO u VALUES(1,NULL)");
    assert_eq!(count(&mut c, "u"), 2);
}

#[test]
fn a_composite_primary_key_inherits_the_null_exemption() {
    // Because sqlite3 implements it as an autoindex, which exempts the same
    // way a UNIQUE does. Measured: 2.
    let mut c = mem();
    run(&mut c, "CREATE TABLE u(a,b,PRIMARY KEY(a,b))");
    run(&mut c, "INSERT INTO u VALUES(NULL,1)");
    run(&mut c, "INSERT INTO u VALUES(NULL,1)");
    assert_eq!(count(&mut c, "u"), 2);
}

// -- the message names every column of the key ---------------------------

#[test]
fn a_multi_column_violation_names_every_column_comma_separated() {
    // `msg::unique_constraint(table, column)` takes one name and cannot express
    // this, which is why the multi-column form needs its own message builder.
    let mut c = mem();
    run(&mut c, "CREATE TABLE u(a,b, UNIQUE(a,b))");
    run(&mut c, "INSERT INTO u VALUES(1,1)");
    run(&mut c, "INSERT INTO u VALUES(1,2)");
    assert_eq!(
        err(&mut c, "INSERT INTO u VALUES(1,1)"),
        "UNIQUE constraint failed: u.a, u.b"
    );
    assert_eq!(count(&mut c, "u"), 2);
}

#[test]
fn a_unique_index_over_a_pair_matches_a_table_level_constraint() {
    // Byte-identical to the table-level form above, so the two agree.
    let mut c = mem();
    run(&mut c, "CREATE TABLE u(a,b)");
    run(&mut c, "CREATE UNIQUE INDEX i ON u(a,b)");
    run(&mut c, "INSERT INTO u VALUES(1,1)");
    run(&mut c, "INSERT INTO u VALUES(1,2)");
    assert_eq!(
        err(&mut c, "INSERT INTO u VALUES(1,1)"),
        "UNIQUE constraint failed: u.a, u.b"
    );
    assert_eq!(count(&mut c, "u"), 2);
}

#[test]
fn two_column_uniques_are_two_independent_constraints() {
    // The violation names the column that collided, not the table.
    let mut c = mem();
    run(&mut c, "CREATE TABLE u(a UNIQUE, b UNIQUE)");
    run(&mut c, "INSERT INTO u VALUES(1,1)");
    assert_eq!(
        err(&mut c, "INSERT INTO u VALUES(1,2)"),
        "UNIQUE constraint failed: u.a"
    );
    assert_eq!(count(&mut c, "u"), 1);
}

#[test]
fn a_second_column_constraint_reports_its_own_column() {
    let mut c = mem();
    run(&mut c, "CREATE TABLE u(a UNIQUE, b UNIQUE)");
    run(&mut c, "INSERT INTO u VALUES(1,1)");
    assert_eq!(
        err(&mut c, "INSERT INTO u VALUES(2,1)"),
        "UNIQUE constraint failed: u.b"
    );
    assert_eq!(count(&mut c, "u"), 1);
}

// -- primary key shapes that are not the rowid alias ---------------------

#[test]
fn a_composite_primary_key_is_a_uniqueness_constraint() {
    // Measured: `UNIQUE constraint failed: u.a, u.b`, count 1. The message is a
    // UNIQUE message, not a PRIMARY KEY one.
    let mut c = mem();
    run(&mut c, "CREATE TABLE u(a,b,PRIMARY KEY(a,b))");
    run(&mut c, "INSERT INTO u VALUES(1,1)");
    assert_eq!(
        err(&mut c, "INSERT INTO u VALUES(1,1)"),
        "UNIQUE constraint failed: u.a, u.b"
    );
    assert_eq!(count(&mut c, "u"), 1);
}

#[test]
fn a_non_integer_primary_key_is_a_uniqueness_constraint_not_a_rowid_alias() {
    // `a PRIMARY KEY` without INTEGER is NOT the alias, so nothing but a real
    // constraint check refuses it -- and nothing did before.
    let mut c = mem();
    run(&mut c, "CREATE TABLE w(a PRIMARY KEY)");
    run(&mut c, "INSERT INTO w VALUES(1)");
    assert_eq!(
        err(&mut c, "INSERT INTO w VALUES(1)"),
        "UNIQUE constraint failed: w.a"
    );
    assert_eq!(count(&mut c, "w"), 1);
}

#[test]
fn the_rowid_alias_is_not_treated_as_a_uniqueness_constraint() {
    // The alias is already enforced by the b-tree. Enforcing it a second time
    // would double every check and, worse, under OR REPLACE would make the
    // conflict handling delete the row it is about to write. Two NULLs are two
    // distinct rows, because the alias auto-assigns a fresh rowid each time.
    let mut c = mem();
    run(&mut c, "CREATE TABLE p(a INTEGER PRIMARY KEY)");
    run(&mut c, "INSERT INTO p VALUES(NULL)");
    run(&mut c, "INSERT INTO p VALUES(NULL)");
    assert_eq!(count(&mut c, "p"), 2);
}

// -- the statement is atomic ---------------------------------------------

#[test]
fn a_duplicate_within_one_statement_writes_nothing() {
    // Measured: count 0, NOT 1. The whole statement is atomic, not just the
    // failing row -- which contradicts a comment that used to claim otherwise.
    let mut c = mem();
    run(&mut c, "CREATE TABLE u(a UNIQUE)");
    assert_eq!(
        err(&mut c, "INSERT INTO u VALUES(1),(1)"),
        "UNIQUE constraint failed: u.a"
    );
    assert_eq!(count(&mut c, "u"), 0);
}

#[test]
fn rows_written_before_the_duplicate_are_rolled_back_too() {
    // The stronger half of atomicity, and the one that is easy to miss: the
    // (2,1) written before the (1,2) that fails is gone as well.
    let mut c = mem();
    run(&mut c, "CREATE TABLE u(a UNIQUE,b)");
    run(&mut c, "INSERT INTO u VALUES(1,1)");
    run(&mut c, "INSERT INTO u VALUES(2,2)");
    assert_eq!(
        err(&mut c, "INSERT INTO u VALUES(2,1),(1,2)"),
        "UNIQUE constraint failed: u.a"
    );
    assert_eq!(contents(&mut c, "u"), vec!["1|1", "2|2"]);
}

#[test]
fn insert_select_is_atomic_the_same_way() {
    // Measured: count 0, not 1.
    let mut c = mem();
    run(&mut c, "CREATE TABLE u(a UNIQUE)");
    run(&mut c, "CREATE TABLE s(x)");
    run(&mut c, "INSERT INTO s VALUES(1),(2),(1)");
    assert_eq!(
        err(&mut c, "INSERT INTO u SELECT x FROM s"),
        "UNIQUE constraint failed: u.a"
    );
    assert_eq!(count(&mut c, "u"), 0);
}

#[test]
fn a_refused_insert_leaves_the_table_exactly_as_it_was() {
    // The rows, not merely the count: a refusal that deleted the conflicting
    // row would leave the same count as one that refused it.
    let mut c = mem();
    run(&mut c, "CREATE TABLE u(a UNIQUE,b)");
    run(&mut c, "INSERT INTO u VALUES(1,'x')");
    run(&mut c, "INSERT INTO u VALUES(2,'y')");
    let before = contents(&mut c, "u");
    assert!(err(&mut c, "INSERT INTO u VALUES(1,'z')").starts_with("UNIQUE constraint failed"));
    assert_eq!(contents(&mut c, "u"), before);
}

// -- conflict actions -----------------------------------------------------

#[test]
fn or_ignore_skips_the_conflicting_row_and_keeps_the_rest() {
    // Per ROW, not per statement: the 2 and the 3 both land.
    let mut c = mem();
    run(&mut c, "CREATE TABLE u(a UNIQUE)");
    run(&mut c, "INSERT INTO u VALUES(1)");
    run(&mut c, "INSERT OR IGNORE INTO u VALUES(2),(3),(1)");
    assert_eq!(count(&mut c, "u"), 3);
    assert_eq!(contents(&mut c, "u"), vec!["1", "2", "3"]);
}

#[test]
fn or_abort_and_the_default_roll_the_whole_statement_back() {
    // Measured for the default and for OR ABORT: count 1, holding just the 1.
    for clause in ["", " OR ABORT"] {
        let mut c = mem();
        run(&mut c, "CREATE TABLE u(a UNIQUE)");
        run(&mut c, "INSERT INTO u VALUES(1)");
        let sql = format!("INSERT{clause} INTO u VALUES(2),(1)");
        assert_eq!(
            err(&mut c, &sql),
            "UNIQUE constraint failed: u.a",
            "clause {clause:?}"
        );
        assert_eq!(count(&mut c, "u"), 1, "clause {clause:?}");
        assert_eq!(contents(&mut c, "u"), vec!["1"], "clause {clause:?}");
    }
}

#[test]
fn or_replace_deletes_the_old_row_so_the_rowid_changes() {
    // The surprise, and the reason a first implementation is wrong twice by
    // "updating the row in place": OR REPLACE is a DELETE followed by an
    // INSERT, so the surviving row carries a NEW rowid. Measured: 2|1|new,
    // where an in-place update would have left it at rowid 1 -- and would also
    // have left anything pointing at the old row valid, which the reference
    // does not.
    let mut c = mem();
    run(&mut c, "CREATE TABLE u(a UNIQUE,b)");
    run(&mut c, "INSERT INTO u VALUES(1,'old')");
    run(&mut c, "INSERT OR REPLACE INTO u VALUES(1,'new')");
    assert_eq!(count(&mut c, "u"), 1);
    assert_eq!(contents(&mut c, "u"), vec!["1|new"]);
    // The rowid is 2, not the 1 the first insert used.
    match run(&mut c, "SELECT rowid FROM u") {
        Outcome::Query { rows, .. } => {
            assert_eq!(rows[0].values[0].as_i64(), Some(2), "OR REPLACE keeps the old rowid")
        }
        other => panic!("expected a query, got {other:?}"),
    }
}

#[test]
fn or_replace_is_per_row() {
    // The non-conflicting row in the same statement survives, which is the
    // contrast with ABORT above.
    let mut c = mem();
    run(&mut c, "CREATE TABLE u(a UNIQUE)");
    run(&mut c, "INSERT INTO u VALUES(1)");
    run(&mut c, "INSERT INTO u VALUES(2)");
    run(&mut c, "INSERT INTO u VALUES(3)");
    run(&mut c, "INSERT OR REPLACE INTO u VALUES(3),(1)");
    assert_eq!(count(&mut c, "u"), 3);
    assert_eq!(contents(&mut c, "u"), vec!["2", "3", "1"]);
}

#[test]
fn or_ignore_does_not_cover_a_not_null_violation() {
    // A NOT NULL failure in the same statement still aborts it, because it is
    // not a constraint *conflict*. Conflating the two would make OR IGNORE
    // swallow a row sqlite3 refuses.
    let mut c = mem();
    run(&mut c, "CREATE TABLE u(a NOT NULL, b UNIQUE)");
    assert!(err(&mut c, "INSERT OR IGNORE INTO u VALUES(NULL,1)").starts_with("NOT NULL"));
    assert_eq!(count(&mut c, "u"), 0);
}

// -- UPDATE is a different code path -------------------------------------

#[test]
fn an_update_into_an_existing_value_is_refused() {
    // A separate path from INSERT, and easy to miss for exactly that reason.
    // The message is the same, and the table is left as it was.
    let mut c = mem();
    run(&mut c, "CREATE TABLE u(a UNIQUE)");
    run(&mut c, "INSERT INTO u VALUES(1)");
    run(&mut c, "INSERT INTO u VALUES(2)");
    assert_eq!(
        err(&mut c, "UPDATE u SET a=1 WHERE a=2"),
        "UNIQUE constraint failed: u.a"
    );
    assert_eq!(count(&mut c, "u"), 2);
    assert_eq!(contents(&mut c, "u"), vec!["1", "2"]);
}

#[test]
fn an_update_that_does_not_move_a_row_is_not_a_violation() {
    // The check must skip the row being updated. UPDATE removes the old row
    // before writing the new one, so a row that keeps its own value would
    // otherwise collide with itself.
    let mut c = mem();
    run(&mut c, "CREATE TABLE u(a UNIQUE)");
    run(&mut c, "INSERT INTO u VALUES(1)");
    run(&mut c, "UPDATE u SET a=1");
    assert_eq!(count(&mut c, "u"), 1);
}

#[test]
fn a_composite_update_violation_names_every_column() {
    let mut c = mem();
    run(&mut c, "CREATE TABLE u(a,b,UNIQUE(a,b))");
    run(&mut c, "INSERT INTO u VALUES(1,1)");
    run(&mut c, "INSERT INTO u VALUES(1,2)");
    assert_eq!(
        err(&mut c, "UPDATE u SET b=1 WHERE b=2"),
        "UNIQUE constraint failed: u.a, u.b"
    );
    assert_eq!(count(&mut c, "u"), 2);
}

#[test]
fn update_or_ignore_leaves_the_offending_row_alone() {
    // Measured: count stays 2.
    let mut c = mem();
    run(&mut c, "CREATE TABLE u(a UNIQUE)");
    run(&mut c, "INSERT INTO u VALUES(1)");
    run(&mut c, "INSERT INTO u VALUES(2)");
    run(&mut c, "UPDATE OR IGNORE u SET a=1 WHERE a=2");
    assert_eq!(count(&mut c, "u"), 2);
}

#[test]
fn update_or_replace_drops_the_conflicting_row() {
    // Measured: count 1.
    let mut c = mem();
    run(&mut c, "CREATE TABLE u(a UNIQUE)");
    run(&mut c, "INSERT INTO u VALUES(1)");
    run(&mut c, "INSERT INTO u VALUES(2)");
    run(&mut c, "UPDATE OR REPLACE u SET a=1 WHERE a=2");
    assert_eq!(count(&mut c, "u"), 1);
}

// -- storage classes: the ordering decides, not `==` ---------------------

#[test]
fn one_and_one_point_zero_are_the_same_class_and_collide() {
    // The compare is `Value::compare`, not `==` and not a hash of the bytes: an
    // integer 1 and the real 1.0 compare equal. Either shortcut gets this
    // wrong, and the record stream will not show which class survived.
    let mut c = mem();
    run(&mut c, "CREATE TABLE u(a UNIQUE)");
    run(&mut c, "INSERT INTO u VALUES(1)");
    assert_eq!(
        err(&mut c, "INSERT INTO u VALUES(1.0)"),
        "UNIQUE constraint failed: u.a"
    );
    assert_eq!(count(&mut c, "u"), 1);
    // Confirmed by value, not by the record stream: the FIRST row wins, so the
    // survivor is the integer, not the real.
    let mut c2 = mem();
    run(&mut c2, "CREATE TABLE u(a UNIQUE)");
    run(&mut c2, "INSERT INTO u VALUES(1)");
    let _ = c2.execute_script("INSERT INTO u VALUES(1.0)");
    match run(&mut c2, "SELECT typeof(a) FROM u") {
        Outcome::Query { rows, .. } => assert_eq!(rows[0].values[0].to_string(), "integer"),
        other => panic!("expected a query, got {other:?}"),
    }
}

#[test]
fn one_and_the_text_one_are_different_classes_and_both_land() {
    // The boundary of the rule above.
    let mut c = mem();
    run(&mut c, "CREATE TABLE u(a UNIQUE)");
    run(&mut c, "INSERT INTO u VALUES(1)");
    run(&mut c, "INSERT INTO u VALUES('1')");
    assert_eq!(count(&mut c, "u"), 2);
}

#[test]
fn an_integer_and_a_blob_are_different_classes() {
    let mut c = mem();
    run(&mut c, "CREATE TABLE u(a UNIQUE)");
    run(&mut c, "INSERT INTO u VALUES(1)");
    run(&mut c, "INSERT INTO u VALUES(x'31')");
    assert_eq!(count(&mut c, "u"), 2);
}

#[test]
fn two_identical_blobs_collide() {
    // BLOB is a different class from INTEGER, but two of the same blob are
    // equal. So do two identical reals, and two empty texts/blobs.
    let mut c = mem();
    run(&mut c, "CREATE TABLE u(a UNIQUE)");
    run(&mut c, "INSERT INTO u VALUES(x'0102')");
    assert_eq!(
        err(&mut c, "INSERT INTO u VALUES(x'0102')"),
        "UNIQUE constraint failed: u.a"
    );
    assert_eq!(count(&mut c, "u"), 1);
}

#[test]
fn zero_point_zero_and_negative_zero_collide() {
    let mut c = mem();
    run(&mut c, "CREATE TABLE u(a UNIQUE)");
    run(&mut c, "INSERT INTO u VALUES(0.0)");
    assert_eq!(
        err(&mut c, "INSERT INTO u VALUES(-0.0)"),
        "UNIQUE constraint failed: u.a"
    );
    assert_eq!(count(&mut c, "u"), 1);
}

#[test]
fn two_empty_strings_collide() {
    // Empty string and empty blob are ordinary values, not NULLs.
    let mut c = mem();
    run(&mut c, "CREATE TABLE u(a UNIQUE)");
    run(&mut c, "INSERT INTO u VALUES('')");
    assert_eq!(
        err(&mut c, "INSERT INTO u VALUES('')"),
        "UNIQUE constraint failed: u.a"
    );
    assert_eq!(count(&mut c, "u"), 1);
}

#[test]
fn text_is_compared_by_bytes() {
    // '1' and '1 ' are distinct, both accepted.
    let mut c = mem();
    run(&mut c, "CREATE TABLE u(a UNIQUE)");
    run(&mut c, "INSERT INTO u VALUES('1')");
    run(&mut c, "INSERT INTO u VALUES('1 ')");
    assert_eq!(count(&mut c, "u"), 2);
}

#[test]
fn large_integers_are_compared_losslessly_not_as_doubles() {
    // 2^53+1 is NOT equal to 2^53.0. A check that routes integers through f64
    // gets this wrong, and the difference is a row that should not be there.
    let mut c = mem();
    run(&mut c, "CREATE TABLE u(a UNIQUE)");
    run(&mut c, "INSERT INTO u VALUES(9007199254740993)");
    run(&mut c, "INSERT INTO u VALUES(9007199254740992.0)");
    assert_eq!(count(&mut c, "u"), 2);
}

// -- affinity applies before the test ------------------------------------

#[test]
fn affinity_is_applied_before_the_uniqueness_test() {
    // A TEXT column holding 1 is already the text '1' when the compare happens,
    // so these two collide -- and the survivor is the TEXT value, which is what
    // says the conversion ran first. Checking the raw expression instead would
    // let both through.
    let mut c = mem();
    run(&mut c, "CREATE TABLE u(a TEXT UNIQUE)");
    run(&mut c, "INSERT INTO u VALUES(1)");
    assert_eq!(
        err(&mut c, "INSERT INTO u VALUES('1')"),
        "UNIQUE constraint failed: u.a"
    );
    assert_eq!(count(&mut c, "u"), 1);
    match run(&mut c, "SELECT typeof(a) FROM u") {
        Outcome::Query { rows, .. } => assert_eq!(rows[0].values[0].to_string(), "text"),
        other => panic!("expected a query, got {other:?}"),
    }
}

#[test]
fn integer_affinity_coerces_before_the_compare_too() {
    let mut c = mem();
    run(&mut c, "CREATE TABLE u(a INTEGER UNIQUE)");
    run(&mut c, "INSERT INTO u VALUES('1')");
    assert_eq!(
        err(&mut c, "INSERT INTO u VALUES(1)"),
        "UNIQUE constraint failed: u.a"
    );
    assert_eq!(count(&mut c, "u"), 1);
}

#[test]
fn a_unique_index_over_an_affinity_column_behaves_the_same() {
    // No declared constraint here, so the class rules alone decide -- and they
    // decide the same way they did for the column constraint.
    let mut c = mem();
    run(&mut c, "CREATE TABLE u(a)");
    run(&mut c, "CREATE UNIQUE INDEX i ON u(a)");
    run(&mut c, "INSERT INTO u VALUES(1)");
    assert_eq!(
        err(&mut c, "INSERT INTO u VALUES(1.0)"),
        "UNIQUE constraint failed: u.a"
    );
    assert_eq!(count(&mut c, "u"), 1);
}

// -- a table with no uniqueness constraint is untouched ------------------

#[test]
fn a_table_without_a_unique_constraint_still_takes_duplicates() {
    // The negative case, which is what makes the positive ones meaningful: if
    // the check ran unconditionally these two would be refused.
    let mut c = mem();
    run(&mut c, "CREATE TABLE t(a,b)");
    run(&mut c, "INSERT INTO t VALUES(1,1)");
    run(&mut c, "INSERT INTO t VALUES(1,1)");
    assert_eq!(count(&mut c, "t"), 2);
}

#[test]
fn a_non_unique_index_does_not_enforce() {
    // Only a UNIQUE index constrains; a plain one is for lookup and must stay
    // silent, or the enforcement would be over-broad.
    let mut c = mem();
    run(&mut c, "CREATE TABLE t(a)");
    run(&mut c, "CREATE INDEX i ON t(a)");
    run(&mut c, "INSERT INTO t VALUES(1)");
    run(&mut c, "INSERT INTO t VALUES(1)");
    assert_eq!(count(&mut c, "t"), 2);
}

#[test]
fn a_unique_index_is_recorded_as_unique() {
    // The bug this locks in: the parser's CREATE dispatch ate the UNIQUE and
    // then `create_index` looked for it a second time, so every UNIQUE index
    // was recorded as non-unique. Nothing then enforced it, and the write path
    // cannot tell an unenforced index from a non-unique one -- the flag is the
    // whole difference. Measured: sqlite3's `PRAGMA index_list` reports 1.
    let mut c = mem();
    run(&mut c, "CREATE TABLE u(a)");
    run(&mut c, "CREATE UNIQUE INDEX i ON u(a)");
    match run(&mut c, "PRAGMA index_list(u)") {
        Outcome::Query { rows, .. } => {
            let flags = &rows[0].values;
            assert_eq!(flags[2].as_i64(), Some(1), "PRAGMA index_list unique flag: {flags:?}");
        }
        other => panic!("expected a query, got {other:?}"),
    }
}
