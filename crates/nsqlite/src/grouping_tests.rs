//! Aggregates and GROUP BY, as `Connection::select` executes them.
//!
//! Every expectation in this file was recorded from `sqlite3` 3.53.4, run by
//! hand against the same fixture. The queries and the observed output are in
//! the comment above each test, so a disagreement with SQLite is visible
//! without re-running the CLI.
//!
//! Three rules here are the opposite of what the API comments first suggested,
//! and all three are SQLite's, checked rather than assumed:
//!
//! * A bare column beside an aggregate is **not** an error. It takes its group
//!   's first row in scan order: `SELECT a, count(*)` over a table whose first
//!   row has `a = 1` reports 1.
//! * The output columns keep the order they were **written**. SQLite does not
//!   hoist the GROUP BY columns to the front, so `SELECT sum(a), b` has
//!   `sum(a)` first.
//! * Groups come out **sorted by key**, not in the order the rows arrived.

use crate::connection::{Connection, Outcome};
use crate::value::Value;

/// The fixture every test shares: three groups, a NULL, and five rows.
///
/// ```sql
/// CREATE TABLE t(a,b);
/// INSERT INTO t VALUES(1,'x'),(2,'y'),(3,'x'),(NULL,'z'),(5,'y');
/// ```
fn fixture() -> Connection {
    let mut c = Connection::open_memory().unwrap();
    run(&mut c, "CREATE TABLE t(a, b)");
    run(
        &mut c,
        "INSERT INTO t VALUES(1,'x'), (2,'y'), (3,'x'), (NULL,'z'), (5,'y')",
    );
    c
}

/// A table with the same shape but nothing in it.
fn empty() -> Connection {
    let mut c = Connection::open_memory().unwrap();
    run(&mut c, "CREATE TABLE t(a, b)");
    c
}

fn run(c: &mut Connection, sql: &str) -> Outcome {
    c.execute_script(sql)
        .unwrap_or_else(|e| panic!("{sql:?} failed: {e}"))
        .into_iter()
        .last()
        .unwrap()
}

fn query(c: &mut Connection, sql: &str) -> (Vec<String>, Vec<Vec<Value>>) {
    match run(c, sql) {
        Outcome::Query { columns, rows } => (
            columns,
            rows.into_iter().map(|r| r.values).collect(),
        ),
        other => panic!("expected a query, got {other:?}"),
    }
}

fn rows(c: &mut Connection, sql: &str) -> Vec<Vec<Value>> {
    query(c, sql).1
}

fn error(c: &mut Connection, sql: &str) -> String {
    c.execute_script(sql).unwrap_err().message
}

fn ints(v: &[i64]) -> Vec<Value> {
    v.iter().map(|i| Value::Integer(*i)).collect()
}

fn text(s: &str) -> Value {
    Value::Text(s.into())
}

fn real(r: f64) -> Value {
    Value::real(r)
}

// --- the queries the track names ----------------------------------------

/// `SELECT count(*) FROM t` → `5`
#[test]
fn count_star_counts_rows() {
    assert_eq!(rows(&mut fixture(), "SELECT count(*) FROM t"), [ints(&[5])]);
}

/// `SELECT sum(a), avg(a), min(a), max(a) FROM t` → `11|2.75|1|5`
///
/// The average is a real even though every value is an integer, which is what
/// `typeof(avg(a))` reports in SQLite.
#[test]
fn the_four_numeric_aggregates() {
    let mut c = fixture();
    let (_, got) = query(&mut c, "SELECT sum(a), avg(a), min(a), max(a) FROM t");
    assert_eq!(
        got,
        [vec![
            Value::Integer(11),
            real(2.75),
            Value::Integer(1),
            Value::Integer(5),
        ]]
    );
}

/// `SELECT b, count(*) FROM t GROUP BY b` → `x|2, y|2, z|1`
///
/// The groups come out sorted by key: the first row of each group in scan
/// order is x, y, z, which happens to also be sorted, so the separate test
/// `groups_are_sorted_by_key_not_by_arrival` is the one that pins the rule.
#[test]
fn group_by_splits_and_counts() {
    let mut c = fixture();
    let (names, got) = query(&mut c, "SELECT b, count(*) FROM t GROUP BY b");
    assert_eq!(names, vec!["b".to_string(), "count(*)".to_string()]);
    assert_eq!(
        got,
        [vec![text("x"), Value::Integer(2)],]
            .into_iter()
            .chain([vec![text("y"), Value::Integer(2)]])
            .chain([vec![text("z"), Value::Integer(1)]])
            .collect::<Vec<_>>()
    );
}

