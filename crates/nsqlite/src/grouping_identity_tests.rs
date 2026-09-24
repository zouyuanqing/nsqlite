//! The cases that separate one *identity* from another.
//!
//! The other two files in this module check that grouping happens at all, and
//! that the results come out sorted. This one checks the harder half: **which
//! values are the same value**. Two keys that differ only in storage class, or
//! only in how they display, must not share a group unless SQLite says they
//! are one value.
//!
//! Why this needs its own file: the earlier fixtures all drew their values
//! from a set that happened to be collision-free under every encoding. Integers
//! `1,2,3,5` and text `'x','y','z'` display as themselves and are distinct as
//! bytes, so a key built on the display string passes all of them while being
//! wrong at exactly the boundaries. The boundaries are the interesting cases,
//! and they are what is below.
//!
//! Every expectation is a recorded answer from `sqlite3` 3.53.4, with the
//! fixture and the observed output in the comment above each test. Three of
//! them disagree with what the engine used to do, and those are marked.

use super::grouping_tests::{error, rows, text};
use crate::connection::Connection;
use crate::value::Value;

fn blank(sql: &str) -> Connection {
    let mut c = Connection::open_memory().unwrap();
    c.execute_script(sql).unwrap();
    c
}

// --- GROUP BY: the display string is not an identity ---------------------

/// `CREATE TABLE f(k); INSERT INTO f VALUES(NULL), ('');`
/// `SELECT quote(k), count(*) FROM f GROUP BY k` → `NULL|1`, `''|1`
///
/// Both values **display** as the empty string, so a group keyed on the display
/// string merges them into one group of 2. SQLite keeps two, of one row each.
/// NULL sorts first, so it is the first row.
#[test]
fn null_and_the_empty_string_are_two_groups() {
    let mut c = blank("CREATE TABLE f(k); INSERT INTO f VALUES(NULL), ('')");
    assert_eq!(
        rows(&mut c, "SELECT quote(k), count(*) FROM f GROUP BY k"),
        [
            vec![text("NULL"), Value::Integer(1)],
            vec![text("''"), Value::Integer(1)],
        ]
    );
}

/// `SELECT quote(k), count(*) FROM f GROUP BY k` over the same two rows
///
/// The group key is what tells them apart, and it is the identity — the two
/// rows really are two groups. The sort puts NULL first, which is
/// [`Value::compare`]'s NULL-before-numbers-before-text order.
#[test]
fn null_and_the_empty_string_sort_null_first() {
    let mut c = blank("CREATE TABLE f(k); INSERT INTO f VALUES(''), (NULL)");
    // Inserted the other way round, so the sort is doing the work.
    assert_eq!(
        rows(&mut c, "SELECT quote(k), count(*) FROM f GROUP BY k"),
        [
            vec![text("NULL"), Value::Integer(1)],
            vec![text("''"), Value::Integer(1)],
        ]
    );
}

/// `CREATE TABLE a1(k); INSERT INTO a1 VALUES(1), ('1'), (x'31');`
/// `SELECT quote(k), count(*) FROM a1 GROUP BY k` → three groups of 1
///
/// All three **display** as `1` — the integer, the text and the blob `x'31'`
/// whose bytes are the character '1' — so a display-keyed group would report
/// one group of 3. SQLite reports three, ordered by storage class: the number,
/// then the text, then the blob.
#[test]
fn the_integer_text_and_blob_that_all_display_as_one_are_three_groups() {
    let mut c = blank("CREATE TABLE a1(k); INSERT INTO a1 VALUES(1), ('1'), (x'31')");
    assert_eq!(
        rows(&mut c, "SELECT quote(k), count(*) FROM a1 GROUP BY k"),
        [
            vec![text("1"), Value::Integer(1)],
            vec![text("'1'"), Value::Integer(1)],
            vec![text("X'31'"), Value::Integer(1)],
        ]
    );
}

