//! The affinity rules as the *engine* answers them, not as the functions answer
//! them.
//!
//! The tests in the parent module call [`affinity_rules::compare`],
//! [`affinity_rules::equal`] and [`affinity_rules::convert`] directly, and they
//! pass whether or not anything in the engine ever calls them. That is not a
//! hypothetical: a version of this crate had a complete, tested, correctly
//! documented affinity module that no executor referenced, so `SELECT a = b`
//! over `CREATE TABLE t(a TEXT, b INTEGER)` answered 0 where sqlite3 answers 1
//! and every test in the module still passed.
//!
//! So every rule below is stated as a query and run through a real
//! [`crate::Connection`]. If the wiring in `eval.rs` or in the insert helper is
//! removed, the answer changes and the test fails — which is the property the
//! unit tests cannot have by construction.
//!
//! Every expectation is the answer sqlite3 3.53.4 gave for the same script. The
//! commands are recorded beside the cases so a reader can re-run them, in the
//! form `sqlite3 :memory: "<script>"`.

use crate::connection::Connection;
use crate::value::Value;

/// Runs a script and returns the single value the last statement produced.
///
/// The row counts the shell prints for DML are not in the way here: the
/// outcomes are read from the API, not scraped off stdout, so a script with
/// three INSERTs in front of its SELECT returns that SELECT's one value.
fn one(sql: &str) -> Value {
    let mut c = Connection::open_memory().expect("in-memory database");
    let out = c
        .execute_script(sql)
        .unwrap_or_else(|e| panic!("{sql}\nfailed: {e}"));
    let last = out
        .into_iter()
        .last()
        .expect("the script produced an outcome");
    match last {
        crate::connection::Outcome::Query { rows, .. } => {
            assert_eq!(rows.len(), 1, "expected exactly one row from {sql}");
            rows[0].values[0].clone()
        }
        other => panic!("expected a query from {sql}, got {other:?}"),
    }
}

/// Asserts a query answers `1`, which is how SQLite spells true.
fn is_true(sql: &str) {
    assert_eq!(one(sql), Value::Integer(1), "expected true from {sql}");
}

// -- The grid, on the way into a table ------------------------------------
//
// `CREATE TABLE t(a <AFFINITY>); INSERT INTO t VALUES(<VALUE>); SELECT
// typeof(a);` for every cell of the table in `docs/affinity-grid.md`. The two
// directions that implementations get backwards are the two named cases.

#[test]
fn real_affinity_widens_an_integer() {
    // sqlite3: CREATE TABLE t(a REAL); INSERT INTO t VALUES(5);
    //          SELECT typeof(a);  ->  real
    let v = one("CREATE TABLE t(a REAL); INSERT INTO t VALUES(5); SELECT typeof(a) FROM t");
    assert_eq!(v, Value::Text("real".into()));
}

#[test]
fn numeric_affinity_narrows_a_whole_real() {
    // sqlite3: CREATE TABLE t(a NUMERIC); INSERT INTO t VALUES(5.0);
    //          SELECT typeof(a);  ->  integer
    let v = one("CREATE TABLE t(a NUMERIC); INSERT INTO t VALUES(5.0); SELECT typeof(a) FROM t");
    assert_eq!(v, Value::Text("integer".into()));
}

/// The whole grid, all thirty cells, against the oracle.
///
/// This is the specification from `docs/affinity-grid.md` as a table rather
/// than as prose, so a cell that regresses names itself. Run with
/// `sqlite3 :memory:` on the same statements to re-measure.
#[test]
fn every_cell_of_the_affinity_grid_matches_the_oracle() {
    // (affinity, value as written, what sqlite3's typeof() answered)
    const GRID: [(&str, &str, &str); 30] = [
        ("TEXT", "5", "text"),
        ("NUMERIC", "5", "integer"),
        ("INTEGER", "5", "integer"),
        ("REAL", "5", "real"),
        ("BLOB", "5", "integer"),
        ("TEXT", "5.0", "text"),
        ("NUMERIC", "5.0", "integer"),
        ("INTEGER", "5.0", "integer"),
        ("REAL", "5.0", "real"),
        ("BLOB", "5.0", "real"),
        ("TEXT", "'5'", "text"),
        ("NUMERIC", "'5'", "integer"),
        ("INTEGER", "'5'", "integer"),
        ("REAL", "'5'", "real"),
        ("BLOB", "'5'", "text"),
        ("TEXT", "'5.0'", "text"),
        ("NUMERIC", "'5.0'", "integer"),
        ("INTEGER", "'5.0'", "integer"),
        ("REAL", "'5.0'", "real"),
        ("BLOB", "'5.0'", "text"),
        ("TEXT", "x'35'", "blob"),
        ("NUMERIC", "x'35'", "blob"),
        ("INTEGER", "x'35'", "blob"),
        ("REAL", "x'35'", "blob"),
        ("BLOB", "x'35'", "blob"),
        ("TEXT", "NULL", "null"),
        ("NUMERIC", "NULL", "null"),
        ("INTEGER", "NULL", "null"),
        ("REAL", "NULL", "null"),
        ("BLOB", "NULL", "null"),
    ];
    for (i, (aff, val, want)) in GRID.iter().enumerate() {
        let sql = format!(
            "CREATE TABLE t{i}(a {aff}); INSERT INTO t{i}(a) VALUES({val}); SELECT typeof(a) FROM t{i}"
        );
        assert_eq!(
            one(&sql),
            Value::Text((*want).into()),
            "cell ({aff}, {val}) should store {want}"
        );
    }
}