/// `SELECT b, count(*) FROM t GROUP BY b HAVING count(*) > 2` → no rows.
///
/// Every group has at most two rows, so the predicate drops all three. The
/// point is that HAVING runs *after* folding, so it sees the group's count.
#[test]
fn having_filters_on_an_aggregate() {
    let mut c = fixture();
    assert!(rows(
        &mut c,
        "SELECT b, count(*) FROM t GROUP BY b HAVING count(*) > 2"
    )
    .is_empty());
}

/// `SELECT b, count(*) FROM t GROUP BY b HAVING count(*) >= 2` → `x|2, y|2`
#[test]
fn having_keeps_the_groups_that_pass() {
    let mut c = fixture();
    assert_eq!(
        rows(
            &mut c,
            "SELECT b, count(*) FROM t GROUP BY b HAVING count(*) >= 2"
        ),
        [vec![text("x"), Value::Integer(2)], vec![text("y"), Value::Integer(2)]]
    );
}

/// `SELECT b, sum(a) FROM t GROUP BY b ORDER BY b` → `x|4, y|7, z|NULL`
#[test]
fn group_by_with_order_by() {
    let mut c = fixture();
    assert_eq!(
        rows(&mut c, "SELECT b, sum(a) FROM t GROUP BY b ORDER BY b"),
        [
            vec![text("x"), Value::Integer(4)],
            vec![text("y"), Value::Integer(7)],
            vec![text("z"), Value::Null],
        ]
    );
}

/// `SELECT count(DISTINCT b) FROM t` → `3`
#[test]
fn count_distinct_sees_each_value_once() {
    assert_eq!(
        rows(&mut fixture(), "SELECT count(DISTINCT b) FROM t"),
        [ints(&[3])]
    );
}

/// `SELECT b, group_concat(a) FROM t GROUP BY b` → `x|1,3, y|2,5, z|NULL`
///
/// The elements are in arrival order within the group, and a group whose only
/// member is NULL concatenates to NULL rather than to the empty string.
#[test]
fn group_concat_joins_within_each_group() {
    let mut c = fixture();
    assert_eq!(
        rows(&mut c, "SELECT b, group_concat(a) FROM t GROUP BY b"),
        [
            vec![text("x"), text("1,3")],
            vec![text("y"), text("2,5")],
            vec![text("z"), Value::Null],
        ]
    );
}

// --- the rules the API comments had backwards ---------------------------

/// `SELECT sum(a), b FROM t GROUP BY b` → `4|x, 7|y, NULL|z`
///
/// SQLite does **not** put the GROUP BY columns first. The output is the result
/// columns in the order written, so `sum(a)` leads here even though `b` is the
/// grouping key.
#[test]
fn output_columns_keep_the_order_written() {
    let mut c = fixture();
    let (names, got) = query(&mut c, "SELECT sum(a), b FROM t GROUP BY b");
    assert_eq!(names, vec!["sum(a)".to_string(), "b".to_string()]);
    assert_eq!(
        got,
        [
            vec![Value::Integer(4), text("x")],
            vec![Value::Integer(7), text("y")],
            vec![Value::Null, text("z")],
        ]
    );
}

/// `SELECT count(*), b FROM t GROUP BY b` → `2|x, 2|y, 1|z`
///
/// The aggregate can lead and the key follow, which is the mirror of the test
/// above and the same rule.
#[test]
fn an_aggregate_can_come_before_the_key() {
    let mut c = fixture();
    assert_eq!(
        rows(&mut c, "SELECT count(*), b FROM t GROUP BY b"),
        [
            vec![Value::Integer(2), text("x")],
            vec![Value::Integer(2), text("y")],
            vec![Value::Integer(1), text("z")],
        ]
    );
}

