// More of the rules, each expectation recorded from sqlite3 3.53.4 the same
// way as the tests in grouping_tests.rs. These cover the shapes the first
// batch does not: grouping by an expression rather than a column, a key of
// more than one column, DISTINCT scoped to a group, and the ordering of an
// aggregate that the projection does not include.

use super::grouping_tests::run;
use crate::connection::Connection;
use crate::value::Value;

use super::grouping_tests::{error, fixture, ints, query, rows, text};

/// A three-row table, used where the shared five-row fixture is more than the
/// test needs.
fn small() -> Connection {
    let mut c = Connection::open_memory().unwrap();
    run(&mut c, "CREATE TABLE m(a, b)");
    run(&mut c, "INSERT INTO m VALUES(1,10), (2,20), (3,30)");
    c
}

/// `CREATE TABLE m(a,b); INSERT INTO m VALUES(1,10),(2,20),(3,30);`
/// `SELECT a%2, count(*) FROM m GROUP BY a%2` → `0|1, 1|2`
///
/// A GROUP BY names an expression, not a column, so the key is that
/// expression's value: row 2 is even and rows 1 and 3 are odd.
#[test]
fn a_group_by_expression_splits_on_its_value() {
    let mut c = small();
    assert_eq!(
        rows(&mut c, "SELECT a%2, count(*) FROM m GROUP BY a%2"),
        [
            vec![Value::Integer(0), Value::Integer(1)],
            vec![Value::Integer(1), Value::Integer(2)],
        ]
    );
}

/// `SELECT k, count(*) FROM g GROUP BY k%2` over `'m','z','a','m'` → `m|4`
///
/// The grouping expression need not be the result column, and the bare `k` is
/// read from the group's first row. There is one group rather than three
/// because every value of `k` is text, and `k % 2` converts text to a number
/// first: each of `'m'`, `'z'` and `'a'` is zero, so the key is 0 four times.
#[test]
fn a_group_by_expression_coerces_text_to_a_number() {
    let mut c = Connection::open_memory().unwrap();
    run(&mut c, "CREATE TABLE g(k)");
    run(&mut c, "INSERT INTO g VALUES('m'), ('z'), ('a'), ('m')");
    assert_eq!(
        rows(&mut c, "SELECT k, count(*) FROM g GROUP BY k%2"),
        [vec![text("m"), Value::Integer(4)]]
    );
}

/// `SELECT b, count(*) FROM t GROUP BY b, a` → five rows of 1
///
/// Two grouping expressions make a two-column key, so a group is a pair and
/// each of the five fixture rows is alone. The output is still sorted by the
/// key, and a pair sorts by its first column then its second.
#[test]
fn two_group_by_expressions_make_a_pair_key() {
    let mut c = fixture();
    assert_eq!(
        rows(&mut c, "SELECT b, count(*) FROM t GROUP BY b, a"),
        [
            vec![text("x"), Value::Integer(1)],
            vec![text("x"), Value::Integer(1)],
            vec![text("y"), Value::Integer(1)],
            vec![text("y"), Value::Integer(1)],
            vec![text("z"), Value::Integer(1)],
        ]
    );
}

/// `SELECT count(*), count(*) FROM t` → `5|5`
///
/// The same call written twice is folded once and read twice, which is what
/// makes `count(*) + 1` work without folding it twice.
#[test]
fn a_repeated_aggregate_is_folded_once() {
    let mut c = fixture();
    assert_eq!(
        rows(&mut c, "SELECT count(*), count(*) FROM t"),
        [vec![Value::Integer(5), Value::Integer(5)]]
    );
}

/// `SELECT b, count(*) FROM t GROUP BY b ORDER BY a` → `z|1, x|2, y|2`
///
/// An ORDER BY may name a table column the projection did not include; it
/// sorts on the group's value for it, which is the first row's. The z group's
/// first row has a NULL `a`, and a NULL sorts before every number, so z leads
/// even though its count is the smallest.
#[test]
fn order_by_may_name_a_column_the_projection_omits() {
    let mut c = fixture();
    assert_eq!(
        rows(&mut c, "SELECT b, count(*) FROM t GROUP BY b ORDER BY a"),
        [
            vec![text("z"), Value::Integer(1)],
            vec![text("x"), Value::Integer(2)],
            vec![text("y"), Value::Integer(2)],
        ]
    );
}