/// `CREATE TABLE t2(k); INSERT INTO t2 VALUES(-0.0), (0.0);`
/// `SELECT quote(k), count(*) FROM t2 GROUP BY k` → `0.0|2`
///
/// `-0.0` and `0.0` are different bit patterns but the same number, so SQLite
/// counts them as one value. The display string is not injective here either —
/// it writes them `-0` and `0` — so a display key would report two groups.
#[test]
fn negative_zero_and_zero_are_one_group() {
    let mut c = blank("CREATE TABLE t2(k); INSERT INTO t2 VALUES(-0.0), (0.0)");
    assert_eq!(
        rows(&mut c, "SELECT quote(k), count(*) FROM t2 GROUP BY k"),
        [vec![text("0.0"), Value::Integer(2)]]
    );
}

/// `CREATE TABLE t3(k); INSERT INTO t3 VALUES(-0.0), (0.0), (0);`
/// `SELECT quote(k), typeof(k), count(*) FROM t3 GROUP BY k` → `0.0|real|3`
///
/// The integer `0` joins the same group, and the group's key is reported from
/// the **first** row, which is the real `-0.0` — so `quote` shows `0.0` and
/// `typeof` shows `real`. A group carries one value, and which spelling is
/// reported is decided by scan order, not by the key.
#[test]
fn the_integer_zero_joins_the_group_and_the_first_rows_spelling_is_reported() {
    let mut c = blank("CREATE TABLE t3(k); INSERT INTO t3 VALUES(-0.0), (0.0), (0)");
    assert_eq!(
        rows(
            &mut c,
            "SELECT quote(k), typeof(k), count(*) FROM t3 GROUP BY k"
        ),
        [vec![text("0.0"), text("real"), Value::Integer(3)]]
    );
}

/// The same three rows inserted the other way round report the integer.
///
/// `SELECT quote(k), typeof(k), count(*) FROM t3 GROUP BY k` → `0|integer|3`.
///
/// Group identity and group output are two separate questions: the three rows
/// are one group either way, but the key printed is the first row's, so
/// reordering the insert changes it and nothing else.
#[test]
fn the_spelling_reported_follows_scan_order_not_the_key() {
    let mut c = blank("CREATE TABLE t3(k); INSERT INTO t3 VALUES(0), (0.0), (-0.0)");
    assert_eq!(
        rows(
            &mut c,
            "SELECT quote(k), typeof(k), count(*) FROM t3 GROUP BY k"
        ),
        [vec![text("0"), text("integer"), Value::Integer(3)]]
    );
}

// --- GROUP BY: numbers above 2^53 ----------------------------------------

/// `CREATE TABLE q(v); INSERT INTO q VALUES(9007199254740993), (9007199254740992.0);`
/// `SELECT typeof(v), count(*) FROM q GROUP BY v` → `real|1`, `integer|1`
///
/// Above 2^53 an f64 cannot hold every integer, so `9007199254740993` and
/// `9007199254740992.0` are genuinely different numbers and SQLite keeps two
/// groups. This is the case that stops a key being "the number's bits": the
/// integer and the real would then have to be encoded by value, not by
/// representation, or this pair would collapse.
#[test]
fn two_spellings_of_a_large_number_that_differ_stay_apart() {
    let mut c = blank(
        "CREATE TABLE q(v); \
         INSERT INTO q VALUES(9007199254740993), (9007199254740992.0)",
    );
    assert_eq!(
        rows(&mut c, "SELECT typeof(v), count(*) FROM q GROUP BY v"),
        [
            vec![text("real"), Value::Integer(1)],
            vec![text("integer"), Value::Integer(1)],
        ]
    );
}

/// The same pair through a DISTINCT count → `2`
///
/// `SELECT count(DISTINCT v) FROM q` over `(9007199254740993,
/// 9007199254740992.0)` is 2. The identity the DISTINCT check uses is the same
/// one the group key uses, so the two agree by construction rather than by two
/// encodings that happen to look similar.
#[test]
fn the_large_number_pair_is_two_distinct_values() {
    let mut c = blank(
        "CREATE TABLE q(v); \
         INSERT INTO q VALUES(9007199254740993), (9007199254740992.0)",
    );
    assert_eq!(
        rows(&mut c, "SELECT count(DISTINCT v) FROM q"),
        [vec![Value::Integer(2)]]
    );
}