/// `CREATE TABLE f(k,v); INSERT INTO f VALUES('a',5),('a',1),('a',9);`
/// `SELECT v, count(*) FROM f GROUP BY k` → `5|3`
///
/// A bare column beside an aggregate is not an error. It reads the group's
/// **first row in scan order**, which is 5 — not the minimum 1, and not the
/// maximum 9.
#[test]
fn a_bare_column_takes_the_first_row_of_its_group() {
    let mut c = Connection::open_memory().unwrap();
    run(&mut c, "CREATE TABLE f(k, v)");
    run(&mut c, "INSERT INTO f VALUES('a',5), ('a',1), ('a',9)");
    assert_eq!(
        rows(&mut c, "SELECT v, count(*) FROM f GROUP BY k"),
        [vec![Value::Integer(5), Value::Integer(3)]]
    );
}

/// The same query with the rows inserted in the other order → `9|3`
///
/// This is what proves the rule is "first row in scan order" and not, say, the
/// minimum: the value follows the insertion order.
#[test]
fn the_first_row_is_the_first_one_inserted() {
    let mut c = Connection::open_memory().unwrap();
    run(&mut c, "CREATE TABLE f(k, v)");
    run(&mut c, "INSERT INTO f VALUES('a',9), ('a',1), ('a',5)");
    assert_eq!(
        rows(&mut c, "SELECT v, count(*) FROM f GROUP BY k"),
        [vec![Value::Integer(9), Value::Integer(3)]]
    );
}

/// `SELECT a, count(*) FROM t` → `1|5`
///
/// A bare column with no GROUP BY reads the first row of the whole table,
/// because with no key there is one group and it starts at the first row.
#[test]
fn a_bare_column_with_no_group_by_reads_the_first_row() {
    let mut c = fixture();
    assert_eq!(
        rows(&mut c, "SELECT a, count(*) FROM t"),
        [vec![Value::Integer(1), Value::Integer(5)]]
    );
}

/// `SELECT b, a, count(*) FROM t GROUP BY b` → `x|1|2, y|2|2, z|NULL|1`
#[test]
fn a_bare_column_beside_a_key_reads_the_first_row() {
    let mut c = fixture();
    assert_eq!(
        rows(&mut c, "SELECT b, a, count(*) FROM t GROUP BY b"),
        [
            vec![text("x"), Value::Integer(1), Value::Integer(2)],
            vec![text("y"), Value::Integer(2), Value::Integer(2)],
            vec![text("z"), Value::Null, Value::Integer(1)],
        ]
    );
}

/// `CREATE TABLE g(k); INSERT INTO g VALUES('m'),('z'),('a'),('m');`
/// `SELECT k, count(*) FROM g GROUP BY k` → `a|1, m|2, z|1`
///
/// The first row of each group arrives as m, z, a, m. The output is a, m, z,
/// so groups are **sorted by key** rather than kept in arrival order.
#[test]
fn groups_are_sorted_by_key_not_by_arrival() {
    let mut c = Connection::open_memory().unwrap();
    run(&mut c, "CREATE TABLE g(k)");
    run(&mut c, "INSERT INTO g VALUES('m'), ('z'), ('a'), ('m')");
    assert_eq!(
        rows(&mut c, "SELECT k, count(*) FROM g GROUP BY k"),
        [
            vec![text("a"), Value::Integer(1)],
            vec![text("m"), Value::Integer(2)],
            vec![text("z"), Value::Integer(1)],
        ]
    );
}

/// `CREATE TABLE n(k); INSERT INTO n VALUES(10),(9),(2),(100);`
/// `SELECT k FROM n GROUP BY k` → `2, 9, 10, 100`
///
/// Numbers sort by value, not by text, so 9 comes before 10.
#[test]
fn numeric_keys_sort_by_value() {
    let mut c = Connection::open_memory().unwrap();
    run(&mut c, "CREATE TABLE n(k)");
    run(&mut c, "INSERT INTO n VALUES(10), (9), (2), (100)");
    assert_eq!(
        rows(&mut c, "SELECT k, count(*) FROM n GROUP BY k"),
        [
            vec![Value::Integer(2), Value::Integer(1)],
            vec![Value::Integer(9), Value::Integer(1)],
            vec![Value::Integer(10), Value::Integer(1)],
            vec![Value::Integer(100), Value::Integer(1)],
        ]
    );
}