/// `... GROUP BY b HAVING count(*) > 0 ORDER BY b LIMIT 1` → `x|2`
///
/// LIMIT runs after HAVING and after the sort, so it takes the first of what
/// is left rather than the first of everything.
#[test]
fn limit_runs_after_having_and_ordering() {
    let mut c = fixture();
    assert_eq!(
        rows(
            &mut c,
            "SELECT b, count(*) FROM t GROUP BY b HAVING count(*) > 0 ORDER BY b LIMIT 1"
        ),
        [vec![text("x"), Value::Integer(2)]]
    );
}

/// `SELECT b, min(a,b) FROM t GROUP BY b` → `x|1, y|2, z|NULL`
///
/// The two-argument min is a scalar, so it is evaluated against each group's
/// first row. On the z group that row's `a` is NULL, and the scalar form is
/// NULL if any argument is — so this is the one group that does not collapse
/// to a number. The aggregate min skips NULLs, which is the other half of the
/// same arity rule.
#[test]
fn a_scalar_min_reads_the_first_row_of_its_group() {
    let mut c = fixture();
    assert_eq!(
        rows(&mut c, "SELECT b, min(a,b) FROM t GROUP BY b"),
        [
            vec![text("x"), Value::Integer(1)],
            vec![text("y"), Value::Integer(2)],
            vec![text("z"), Value::Null],
        ]
    );
}

/// `SELECT a, count(DISTINCT b) FROM t GROUP BY a` → five rows of 1
///
/// DISTINCT is scoped to the group, not the whole table: each of these groups
/// holds one row, so each count is 1, while the global `count(DISTINCT b)` is
/// 3.
#[test]
fn distinct_is_per_group_not_global() {
    let mut c = fixture();
    assert_eq!(
        rows(&mut c, "SELECT a, count(DISTINCT b) FROM t GROUP BY a"),
        [
            vec![Value::Null, Value::Integer(1)],
            vec![Value::Integer(1), Value::Integer(1)],
            vec![Value::Integer(2), Value::Integer(1)],
            vec![Value::Integer(3), Value::Integer(1)],
            vec![Value::Integer(5), Value::Integer(1)],
        ]
    );
}

/// `SELECT a, b, count(*) FROM t GROUP BY b` → `1|x|2, 2|y|2, NULL|z|1`
#[test]
fn two_bare_columns_both_read_the_first_row() {
    let mut c = fixture();
    assert_eq!(
        rows(&mut c, "SELECT a, b, count(*) FROM t GROUP BY b"),
        [
            vec![Value::Integer(1), text("x"), Value::Integer(2)],
            vec![Value::Integer(2), text("y"), Value::Integer(2)],
            vec![Value::Null, text("z"), Value::Integer(1)],
        ]
    );
}

/// `SELECT upper(b), count(*) FROM t GROUP BY b` → `X|2, Y|2, Z|1`
///
/// A bare *expression* beside an aggregate is allowed too, and reads the
/// group's first row the way a bare column does.
#[test]
fn a_bare_expression_beside_an_aggregate_is_allowed() {
    let mut c = fixture();
    assert_eq!(
        rows(&mut c, "SELECT upper(b), count(*) FROM t GROUP BY b"),
        [
            vec![text("X"), Value::Integer(2)],
            vec![text("Y"), Value::Integer(2)],
            vec![text("Z"), Value::Integer(1)],
        ]
    );
}

/// `SELECT b, count(*) FROM t GROUP BY b HAVING a > 1` → `y|2`
///
/// A HAVING may name a column that is not a grouping key, where it reads the
/// group's first row as the projection would.
#[test]
fn a_having_may_name_a_bare_column() {
    let mut c = fixture();
    assert_eq!(
        rows(&mut c, "SELECT b, count(*) FROM t GROUP BY b HAVING a > 1"),
        [vec![text("y"), Value::Integer(2)]]
    );
}