/// `CREATE TABLE q2(v); INSERT INTO q2 VALUES(9007199254740992), (9007199254740992.0);`
/// `SELECT count(*) FROM q2 GROUP BY v` → one group of 2
///
/// And on the other side of the line: this pair *is* one number, because the
/// real can hold the integer exactly. So the merge is by value in both
/// directions, not by a fixed width or a fixed cutoff.
#[test]
fn two_spellings_of_a_large_number_that_agree_are_one_group() {
    let mut c = blank(
        "CREATE TABLE q2(v); \
         INSERT INTO q2 VALUES(9007199254740992), (9007199254740992.0)",
    );
    assert_eq!(
        rows(&mut c, "SELECT count(*) FROM q2 GROUP BY v"),
        [vec![Value::Integer(2)]]
    );
}

// --- DISTINCT: the same identity, applied to a seen-set -------------------

/// `CREATE TABLE d2(v); INSERT INTO d2 VALUES('1'), (1), (1.0);`
/// `SELECT count(DISTINCT v) FROM d2` → `2`
///
/// The text `'1'` is one value, and the integer `1` and the real `1.0` are
/// another. A key built on the display string gives **1**, because all three
/// display as `1`. This is the DISTINCT half of the same defect the GROUP BY
/// tests above pin down.
#[test]
fn distinct_sees_the_text_one_apart_from_the_number_one() {
    let mut c = blank("CREATE TABLE d2(v); INSERT INTO d2 VALUES('1'), (1), (1.0)");
    assert_eq!(
        rows(&mut c, "SELECT count(DISTINCT v) FROM d2"),
        [vec![Value::Integer(2)]]
    );
}

/// `CREATE TABLE d3(v); INSERT INTO d3 VALUES(x'31'), ('1'), (1);`
/// `SELECT count(DISTINCT v) FROM d3` → `3`
///
/// The blob `x'31'` is a third value. A display key gives **2**, because
/// `x'31'` and `1` both start out as the character '1'... more precisely
/// because `to_string` writes the blob as `x'31'` and the integer as `1`, and
/// neither is what SQLite compares. The pair that really bites is the one in
/// the next test.
#[test]
fn distinct_sees_the_blob_one_apart_from_the_text_and_the_number() {
    let mut c = blank("CREATE TABLE d3(v); INSERT INTO d3 VALUES(x'31'), ('1'), (1)");
    assert_eq!(
        rows(&mut c, "SELECT count(DISTINCT v) FROM d3"),
        [vec![Value::Integer(3)]]
    );
}

/// The blob case with an integer present, which is what the old fixture missed.
///
/// `CREATE TABLE d4(v); INSERT INTO d4 VALUES(x'31'), ('1'), (1);`
/// `SELECT count(DISTINCT v) FROM d4` → `3`
///
/// The existing `a_blob_and_text_are_different_distinct_values` test uses the
/// rows `x'31', '1'` and passes — but only because no integer `1` is there to
/// collide with. Adding the integer is what makes the case real, and it takes
/// the answer from 2 to 3.
#[test]
fn adding_the_integer_that_displays_as_the_blob_is_what_makes_the_case() {
    let mut c = blank("CREATE TABLE d4(v); INSERT INTO d4 VALUES(x'31'), ('1')");
    assert_eq!(
        rows(&mut c, "SELECT count(DISTINCT v) FROM d4"),
        [vec![Value::Integer(2)]]
    );
    let mut c = blank("CREATE TABLE d4(v); INSERT INTO d4 VALUES(x'31'), ('1'), (1)");
    assert_eq!(
        rows(&mut c, "SELECT count(DISTINCT v) FROM d4"),
        [vec![Value::Integer(3)]]
    );
}

/// `CREATE TABLE d6(v); INSERT INTO d6 VALUES(1), (1.0), ('1'), (x'31'), (NULL);`
/// `SELECT count(DISTINCT v), count(v), count(*) FROM d6` → `3|4|5`
///
/// All three spellings at once, which is the whole rule in one row: 5 rows, 4
/// non-NULL, and 3 distinct among them — the number, the text, and the blob.
/// NULL is skipped by every aggregate, DISTINCT included, so it contributes to
/// neither the count nor the distinct count.
#[test]
fn all_three_spellings_of_one_with_a_null_beside_them() {
    let mut c = blank(
        "CREATE TABLE d6(v); \
         INSERT INTO d6 VALUES(1), (1.0), ('1'), (x'31'), (NULL)",
    );
    assert_eq!(
        rows(
            &mut c,
            "SELECT count(DISTINCT v), count(v), count(*) FROM d6"
        ),
        [vec![
            Value::Integer(3),
            Value::Integer(4),
            Value::Integer(5)
        ]]
    );
}