/// `CREATE TABLE z(k); INSERT INTO z VALUES(5),(NULL),('t');`
/// `SELECT k FROM z GROUP BY k` → `NULL, 5, t`
///
/// A NULL key is a group of its own and sorts before every value, which is
/// [`Value::compare`]'s rule rather than a special case here.
#[test]
fn a_null_key_is_its_own_group_and_sorts_first() {
    let mut c = Connection::open_memory().unwrap();
    run(&mut c, "CREATE TABLE z(k)");
    run(&mut c, "INSERT INTO z VALUES(5), (NULL), ('t')");
    assert_eq!(
        rows(&mut c, "SELECT k, count(*) FROM z GROUP BY k"),
        [
            vec![Value::Null, Value::Integer(1)],
            vec![Value::Integer(5), Value::Integer(1)],
            vec![text("t"), Value::Integer(1)],
        ]
    );
}

// --- empty sets ---------------------------------------------------------

/// `SELECT count(*) FROM t` on an empty table → `0`
#[test]
fn a_bare_aggregate_over_no_rows_still_produces_a_row() {
    let mut c = empty();
    let (_, got) = query(&mut c, "SELECT count(*) FROM t");
    assert_eq!(got, [ints(&[0])], "count over nothing is one row of 0");
}

/// `SELECT sum(a), avg(a), min(a), max(a), count(*) FROM t` on an empty
/// table → `NULL|NULL|NULL|NULL|0`
///
/// Four aggregates that saw nothing are NULL and one that counts rows is 0.
/// That split is SQLite's and it is not uniform across the set.
#[test]
fn the_empty_set_results_are_null_except_count() {
    let mut c = empty();
    assert_eq!(
        rows(&mut c, "SELECT sum(a), avg(a), min(a), max(a), count(*) FROM t"),
        [vec![
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Integer(0),
        ]]
    );
}

/// `SELECT b, count(*) FROM t GROUP BY b` on an empty table → no rows.
///
/// The asymmetry with the test above is the rule: a bare aggregate has a
/// defined value over no rows, but a grouped query has no groups at all.
#[test]
fn a_group_by_over_no_rows_produces_no_rows() {
    let mut c = empty();
    assert!(rows(&mut c, "SELECT b, count(*) FROM t GROUP BY b").is_empty());
}

/// `SELECT count(*)` with no FROM at all → `1`
///
/// A SELECT with no FROM evaluates against one empty row, so the count is 1
/// rather than an error.
#[test]
fn a_bare_aggregate_with_no_from_is_one_row() {
    let mut c = Connection::open_memory().unwrap();
    assert_eq!(rows(&mut c, "SELECT count(*)"), [ints(&[1])]);
}

/// `SELECT sum(1), avg(1), min(1), max(1), count(*)` with no FROM →
/// `1|1.0|1|1|1`
#[test]
fn a_bare_aggregate_with_no_from_folds_the_empty_row() {
    let mut c = Connection::open_memory().unwrap();
    assert_eq!(
        rows(&mut c, "SELECT sum(1), avg(1), min(1), max(1), count(*)"),
        [vec![
            Value::Integer(1),
            real(1.0),
            Value::Integer(1),
            Value::Integer(1),
            Value::Integer(1),
        ]]
    );
}

/// `SELECT a, count(*) FROM t` on an empty table → `NULL|0`
///
/// There is no row for the bare column to read, so it is NULL — and the query
/// still produces its one row, because the count is there.
#[test]
fn a_bare_column_with_no_rows_is_null() {
    let mut c = empty();
    assert_eq!(
        rows(&mut c, "SELECT a, count(*) FROM t"),
        [vec![Value::Null, Value::Integer(0)]]
    );
}

/// `SELECT count(DISTINCT b), sum(a), group_concat(a) FROM t` on an empty
/// table → `0|NULL|NULL`
#[test]
fn the_other_aggregates_over_no_rows() {
    let mut c = empty();
    assert_eq!(
        rows(
            &mut c,
            "SELECT count(DISTINCT b), sum(a), group_concat(a) FROM t"
        ),
        [vec![Value::Integer(0), Value::Null, Value::Null]]
    );
}

// --- count, and what it counts -----------------------------------------

/// `SELECT count(*), count(a), count(b) FROM t` → `5|4|5`
///
/// count(*) counts rows and count(x) counts non-NULL x. The fixture's `a` has
/// one NULL and its `b` has none, so the middle column is the smaller.
#[test]
fn count_star_and_count_column_differ_on_nulls() {
    let mut c = fixture();
    assert_eq!(
        rows(&mut c, "SELECT count(*), count(a), count(b) FROM t"),
        [vec![Value::Integer(5), Value::Integer(4), Value::Integer(5)]]
    );
}