/// `... GROUP BY b HAVING count(*) * 2 >= 4` → `x|2, y|2`
///
/// The predicate is arithmetic around the aggregate rather than the aggregate
/// itself, and the substitution reaches into the arithmetic.
#[test]
fn a_having_may_arithmetise_an_aggregate() {
    let mut c = fixture();
    assert_eq!(
        rows(
            &mut c,
            "SELECT b, count(*) FROM t GROUP BY b HAVING count(*) * 2 >= 4"
        ),
        [
            vec![text("x"), Value::Integer(2)],
            vec![text("y"), Value::Integer(2)],
        ]
    );
}

/// `SELECT count(*) FROM t GROUP BY b ORDER BY 1` → `1, 2, 2`
///
/// An ordinal naming the only output column sorts by it, and the projection
/// has no GROUP BY column in it at all.
#[test]
fn an_ordinal_can_name_the_only_output_column() {
    let mut c = fixture();
    assert_eq!(
        rows(&mut c, "SELECT count(*) FROM t GROUP BY b ORDER BY 1"),
        [
            vec![Value::Integer(1)],
            vec![Value::Integer(2)],
            vec![Value::Integer(2)],
        ]
    );
}

/// `SELECT b, count(*) AS n FROM t GROUP BY b ORDER BY n` → `z|1, x|2, y|2`
///
/// An alias names the output column, so this sorts by the count, and the
/// one-row group leads.
#[test]
fn an_alias_of_an_aggregate_orders_on_it() {
    let mut c = fixture();
    assert_eq!(
        rows(
            &mut c,
            "SELECT b, count(*) AS n FROM t GROUP BY b ORDER BY n"
        ),
        [
            vec![text("z"), Value::Integer(1)],
            vec![text("x"), Value::Integer(2)],
            vec![text("y"), Value::Integer(2)],
        ]
    );
}

/// `SELECT COUNT(*), Sum(a) FROM t` → column names `COUNT(*)` and `Sum(a)`
///
/// SQLite keeps the spelling the query used, and so does this: the parser
/// recovers a function's name from the source span, so `COUNT(*)` is named
/// `COUNT(*)` rather than the lowercased form the tokenizer folded it to. The
/// shape of the name was always right — a star aggregate is `name(*)`, not
/// `name()` — and the case now matches sqlite3 as well.
///
/// Both spellings were read off sqlite3 3.53.4 with headers on:
/// `SELECT COUNT(*) FROM t` answers `COUNT(*)` and `SELECT Count(*) FROM t`
/// answers `Count(*)`.
#[test]
fn a_result_column_is_named_after_its_expression() {
    let mut c = fixture();
    let (names, got) = query(&mut c, "SELECT COUNT(*), Sum(a) FROM t");
    assert_eq!(names, vec!["COUNT(*)".to_string(), "Sum(a)".to_string()]);
    assert_eq!(got, [vec![Value::Integer(5), Value::Integer(11)]]);
    // A lowercase query still gets a lowercase name, so the recovery does not
    // invent a case the query did not use.
    let (names, _) = query(&mut c, "SELECT count(*), sum(a) FROM t");
    assert_eq!(names, vec!["count(*)".to_string(), "sum(a)".to_string()]);
}

/// `SELECT count(DISTINCT b) FROM t` → column name `count(DISTINCT b)`
#[test]
fn a_distinct_aggregate_names_its_distinct() {
    let mut c = fixture();
    let (names, _) = query(&mut c, "SELECT count(DISTINCT b) FROM t");
    assert_eq!(names, vec!["count(DISTINCT b)".to_string()]);
}

/// `SELECT count(*) FROM t GROUP BY nosuchcol` → `no such column: nosuchcol`
#[test]
fn a_group_by_over_an_unknown_column_is_refused() {
    let mut c = fixture();
    assert_eq!(
        error(&mut c, "SELECT count(*) FROM t GROUP BY nosuchcol"),
        "no such column: nosuchcol"
    );
}

/// `SELECT b, count(*) FROM t GROUP BY b ORDER BY nosuchcol` →
/// `no such column: nosuchcol`
#[test]
fn an_order_by_over_an_unknown_column_is_refused() {
    let mut c = fixture();
    assert_eq!(
        error(
            &mut c,
            "SELECT b, count(*) FROM t GROUP BY b ORDER BY nosuchcol"
        ),
        "no such column: nosuchcol"
    );
}