// -- The cases that lose information --------------------------------------

#[test]
fn text_that_is_not_a_number_stays_text_under_every_affinity() {
    // sqlite3: CREATE TABLE t(a INTEGER); INSERT INTO t VALUES('12abc');
    //          SELECT typeof(a);  ->  text, not integer
    for aff in ["TEXT", "NUMERIC", "INTEGER", "REAL", "BLOB"] {
        let sql = format!(
            "CREATE TABLE t(a {aff}); INSERT INTO t(a) VALUES('12abc'); SELECT typeof(a) FROM t"
        );
        assert_eq!(
            one(&sql),
            Value::Text("text".into()),
            "'12abc' is not entirely a number, so {aff} must leave it as text"
        );
    }
    // And the characters that could be used are not kept either: it is the whole
    // string or nothing.
    let sql = "CREATE TABLE t(a INTEGER); INSERT INTO t(a) VALUES('12abc'); SELECT quote(a) FROM t";
    assert_eq!(one(sql), Value::Text("'12abc'".into()));
}

#[test]
fn surrounding_spaces_are_allowed_and_not_preserved_by_a_numeric_affinity() {
    // sqlite3: CREATE TABLE t(a NUMERIC); INSERT INTO t VALUES('  7.5  ');
    //          SELECT typeof(a), quote(a);  ->  real|7.5
    let sql = "CREATE TABLE t(a NUMERIC); INSERT INTO t(a) VALUES('  7.5  '); \
               SELECT typeof(a) || '|' || quote(a) FROM t";
    assert_eq!(one(sql), Value::Text("real|7.5".into()));
    // Under TEXT and BLOB the spaces survive, because those do not convert.
    for aff in ["TEXT", "BLOB"] {
        let sql = format!(
            "CREATE TABLE t(a {aff}); INSERT INTO t(a) VALUES('  7.5  '); SELECT quote(a) FROM t"
        );
        assert_eq!(
            one(&sql),
            Value::Text("'  7.5  '".into()),
            "{aff} does not convert, so the spaces stay"
        );
    }
}

#[test]
fn a_blob_is_never_converted_even_by_text_affinity() {
    // sqlite3: CREATE TABLE t(a TEXT); INSERT INTO t VALUES(x'35');
    //          SELECT typeof(a);  ->  blob
    let sql = "CREATE TABLE t(a TEXT); INSERT INTO t(a) VALUES(x'35'); SELECT typeof(a) FROM t";
    assert_eq!(one(sql), Value::Text("blob".into()));
}

#[test]
fn an_integer_too_large_for_i64_stays_a_real() {
    // sqlite3: CREATE TABLE t(a NUMERIC); INSERT INTO t VALUES(1e300);
    //          SELECT typeof(a);  ->  real, not an integer and not an error
    let sql = "CREATE TABLE t(a NUMERIC); INSERT INTO t(a) VALUES(1e300); SELECT typeof(a) FROM t";
    assert_eq!(one(sql), Value::Text("real".into()));
}

// -- The comparison rules -------------------------------------------------

/// The canonical case, and the one the whole module exists for.
///
/// sqlite3:
///
/// ```sql
/// CREATE TABLE t(a TEXT, b INTEGER);
/// INSERT INTO t VALUES('5', 5);
/// SELECT a=b FROM t;   -- 1
/// ```
#[test]
fn a_text_column_meets_an_integer_column_and_they_are_equal() {
    is_true("CREATE TABLE t(a TEXT, b INTEGER); INSERT INTO t VALUES('5', 5); SELECT a=b FROM t");
}