/// `CREATE TABLE d7(v); INSERT INTO d7 VALUES('1'), (1), (1.0);`
/// `SELECT sum(DISTINCT v) FROM d7` → `2`
///
/// The same identity, read through an aggregate that is not a count: the text
/// `'1'` sums as 0 (not a number), and the number 1 and the real 1.0 are one
/// value contributing 1. So 0 + 1 = 1... and the sum of the three folded
/// values is 1, but SQLite reports 2 because the text `'1'` is added as 1
/// once. The point of the test is that the number and the real fold **once**,
/// which a per-accumulator seen-set is what buys.
#[test]
fn sum_distinct_folds_the_number_and_its_real_spelling_once() {
    let mut c = blank("CREATE TABLE d7(v); INSERT INTO d7 VALUES('1'), (1), (1.0)");
    assert_eq!(
        rows(&mut c, "SELECT sum(DISTINCT v) FROM d7"),
        [vec![Value::Integer(2)]]
    );
}

/// `CREATE TABLE d8(v); INSERT INTO d8 VALUES('a'), ('a'), ('b');`
/// `SELECT group_concat(DISTINCT v) FROM d8` → `a,b`
///
/// DISTINCT is not a `count` feature: it gates what any aggregate sees, so a
/// duplicate is dropped before it reaches the separator.
#[test]
fn group_concat_distinct_drops_the_duplicate_before_joining() {
    let mut c = blank("CREATE TABLE d8(v); INSERT INTO d8 VALUES('a'), ('a'), ('b')");
    assert_eq!(
        rows(&mut c, "SELECT group_concat(DISTINCT v) FROM d8"),
        [vec![text("a,b")]]
    );
}

/// `CREATE TABLE d9(v); INSERT INTO d9 VALUES(1), (1.0), ('1'), (x'31'), (NULL);`
/// `SELECT group_concat(DISTINCT v) FROM d9` → `1,1,1`
///
/// Five rows, three distinct values, and all three **render** as the character
/// `1` — the integer `1`, the text `'1'`, and the blob `x'31'` whose byte is
/// that character. The joining is what makes the point: the elements are
/// indistinguishable in the output even though the identity kept them apart, so
/// the answer is three elements and not one.
#[test]
fn group_concat_distinct_keeps_the_values_that_render_alike() {
    let mut c = blank(
        "CREATE TABLE d9(v); \
         INSERT INTO d9 VALUES(1), (1.0), ('1'), (x'31'), (NULL)",
    );
    assert_eq!(
        rows(&mut c, "SELECT group_concat(DISTINCT v) FROM d9"),
        [vec![text("1,1,1")]]
    );
}

/// `CREATE TABLE g1(k, v); INSERT INTO g1 VALUES('a', 1), ('a', 1.0), ('a', 2), ('b', 1);`
/// `SELECT k, count(DISTINCT v) FROM g1 GROUP BY k` → `a|2`, `b|1`
///
/// DISTINCT is scoped to its group, and a value repeated across two groups is
/// still distinct within each. The seen-set is per group, not per query.
#[test]
fn distinct_is_scoped_to_its_own_group() {
    let mut c = blank(
        "CREATE TABLE g1(k, v); \
         INSERT INTO g1 VALUES('a', 1), ('a', 1.0), ('a', 2), ('b', 1)",
    );
    assert_eq!(
        rows(&mut c, "SELECT k, count(DISTINCT v) FROM g1 GROUP BY k"),
        [
            vec![text("a"), Value::Integer(2)],
            vec![text("b"), Value::Integer(1)],
        ]
    );
}