/// `SELECT count() FROM t` → `5`
///
/// With no argument, count is count(*), which is why SQLite accepts it.
#[test]
fn count_with_no_argument_counts_rows() {
    assert_eq!(rows(&mut fixture(), "SELECT count() FROM t"), [ints(&[5])]);
}

/// `SELECT b, count(a) FROM t GROUP BY b` → `x|2, y|2, z|0`
///
/// The z group has one row, and its `a` is NULL, so the group counts zero.
#[test]
fn count_of_a_column_skips_nulls_inside_a_group() {
    let mut c = fixture();
    assert_eq!(
        rows(&mut c, "SELECT b, count(a) FROM t GROUP BY b"),
        [
            vec![text("x"), Value::Integer(2)],
            vec![text("y"), Value::Integer(2)],
            vec![text("z"), Value::Integer(0)],
        ]
    );
}

/// `SELECT count(DISTINCT a) FROM t` → `4`
///
/// The distinct values of `a` are 1, 2, 3, 5 — the NULL is not a value.
#[test]
fn count_distinct_ignores_null() {
    assert_eq!(
        rows(&mut fixture(), "SELECT count(DISTINCT a) FROM t"),
        [ints(&[4])]
    );
}

/// `SELECT sum(DISTINCT a) FROM t` → `11`
///
/// The same four values, summed, which is 1+2+3+5.
#[test]
fn sum_distinct_folds_each_value_once() {
    assert_eq!(
        rows(&mut fixture(), "SELECT sum(DISTINCT a) FROM t"),
        [ints(&[11])]
    );
}

/// `SELECT group_concat(DISTINCT b) FROM t` → `x,y,z`
#[test]
fn group_concat_distinct_folds_each_value_once() {
    assert_eq!(
        rows(&mut fixture(), "SELECT group_concat(DISTINCT b) FROM t"),
        [[text("x,y,z")]]
    );
}

/// `SELECT min(DISTINCT a), max(DISTINCT a) FROM t` → `1|5`
///
/// DISTINCT does not change min or max, which is what makes the rule "each
/// value once" rather than "some other sum-like rule".
#[test]
fn min_and_max_distinct_agree_with_the_plain_form() {
    let mut c = fixture();
    assert_eq!(
        rows(&mut c, "SELECT min(DISTINCT a), max(DISTINCT a) FROM t"),
        [vec![Value::Integer(1), Value::Integer(5)]]
    );
}

// --- sum, avg, total ----------------------------------------------------

/// `SELECT sum(a), avg(a), total(a) FROM t` → `11|2.75|11.0`
///
/// sum stays an integer because every input is one; total is a real because it
/// always is; avg is a real for the same reason.
#[test]
fn sum_stays_an_integer_and_avg_and_total_are_reals() {
    let mut c = fixture();
    assert_eq!(
        rows(&mut c, "SELECT sum(a), avg(a), total(a) FROM t"),
        [vec![Value::Integer(11), real(2.75), real(11.0)]]
    );
}

/// `SELECT typeof(avg(1))` → `real`
#[test]
fn avg_is_a_real_even_over_integers() {
    let mut c = Connection::open_memory().unwrap();
    assert_eq!(rows(&mut c, "SELECT typeof(avg(1))"), [[text("real")]]);
}

/// `SELECT total(x) FROM t WHERE 0` → `0.0`
///
/// total is the one aggregate that is not NULL over an empty set: it reports
/// zero, because a total of nothing is zero rather than unknown.
#[test]
fn total_of_nothing_is_zero_not_null() {
    let mut c = fixture();
    assert_eq!(
        rows(&mut c, "SELECT total(a) FROM t WHERE a > 100"),
        [[real(0.0)]]
    );
}

/// `SELECT b, sum(a) FROM t GROUP BY b` → `x|4, y|7, z|NULL`
///
/// The z group's only value is NULL, so the sum saw nothing and is NULL — the
/// same rule as the empty table.
#[test]
fn a_group_of_only_nulls_sums_to_null() {
    let mut c = fixture();
    assert_eq!(
        rows(&mut c, "SELECT b, sum(a) FROM t GROUP BY b"),
        [
            vec![text("x"), Value::Integer(4)],
            vec![text("y"), Value::Integer(7)],
            vec![text("z"), Value::Null],
        ]
    );
}