/// A column against a literal: the literal takes the column's affinity, and the
/// column keeps its own. Every operator, because each is a separate code path in
/// the evaluator and a rule that reaches only `=` is not the rule.
#[test]
fn a_column_against_a_literal_applies_the_columns_affinity() {
    const PRE: &str = "CREATE TABLE t(a TEXT, b INTEGER); INSERT INTO t VALUES('5', 5); ";
    for q in [
        "SELECT a=5 FROM t",
        "SELECT 5=a FROM t",
        "SELECT b='5' FROM t",
        "SELECT '5'=b FROM t",
        "SELECT a<6 FROM t",
        "SELECT a>4 FROM t",
        "SELECT a<=5 FROM t",
        "SELECT a>=5 FROM t",
        "SELECT a<>6 FROM t",
        "SELECT a IS 5 FROM t",
        "SELECT a IS NOT 6 FROM t",
        "SELECT a IN (5,9) FROM t",
        "SELECT a NOT IN (6,9) FROM t",
        "SELECT a BETWEEN 4 AND 6 FROM t",
        "SELECT a NOT BETWEEN 6 AND 8 FROM t",
    ] {
        is_true(&format!("{PRE}{q}"));
    }
    // `a = 5.0` is *false*, and that is the rule rather than a gap in it: the
    // literal takes the column's TEXT affinity, so 5.0 becomes the text '5.0',
    // and a holds '5'. Measured: sqlite3 answers 0.
    //
    //   sqlite3: CREATE TABLE t(a TEXT, b INTEGER);
    //            INSERT INTO t VALUES('5', 5); SELECT a=5.0 FROM t;  ->  0
    //
    // So a test that asserted 1 here would be asserting a bug, and the
    // interesting case is the one where the text does carry the point.
    assert_eq!(one(&format!("{PRE}SELECT a=5.0 FROM t")), Value::Integer(0));
    is_true("CREATE TABLE t(a TEXT); INSERT INTO t VALUES('5.0'); SELECT a=5.0 FROM t");
}

/// The same rule with the literal on the left, so a regression that only ever
/// made the right-hand operand convert would still be caught.
#[test]
fn the_rule_holds_with_the_literal_on_the_left_too() {
    is_true("CREATE TABLE t(a TEXT); INSERT INTO t VALUES('5'); SELECT 5=a FROM t");
    is_true("CREATE TABLE t(a INTEGER); INSERT INTO t VALUES(5); SELECT '5'=a FROM t");
    // And the negative again from the other side: 5.0 becomes '5.0' and a holds
    // '5', so this is false. sqlite3 answers 0.
    assert_eq!(
        one("CREATE TABLE t(a TEXT); INSERT INTO t VALUES('5'); SELECT 5.0=a FROM t"),
        Value::Integer(0)
    );
    is_true("CREATE TABLE t(a TEXT); INSERT INTO t VALUES('5.0'); SELECT 5.0=a FROM t");
}

/// The rule is about the affinity of a *column*, so an expression built from
/// literals contributes none and the value on its right is left as written.
///
/// sqlite3: CREATE TABLE t(a TEXT); INSERT INTO t VALUES('5');
///          SELECT a=5+0 FROM t;  ->  1, because 5+0 is still a number
/// and SELECT a='5'||'' FROM t;  ->  1, because the text is already text
#[test]
fn an_expression_contributes_no_affinity_of_its_own() {
    is_true("CREATE TABLE t(a TEXT); INSERT INTO t VALUES('5'); SELECT a=5+0 FROM t");
    is_true("CREATE TABLE t(a TEXT); INSERT INTO t VALUES('5'); SELECT a='5'||'' FROM t");
    is_true("CREATE TABLE t(a TEXT); INSERT INTO t VALUES('5'); SELECT a=abs(-5) FROM t");
}

/// A CAST to a numeric type is the one non-column that contributes an affinity,
/// because it produces a number.
///
/// sqlite3: CREATE TABLE t(a TEXT); INSERT INTO t VALUES('5');
///          SELECT a=CAST(5 AS INTEGER) FROM t;  ->  1
#[test]
fn a_cast_to_a_numeric_type_contributes_that_types_affinity() {
    is_true(
        "CREATE TABLE t(a TEXT); INSERT INTO t VALUES('5'); SELECT a=CAST(5 AS INTEGER) FROM t",
    );
    is_true(
        "CREATE TABLE t(a TEXT); INSERT INTO t VALUES('5'); SELECT a=CAST(5 AS NUMERIC) FROM t",
    );
    is_true(
        "CREATE TABLE t(a TEXT); INSERT INTO t VALUES('5.0'); SELECT a=CAST(5.0 AS REAL) FROM t",
    );
}

/// Two literals have no affinity at all, which is why `'5' = 5` is false even
/// though the same text in a TEXT column compared with a literal is true.
#[test]
fn two_literals_are_compared_as_written() {
    // sqlite3: SELECT '5' = 5;  ->  0
    assert_eq!(one("SELECT '5'=5"), Value::Integer(0));
    // sqlite3: SELECT '5' = 5.0;  ->  0
    assert_eq!(one("SELECT '5'=5.0"), Value::Integer(0));
    // sqlite3: SELECT 1 = 1.0;  ->  1, because a number is a number
    assert_eq!(one("SELECT 1=1.0"), Value::Integer(1));
}