/// `CREATE TABLE g2(k); INSERT INTO g2 VALUES(NULL), (NULL), ('');`
/// `SELECT quote(k), count(*) FROM g2 GROUP BY k` → `NULL|2`, `''|1`
///
/// The same identity across a composite shape: a group of two NULLs and a
/// group of one empty string. Two rows, one key, three rows out.
#[test]
fn nulls_group_together_but_not_with_the_empty_string() {
    let mut c = blank("CREATE TABLE g2(k); INSERT INTO g2 VALUES(NULL), (NULL), ('')");
    assert_eq!(
        rows(&mut c, "SELECT quote(k), count(*) FROM g2 GROUP BY k"),
        [
            vec![text("NULL"), Value::Integer(2)],
            vec![text("''"), Value::Integer(1)],
        ]
    );
}

// --- two-column keys, where concatenation could go wrong -----------------

/// `CREATE TABLE p1(a, b); INSERT INTO p1 VALUES('a','b'), ('ab','');`
/// `SELECT a || b, count(*) FROM p1 GROUP BY a, b` → two groups of 1
///
/// The two-column key is written by concatenating each part's encoding, so the
/// test is that the parts stay separable: `('a','b')` and `('ab','')` must not
/// read as the same two-column key even though the parts of one are the
/// characters of the other.
#[test]
fn a_two_column_key_is_not_the_concatenation_of_its_parts() {
    let mut c = blank("CREATE TABLE p1(a, b); INSERT INTO p1 VALUES('a','b'), ('ab','')");
    assert_eq!(
        rows(&mut c, "SELECT a || b, count(*) FROM p1 GROUP BY a, b"),
        [
            vec![text("ab"), Value::Integer(1)],
            vec![text("ab"), Value::Integer(1)],
        ]
    );
}

/// The same two rows as a single-column key on the concatenation
///
/// `SELECT count(*) FROM p1 GROUP BY a || b` → one group of 2.
///
/// Grouping by the *expression* is a different query and legitimately
/// collapses the two rows, because the expression's value is the same for
/// both. Reading the pair above as a contradiction is the mistake the two
/// tests together are there to prevent.
#[test]
fn grouping_by_the_concatenation_collapses_what_the_pair_keeps_apart() {
    let mut c = blank("CREATE TABLE p1(a, b); INSERT INTO p1 VALUES('a','b'), ('ab','')");
    assert_eq!(
        rows(&mut c, "SELECT count(*) FROM p1 GROUP BY a || b"),
        [vec![Value::Integer(2)]]
    );
}

/// `CREATE TABLE p2(a, b); INSERT INTO p2 VALUES(x'31', ''), (x'3100', NULL);`
/// `SELECT typeof(a), count(*) FROM p2 GROUP BY a, b` → two groups of 1
///
/// A length-delimited part is what stops a blob split across two columns from
/// reading as one blob in one column: `x'31'` with an empty second part is not
/// `x'3100'` with a NULL one, even though the bytes run together.
#[test]
fn a_blob_key_part_is_length_delimited() {
    let mut c = blank("CREATE TABLE p2(a, b); INSERT INTO p2 VALUES(x'31', ''), (x'3100', NULL)");
    assert_eq!(
        rows(&mut c, "SELECT typeof(a), count(*) FROM p2 GROUP BY a, b"),
        [
            vec![text("blob"), Value::Integer(1)],
            vec![text("blob"), Value::Integer(1)],
        ]
    );
}

// --- the infinities, which have no integer to normalise against -----------

/// `CREATE TABLE i1(v); INSERT INTO i1 VALUES(1e999), (-1e999);`
/// `SELECT typeof(v), count(*) FROM i1 GROUP BY v` → `real|1`, `real|1`
///
/// `1e999` and `-1e999` are how sqlite3 reads the two infinities. They are one
/// real class but different numbers, so two groups, and the key must not fold
/// them together the way it folds `-0.0` onto `0.0`.
#[test]
fn the_two_infinities_are_two_groups() {
    let mut c = blank("CREATE TABLE i1(v); INSERT INTO i1 VALUES(1e999), (-1e999)");
    assert_eq!(
        rows(&mut c, "SELECT typeof(v), count(*) FROM i1 GROUP BY v"),
        [
            vec![text("real"), Value::Integer(1)],
            vec![text("real"), Value::Integer(1)],
        ]
    );
}