/// `SELECT b, avg(a) FROM t GROUP BY b` → `x|2.0, y|3.5, z|NULL`
#[test]
fn avg_per_group_divides_by_the_rows_that_counted() {
    let mut c = fixture();
    assert_eq!(
        rows(&mut c, "SELECT b, avg(a) FROM t GROUP BY b"),
        [
            vec![text("x"), real(2.0)],
            vec![text("y"), real(3.5)],
            vec![text("z"), Value::Null],
        ]
    );
}

/// `SELECT b, min(a), max(a) FROM t GROUP BY b` → `x|1|3, y|2|5, z|NULL|NULL`
#[test]
fn min_and_max_per_group() {
    let mut c = fixture();
    assert_eq!(
        rows(&mut c, "SELECT b, min(a), max(a) FROM t GROUP BY b"),
        [
            vec![text("x"), Value::Integer(1), Value::Integer(3)],
            vec![text("y"), Value::Integer(2), Value::Integer(5)],
            vec![text("z"), Value::Null, Value::Null],
        ]
    );
}

// --- group_concat -------------------------------------------------------

/// `SELECT b, group_concat(a, '-') FROM t GROUP BY b` → `x|1-3, y|2-5, z|NULL`
#[test]
fn group_concat_takes_a_separator() {
    let mut c = fixture();
    assert_eq!(
        rows(&mut c, "SELECT b, group_concat(a, '-') FROM t GROUP BY b"),
        [
            vec![text("x"), text("1-3")],
            vec![text("y"), text("2-5")],
            vec![text("z"), Value::Null],
        ]
    );
}

/// `SELECT group_concat(a, NULL) FROM t` → `1235`
///
/// A NULL separator is not an error: it means no separator at all, so the
/// elements run together.
#[test]
fn a_null_separator_joins_with_nothing() {
    assert_eq!(
        rows(&mut fixture(), "SELECT group_concat(a, NULL) FROM t"),
        [[text("1235")]]
    );
}

/// `SELECT string_agg(b, '-') FROM t` → `x-y-x-z-y`
///
/// string_agg is group_concat under a different name, and unlike group_concat
/// it requires its separator.
#[test]
fn string_agg_is_group_concat_with_a_required_separator() {
    assert_eq!(
        rows(&mut fixture(), "SELECT string_agg(b, '-') FROM t"),
        [[text("x-y-x-z-y")]]
    );
}

/// `SELECT group_concat(a*2) FROM t GROUP BY b` → `x|2,6, y|4,10`
///
/// The argument is an expression, and it is evaluated per row before the fold.
#[test]
fn group_concat_takes_an_expression_argument() {
    let mut c = fixture();
    assert_eq!(
        rows(&mut c, "SELECT b, group_concat(a*2) FROM t GROUP BY b"),
        [
            vec![text("x"), text("2,6")],
            vec![text("y"), text("4,10")],
        ]
    );
}

/// `SELECT sum(a*2) FROM t` → `22`
#[test]
fn an_aggregate_argument_is_an_expression() {
    assert_eq!(rows(&mut fixture(), "SELECT sum(a*2) FROM t"), [ints(&[22])]);
}

// --- WHERE, ordering, and the misuse errors -----------------------------

/// `SELECT b, count(*) FROM t WHERE a > 1 GROUP BY b` → `y|2`
///
/// The WHERE runs before the fold, so it drops rows and the groups are
/// whatever survives.
#[test]
fn where_runs_before_grouping() {
    let mut c = fixture();
    assert_eq!(
        rows(
            &mut c,
            "SELECT b, count(*) FROM t WHERE a > 1 GROUP BY b"
        ),
        [vec![text("y"), Value::Integer(2)]]
    );
}

/// `SELECT b FROM t WHERE count(*) > 1` → `misuse of aggregate function count()`
///
/// An aggregate in the WHERE is refused before any row is read. With no
/// aggregate in scope anywhere, SQLite uses the "function" wording.
#[test]
fn an_aggregate_in_where_is_misuse() {
    let mut c = fixture();
    assert_eq!(
        error(&mut c, "SELECT b FROM t WHERE count(*) > 1"),
        "misuse of aggregate function count()"
    );
}