/// `SELECT nosuchfn(a) FROM t GROUP BY b` → `no such function: nosuchfn`
#[test]
fn an_unknown_function_in_a_grouped_query_is_refused() {
    let mut c = fixture();
    assert_eq!(
        error(&mut c, "SELECT nosuchfn(a) FROM t GROUP BY b"),
        "no such function: nosuchfn"
    );
}

/// `SELECT b, count(*) FROM t GROUP BY b HAVING nosuchfn(1)` →
/// `no such function: nosuchfn`
#[test]
fn an_unknown_function_in_a_having_is_refused() {
    let mut c = fixture();
    assert_eq!(
        error(
            &mut c,
            "SELECT b, count(*) FROM t GROUP BY b HAVING nosuchfn(1)"
        ),
        "no such function: nosuchfn"
    );
}

/// The two queries the track names, run side by side, so the pair of rules
/// they pin — a bare aggregate collapsing the table, and a group sorted by its
/// key — are visible in one place.
///
/// `SELECT count(*) FROM t` → 5, and `SELECT b, sum(a) FROM t GROUP BY b
/// ORDER BY b` → `x|4, y|7, z|NULL`.
#[test]
fn the_two_track_queries_side_by_side() {
    let mut c = fixture();
    assert_eq!(rows(&mut c, "SELECT count(*) FROM t"), [ints(&[5])]);
    assert_eq!(
        rows(&mut c, "SELECT b, sum(a) FROM t GROUP BY b ORDER BY b"),
        [
            vec![text("x"), Value::Integer(4)],
            vec![text("y"), Value::Integer(7)],
            vec![text("z"), Value::Null],
        ]
    );
}

/// `CREATE TABLE bt(k); INSERT INTO bt VALUES(x'31'), ('1');`
/// `SELECT k, count(*) FROM bt GROUP BY k` → two groups, text first
///
/// A blob and a text that look alike are still different values, and a blob
/// renders as `x'31'` rather than `1`, so the two never share a group key.
/// The order is the sort order of [`Value::compare`]: text before blob.
#[test]
fn a_blob_and_text_are_different_groups() {
    let mut c = Connection::open_memory().unwrap();
    run(&mut c, "CREATE TABLE bt(k)");
    run(&mut c, "INSERT INTO bt VALUES(x'31'), ('1')");
    assert_eq!(
        rows(&mut c, "SELECT k, count(*) FROM bt GROUP BY k"),
        [
            vec![text("1"), Value::Integer(1)],
            vec![Value::Blob(vec![0x31]), Value::Integer(1)],
        ]
    );
}

/// `SELECT count(DISTINCT k) FROM bt` → `2`
#[test]
fn a_blob_and_text_are_different_distinct_values() {
    let mut c = Connection::open_memory().unwrap();
    run(&mut c, "CREATE TABLE bt(k)");
    run(&mut c, "INSERT INTO bt VALUES(x'31'), ('1')");
    assert_eq!(
        rows(&mut c, "SELECT count(DISTINCT k) FROM bt"),
        [vec![Value::Integer(2)]]
    );
}

/// `CREATE TABLE rt(k); INSERT INTO rt VALUES(1), (1.0);`
/// `SELECT k, count(*) FROM rt GROUP BY k` → one group of 2
///
/// A real and an integer that compare equal are one group, because SQLite
/// compares the two numeric classes by value. This is the other side of the
/// blob case above: the two spellings of one number merge, the two kinds of
/// string do not.
#[test]
fn a_real_and_an_integer_that_compare_equal_are_one_group() {
    let mut c = Connection::open_memory().unwrap();
    run(&mut c, "CREATE TABLE rt(k)");
    run(&mut c, "INSERT INTO rt VALUES(1), (1.0)");
    assert_eq!(
        rows(&mut c, "SELECT k, count(*) FROM rt GROUP BY k"),
        [vec![Value::Integer(1), Value::Integer(2)]]
    );
}