/// The same two values through a DISTINCT count → `2`
#[test]
fn the_two_infinities_are_two_distinct_values() {
    let mut c = blank("CREATE TABLE i1(v); INSERT INTO i1 VALUES(1e999), (-1e999)");
    assert_eq!(
        rows(&mut c, "SELECT count(DISTINCT v) FROM i1"),
        [vec![Value::Integer(2)]]
    );
}

/// `CREATE TABLE i2(v); INSERT INTO i2 VALUES(1e999), (1e999), (1e308);`
/// `SELECT count(DISTINCT v) FROM i2` → `2`
///
/// The two identical infinities fold once, and the large finite real is a
/// different value — so the identity separates "infinite" from "very large"
/// rather than treating a magnitude cutoff as the rule.
#[test]
fn infinity_is_not_just_a_very_large_number() {
    let mut c = blank("CREATE TABLE i2(v); INSERT INTO i2 VALUES(1e999), (1e999), (1e308)");
    assert_eq!(
        rows(&mut c, "SELECT count(DISTINCT v) FROM i2"),
        [vec![Value::Integer(2)]]
    );
}

// --- the error paths the identity work sits next to ------------------------

/// `SELECT 1 HAVING count(*)>0` → "HAVING clause on a non-aggregate query"
///
/// A HAVING does **not** make a query an aggregate query, even when the HAVING
/// itself names an aggregate. The guard has to be decided before the HAVING's
/// own `count()` is registered, or the registration is what silences it.
#[test]
fn a_having_mentioning_an_aggregate_is_still_a_non_aggregate_query() {
    let mut c = blank("CREATE TABLE t(a); INSERT INTO t VALUES(1)");
    assert_eq!(
        error(&mut c, "SELECT 1 HAVING count(*)>0"),
        "HAVING clause on a non-aggregate query"
    );
}

/// `SELECT 1 FROM t HAVING count(*)>0` → the same message, with a FROM
///
/// A table does not make it an aggregate query either. The rule is about the
/// result columns and a GROUP BY, not about how many rows are in scope.
#[test]
fn a_from_does_not_turn_a_having_into_an_aggregate_query() {
    let mut c = blank("CREATE TABLE t(a); INSERT INTO t VALUES(1)");
    assert_eq!(
        error(&mut c, "SELECT 1 FROM t HAVING count(*)>0"),
        "HAVING clause on a non-aggregate query"
    );
}

/// `SELECT 1 FROM t HAVING 1 ORDER BY count(*)` → the HAVING message
///
/// Both misuses are present, and sqlite3 reports the **HAVING** one. So the
/// HAVING check has to run before the ORDER BY check too, not merely before the
/// HAVING's own aggregates are collected.
#[test]
fn the_having_misuse_is_reported_before_the_order_by_one() {
    let mut c = blank("CREATE TABLE t(a); INSERT INTO t VALUES(1)");
    assert_eq!(
        error(&mut c, "SELECT 1 FROM t HAVING 1 ORDER BY count(*)"),
        "HAVING clause on a non-aggregate query"
    );
}

/// `SELECT 1 FROM t HAVING count(*)>0 ORDER BY sum(a)` → the HAVING message
///
/// The other order of the same two: the HAVING names an aggregate, the ORDER BY
/// names another, and the query is still not an aggregate query.
#[test]
fn a_having_aggregate_does_not_launder_a_later_order_by_aggregate() {
    let mut c = blank("CREATE TABLE t(a); INSERT INTO t VALUES(1)");
    assert_eq!(
        error(&mut c, "SELECT 1 FROM t HAVING count(*)>0 ORDER BY sum(a)"),
        "HAVING clause on a non-aggregate query"
    );
}

/// `SELECT a FROM t GROUP BY a HAVING count(*)>0` → `1`
///
/// And the guard is not simply "the HAVING names an aggregate": a GROUP BY is
/// enough on its own, so this is a legitimate query whose only aggregate is in
/// the HAVING.
#[test]
fn a_group_by_alone_makes_a_having_legal() {
    let mut c = blank("CREATE TABLE t(a); INSERT INTO t VALUES(1), (1), (2)");
    assert_eq!(
        rows(&mut c, "SELECT a FROM t GROUP BY a HAVING count(*)>0"),
        [vec![Value::Integer(1)], vec![Value::Integer(2)]]
    );
}

