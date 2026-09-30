//! What each ORDER BY term names, checked against the real sqlite3.
//!
//! Every expectation here is the answer sqlite3 3.53.4 gave for the same
//! statement over the same fixture, run by hand against its own database file
//! and compared. The oracle answer is quoted above each test, because the point
//! of several of them is a message that has to agree byte for byte.
//!
//! The fixture most tests use is a two-column table whose rows arrive in an
//! order neither column is in, so a sort that quietly became a no-op cannot
//! pass by accident:
//!
//! ```text
//! t(a, b) = (1,20), (2,10), (3,30)
//! ```
//!
//! The rows as inserted are already in `a` order, so a key on `a` cannot tell
//! a real sort from no sort at all. The `b` keys, the NULL fixture and the
//! ties are what separate them.

use crate::connection::Connection;
use crate::value::Value;

/// A connection with the two-column fixture, rows inserted in `a` order.
fn t() -> Connection {
    let mut c = Connection::open_memory().expect("in-memory");
    run(&mut c, "CREATE TABLE t(a,b);");
    for (a, b) in [(1, 20), (2, 10), (3, 30)] {
        run(&mut c, &format!("INSERT INTO t VALUES({a},{b});"));
    }
    c
}

/// A connection whose fixture has a NULL in the first column.
///
/// `n(x, y) = (2,'b'), (NULL,'a'), (1,'c')` — the NULL row is in the middle as
/// inserted, so what is under test is where NULL sits relative to the others,
/// not where the rows were put.
fn nulls() -> Connection {
    let mut c = Connection::open_memory().expect("in-memory");
    run(&mut c, "CREATE TABLE n(x,y);");
    for (x, y) in [("2", "b"), ("NULL", "a"), ("1", "c")] {
        run(&mut c, &format!("INSERT INTO n VALUES({x},'{y}');"));
    }
    c
}

/// A two-group fixture whose sums differ, so a tie cannot hide a wrong order.
fn groups() -> Connection {
    let mut c = Connection::open_memory().expect("in-memory");
    run(&mut c, "CREATE TABLE g(a,b);");
    for (a, b) in [(1, 10), (1, 20), (2, 40)] {
        run(&mut c, &format!("INSERT INTO g VALUES({a},{b});"));
    }
    c
}

/// Runs a statement, which is expected to succeed.
fn run(c: &mut Connection, sql: &str) -> crate::connection::Outcome {
    let out = c
        .execute_script(sql)
        .unwrap_or_else(|e| panic!("{sql:?} failed: {e}"));
    out.into_iter().last().expect("one outcome")
}