/// `SELECT b, count(*) FROM t WHERE count(*) > 1` → `misuse of aggregate: count()`
///
/// The same query with an aggregate in the result gets the shorter wording.
/// The two messages are a flag in SQLite's name resolver rather than a
/// difference in the query, and the suite compares the text exactly.
#[test]
fn an_aggregate_in_where_with_one_in_scope_says_misuse_of_aggregate() {
    let mut c = fixture();
    assert_eq!(
        error(&mut c, "SELECT b, count(*) FROM t WHERE count(*) > 1"),
        "misuse of aggregate: count()"
    );
}

/// `SELECT b FROM t ORDER BY count(*)` → `misuse of aggregate: count()`
///
/// An ORDER BY on a query with no aggregate has no group to fold over, so it
/// is refused. On a *grouped* query the same ORDER BY is legal, which the next
/// test shows.
#[test]
fn an_aggregate_in_order_by_needs_a_group() {
    let mut c = fixture();
    assert_eq!(
        error(&mut c, "SELECT b FROM t ORDER BY count(*)"),
        "misuse of aggregate: count()"
    );
}

/// `SELECT b, count(*) FROM t GROUP BY b ORDER BY count(*)` → `z, x, y`
///
/// With a GROUP BY the ORDER BY does have a group, so the aggregate folds per
/// group and orders by it. Ascending, the two one-row groups come first, and
/// between them the group key breaks the tie.
#[test]
fn an_aggregate_in_order_by_of_a_grouped_query_folds() {
    let mut c = fixture();
    assert_eq!(
        rows(&mut c, "SELECT b, count(*) FROM t GROUP BY b ORDER BY count(*)"),
        [
            vec![text("z"), Value::Integer(1)],
            vec![text("x"), Value::Integer(2)],
            vec![text("y"), Value::Integer(2)],
        ]
    );
}

/// `SELECT b, count(*) FROM t GROUP BY b ORDER BY 2 DESC` → `x, y, z`
///
/// An ORDER BY ordinal reads the second output column, which is the count.
#[test]
fn an_ordinal_in_order_by_names_the_output_column() {
    let mut c = fixture();
    assert_eq!(
        rows(
            &mut c,
            "SELECT b, count(*) FROM t GROUP BY b ORDER BY 2 DESC"
        ),
        [
            vec![text("x"), Value::Integer(2)],
            vec![text("y"), Value::Integer(2)],
            vec![text("z"), Value::Integer(1)],
        ]
    );
}

/// `SELECT b, count(*) AS n FROM t GROUP BY b ORDER BY n DESC` → `x, y, z`
#[test]
fn an_alias_in_order_by_names_the_output_column() {
    let mut c = fixture();
    assert_eq!(
        rows(
            &mut c,
            "SELECT b, count(*) AS n FROM t GROUP BY b ORDER BY n DESC"
        ),
        [
            vec![text("x"), Value::Integer(2)],
            vec![text("y"), Value::Integer(2)],
            vec![text("z"), Value::Integer(1)],
        ]
    );
}

/// `SELECT b, count(*) FROM t GROUP BY b ORDER BY count(*) DESC LIMIT 2`
/// → `x|2, y|2`
///
/// LIMIT runs after the sort, so it takes the two largest groups.
#[test]
fn limit_applies_after_grouping() {
    let mut c = fixture();
    assert_eq!(
        rows(
            &mut c,
            "SELECT b, count(*) FROM t GROUP BY b ORDER BY b DESC LIMIT 1"
        ),
        [vec![text("z"), Value::Integer(1)]]
    );
}

/// `SELECT b, count(*) FROM t GROUP BY b HAVING count(*) > 0 ORDER BY sum(a)`
/// → `x|2, y|2`
///
/// ORDER BY may name an aggregate the projection did not include; it folds the
/// group like any other.
#[test]
fn order_by_folds_an_aggregate_the_projection_omits() {
    let mut c = fixture();
    assert_eq!(
        rows(
            &mut c,
            "SELECT b, count(*) FROM t GROUP BY b HAVING count(*) > 0 ORDER BY sum(a)"
        ),
        [
            vec![text("x"), Value::Integer(2)],
            vec![text("y"), Value::Integer(2)],
        ]
    );
}

/// `SELECT count(*) FROM t GROUP BY count(*)` →
/// `aggregate functions are not allowed in the GROUP BY clause`
#[test]
fn an_aggregate_in_group_by_is_refused() {
    let mut c = fixture();
    assert_eq!(
        error(&mut c, "SELECT count(*) FROM t GROUP BY count(*)"),
        "aggregate functions are not allowed in the GROUP BY clause"
    );
}