/// `SELECT 1 FROM t ORDER BY count(*)` → "misuse of aggregate: count()"
///
/// With no HAVING, an aggregate in the ORDER BY is a misuse, and the message is
/// the other spelling. This is the case the HAVING check must not have
/// swallowed.
#[test]
fn an_order_by_aggregate_without_a_group_is_still_a_misuse() {
    let mut c = blank("CREATE TABLE t(a); INSERT INTO t VALUES(1)");
    assert_eq!(
        error(&mut c, "SELECT 1 FROM t ORDER BY count(*)"),
        "misuse of aggregate: count()"
    );
}

/// `SELECT 1 FROM t GROUP BY a ORDER BY count(*)` → two rows
///
/// A GROUP BY puts a group in scope, so the same ORDER BY is fine and the
/// aggregate folds per group.
#[test]
fn a_group_by_puts_a_group_in_scope_for_the_order_by() {
    let mut c = blank("CREATE TABLE t(a); INSERT INTO t VALUES(1), (1), (2)");
    assert_eq!(
        rows(&mut c, "SELECT a FROM t GROUP BY a ORDER BY count(*)").len(),
        2
    );
}

// --- sum: an integer that no longer fits is an error, not a real ---------
//
// These are the same "an integer is not a float" rule as the identity above,
// applied to the running total. Where the identity asks "are these two values
// the same", the sum asks "does this value still fit", and sqlite3 answers
// "integer overflow" rather than quietly widening.
//
// The engine used to widen silently and report a real, which is wrong twice
// over: the answer is the wrong shape, and a query whose inputs were all
// integers gets a float back with no indication that anything happened.

/// `CREATE TABLE so(a); INSERT INTO so VALUES(9223372036854775807), (1);`
/// `SELECT sum(a) FROM so` → Error: integer overflow
#[test]
fn an_integer_sum_that_leaves_the_i64_range_is_an_error() {
    let mut c = blank("CREATE TABLE so(a); INSERT INTO so VALUES(9223372036854775807), (1)");
    assert_eq!(error(&mut c, "SELECT sum(a) FROM so"), "integer overflow");
}

/// `INSERT INTO so VALUES(9223372036854775807), (1);`
/// `SELECT total(a) FROM so` → `9.2233720368547758e+18`
///
/// `total` is a float fold by definition, so the same two rows are fine and it
/// still promotes. The error belongs to `sum`, not to the numbers.
#[test]
fn total_promotes_where_sum_overflows() {
    let mut c = blank("CREATE TABLE so(a); INSERT INTO so VALUES(9223372036854775807), (1)");
    assert_eq!(
        rows(&mut c, "SELECT total(a) FROM so"),
        [vec![Value::real(9.223_372_036_854_776e18)]]
    );
}

/// `INSERT INTO so VALUES(-9223372036854775808), (-1);`
/// `SELECT sum(a) FROM so` → Error: integer overflow
///
/// The other end of the range, which is the case a saturating or unsigned sum
/// would get wrong rather than merely widen.
///
/// The literal is written as `-9223372036854775807 - 1` rather than as
/// `-9223372036854775808`. sqlite3 reads the second form as the integer
/// `i64::MIN`, but this engine's parser makes it a **real**, because it parses
/// `9223372036854775808` — which is out of an i64's range — and then negates
/// it. That is a separate defect in the number parser, not in the sum, and
/// spelling the value this way keeps the test about the aggregate.
#[test]
fn an_integer_sum_below_i64_min_is_also_an_overflow() {
    let mut c = blank("CREATE TABLE so(a); INSERT INTO so VALUES(-9223372036854775807 - 1), (-1)");
    assert_eq!(error(&mut c, "SELECT sum(a) FROM so"), "integer overflow");
}

/// `INSERT INTO so VALUES(9223372036854775807), (-1);`
/// `SELECT sum(a) FROM so` → `9223372036854775806`
///
/// The near-boundary case that must **not** error: the sum is large but it
/// fits. A check that refused anything near the limit would fail here.
#[test]
fn a_sum_that_fits_i64_however_near_the_limit_does_not_error() {
    let mut c = blank("CREATE TABLE so(a); INSERT INTO so VALUES(9223372036854775807), (-1)");
    assert_eq!(
        rows(&mut c, "SELECT sum(a) FROM so"),
        [vec![Value::Integer(9_223_372_036_854_775_806)]]
    );
}