/// The rows of a query as joined text, a NULL being the empty string.
fn query(c: &mut Connection, sql: &str) -> Vec<String> {
    let crate::connection::Outcome::Query { rows, .. } = run(c, sql) else {
        panic!("{sql:?} is not a query");
    };
    rows.iter()
        .map(|r| {
            r.values
                .iter()
                .map(|v| match v {
                    Value::Null => String::new(),
                    other => other.to_string(),
                })
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect()
}

/// The message of a statement the oracle refuses.
fn fails(c: &mut Connection, sql: &str) -> String {
    c.execute_script(sql)
        .expect_err(&format!("{sql:?} should have failed"))
        .message
}

/// A fresh in-memory connection for a query that needs no fixture.
fn empty() -> Connection {
    Connection::open_memory().expect("in-memory")
}

// ---- An integer literal is an ordinal ------------------------------------

#[test]
fn an_integer_literal_sorts_on_the_column_it_names() {
    // sqlite3: 2|10  1|20  3|30
    let mut c = t();
    assert_eq!(
        query(&mut c, "SELECT a,b FROM t ORDER BY 2"),
        vec!["2|10", "1|20", "3|30"]
    );
}

#[test]
fn an_ordinal_names_the_whole_projection_not_the_table() {
    // sqlite3: 3|20  6|10  9|30 -- the first result column is the alias `a*3`.
    let mut c = t();
    assert_eq!(
        query(&mut c, "SELECT a*3 AS c, b FROM t ORDER BY 1"),
        vec!["3|20", "6|10", "9|30"]
    );
}

#[test]
fn an_ordinal_reverses_under_desc() {
    // sqlite3: 3|30  1|20  2|10 -- b is 20,10,30, so descending it is 30,20,10
    // and the rows come out 3,1,2. The rows are not a simple reversal of the
    // ascending answer, because the two columns are not a permutation of each
    // other.
    let mut c = t();
    assert_eq!(
        query(&mut c, "SELECT a,b FROM t ORDER BY 2 DESC"),
        vec!["3|30", "1|20", "2|10"]
    );
}

#[test]
fn an_ordinal_beyond_the_last_column_is_refused() {
    // sqlite3: "1st ORDER BY term out of range - should be between 1 and 2"
    let mut c = t();
    assert_eq!(
        fails(&mut c, "SELECT a,b FROM t ORDER BY 3"),
        "1st ORDER BY term out of range - should be between 1 and 2"
    );
}

#[test]
fn the_message_names_the_term_that_failed_and_not_the_first() {
    // sqlite3: "2nd ORDER BY term out of range - should be between 1 and 2"
    let mut c = t();
    assert_eq!(
        fails(&mut c, "SELECT a,b FROM t ORDER BY 1,9"),
        "2nd ORDER BY term out of range - should be between 1 and 2"
    );
}

#[test]
fn the_first_out_of_range_term_is_the_one_reported() {
    // sqlite3: "1st ORDER BY term out of range - should be between 1 and 2"
    // for both `ORDER BY 9,3` and `ORDER BY 3,9` — the check stops at the
    // first bad term rather than reporting every one.
    let mut c = t();
    for sql in [
        "SELECT a,b FROM t ORDER BY 9,3",
        "SELECT a,b FROM t ORDER BY 3,9",
    ] {
        assert_eq!(
            fails(&mut c, sql),
            "1st ORDER BY term out of range - should be between 1 and 2",
            "{sql}"
        );
    }
}

// ---- The message's suffix agrees with English ----------------------------

#[test]
fn the_suffix_agrees_with_sqlite_for_every_position_up_to_the_teens() {
    // sqlite3, with the bad term at position n: 1st 2nd 3rd 4th 5th 6th 7th 8th
    // 9th 10th 11th 12th 13th. The teens are the cases a mod-10 rule gets
    // wrong, which is why 11 is 11th and not 11st.
    let want = [
        "1st", "2nd", "3rd", "4th", "5th", "6th", "7th", "8th", "9th", "10th", "11th", "12th",
        "13th",
    ];
    for (i, suffix) in want.iter().enumerate() {
        let mut c = t();
        // `i` good terms then the bad one, so the bad term is at position i+1.
        let mut terms: Vec<&str> = vec!["1"; i];
        terms.push("9");
        let sql = format!("SELECT a,b FROM t ORDER BY {}", terms.join(","));
        let got = fails(&mut c, &sql);
        assert!(
            got.starts_with(suffix),
            "term {} should be {suffix}, got {got:?}",
            i + 1
        );
    }
}

#[test]
fn the_twenty_first_term_is_21st() {
    // sqlite3: "21st ORDER BY term out of range - should be between 1 and 2"
    let mut c = t();
    let keys = vec!["1"; 20];
    let sql = format!("SELECT a,b FROM t ORDER BY {},9", keys.join(","));
    assert_eq!(
        fails(&mut c, &sql),
        "21st ORDER BY term out of range - should be between 1 and 2"
    );
}

// ---- Zero, and a sign on the literal -------------------------------------

#[test]
fn zero_is_out_of_range_rather_than_an_expression() {
    // sqlite3: "1st ORDER BY term out of range - should be between 1 and 2"
    // `ORDER BY 0` is a parse error, not a key that ties every row together.
    let mut c = t();
    assert_eq!(
        fails(&mut c, "SELECT a,b FROM t ORDER BY 0"),
        "1st ORDER BY term out of range - should be between 1 and 2"
    );
}

#[test]
fn a_folded_plus_is_still_the_ordinal() {
    // sqlite3: 1|20  2|10  3|30 -- `+1` folds to the ordinal 1
    let mut c = t();
    assert_eq!(
        query(&mut c, "SELECT a,b FROM t ORDER BY +1"),
        vec!["1|20", "2|10", "3|30"]
    );
}

#[test]
fn a_folded_plus_within_range_sorts_on_the_column_it_names() {
    // sqlite3: 2|10  1|20  3|30 -- `+2` is in range here and names `b`, which
    // is the same answer as `ORDER BY 2`.
    let mut c = t();
    assert_eq!(
        query(&mut c, "SELECT a,b FROM t ORDER BY +2"),
        vec!["2|10", "1|20", "3|30"]
    );
}

#[test]
fn a_folded_plus_past_the_last_column_is_still_out_of_range() {
    // sqlite3: "1st ORDER BY term out of range - should be between 1 and 1"
    // over a one-column table. `+2` has to fold to the ordinal 2 for the count
    // in the message to be 1; as an ordinary expression it would sort fine and
    // raise nothing.
    let mut c = empty();
    run(&mut c, "CREATE TABLE p(a);");
    assert_eq!(
        fails(&mut c, "SELECT a FROM p ORDER BY +2"),
        "1st ORDER BY term out of range - should be between 1 and 1"
    );
}

#[test]
fn an_arithmetic_expression_on_a_literal_is_not_an_ordinal() {
    // sqlite3: 1|20  2|10  3|30 -- `1+0` is the constant 1, so every row ties
    // and the stable sort leaves them as inserted. Sorting on the ordinal would
    // give these rows too, so the pair of tests is what pins it down.
    let mut c = t();
    assert_eq!(
        query(&mut c, "SELECT a,b FROM t ORDER BY 1+0"),
        vec!["1|20", "2|10", "3|30"]
    );
}

#[test]
fn an_expression_mentioning_the_second_column_is_a_key_on_it() {
    // sqlite3: 1|20  2|10  3|30 -- `2-0` is the constant 2, so every row ties
    // and the rows stay as inserted. Read as the ordinal 2 the answer would be
    // 2|10  1|20  3|30, which is what the test above would then also have to
    // produce; the two readings cannot both be right.
    let mut c = t();
    assert_eq!(
        query(&mut c, "SELECT a,b FROM t ORDER BY 2-0"),
        vec!["1|20", "2|10", "3|30"]
    );
}

#[test]
fn a_bitwise_not_on_a_literal_is_not_an_ordinal() {
    // sqlite3: 1|20  2|10  3|30 -- `~1` is a constant key and the rows stay as
    // inserted. It is deliberately not an ordinal: ~1 is -2, which is out of
    // range, and sqlite3 does not raise here.
    let mut c = t();
    assert_eq!(
        query(&mut c, "SELECT a,b FROM t ORDER BY ~1"),
        vec!["1|20", "2|10", "3|30"]
    );
}

#[test]
fn a_star_counts_as_every_column_it_expanded_to() {
    // sqlite3: 2|10  1|20  3|30 -- `SELECT *` over t is two result columns, so
    // the ordinal 2 is in range and names `b`.
    let mut c = t();
    assert_eq!(
        query(&mut c, "SELECT * FROM t ORDER BY 2"),
        vec!["2|10", "1|20", "3|30"]
    );
}

#[test]
fn an_ordinal_past_a_star_is_out_of_range() {
    // sqlite3: "1st ORDER BY term out of range - should be between 1 and 3"
    let mut c = empty();
    run(&mut c, "CREATE TABLE s(a,b,c);");
    assert_eq!(
        fails(&mut c, "SELECT * FROM s ORDER BY 4"),
        "1st ORDER BY term out of range - should be between 1 and 3"
    );
}

// ---- The interaction with an alias ---------------------------------------

#[test]
fn an_ordinal_reads_the_projection_even_when_the_alias_shadows_a_column() {
    // sqlite3: 3|20  6|10  9|30 -- the first result column is the alias `a*3`
    // and the second is the real `b`, so the ordinal 1 is the alias.
    let mut c = t();
    assert_eq!(
        query(&mut c, "SELECT a*3 AS b, b FROM t ORDER BY 1"),
        vec!["3|20", "6|10", "9|30"]
    );
}

#[test]
fn a_bare_name_reads_the_projection_even_when_the_alias_shadows_a_column() {
    // sqlite3: 1|20  2|10  3|30 -- the first column named `b` is the alias
    // `a AS b`, so the bare name sorts on 1,2,3 and not on the table's b.
    let mut c = t();
    assert_eq!(
        query(&mut c, "SELECT a AS b, b FROM t ORDER BY b"),
        vec!["1|20", "2|10", "3|30"]
    );
}

#[test]
fn a_bare_name_reads_the_first_result_column_carrying_it() {
    // sqlite3: 20|1  10|2  30|3 -- here the *first* column named `b` is the
    // table's own, so the bare name sorts on 20,10,30 rather than on the alias.
    let mut c = t();
    assert_eq!(
        query(&mut c, "SELECT b, a AS b FROM t ORDER BY b"),
        vec!["20|1", "10|2", "30|3"]
    );
}

#[test]
fn an_alias_with_no_column_of_that_name_is_substituted_into_an_expression() {
    // sqlite3: 1  2  3 -- `zz+1` becomes `a+1`, which sorts 2,3,4, but the query
    // projects `a` under the name zz, so the rows printed are 1, 2, 3.
    let mut c = t();
    assert_eq!(
        query(&mut c, "SELECT a AS zz FROM t ORDER BY zz+1"),
        vec!["1", "2", "3"]
    );
}

#[test]
fn a_substituted_alias_is_the_expression_it_stands_for() {
    // sqlite3: 2  3  4 -- `c*2` becomes `(a+1)*2` = 4,6,8, which sorts 2,3,4.
    let mut c = t();
    assert_eq!(
        query(&mut c, "SELECT a+1 AS c FROM t ORDER BY c*2"),
        vec!["2", "3", "4"]
    );
}

#[test]
fn an_alias_mixed_with_a_column_sorts_on_the_sum() {
    // sqlite3: 2|10  1|20  3|30 -- `zz+b` becomes `a+b` = 21,12,33.
    let mut c = t();
    assert_eq!(
        query(&mut c, "SELECT a AS zz, b FROM t ORDER BY zz+b"),
        vec!["2|10", "1|20", "3|30"]
    );
}

#[test]
fn an_alias_name_matches_case_insensitively() {
    // sqlite3: 10  20  30
    let mut c = t();
    assert_eq!(
        query(&mut c, "SELECT b AS bb FROM t ORDER BY BB"),
        vec!["10", "20", "30"]
    );
}

#[test]
fn a_real_column_wins_over_an_alias_inside_an_expression() {
    // sqlite3: 2  1  3 -- `b+1` is the *table's* b, 20,10,30, so 21,11,31.
    // Had the alias `a` been substituted the answer would be 1,2,3.
    let mut c = t();
    assert_eq!(
        query(&mut c, "SELECT a AS b FROM t ORDER BY b+1"),
        vec!["2", "1", "3"]
    );
}

#[test]
fn a_real_column_wins_inside_an_expression_even_beside_the_alias() {
    // sqlite3: 2|10  1|20  3|30 -- the table's b, 20,10,30, plus 1.
    let mut c = t();
    assert_eq!(
        query(&mut c, "SELECT a AS b, b FROM t ORDER BY b+1"),
        vec!["2|10", "1|20", "3|30"]
    );
}

#[test]
fn a_qualified_name_is_never_an_alias() {
    // sqlite3: 1  2  3 -- the result is the single column `zz`, so this is the
    // plain table column `t.a` and not the alias, whatever the result calls it.
    let mut c = t();
    assert_eq!(
        query(&mut c, "SELECT a AS zz FROM t ORDER BY t.a"),
        vec!["1", "2", "3"]
    );
}

// ---- A name that is nothing is "no such column" ---------------------------

#[test]
fn a_name_that_is_neither_a_column_nor_an_alias_is_no_such_column() {
    // sqlite3: "no such column: zzz"
    let mut c = t();
    assert_eq!(
        fails(&mut c, "SELECT a,b FROM t ORDER BY zzz"),
        "no such column: zzz"
    );
}

#[test]
fn an_unknown_name_beside_a_valid_ordinal_is_still_reported() {
    // sqlite3: "no such column: zzz" -- the ordinal is in range, so only the
    // name is wrong.
    let mut c = t();
    assert_eq!(
        fails(&mut c, "SELECT a,b FROM t ORDER BY 1,zzz"),
        "no such column: zzz"
    );
}

#[test]
fn a_name_is_reported_before_an_out_of_range_ordinal() {
    // sqlite3: "no such column: zzz" for `ORDER BY 9,zzz`. The ordinal is
    // inside the range SQLite range-checks eagerly, so its error waits until
    // every name has been resolved, and the name at term 2 is what is left.
    // The engine reaches the same message through its own bind_all, which is
    // called before the ORDER BY is looked at; this is the resolver's own
    // answer, which is what a call site with no FROM to bind has to produce.
    assert_eq!(
        refuses("SELECT a,b FROM t ORDER BY 9,zzz"),
        "no such column: zzz"
    );
}

#[test]
fn a_zero_ordinal_outranks_a_later_name() {
    // sqlite3: "1st ORDER BY term out of range - should be between 1 and 2"
    // for `ORDER BY 0,zzz`. Zero is settled by a sign test in the statement's
    // own term order, so it beats a `zzz` that would otherwise be reported.
    assert_eq!(
        refuses("SELECT a,b FROM t ORDER BY 0,zzz"),
        "1st ORDER BY term out of range - should be between 1 and 2"
    );
}

#[test]
fn a_negative_ordinal_outranks_a_later_name() {
    // sqlite3: "1st ORDER BY term out of range - should be between 1 and 2"
    // for `ORDER BY -1,zzz` — the other half of the sign test.
    assert_eq!(
        refuses("SELECT a,b FROM t ORDER BY -1,zzz"),
        "1st ORDER BY term out of range - should be between 1 and 2"
    );
}

#[test]
fn a_name_before_an_eager_ordinal_still_wins() {
    // sqlite3: "no such column: zzz" for `ORDER BY zzz,65536`. The two checks
    // are in term order, so the name at term 1 is reported first even though
    // the ordinal at term 2 is eager.
    assert_eq!(
        refuses("SELECT a,b FROM t ORDER BY zzz,65536"),
        "no such column: zzz"
    );
}

#[test]
fn an_ordinal_above_the_eager_bound_outranks_a_later_name() {
    // sqlite3: "1st ORDER BY term out of range - should be between 1 and 2"
    // for `ORDER BY 65536,zzz`. This is the upper-bound half of the split:
    // 0x10000 and up is settled in term order, so it beats a `zzz` that would
    // otherwise be reported.
    assert_eq!(
        refuses("SELECT a,b FROM t ORDER BY 65536,zzz"),
        "1st ORDER BY term out of range - should be between 1 and 2"
    );
}

#[test]
fn an_ordinal_just_under_the_eager_bound_does_not_outrank_a_name() {
    // sqlite3: "no such column: zzz" for `ORDER BY 65535,zzz`. 0xffff is the
    // last value whose error is held back, so the name is reported even though
    // the ordinal is far out of range.
    assert_eq!(
        refuses("SELECT a,b FROM t ORDER BY 65535,zzz"),
        "no such column: zzz"
    );
}

#[test]
fn a_second_eager_ordinal_is_the_one_named() {
    // sqlite3: "2nd ORDER BY term out of range - should be between 1 and 2"
    // for `ORDER BY 1,65536,zzz` — the first term is in range, the second is
    // eager, and the name at term 3 never gets a look-in.
    assert_eq!(
        refuses("SELECT a,b FROM t ORDER BY 1,65536,zzz"),
        "2nd ORDER BY term out of range - should be between 1 and 2"
    );
}

#[test]
fn a_deferred_ordinal_is_reported_only_once_the_names_are_all_good() {
    // sqlite3: "1st ORDER BY term out of range - should be between 1 and 2"
    // for `ORDER BY 9,a`. `a` is a column of the FROM, so it resolves and the
    // deferred ordinal is left to be reported, while the same statement with an
    // unresolvable name reports the name instead.
    assert_eq!(
        refuses("SELECT a,b FROM t ORDER BY 9,a"),
        "1st ORDER BY term out of range - should be between 1 and 2"
    );
}

#[test]
fn the_first_deferred_ordinal_is_the_one_reported() {
    // sqlite3: "1st ORDER BY term out of range - should be between 1 and 2"
    // for `ORDER BY 9,3,9` — term 3 is out of range too, but term 1 is
    // reported first.
    assert_eq!(
        refuses("SELECT a,b FROM t ORDER BY 9,3,9"),
        "1st ORDER BY term out of range - should be between 1 and 2"
    );
}

#[test]
fn a_later_term_is_the_one_named_when_the_earlier_ones_are_in_range() {
    // sqlite3: "3rd ORDER BY term out of range - should be between 1 and 2"
    // for `ORDER BY 1,2,3` — the first bad term is reported, and the terms
    // above it are each in range.
    assert_eq!(
        refuses("SELECT a,b FROM t ORDER BY 1,2,3"),
        "3rd ORDER BY term out of range - should be between 1 and 2"
    );
}

// ---- The ordinal resolves, whatever a call site has to do to reach it ------
//
// The engine refuses a folded `+2` and a no-FROM ORDER BY of its own accord, so
// these read the resolver directly. They are the behaviour the engine has to
// reach once it hands the ORDER BY to this module, and the wording is the one
// sqlite3 gives.

#[test]
fn a_folded_plus_is_the_ordinal_it_signs() {
    // sqlite3: 2|10  1|20  3|30 -- `+2` names the second result column, which
    // is the same key `ORDER BY 2` names.
    let keys = keys_of("SELECT a,b FROM t ORDER BY +2");
    assert_eq!(keys[0].kind, crate::orderby::KeyKind::Ordinal);
    assert_eq!(keys[0].at, Some(1));
    assert_eq!(keys_of("SELECT a,b FROM t ORDER BY 2")[0].at, Some(1));
}

#[test]
fn a_folded_plus_past_the_last_column_is_out_of_range() {
    // sqlite3: "1st ORDER BY term out of range - should be between 1 and 1"
    // over a one-column table. `+2` has to fold to the ordinal 2 for the count
    // in the message to be 1; read as an expression it would sort fine and
    // raise nothing.
    assert_eq!(
        refuses("SELECT a FROM p ORDER BY +2"),
        "1st ORDER BY term out of range - should be between 1 and 1"
    );
}

#[test]
fn a_double_negative_is_the_positive_ordinal() {
    // sqlite3: 1|20  2|10  3|30 -- `-(-1)` folds twice, to the ordinal 1.
    let keys = keys_of("SELECT a,b FROM t ORDER BY -(-1)");
    assert_eq!(keys[0].kind, crate::orderby::KeyKind::Ordinal);
    assert_eq!(keys[0].at, Some(0));
}

#[test]
fn a_negative_ordinal_is_out_of_range() {
    // sqlite3: "1st ORDER BY term out of range - should be between 1 and 2"
    assert_eq!(
        refuses("SELECT a,b FROM t ORDER BY -1"),
        "1st ORDER BY term out of range - should be between 1 and 2"
    );
}

#[test]
fn an_ordinal_is_checked_against_the_result_columns_with_no_from() {
    // sqlite3: 7|3 for `SELECT 7,3 ORDER BY 2`, and
    // "1st ORDER BY term out of range - should be between 1 and 2" for
    // `SELECT 1,2 ORDER BY 3`. There is no FROM to resolve against, so the
    // result columns are all there is.
    let keys = keys_of("SELECT 7,3 ORDER BY 2");
    assert_eq!(keys[0].kind, crate::orderby::KeyKind::Ordinal);
    assert_eq!(keys[0].at, Some(1));
    assert_eq!(
        refuses("SELECT 1,2 ORDER BY 3"),
        "1st ORDER BY term out of range - should be between 1 and 2"
    );
}

#[test]
fn a_name_with_no_from_resolves_against_the_projection() {
    // sqlite3: 1 -- `q` is the alias of the only result column, and with no FROM
    // there is no source column that could shadow it.
    let keys = keys_of("SELECT 1 AS q ORDER BY q");
    assert_eq!(keys[0].kind, crate::orderby::KeyKind::OutputName);
    assert_eq!(keys[0].at, Some(0));
}

#[test]
fn an_unknown_name_with_no_from_is_still_no_such_column() {
    // sqlite3: "no such column: zzz"
    assert_eq!(refuses("SELECT 1 AS q ORDER BY zzz"), "no such column: zzz");
}

#[test]
fn an_out_of_range_ordinal_with_no_from_counts_the_result_columns() {
    // sqlite3: "1st ORDER BY term out of range - should be between 1 and 1"
    assert_eq!(
        refuses("SELECT 1 AS q ORDER BY 2"),
        "1st ORDER BY term out of range - should be between 1 and 1"
    );
}

#[test]
fn an_alias_of_an_alias_is_not_resolved() {
    // sqlite3: "no such column: c" — the projection's own `c AS d` is refused
    // while the query is being built, so the name `d` never becomes a key.
    let mut c = t();
    assert_eq!(
        fails(&mut c, "SELECT a AS c, c AS d FROM t ORDER BY d"),
        "no such column: c"
    );
}

#[test]
fn an_ambiguous_name_is_reported_where_a_bare_key_would_be_read() {
    // sqlite3: "ambiguous column name: x" — `x` is a column of both tables, so
    // neither side can be chosen.
    let mut c = empty();
    run(&mut c, "CREATE TABLE a(x); CREATE TABLE b(x);");
    assert_eq!(
        fails(&mut c, "SELECT a.x FROM a JOIN b ON 1 ORDER BY x"),
        "ambiguous column name: x"
    );
}

// ---- ASC, DESC, and where NULL goes --------------------------------------

#[test]
fn a_null_sorts_first_on_ascending() {
    // sqlite3: NULL|'a'  1|'c'  2|'b' — NULL before 1, the opposite of most
    // languages' default of sorting NULL last.
    let mut c = nulls();
    assert_eq!(
        query(&mut c, "SELECT * FROM n ORDER BY x ASC"),
        vec!["|a", "1|c", "2|b"]
    );
}

#[test]
fn a_null_sorts_last_on_descending() {
    // sqlite3: 2|'b'  1|'c'  NULL|'a' — the same comparison reversed, so NULL
    // moves from the front to the back.
    let mut c = nulls();
    assert_eq!(
        query(&mut c, "SELECT * FROM n ORDER BY x DESC"),
        vec!["2|b", "1|c", "|a"]
    );
}

#[test]
fn an_ordinal_sorts_null_first_without_a_direction_too() {
    // sqlite3: NULL|'a'  1|'c'  2|'b' — ASC is the default and the ordinal is
    // the same key as `ORDER BY x`.
    let mut c = nulls();
    assert_eq!(
        query(&mut c, "SELECT * FROM n ORDER BY 1"),
        vec!["|a", "1|c", "2|b"]
    );
}

#[test]
fn an_ordinal_sorts_null_last_when_reversed() {
    // sqlite3: 2|'b'  1|'c'  NULL|'a'
    let mut c = nulls();
    assert_eq!(
        query(&mut c, "SELECT * FROM n ORDER BY 1 DESC"),
        vec!["2|b", "1|c", "|a"]
    );
}

#[test]
fn a_later_term_only_breaks_a_tie() {
    // sqlite3: 1|'c'  2|'b'  NULL|'a' — `2 DESC, 1 ASC` sorts on the text
    // c,b,a descending and the first key never has to break a tie.
    let mut c = nulls();
    assert_eq!(
        query(&mut c, "SELECT * FROM n ORDER BY 2 DESC, 1 ASC"),
        vec!["1|c", "2|b", "|a"]
    );
}

#[test]
fn each_term_carries_its_own_direction() {
    // sqlite3: 2|10  1|20  3|30 — b ascending, so 10,20,30 whatever the DESC on
    // the other term says.
    let mut c = t();
    assert_eq!(
        query(&mut c, "SELECT a,b FROM t ORDER BY 2, 1 DESC"),
        vec!["2|10", "1|20", "3|30"]
    );
}

#[test]
fn rows_that_tie_keep_the_order_they_arrived_in() {
    // sqlite3: 3|1  1|1  2|1  — every key ties, and both directions leave the
    // rows as inserted. SQLite does not promise this, but a stable sort is the
    // reproducible reading of "unspecified" and is what this engine does.
    let mut c = empty();
    run(&mut c, "CREATE TABLE u(id,k);");
    for (id, k) in [(3, 1), (1, 1), (2, 1)] {
        run(&mut c, &format!("INSERT INTO u VALUES({id},{k});"));
    }
    for sql in [
        "SELECT * FROM u ORDER BY k",
        "SELECT * FROM u ORDER BY k DESC",
    ] {
        assert_eq!(query(&mut c, sql), vec!["3|1", "1|1", "2|1"], "{sql}");
    }
}

// ---- A SELECT with no FROM ------------------------------------------------

#[test]
fn an_ordinal_works_without_a_from_clause() {
    // sqlite3: 7|3 — a one-row query sorts trivially, but the ordinal is still
    // checked and the key still resolved.
    let mut c = empty();
    assert_eq!(query(&mut c, "SELECT 7,3 ORDER BY 2"), vec!["7|3"]);
}

#[test]
fn an_out_of_range_ordinal_without_a_from_clause_is_still_refused() {
    // sqlite3: "1st ORDER BY term out of range - should be between 1 and 2"
    let mut c = empty();
    assert_eq!(
        fails(&mut c, "SELECT 1,2 ORDER BY 3"),
        "1st ORDER BY term out of range - should be between 1 and 2"
    );
}

#[test]
fn a_name_without_a_from_clause_resolves_against_the_projection() {
    // sqlite3: 1 — `q` is the alias of the only result column.
    let mut c = empty();
    assert_eq!(query(&mut c, "SELECT 1 AS q ORDER BY q"), vec!["1"]);
}

#[test]
fn an_unknown_name_without_a_from_clause_is_still_no_such_column() {
    // sqlite3: "no such column: zzz"
    let mut c = empty();
    assert_eq!(
        fails(&mut c, "SELECT 1 AS q ORDER BY zzz"),
        "no such column: zzz"
    );
}

#[test]
fn an_out_of_range_ordinal_without_a_from_clause_counts_the_result_columns() {
    // sqlite3: "1st ORDER BY term out of range - should be between 1 and 1"
    let mut c = empty();
    assert_eq!(
        fails(&mut c, "SELECT 1 AS q ORDER BY 2"),
        "1st ORDER BY term out of range - should be between 1 and 1"
    );
}

// ---- A grouped query ------------------------------------------------------

#[test]
fn an_ordinal_names_an_aggregate_result_of_a_grouped_query() {
    // sqlite3: 1|30  2|40 — the ordinal 2 is `sum(b)`.
    let mut c = groups();
    assert_eq!(
        query(&mut c, "SELECT a, sum(b) AS s FROM g GROUP BY a ORDER BY 1"),
        vec!["1|30", "2|40"]
    );
}

#[test]
fn an_aggregate_alias_is_substituted_into_an_expression() {
    // sqlite3: 1|30  2|40 — `s*1` becomes `sum(b)*1`, which sorts 30,40.
    let mut c = groups();
    assert_eq!(
        query(
            &mut c,
            "SELECT a, sum(b) AS s FROM g GROUP BY a ORDER BY s*1"
        ),
        vec!["1|30", "2|40"]
    );
}

#[test]
fn a_negated_aggregate_alias_reverses() {
    // sqlite3: 2|40  1|30 — `-s` is -30 and -40, ascending, so 40 comes first.
    let mut c = groups();
    assert_eq!(
        query(
            &mut c,
            "SELECT a, sum(b) AS s FROM g GROUP BY a ORDER BY -s ASC"
        ),
        vec!["2|40", "1|30"]
    );
}

#[test]
fn an_out_of_range_ordinal_in_a_grouped_query_names_the_group_column_count() {
    // sqlite3: "1st ORDER BY term out of range - should be between 1 and 2"
    let mut c = groups();
    assert_eq!(
        fails(&mut c, "SELECT a, sum(b) AS s FROM g GROUP BY a ORDER BY 3"),
        "1st ORDER BY term out of range - should be between 1 and 2"
    );
}

#[test]
fn a_grouped_query_with_no_rows_still_reports_an_out_of_range_ordinal() {
    // sqlite3: "1st ORDER BY term out of range - should be between 1 and 2"
    // over an empty table — the check does not depend on there being a row.
    let mut c = empty();
    run(&mut c, "CREATE TABLE e(a,b);");
    assert_eq!(
        fails(&mut c, "SELECT a,b FROM e WHERE 0 ORDER BY 7"),
        "1st ORDER BY term out of range - should be between 1 and 2"
    );
}

#[test]
fn a_query_whose_rows_are_all_filtered_away_still_checks_the_ordinal() {
    // sqlite3: "1st ORDER BY term out of range - should be between 1 and 2"
    let mut c = t();
    assert_eq!(
        fails(&mut c, "SELECT a,b FROM t WHERE 0 ORDER BY 7"),
        "1st ORDER BY term out of range - should be between 1 and 2"
    );
}

// ---- The keys the module itself resolves ----------------------------------

#[test]
fn a_key_is_built_for_every_term_with_its_own_direction() {
    let keys = keys_of("SELECT a,b FROM t ORDER BY 1, 2 DESC");
    assert_eq!(keys.len(), 2);
    assert_eq!(keys[0].kind, crate::orderby::KeyKind::Ordinal);
    assert!(keys[0].ascending);
    assert_eq!(keys[1].kind, crate::orderby::KeyKind::Ordinal);
    assert!(!keys[1].ascending);
}

#[test]
fn a_bare_name_and_an_ordinal_name_the_same_column() {
    let by_ordinal = keys_of("SELECT b AS k FROM t ORDER BY 1");
    let by_name = keys_of("SELECT b AS k FROM t ORDER BY k");
    assert_eq!(by_ordinal[0].kind, crate::orderby::KeyKind::Ordinal);
    assert_eq!(by_name[0].kind, crate::orderby::KeyKind::OutputName);
    assert_eq!(by_ordinal[0].at, by_name[0].at);
    assert_eq!(by_ordinal[0].at, Some(0));
}

#[test]
fn a_key_that_names_a_column_reads_the_projection() {
    // The two key kinds that read a projected value are indistinguishable once
    // resolved, which is what `read` relies on.
    let keys = keys_of("SELECT a,b FROM t ORDER BY 2");
    let projected = vec![Value::from(1i64), Value::from(9i64)];
    let got = crate::orderby::read(&keys[0], &projected, |_| {
        panic!("an ordinal has no expression to evaluate")
    })
    .expect("read");
    assert_eq!(got, Value::from(9i64));
}

// ---- What the resolver does with an alias inside an expression ------------
//
// These read the resolved expression rather than running a query, because the
// substitution is decided before any row exists: the difference between an
// alias winning and a real column winning is visible in the tree, and running
// it would only show the same difference one step later.

/// The expression a key evaluates, as its source text.
fn key_source(sql: &str) -> String {
    let keys = keys_of(sql);
    assert_eq!(keys.len(), 1, "{sql:?} should resolve to one key");
    assert_eq!(
        keys[0].kind,
        crate::orderby::KeyKind::Expression,
        "{sql:?} is not an expression key"
    );
    format!(
        "{:?}",
        keys[0]
            .expr
            .as_ref()
            .expect("an expression key carries one")
    )
}

#[test]
fn an_alias_is_substituted_into_an_expression() {
    // sqlite3: 1  2  3 -- `zz+1` becomes `a+1`, so the key reads the column a
    // and sorts 2,3,4, and the rows printed are the projection's 1,2,3.
    let got = key_source("SELECT a AS zz FROM t ORDER BY zz+1");
    assert!(
        !got.contains("zz"),
        "the alias should have been substituted away, got {got}"
    );
    assert!(
        got.contains("\"a\""),
        "the key should read column a, got {got}"
    );
}

#[test]
fn a_real_column_is_left_alone_beside_an_alias_of_the_same_name() {
    // sqlite3: 2  1  3 -- `b+1` is the *table's* b, 20,10,30, so the key reads
    // it rather than the alias `a AS b`. Had the alias been substituted the key
    // would be 1,2,3 and the rows would come out in the other order.
    let got = key_source("SELECT a AS b FROM t ORDER BY b+1");
    assert!(
        got.contains("\"b\""),
        "the key should read column b, got {got}"
    );
    assert!(
        !got.contains("\"a\""),
        "the alias should not have been substituted, got {got}"
    );
}

#[test]
fn a_real_column_is_left_alone_beside_a_later_alias_of_the_same_name() {
    // sqlite3: 2|10  1|20  3|30 -- the same rule when the alias is the second
    // result column rather than the first.
    let got = key_source("SELECT b, a AS b FROM t ORDER BY b+1");
    assert!(
        got.contains("\"b\""),
        "the key should read column b, got {got}"
    );
}

#[test]
fn a_star_leaves_the_expression_alone() {
    // sqlite3: 1|b  2|c  3|a -- `x+1` reads the table's x. A star has no
    // written expressions to substitute from, and the names it reports are
    // already table columns, so nothing is substituted. Substituting anyway
    // would look for the name in the one `*` the SELECT list holds and build a
    // call to it, which is the `misuse of aggregate function *()` a star must
    // never turn into.
    let got = key_source("SELECT * FROM t2 ORDER BY x+1");
    assert!(
        got.contains("\"x\""),
        "the key should read column x, got {got}"
    );
    assert!(
        !got.contains("star: true"),
        "the star must not be substituted into the key, got {got}"
    );
}

#[test]
fn a_name_that_is_nothing_is_refused() {
    // sqlite3: "no such column: zzz"
    assert_eq!(
        refuses("SELECT a,b FROM t ORDER BY zzz"),
        "no such column: zzz"
    );
}

#[test]
fn an_ordinal_and_an_output_name_both_read_the_projection() {
    // The two key kinds that read a projected value are the same to `read`,
    // and naming the same column either way has to agree on which one.
    let by_ordinal = keys_of("SELECT b,a FROM t ORDER BY 1");
    let by_name = keys_of("SELECT b,a FROM t ORDER BY b");
    assert_eq!(by_ordinal[0].kind, crate::orderby::KeyKind::Ordinal);
    assert_eq!(by_name[0].kind, crate::orderby::KeyKind::OutputName);
    assert_eq!(by_ordinal[0].at, by_name[0].at);
    assert_eq!(by_ordinal[0].at, Some(0));
}

/// The keys a statement's ORDER BY resolves to, parsed but not run.
///
/// A key is decided by the shape of the statement alone — the result list and
/// the ORDER BY terms — so a query with no FROM is enough to build one, and a
/// test of the resolver does not need a fixture behind it. The FROM is built
/// from the statement's own FROM clause when it has one, so the rule that a
/// real column beats an alias inside an expression is exercised for real rather
/// than stubbed out.
fn keys_of(sql: &str) -> Vec<crate::orderby::Key> {
    resolve(sql).expect("the keys resolve")
}

/// The outcome of resolving a statement's ORDER BY, parsed but not run.
fn resolve(sql: &str) -> crate::error::Result<Vec<crate::orderby::Key>> {
    let crate::parser::Stmt::Select(sel) = crate::parser::parse_one(sql).expect("parses") else {
        panic!("{sql:?} is not a SELECT");
    };
    let crate::parser::SelectBody::Simple { columns, .. } = &sel.body else {
        panic!("{sql:?} is not a simple SELECT");
    };
    // The names are the ones the caller would report: the alias, the column's
    // own name, or a star's expansion. A star is the one result list that does
    // not have one written expression per name.
    let (names, resolved_from): (Vec<String>, Option<crate::join::From>) =
        if columns.len() == 1 && is_star(&columns[0].expr) {
            let from = from_from(sql).expect("a star needs a FROM to expand against");
            let names = from.star.iter().map(|(n, _, _)| n.clone()).collect();
            (names, Some(from))
        } else {
            let names = columns
                .iter()
                .map(|rc| rc.alias.clone().unwrap_or_else(|| name_of(&rc.expr)))
                .collect();
            (names, from_from(sql))
        };
    crate::orderby::resolve_keys(&sel, columns, &names, resolved_from.as_ref())
}

/// The message a statement's ORDER BY is refused with, resolved but not run.
///
/// This is the module's own answer rather than the engine's: the engine resolves
/// every name in a statement before it looks at the ORDER BY at all, so a
/// statement it refuses for an unknown name never reaches the resolver. These
/// are the answers a call site has to produce on its own, and the ones the
/// ordering between the two kinds of error can only be seen in.
fn refuses(sql: &str) -> String {
    match resolve(sql) {
        Ok(keys) => panic!("{sql:?} was accepted, with keys {keys:?}"),
        Err(e) => e.message,
    }
}

/// The FROM a statement's FROM clause resolves to, or `None` when it has none.
fn from_from(sql: &str) -> Option<crate::join::From> {
    let crate::parser::Stmt::Select(sel) = crate::parser::parse_one(sql).expect("parses") else {
        panic!("{sql:?} is not a SELECT");
    };
    let crate::parser::SelectBody::Simple { from, .. } = &sel.body else {
        panic!("{sql:?} is not a simple SELECT");
    };
    if from.is_empty() {
        return None;
    }
    let sources = crate::join::sources_from(&fixture_tables(), from).expect("the FROM resolves");
    Some(crate::join::resolve(sources).expect("the FROM is resolved"))
}

/// The tables the test statements are written against.
///
/// The resolver reads the column names out of a `From` and nothing else, so the
/// catalog these describe is built by hand rather than read back from a
/// connection — whose catalog is private, and which would have to be asked for
/// tables it only holds once a `CREATE TABLE` has run against it.
fn fixture_tables() -> Vec<crate::catalog::Table> {
    fn table(name: &str, columns: &[&str]) -> crate::catalog::Table {
        crate::catalog::Table {
            name: name.to_string(),
            columns: columns
                .iter()
                .map(|c| crate::catalog::Column {
                    name: c.to_string(),
                    declared_type: String::new(),
                    affinity: crate::affinity::Affinity::Blob,
                    not_null: false,
                    default: None,
                    rowid_alias: false,
                })
                .collect(),
            rowid_alias: None,
            unique_sets: Vec::new(),
            without_rowid: false,
            root_page: 0,
        // An ordinary table: no module hosts it.
        virtual_module: None,
        // Declared by hand here, so there is no DDL and no CHECKs to carry.
        checks: Vec::new(),
        }
    }
    vec![
        table("t", &["a", "b"]),
        table("t2", &["x", "y"]),
        table("n", &["x", "y"]),
        table("g", &["a", "b"]),
        table("s", &["a", "b", "c"]),
        table("p", &["a"]),
        table("u", &["id", "k"]),
    ]
}

/// The name a result column with no alias is reported under.
fn name_of(expr: &crate::parser::Expr) -> String {
    match expr {
        crate::parser::Expr::Column { name, .. } => name.clone(),
        other => format!("{other:?}"),
    }
}

/// Whether an expression is a bare `*`, which the parser keeps as a function.
fn is_star(e: &crate::parser::Expr) -> bool {
    matches!(
        e,
        crate::parser::Expr::Function { name, star: true, .. } if name == "*"
    )
}

#[test]
fn an_explicit_alias_outranks_a_bare_column_of_the_same_name() {
    // sqlite3: 1|30  2|20  3|10 -- `SELECT x AS y, y FROM t ORDER BY y` reads
    // the alias x, not the table's y, and the alias is the first result column
    // so the list order alone would say the same thing.
    let keys = keys_of("SELECT x AS y, y FROM t ORDER BY y");
    assert_eq!(keys[0].kind, crate::orderby::KeyKind::OutputName);
    assert_eq!(keys[0].at, Some(0));
}

#[test]
fn an_explicit_alias_outranks_a_bare_column_ahead_of_it() {
    // sqlite3: 30|1 20|2 10|3 -- `SELECT y, x AS y FROM t ORDER BY y` reads the
    // alias x even though the table's y is projected first. The first result
    // column carrying the name is the table's, so this is the case where an
    // author-written name and the list order disagree, and the alias wins.
    let keys = keys_of("SELECT y, x AS y FROM t ORDER BY y");
    assert_eq!(keys[0].kind, crate::orderby::KeyKind::OutputName);
    assert_eq!(keys[0].at, Some(1));
}

#[test]
fn a_bare_column_is_read_when_no_alias_carries_the_name() {
    // sqlite3: 10|3 20|2 30|1 -- nothing in the list is called b by an author,
    // so the name is the table's own b and the projection reads it.
    let keys = keys_of("SELECT y, x, z FROM t ORDER BY y");
    assert_eq!(keys[0].kind, crate::orderby::KeyKind::OutputName);
    assert_eq!(keys[0].at, Some(0));
}

#[test]
fn the_first_of_two_aliases_of_one_name_is_read() {
    // sqlite3: 1 2 3 -- `SELECT x AS y, x*2 AS y FROM t ORDER BY y` reads the
    // first, which is x, so the rows come out 1,2,3.
    let keys = keys_of("SELECT x AS y, x*2 AS y FROM t ORDER BY y");
    assert_eq!(keys[0].kind, crate::orderby::KeyKind::OutputName);
    assert_eq!(keys[0].at, Some(0));
}