/// `SELECT b FROM t HAVING b = 'x'` → `HAVING clause on a non-aggregate query`
///
/// A HAVING filters groups, so a query with neither an aggregate nor a GROUP BY
/// has nothing for it to filter.
#[test]
fn a_having_on_a_non_aggregate_query_is_refused() {
    let mut c = fixture();
    assert_eq!(
        error(&mut c, "SELECT b FROM t HAVING b = 'x'"),
        "HAVING clause on a non-aggregate query"
    );
}

/// `SELECT b, count(*) FROM t GROUP BY b HAVING b = 'x'` → `x|2`
///
/// A HAVING may name a grouping column as well as an aggregate.
#[test]
fn a_having_may_name_a_grouping_column() {
    let mut c = fixture();
    assert_eq!(
        rows(&mut c, "SELECT b, count(*) FROM t GROUP BY b HAVING b = 'x'"),
        [vec![text("x"), Value::Integer(2)]]
    );
}

/// `SELECT count(*)+1, count(*)*2 FROM t` → `6|10`
///
/// An aggregate nested in an expression still folds once per group; the
/// repeated `count(*)` is the same accumulator read twice.
#[test]
fn an_aggregate_inside_an_expression_folds_once() {
    let mut c = fixture();
    assert_eq!(
        rows(&mut c, "SELECT count(*)+1, count(*)*2 FROM t"),
        [vec![Value::Integer(6), Value::Integer(10)]]
    );
}

/// `SELECT sum(count(a)) FROM t` → `misuse of aggregate function count()`
#[test]
fn an_aggregate_inside_another_aggregate_is_refused() {
    let mut c = fixture();
    assert_eq!(
        error(&mut c, "SELECT sum(count(a)) FROM t"),
        "misuse of aggregate function count()"
    );
}

/// `SELECT count(a,b) FROM t` → `wrong number of arguments to function count()`
#[test]
fn the_wrong_arity_is_reported() {
    let mut c = fixture();
    assert_eq!(
        error(&mut c, "SELECT count(a,b) FROM t"),
        "wrong number of arguments to function count()"
    );
}

/// `SELECT min(a,b) FROM t` → one row per input row: `1, 2, 3, NULL, 5`
///
/// min and max are both a scalar and an aggregate in SQLite, and the arity
/// picks: one argument folds a group, two are a scalar call on one row. Four
/// rows come out for four non-NULL pairs. A number sorts below text, so each
/// `a` is its own minimum, and the row whose `a` is NULL has no minimum at
/// all.
#[test]
fn min_with_two_arguments_is_a_scalar_not_a_fold() {
    let mut c = fixture();
    let (_, got) = query(&mut c, "SELECT min(a,b) FROM t");
    assert_eq!(
        got,
        [
            vec![Value::Integer(1)],
            vec![Value::Integer(2)],
            vec![Value::Integer(3)],
            vec![Value::Null],
            vec![Value::Integer(5)],
        ]
    );
}

/// `SELECT min(a) FROM t` → `1`, against the `1` that `min(a,b)` gives for the
/// same column.
///
/// The pair of tests is what shows the arity is what chooses, rather than the
/// name.
#[test]
fn min_with_one_argument_folds() {
    assert_eq!(rows(&mut fixture(), "SELECT min(a) FROM t"), [ints(&[1])]);
}

/// `SELECT string_agg(b) FROM t` → `wrong number of arguments to function
/// string_agg()`
///
/// group_concat defaults its separator to a comma; string_agg has no default
/// and needs one.
#[test]
fn string_agg_needs_its_separator() {
    let mut c = fixture();
    assert_eq!(
        error(&mut c, "SELECT string_agg(b) FROM t"),
        "wrong number of arguments to function string_agg()"
    );
}

/// `SELECT product(a) FROM t` → `no such function: product`
///
/// `crate::aggregate::Acc` has a Product accumulator, but SQLite 3.53.4 has no
/// `product` function, so a query naming one is not a fold here.
#[test]
fn product_is_not_an_aggregate_in_sqlite() {
    let mut c = fixture();
    assert_eq!(
        error(&mut c, "SELECT product(a) FROM t"),
        "no such function: product"
    );
}