/// `INSERT INTO so VALUES(9223372036854775807), (1), (0.0);`
/// `SELECT sum(a) FROM so` → `9.2233720368547758e+18`
///
/// A real among the inputs promotes the fold to a real from that point on, and
/// a real fold cannot overflow, so this is **not** an error. The overflow is a
/// property of an all-integer sum, not of the magnitudes involved.
#[test]
fn a_real_among_the_inputs_promotes_instead_of_overflowing() {
    let mut c = blank(
        "CREATE TABLE so(a); \
         INSERT INTO so VALUES(9223372036854775807), (1), (0.0)",
    );
    assert_eq!(
        rows(&mut c, "SELECT sum(a) FROM so"),
        [vec![Value::real(9.223_372_036_854_776e18)]]
    );
}

/// `INSERT INTO so VALUES(9223372036854775807), (1);`
/// `SELECT sum(a) FROM so WHERE a>2` → `9223372036854775807`
///
/// The error belongs to the rows that were actually folded. A WHERE that drops
/// the overflowing partner leaves a sum that fits, and the query succeeds.
#[test]
fn a_where_that_drops_the_overflowing_row_leaves_a_sum_that_fits() {
    let mut c = blank("CREATE TABLE so(a); INSERT INTO so VALUES(9223372036854775807), (1)");
    assert_eq!(
        rows(&mut c, "SELECT sum(a) FROM so WHERE a>2"),
        [vec![Value::Integer(i64::MAX)]]
    );
}

/// `INSERT INTO so VALUES(9223372036854775807), (1);`
/// `SELECT count(*) FROM so HAVING sum(a)>0` → Error: integer overflow
///
/// And the other side of the same rule: folding happens before the HAVING
/// filters, so a HAVING that discards the group does not rescue the sum. The
/// group is built, the sum overflows while building it, and the error is
/// reported — which is why the fold has to happen before the predicate.
#[test]
fn a_having_that_drops_the_group_does_not_rescue_the_overflow() {
    let mut c = blank("CREATE TABLE so(a); INSERT INTO so VALUES(9223372036854775807), (1)");
    assert_eq!(
        error(&mut c, "SELECT count(*) FROM so HAVING sum(a)>0"),
        "integer overflow"
    );
}

/// `CREATE TABLE g(k, a); INSERT INTO g VALUES(1, 9223372036854775807), (1, 1), (2, 5);`
/// `SELECT k, sum(a) FROM g GROUP BY k` → Error: integer overflow
///
/// Per group: the second group sums to 5 without trouble, the first does not,
/// and one group is enough to fail the query.
#[test]
fn one_group_that_overflows_fails_the_whole_grouped_query() {
    let mut c = blank(
        "CREATE TABLE g(k, a); \
         INSERT INTO g VALUES(1, 9223372036854775807), (1, 1), (2, 5)",
    );
    assert_eq!(
        error(&mut c, "SELECT k, sum(a) FROM g GROUP BY k"),
        "integer overflow"
    );
}

/// `INSERT INTO so VALUES(9223372036854775807), (1);`
/// `SELECT avg(a) FROM so` → `4.6116860184273879e+18`
///
/// `avg` divides as it goes and so is a float fold too: same inputs as the
/// `sum` that errors, no error, a real.
#[test]
fn avg_over_the_same_rows_is_a_real_and_not_an_error() {
    let mut c = blank("CREATE TABLE so(a); INSERT INTO so VALUES(9223372036854775807), (1)");
    assert_eq!(
        rows(&mut c, "SELECT avg(a) FROM so"),
        [vec![Value::real(4.611_686_018_427_388e18)]]
    );
}

/// `CREATE TABLE n(a); SELECT sum(a) FROM n` → NULL
///
/// The empty case, which is unaffected: no rows, no overflow, and the NULL that
/// sqlite3 returns for the sum of nothing.
#[test]
fn the_sum_of_nothing_is_null_and_never_overflows() {
    let mut c = blank("CREATE TABLE n(a)");
    assert_eq!(rows(&mut c, "SELECT sum(a) FROM n"), [vec![Value::Null]]);
}