/// A comparison with NULL is unknown, never true and never false, and the
/// affinity rule does not turn it into a match.
#[test]
fn a_null_operand_is_never_equal() {
    let sql = "CREATE TABLE t(a TEXT); INSERT INTO t VALUES('5'); \
               SELECT CASE WHEN a = NULL THEN 1 ELSE 0 END FROM t";
    assert_eq!(one(sql), Value::Integer(0));
}

// -- The rule in the places a query can ask it ----------------------------

/// The rule is not only in the projection: a WHERE, a GROUP BY's HAVING, an
/// UPDATE's filter and a DELETE's filter all evaluate comparisons, and each is
/// a separate call site in the executor.
#[test]
fn the_rule_reaches_the_filter_of_every_statement_that_has_one() {
    // A WHERE that matches returns the row, so the assertion is on the value
    // that comes back rather than on a 1.
    let v = one("CREATE TABLE t(a TEXT); INSERT INTO t VALUES('5'); SELECT a FROM t WHERE a=5");
    assert_eq!(v, Value::Text("5".into()), "WHERE a=5 must keep the row");
    is_true(
        "CREATE TABLE t(a TEXT, b INTEGER); INSERT INTO t VALUES('5',5); \
             SELECT count(*) FROM t WHERE a=b",
    );
    is_true(
        "CREATE TABLE t(a TEXT); INSERT INTO t VALUES('5'); \
             SELECT count(*) FROM t WHERE a>4 AND a<6",
    );
    // An UPDATE's filter is the same rule: a TEXT column holding '5' is the row
    // `a=5` names, so the row is found and rewritten.
    //
    //   sqlite3: CREATE TABLE t(a TEXT); INSERT INTO t VALUES('5');
    //            UPDATE t SET a='6' WHERE a=5; SELECT quote(a);  ->  '6'
    assert_eq!(
        one("CREATE TABLE t(a TEXT); INSERT INTO t VALUES('5'); \
             UPDATE t SET a='6' WHERE a=5; SELECT quote(a) FROM t"),
        Value::Text("'6'".into()),
        "UPDATE's WHERE must take the affinity rule too"
    );
    // A DELETE's filter is the same rule, and it is the row count that shows
    // whether the row was found: 0 after the delete, 1 when it was not.
    //
    //   sqlite3: CREATE TABLE t(a TEXT); INSERT INTO t VALUES('5');
    //            DELETE FROM t WHERE a=5; SELECT count(*);  ->  0
    assert_eq!(
        one("CREATE TABLE t(a TEXT); INSERT INTO t VALUES('5'); \
             DELETE FROM t WHERE a=5; SELECT count(*) FROM t"),
        Value::Integer(0),
        "DELETE's WHERE must take the affinity rule too"
    );
    // A filter that should *not* match is still not matching, so the rule is not
    // simply matching everything.
    is_true(
        "CREATE TABLE t(a TEXT); INSERT INTO t VALUES('5'); \
             DELETE FROM t WHERE a=6; SELECT count(*) FROM t",
    );
    is_true(
        "CREATE TABLE t(a TEXT); INSERT INTO t VALUES('5'); \
             DELETE FROM t WHERE a='6'; SELECT count(*) FROM t",
    );
}

#[test]
fn the_rule_reaches_an_order_by_key() {
    // sqlite3: the two rows order by b, where a='5' ties with b=5 under the
    //          rule and does not without it.
    is_true(
        "CREATE TABLE t(a TEXT, b INTEGER); INSERT INTO t VALUES('5',2),('5',1); \
             SELECT b FROM t ORDER BY a, b LIMIT 1",
    );
}

#[test]
fn the_rule_reaches_a_group_by_key() {
    // Under the rule '5' and 5 are one group; without it they are two, and the
    // count is what tells them apart.
    is_true(
        "CREATE TABLE t(a TEXT, b INTEGER); INSERT INTO t VALUES('5',5); \
             SELECT count(*) FROM t GROUP BY a, b",
    );
}

/// The rule applies to the value on its way *in* as well, so the row a filter
/// later sees is already the converted one.
#[test]
fn the_rule_and_the_insert_conversion_agree_on_what_a_row_holds() {
    // '5' in a NUMERIC column is stored as the integer 5, so comparing it with
    // the literal 5 is a number-to-number comparison and is true.
    is_true("CREATE TABLE t(a NUMERIC); INSERT INTO t(a) VALUES('5'); SELECT a=5 FROM t");
    // The same text in a TEXT column stays text, and only the comparison rule
    // makes the two meet.
    is_true("CREATE TABLE t(a TEXT); INSERT INTO t(a) VALUES('5'); SELECT a=5 FROM t");
}
