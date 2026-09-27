//! The static aggregate checks, as `sqlite3` 3.53.4 answers them.
//!
//! Every expectation below was recorded by running the statement through the
//! real `sqlite3` on the fixture, and the query and the observed answer are
//! written above each test so a disagreement is visible without re-running the
//! CLI. The fixture is:
//!
//! ```sql
//! CREATE TABLE test1(f1 int, f2 int);
//! INSERT INTO test1(f1,f2) VALUES(11,22),(33,44);
//! CREATE TABLE t(a,b);
//! INSERT INTO t VALUES(1,2),(3,4),(1,9);
//! CREATE TABLE tkt2526(a,b,c PRIMARY KEY);   -- ticket #2526's own table
//! INSERT INTO tkt2526 VALUES('x','y',NULL),('x','z',NULL);
//! ```
//!
//! These tests drive the check directly against a parse tree, so they test
//! the rules and their wording rather than whether `Connection::execute` has
//! been wired up to call them.

use crate::aggcheck::Ctx;
use crate::parser::{parse_one, Expr, SelectBody, Stmt};

/// The check's verdict on a statement, as the error message or `Ok`.
///
/// Driven with no catalog, which is what a caller that has no schema can do.
/// The aggregate rules are all decided from the parse tree and are answered
/// exactly; a name in the statement is invisible to the walk, so a statement
/// that mixes a bad name with an aggregate defect is answered with the
/// aggregate one. That is the one limit [`Names`] documents, and the cases
/// that need a schema drive [`check_with_tables`] instead.
fn check(sql: &str) -> Result<(), String> {
    let stmt: Stmt = parse_one(sql).unwrap_or_else(|e| panic!("{sql:?} did not parse: {e}"));
    Ctx::new().check_without_names(&stmt).map_err(|e| e.message)
}

/// The check's verdict on a statement, with the schema the fixture describes.
///
/// This is the call the connection makes, and the only one that can tell a
/// name that resolves from one that does not — which decides whether a
/// statement mixing a bad name with an aggregate defect reports
/// `no such column` or the aggregate message. The fixture is the one at the
/// top of this file, so `t` is `t(a,b)`, `u` is `u(m,x)` and `tkt2526` is the
/// ticket's own table.
fn check_with_tables(sql: &str) -> Result<(), String> {
    let stmt: Stmt = parse_one(sql).unwrap_or_else(|e| panic!("{sql:?} did not parse: {e}"));
    let tables = fixture_tables();
    Ctx::new().check(&stmt, &tables).map_err(|e| e.message)
}

/// The fixture's tables, as the catalog holds them.
///
/// Built by parsing the fixture's own DDL and handing it to the same
/// `catalog::Table` conversion the connection uses, so the columns here are the
/// ones a real statement would resolve against rather than a hand-written list
/// that could drift from the fixture the oracle answers were recorded against.
fn fixture_tables() -> Vec<crate::catalog::Table> {
    let catalog = crate::catalog::Catalog::new();
    let mut out = Vec::new();
    for sql in [
        "CREATE TABLE test1(f1 int, f2 int)",
        "CREATE TABLE t(a,b)",
        "CREATE TABLE u(m,x)",
        "CREATE TABLE tkt2526(a,b,c PRIMARY KEY)",
    ] {
        let Stmt::CreateTable {
            name,
            columns,
            constraints,
            ..
        } = parse_one(sql).unwrap_or_else(|e| panic!("{sql:?} did not parse: {e}"))
        else {
            panic!("{sql:?} is not a CREATE TABLE");
        };
        out.push(catalog.table_from_create(name.as_str(), &columns, &constraints));
    }
    out
}

/// The check's verdict on one expression, for a CHECK constraint or a partial
/// index's WHERE, which the parser does not keep in the statement.
fn check_expr(sql: &str) -> Result<(), String> {
    let stmt: Stmt = parse_one(sql).unwrap_or_else(|e| panic!("{sql:?} did not parse: {e}"));
    let Stmt::CreateTable { columns, .. } = &stmt else {
        panic!("{sql:?} is not a CREATE TABLE");
    };
    // The CHECK is the only place an aggregate may hide in a CREATE TABLE.
    let mut found: Option<&Expr> = None;
    for c in columns {
        for k in &c.constraints {
            if let crate::parser::Constraint::Check(e) = k {
                found = Some(e);
            }
        }
    }
    let e = found.unwrap_or_else(|| panic!("{sql:?} has no CHECK"));
    Ctx::new().check_no_group(e).map_err(|e| e.message)
}

/// The first result column's expression, for driving a sub-expression.
fn first_column(sql: &str) -> Expr {
    let Stmt::Select(sel) = parse_one(sql).unwrap() else {
        panic!("{sql:?} is not a SELECT");
    };
    let SelectBody::Simple { columns, .. } = &sel.body else {
        panic!("{sql:?} is not a simple SELECT");
    };
    columns[0].expr.clone()
}

// --- 1. arity, select1-2.1 through 2.17 ------------------------------------

/// `SELECT count(f1,f2) FROM test1`
/// → `wrong number of arguments to function count()`
///
/// This is `select1-2.1` and the gap the track names: the old engine answered
/// `misuse of aggregate function count()`, which is a different rule entirely.
#[test]
fn count_with_two_arguments() {
    assert_eq!(
        check("SELECT count(f1,f2) FROM test1"),
        Err("wrong number of arguments to function count()".to_string())
    );
}

/// `SELECT sum(f1,f2) FROM test1` → `wrong number of arguments to function sum()`
#[test]
fn sum_with_two_arguments() {
    assert_eq!(
        check("SELECT sum(f1,f2) FROM test1"),
        Err("wrong number of arguments to function sum()".to_string())
    );
}

/// `SELECT sum() FROM t` → `wrong number of arguments to function sum()`
#[test]
fn sum_with_no_arguments() {
    assert_eq!(
        check("SELECT sum() FROM t"),
        Err("wrong number of arguments to function sum()".to_string())
    );
}

/// `SELECT avg(a,b) FROM t` → `wrong number of arguments to function avg()`
#[test]
fn avg_with_two_arguments() {
    assert_eq!(
        check("SELECT avg(a,b) FROM t"),
        Err("wrong number of arguments to function avg()".to_string())
    );
}

/// `SELECT total(a,b) FROM t` → `wrong number of arguments to function total()`
#[test]
fn total_with_two_arguments() {
    assert_eq!(
        check("SELECT total(a,b) FROM t"),
        Err("wrong number of arguments to function total()".to_string())
    );
}

/// `SELECT avg(*) FROM t` → `wrong number of arguments to function avg()`
///
/// A star is a call with no argument list, so it is a zero-argument `avg`.
#[test]
fn avg_with_a_star() {
    assert_eq!(
        check("SELECT avg(*) FROM t"),
        Err("wrong number of arguments to function avg()".to_string())
    );
}

/// `SELECT total(*) FROM t` → `wrong number of arguments to function total()`
#[test]
fn total_with_a_star() {
    assert_eq!(
        check("SELECT total(*) FROM t"),
        Err("wrong number of arguments to function total()".to_string())
    );
}

/// `SELECT string_agg(a) FROM t`
/// → `wrong number of arguments to function string_agg()`
///
/// `string_agg` requires the separator, so its range is exactly two, where
/// `group_concat`'s is one or two.
#[test]
fn string_agg_needs_two() {
    assert_eq!(
        check("SELECT string_agg(a) FROM t"),
        Err("wrong number of arguments to function string_agg()".to_string())
    );
}

/// `SELECT string_agg() FROM t`
/// → `wrong number of arguments to function string_agg()`
#[test]
fn string_agg_with_no_arguments() {
    assert_eq!(
        check("SELECT string_agg() FROM t"),
        Err("wrong number of arguments to function string_agg()".to_string())
    );
}

/// `SELECT group_concat() FROM t`
/// → `wrong number of arguments to function group_concat()`
#[test]
fn group_concat_with_no_arguments() {
    assert_eq!(
        check("SELECT group_concat() FROM t"),
        Err("wrong number of arguments to function group_concat()".to_string())
    );
}

/// `SELECT count(f1) FROM test1` → no error
#[test]
fn count_with_one_argument_is_fine() {
    assert_eq!(check("SELECT count(f1) FROM test1"), Ok(()));
}

/// `SELECT Count() FROM test1` → no error
///
/// `count` takes zero or one, so a zero-argument `count` is legal and is
/// `count(*)` to SQLite. `select1-2.3`.
#[test]
fn count_with_no_arguments_is_fine() {
    assert_eq!(check("SELECT Count() FROM test1"), Ok(()));
}

/// `SELECT COUNT(*) FROM test1` → no error. `select1-2.4`.
#[test]
fn count_star_is_fine() {
    assert_eq!(check("SELECT COUNT(*) FROM test1"), Ok(()));
}

/// `SELECT group_concat(a,b) FROM t` → no error
#[test]
fn group_concat_with_two_arguments_is_fine() {
    assert_eq!(check("SELECT group_concat(a,b) FROM t"), Ok(()));
}

/// `SELECT string_agg(a,b) FROM t` → no error
#[test]
fn string_agg_with_two_arguments_is_fine() {
    assert_eq!(check("SELECT string_agg(a,b) FROM t"), Ok(()));
}

/// `SELECT min(a,b,a) FROM t` → no error
///
/// `min` and `max` are registered twice in SQLite: as a variadic scalar and as
/// a single-argument aggregate, and the arity picks which. Three arguments is
/// the *scalar*, so it is not a fold and has no arity to get wrong. This is the
/// opposite of `sum(a,b)`, which is an error. Every argument is a real column,
/// so nothing else is reported.
#[test]
fn min_with_three_arguments_is_a_scalar_call() {
    assert_eq!(check("SELECT min(a,b,a) FROM t"), Ok(()));
}

/// `SELECT max(a,b) FROM t` → no error, for the same reason
#[test]
fn max_with_two_arguments_is_a_scalar_call() {
    assert_eq!(check("SELECT max(a,b) FROM t"), Ok(()));
}

// --- 2. a bare column beside an aggregate is accepted -----------------------

/// `SELECT a, b, count(*) FROM t GROUP BY a+0` → no error
///
/// `b` is not in the GROUP BY and SQLite accepts it, taking the group's first
/// row. This is the opposite of what a strict implementation does, and it is
/// worth a test that says so.
#[test]
fn a_bare_column_beside_an_aggregate_is_accepted() {
    assert_eq!(check("SELECT a, b, count(*) FROM t GROUP BY a+0"), Ok(()));
}

/// `SELECT b, count(*) FROM t GROUP BY a` → no error
#[test]
fn a_bare_column_not_in_the_group_by_is_accepted() {
    assert_eq!(check("SELECT b, count(*) FROM t GROUP BY a"), Ok(()));
}

/// `SELECT a, b, count(*) FROM t GROUP BY a+0 HAVING b>0` → no error
///
/// A bare column in a HAVING resolves against the group too, so this is as
/// legal as the same column in the result list.
#[test]
fn a_bare_column_in_a_having_is_accepted() {
    assert_eq!(
        check("SELECT a, b, count(*) FROM t GROUP BY a+0 HAVING b>0"),
        Ok(())
    );
}

/// `SELECT min(a) AS m FROM t GROUP BY m`
/// → `aggregate functions are not allowed in the GROUP BY clause`
///
/// The bare-column case above is legal; the *aggregate* one is not, and this is
/// the message. SQLite does not quote the function name, so neither does this.
#[test]
fn an_aggregate_in_the_group_by_is_refused() {
    assert_eq!(
        check("SELECT min(a) AS m FROM t GROUP BY m"),
        Err("aggregate functions are not allowed in the GROUP BY clause".to_string())
    );
}

// --- 3. ticket #2526: the aliased aggregate -------------------------------

/// `SELECT min(f1) AS m FROM test1 GROUP BY f1 HAVING max(m+5)<10`
/// → `misuse of aliased aggregate m`
///
/// This is `select1-2.21` and the second half of the gap. The alias stands for
/// an aggregate, and an aggregate may not take it as an argument.
#[test]
fn the_ticket_2526_case() {
    assert_eq!(
        check("SELECT min(f1) AS m FROM test1 GROUP BY f1 HAVING max(m+5)<10"),
        Err("misuse of aliased aggregate m".to_string())
    );
}

/// `SELECT coalesce(min(f1)+5,11) AS m FROM test1 GROUP BY f1 HAVING max(m+5)<10`
/// → `misuse of aliased aggregate m`
///
/// `select1-2.22`. The alias wraps the aggregate, and it is still an alias
/// standing for one.
#[test]
fn the_ticket_2526_case_through_coalesce() {
    assert_eq!(
        check("SELECT coalesce(min(f1)+5,11) AS m FROM test1 GROUP BY f1 HAVING max(m+5)<10"),
        Err("misuse of aliased aggregate m".to_string())
    );
}

/// `SELECT count(a) AS cn FROM tkt2526 GROUP BY a HAVING cn<max(cn)`
/// → `misuse of aliased aggregate cn`
///
/// `select1-2.23`, the ticket's own table. The alias appears on both sides, and
/// only the one inside `max` is the misuse.
#[test]
fn the_ticket_2526_table() {
    assert_eq!(
        check("SELECT count(a) AS cn FROM tkt2526 GROUP BY a HAVING cn<max(cn)"),
        Err("misuse of aliased aggregate cn".to_string())
    );
}

/// `SELECT min(a) AS m FROM t GROUP BY a HAVING max(m)<1`
/// → `misuse of aliased aggregate m`
///
/// The form the track's own specification writes.
#[test]
fn an_aliased_aggregate_as_an_argument() {
    assert_eq!(
        check("SELECT min(a) AS m FROM t GROUP BY a HAVING max(m)<1"),
        Err("misuse of aliased aggregate m".to_string())
    );
}

/// `SELECT min(a) AS m FROM t GROUP BY a HAVING sum(m)<1`
/// → `misuse of aliased aggregate m`
///
/// Every aggregate is checked, not just `max`. Each is given the argument count
/// it actually accepts, since the arity check would otherwise come first.
#[test]
fn the_alias_rule_covers_every_aggregate() {
    for (agg, sql) in [
        (
            "sum",
            "SELECT min(a) AS m FROM t GROUP BY a HAVING sum(m)<1",
        ),
        (
            "avg",
            "SELECT min(a) AS m FROM t GROUP BY a HAVING avg(m)<1",
        ),
        (
            "total",
            "SELECT min(a) AS m FROM t GROUP BY a HAVING total(m)<1",
        ),
        (
            "min",
            "SELECT min(a) AS m FROM t GROUP BY a HAVING min(m)<1",
        ),
        (
            "group_concat",
            "SELECT min(a) AS m FROM t GROUP BY a HAVING group_concat(m)<1",
        ),
        (
            "string_agg",
            "SELECT min(a) AS m FROM t GROUP BY a HAVING string_agg(m,'x')<1",
        ),
    ] {
        assert_eq!(
            check(sql),
            Err("misuse of aliased aggregate m".to_string()),
            "for {agg}"
        );
    }
}

/// `SELECT min(a) AS m FROM t GROUP BY a HAVING count(DISTINCT m)>0`
/// → `misuse of aliased aggregate m`
///
/// The DISTINCT shape reaches the alias check too, because the argument list is
/// otherwise legal.
#[test]
fn the_alias_rule_with_distinct() {
    assert_eq!(
        check("SELECT min(a) AS m FROM t GROUP BY a HAVING count(DISTINCT m)>0"),
        Err("misuse of aliased aggregate m".to_string())
    );
}

/// `SELECT min(a) AS m FROM t GROUP BY a ORDER BY max(m)`
/// → `misuse of aliased aggregate m`
///
/// An ORDER BY resolves result aliases too, so the rule holds there as well.
#[test]
fn the_alias_rule_in_an_order_by() {
    assert_eq!(
        check("SELECT min(a) AS m FROM t GROUP BY a ORDER BY max(m)"),
        Err("misuse of aliased aggregate m".to_string())
    );
}

/// `SELECT min(a) AS m FROM t WHERE max(m)<1`
/// → `misuse of aliased aggregate m`
///
/// Even in a WHERE, where the `max` is itself a misuse, the alias check is
/// reached first. A result alias is in scope for the whole SELECT.
#[test]
fn the_alias_rule_in_a_where() {
    assert_eq!(
        check("SELECT min(a) AS m FROM t WHERE max(m)<1"),
        Err("misuse of aliased aggregate m".to_string())
    );
}

/// `SELECT min(a) AS m FROM t GROUP BY a HAVING abs(max(m))<1`
/// → `misuse of aliased aggregate m`
///
/// The alias is substituted into the argument list before the list is walked,
/// so being nested inside `abs` does not hide it.
#[test]
fn the_alias_rule_reaches_inside_another_call() {
    assert_eq!(
        check("SELECT min(a) AS m FROM t GROUP BY a HAVING abs(max(m))<1"),
        Err("misuse of aliased aggregate m".to_string())
    );
}

/// `SELECT a AS m FROM t GROUP BY a HAVING max(m)<1` → no error
///
/// The alias names a column, not an aggregate, so there is nothing to misuse.
#[test]
fn an_alias_that_is_not_an_aggregate_is_fine() {
    assert_eq!(
        check("SELECT a AS m FROM t GROUP BY a HAVING max(m)<1"),
        Ok(())
    );
}

/// `SELECT min(a) AS m FROM t GROUP BY a HAVING m<1` → no error
///
/// The alias on its own is a legal reference in a HAVING; SQLite substitutes
/// the aggregate's value. Only using it as an *argument* of another aggregate
/// is the misuse.
#[test]
fn an_aliased_aggregate_on_its_own_is_fine() {
    assert_eq!(
        check("SELECT min(a) AS m FROM t GROUP BY a HAVING m<1"),
        Ok(())
    );
}

/// `SELECT a+1 AS m FROM t GROUP BY a HAVING max(m)<1` → no error
#[test]
fn a_non_aggregate_expression_alias_is_fine() {
    assert_eq!(
        check("SELECT a+1 AS m FROM t GROUP BY a HAVING max(m)<1"),
        Ok(())
    );
}

/// `SELECT min(a) AS m, min(b) AS n FROM t GROUP BY a HAVING max(m)<0 AND max(n)<0`
/// → `misuse of aliased aggregate m`
///
/// A failed walk stops at the first bad node, so the left one is reported.
#[test]
fn the_leftmost_aliased_aggregate_is_reported() {
    assert_eq!(
        check("SELECT min(a) AS m, min(b) AS n FROM t GROUP BY a HAVING max(m)<0 AND max(n)<0"),
        Err("misuse of aliased aggregate m".to_string())
    );
}

/// `SELECT min(a) AS m, min(b) AS n FROM t GROUP BY a HAVING max(n)<0 AND max(m)<0`
/// → `misuse of aliased aggregate n`
///
/// The mirror of the above, which shows the order is the walk's and not the
/// alias list's.
#[test]
fn the_alias_reported_follows_the_walk() {
    assert_eq!(
        check("SELECT min(a) AS m, min(b) AS n FROM t GROUP BY a HAVING max(n)<0 AND max(m)<0"),
        Err("misuse of aliased aggregate n".to_string())
    );
}

// --- 4. aggregates in WHERE, and the two misuse wordings -------------------

/// `SELECT f1 FROM test1 WHERE count(*)>0`
/// → `misuse of aggregate function count()`
#[test]
fn an_aggregate_in_a_where_with_nothing_in_scope() {
    assert_eq!(
        check("SELECT f1 FROM test1 WHERE count(*)>0"),
        Err("misuse of aggregate function count()".to_string())
    );
}

/// `SELECT b, count(*) FROM t WHERE count(*)>0` → `misuse of aggregate: count()`
///
/// With an aggregate in the result columns the shorter wording applies. The two
/// messages are a flag in SQLite's name resolver, not a difference in the query,
/// and the suite compares the text exactly.
#[test]
fn an_aggregate_in_a_where_with_one_in_scope() {
    assert_eq!(
        check("SELECT b, count(*) FROM t WHERE count(*)>0"),
        Err("misuse of aggregate: count()".to_string())
    );
}

/// `SELECT min(a) FROM t WHERE min(a)>0` → `misuse of aggregate: min()`
#[test]
fn a_grouped_query_shortens_the_where_misuse() {
    assert_eq!(
        check("SELECT min(a) FROM t WHERE min(a)>0"),
        Err("misuse of aggregate: min()".to_string())
    );
}

/// `SELECT b FROM t ORDER BY count(*)` → `misuse of aggregate: count()`
///
/// An ORDER BY on a query with no aggregate has no group to fold over. The
/// shorter wording is used even though nothing is in scope, which is a quirk of
/// where SQLite raises this one.
#[test]
fn an_aggregate_in_an_order_by_with_no_group() {
    assert_eq!(
        check("SELECT b FROM t ORDER BY count(*)"),
        Err("misuse of aggregate: count()".to_string())
    );
}

/// `SELECT 1 FROM t WHERE min(a)>0`
/// → `misuse of aggregate function min()`
#[test]
fn a_where_on_a_constant_projection() {
    assert_eq!(
        check("SELECT 1 FROM t WHERE min(a)>0"),
        Err("misuse of aggregate function min()".to_string())
    );
}

/// `SELECT 1 FROM t ORDER BY count(*)` → `misuse of aggregate: count()`
#[test]
fn an_order_by_on_a_constant_projection() {
    assert_eq!(
        check("SELECT 1 FROM t ORDER BY count(*)"),
        Err("misuse of aggregate: count()".to_string())
    );
}

/// `SELECT b FROM t WHERE 1+min(a)>0`
/// → `misuse of aggregate function min()`
///
/// The aggregate is under an operator, and is still named by its own name.
#[test]
fn an_aggregate_under_an_operator_in_a_where() {
    assert_eq!(
        check("SELECT b FROM t WHERE 1+min(a)>0"),
        Err("misuse of aggregate function min()".to_string())
    );
}

/// `SELECT b FROM t WHERE min(*)>0`
/// → `wrong number of arguments to function min()`
///
/// The arity check precedes the scope check, so a star on a one-argument
/// aggregate is an arity error rather than a misuse.
#[test]
fn arity_beats_scope() {
    assert_eq!(
        check("SELECT b FROM t WHERE min(*)>0"),
        Err("wrong number of arguments to function min()".to_string())
    );
}

/// `SELECT min(a) AS m FROM t GROUP BY a HAVING sum(m,1)<1`
/// → `wrong number of arguments to function sum()`
///
/// The outer call's arity is decided before its arguments are walked, so it
/// wins over the alias misuse the arguments contain.
#[test]
fn arity_beats_the_alias_rule() {
    assert_eq!(
        check("SELECT min(a) AS m FROM t GROUP BY a HAVING sum(m,1)<1"),
        Err("wrong number of arguments to function sum()".to_string())
    );
}

// --- 5. an aggregate inside another aggregate -----------------------------

/// `SELECT SUM(min(f1)) FROM test1`
/// → `misuse of aggregate function min()`
///
/// `select1-2.20`. The inner aggregate is the one named.
#[test]
fn an_aggregate_inside_another() {
    assert_eq!(
        check("SELECT SUM(min(f1)) FROM test1"),
        Err("misuse of aggregate function min()".to_string())
    );
}

/// `SELECT SUM(COUNT(f1)) FROM test1`
/// → `misuse of aggregate function COUNT()`
///
/// The name is quoted as written, so the uppercase spelling survives.
#[test]
fn an_aggregate_inside_another_keeps_its_case() {
    assert_eq!(
        check("SELECT SUM(COUNT(f1)) FROM test1"),
        Err("misuse of aggregate function COUNT()".to_string())
    );
}

/// `SELECT MIN(SUM(f1)) FROM test1`
/// → `misuse of aggregate function SUM()`
#[test]
fn the_outer_aggregate_is_never_the_one_named() {
    assert_eq!(
        check("SELECT MIN(SUM(f1)) FROM test1"),
        Err("misuse of aggregate function SUM()".to_string())
    );
}

/// `SELECT sum(min(max(a))) FROM t` → `misuse of aggregate function max()`
///
/// Three deep: the innermost is the one reported.
#[test]
fn three_aggregates_deep_names_the_innermost() {
    assert_eq!(
        check("SELECT sum(min(max(a))) FROM t"),
        Err("misuse of aggregate function max()".to_string())
    );
}

/// `SELECT SUM(min(f1,f2)) FROM test1` → no error
///
/// `select1-2.19`. With two arguments `min` is the *scalar* form, so it is not
/// a fold and `sum` of it is an ordinary scalar sum.
#[test]
fn a_scalar_min_inside_sum_is_fine() {
    assert_eq!(check("SELECT SUM(min(f1,f2)) FROM test1"), Ok(()));
}

/// `SELECT 1 FROM t WHERE sum(count(*))>0`
/// → `misuse of aggregate function count()`
///
/// The nesting is found inside the outer call's arguments, before the outer
/// call's own scope check.
#[test]
fn nesting_is_found_inside_a_where() {
    assert_eq!(
        check("SELECT 1 FROM t WHERE sum(count(*))>0"),
        Err("misuse of aggregate function count()".to_string())
    );
}

// --- 6. count(*) and count(DISTINCT x) shapes -----------------------------

/// `SELECT count(DISTINCT) FROM t`
/// → `DISTINCT aggregates must have exactly one argument`
///
/// The one shape with its own message, and the one the old engine accepted as
/// `count(*)`.
#[test]
fn count_distinct_with_no_arguments() {
    assert_eq!(
        check("SELECT count(DISTINCT) FROM t"),
        Err("DISTINCT aggregates must have exactly one argument".to_string())
    );
}

/// `SELECT count(DISTINCT a) FROM t` → no error
#[test]
fn count_distinct_with_one_argument_is_fine() {
    assert_eq!(check("SELECT count(DISTINCT a) FROM t"), Ok(()));
}

/// `SELECT count(DISTINCT a,b) FROM t`
/// → `wrong number of arguments to function count()`
///
/// The arity check is name resolution; the DISTINCT check is raised later by
/// code generation, so the arity error wins where both apply.
#[test]
fn count_distinct_with_two_arguments_is_an_arity_error() {
    assert_eq!(
        check("SELECT count(DISTINCT a,b) FROM t"),
        Err("wrong number of arguments to function count()".to_string())
    );
}

/// `SELECT group_concat(DISTINCT a,b) FROM t`
/// → `DISTINCT aggregates must have exactly one argument`
///
/// `group_concat` at two arguments is legal, so this is the one shape that
/// reaches the DISTINCT message rather than the arity one.
#[test]
fn group_concat_distinct_with_two_arguments() {
    assert_eq!(
        check("SELECT group_concat(DISTINCT a,b) FROM t"),
        Err("DISTINCT aggregates must have exactly one argument".to_string())
    );
}

/// `SELECT string_agg(DISTINCT a,b) FROM t`
/// → `DISTINCT aggregates must have exactly one argument`
#[test]
fn string_agg_distinct_with_two_arguments() {
    assert_eq!(
        check("SELECT string_agg(DISTINCT a,b) FROM t"),
        Err("DISTINCT aggregates must have exactly one argument".to_string())
    );
}

/// `SELECT sum(DISTINCT a,b) FROM t`
/// → `wrong number of arguments to function sum()`
#[test]
fn sum_distinct_with_two_arguments_is_an_arity_error() {
    assert_eq!(
        check("SELECT sum(DISTINCT a,b) FROM t"),
        Err("wrong number of arguments to function sum()".to_string())
    );
}

/// `SELECT min(DISTINCT a,b) FROM t` → no error
///
/// Two arguments makes `min` the scalar, so there is no DISTINCT to complain
/// about.
#[test]
fn a_scalar_min_with_distinct_is_fine() {
    assert_eq!(check("SELECT min(DISTINCT a,b) FROM t"), Ok(()));
}

// --- 7. aggregates in a CHECK constraint ----------------------------------

/// `CREATE TABLE q(a CHECK(count(a)>0))`
/// → `misuse of aggregate function count()`
///
/// A CHECK has no group to fold over.
#[test]
fn an_aggregate_in_a_column_check() {
    assert_eq!(
        check_expr("CREATE TABLE q(a CHECK(count(a)>0))"),
        Err("misuse of aggregate function count()".to_string())
    );
}

/// `CREATE TABLE q(a CHECK(sum(a)>0))` → `misuse of aggregate function sum()`
#[test]
fn a_sum_in_a_column_check() {
    assert_eq!(
        check_expr("CREATE TABLE q(a CHECK(sum(a)>0))"),
        Err("misuse of aggregate function sum()".to_string())
    );
}

/// `CREATE TABLE q(a CHECK(count(*)>0))` → `misuse of aggregate function count()`
#[test]
fn a_count_star_in_a_column_check() {
    assert_eq!(
        check_expr("CREATE TABLE q(a CHECK(count(*)>0))"),
        Err("misuse of aggregate function count()".to_string())
    );
}

/// `CREATE TABLE q(a CHECK(min(a)>0))` → `misuse of aggregate function min()`
#[test]
fn a_min_in_a_column_check() {
    assert_eq!(
        check_expr("CREATE TABLE q(a CHECK(min(a)>0))"),
        Err("misuse of aggregate function min()".to_string())
    );
}

/// `CREATE TABLE q(a CHECK(sum(*)>0))`
/// → `wrong number of arguments to function sum()`
///
/// A CHECK gets the arity check too, and the arity check comes first.
#[test]
fn a_star_on_a_one_argument_aggregate_in_a_check() {
    assert_eq!(
        check_expr("CREATE TABLE q(a CHECK(sum(*)>0))"),
        Err("wrong number of arguments to function sum()".to_string())
    );
}

/// `CREATE TABLE q(a,b CHECK(count(a,b)>0))`
/// → `wrong number of arguments to function count()`
///
/// Both arguments are real columns of the table, so the arity check is what
/// fires. With only `a` declared, `sqlite3` reports `no such column: b`
/// instead, because the columns are resolved before the arity of the call they
/// sit in — a static check cannot see the difference, so this case has to name
/// columns that exist.
#[test]
fn an_arity_error_in_a_check() {
    assert_eq!(
        check_expr("CREATE TABLE q(a,b CHECK(count(a,b)>0))"),
        Err("wrong number of arguments to function count()".to_string())
    );
}

/// `CREATE TABLE q(a,b CHECK(string_agg(a)>0))`
/// → `wrong number of arguments to function string_agg()`
#[test]
fn a_short_string_agg_in_a_check() {
    assert_eq!(
        check_expr("CREATE TABLE q(a,b CHECK(string_agg(a)>0))"),
        Err("wrong number of arguments to function string_agg()".to_string())
    );
}

/// `CREATE TABLE q(a CHECK(sum(min(a))>0))`
/// → `misuse of aggregate function min()`
#[test]
fn nesting_inside_a_check() {
    assert_eq!(
        check_expr("CREATE TABLE q(a CHECK(sum(min(a))>0))"),
        Err("misuse of aggregate function min()".to_string())
    );
}

/// `CREATE TABLE q(a CHECK(abs(a)>0))` → no error
#[test]
fn an_ordinary_call_in_a_check_is_fine() {
    assert_eq!(check_expr("CREATE TABLE q(a CHECK(abs(a)>0))"), Ok(()));
}

// --- 8. a partial index's WHERE -------------------------------------------

/// A partial index's WHERE with an aggregate:
/// `CREATE INDEX i ON t(a) WHERE count(*)>0`
/// → `misuse of aggregate function count()`
///
/// Driven through [`check_no_group`] on the expression itself, which is the
/// entry point `CREATE INDEX` will use. The parser reads the expression and
/// drops it — `Stmt::CreateIndex` has nowhere to keep it — so the index's WHERE
/// cannot be checked from a statement today, and these two tests are the
/// oracle's answer for the rule rather than a claim that the path is wired.
/// The fixture is `t(a,b)` with rows `(1,2),(3,4),(1,9)`.
fn partial_index_where(expr: &str) -> Result<(), String> {
    let e = first_column(&format!("SELECT {expr}"));
    Ctx::new().check_no_group(&e).map_err(|e| e.message)
}

/// `count(*)>0` as a partial index's WHERE
/// → `misuse of aggregate function count()`
#[test]
fn an_aggregate_in_a_partial_index_where() {
    assert_eq!(
        partial_index_where("count(*)>0"),
        Err("misuse of aggregate function count()".to_string())
    );
}

/// `count(a,b)>0` as a partial index's WHERE
/// → `wrong number of arguments to function count()`
#[test]
fn an_arity_error_in_a_partial_index_where() {
    assert_eq!(
        partial_index_where("count(a,b)>0"),
        Err("wrong number of arguments to function count()".to_string())
    );
}

/// `min()>0` as a partial index's WHERE
/// → `wrong number of arguments to function min()`
#[test]
fn a_zero_argument_min_in_a_partial_index_where() {
    assert_eq!(
        partial_index_where("min()>0"),
        Err("wrong number of arguments to function min()".to_string())
    );
}

/// `string_agg(a)>0` as a partial index's WHERE
/// → `wrong number of arguments to function string_agg()`
#[test]
fn a_short_string_agg_in_a_partial_index_where() {
    assert_eq!(
        partial_index_where("string_agg(a)>0"),
        Err("wrong number of arguments to function string_agg()".to_string())
    );
}

/// `min(a)>0` as a partial index's WHERE
/// → `misuse of aggregate function min()`
#[test]
fn a_min_in_a_partial_index_where() {
    assert_eq!(
        partial_index_where("min(a)>0"),
        Err("misuse of aggregate function min()".to_string())
    );
}

// --- 9. DML ---------------------------------------------------------------

/// `UPDATE t SET a=count(*)` → `misuse of aggregate function count()`
#[test]
fn an_aggregate_in_an_update_set() {
    assert_eq!(
        check("UPDATE t SET a=count(*)"),
        Err("misuse of aggregate function count()".to_string())
    );
}

/// `UPDATE t SET a=a WHERE count(*)>0`
/// → `misuse of aggregate function count()`
#[test]
fn an_aggregate_in_an_update_where() {
    assert_eq!(
        check("UPDATE t SET a=a WHERE count(*)>0"),
        Err("misuse of aggregate function count()".to_string())
    );
}

/// `DELETE FROM t WHERE count(*)>0` → `misuse of aggregate function count()`
#[test]
fn an_aggregate_in_a_delete_where() {
    assert_eq!(
        check("DELETE FROM t WHERE count(*)>0"),
        Err("misuse of aggregate function count()".to_string())
    );
}

/// `INSERT INTO t VALUES(count(*),2)`
/// → `misuse of aggregate function count()`
#[test]
fn an_aggregate_in_an_insert_values() {
    assert_eq!(
        check("INSERT INTO t VALUES(count(*),2)"),
        Err("misuse of aggregate function count()".to_string())
    );
}

// --- 10. a HAVING that needs a group -------------------------------------

/// `SELECT 1 FROM t HAVING 1` → `HAVING clause on a non-aggregate query`
#[test]
fn a_having_on_a_non_aggregate_query() {
    assert_eq!(
        check("SELECT 1 FROM t HAVING 1"),
        Err("HAVING clause on a non-aggregate query".to_string())
    );
}

/// `SELECT 1 FROM t HAVING count(*)>0`
/// → `HAVING clause on a non-aggregate query`
///
/// A HAVING does not make a query an aggregate query. The count it names is
/// reached only after the test, so the test wins.
#[test]
fn a_having_does_not_make_a_query_aggregate() {
    assert_eq!(
        check("SELECT 1 FROM t HAVING count(*)>0"),
        Err("HAVING clause on a non-aggregate query".to_string())
    );
}

/// `SELECT 1 FROM t GROUP BY a HAVING max(b)>0` → no error
#[test]
fn a_having_on_a_group_by_is_fine() {
    assert_eq!(check("SELECT 1 FROM t GROUP BY a HAVING max(b)>0"), Ok(()));
}

/// `SELECT count(*) FROM t GROUP BY a HAVING count(*)>2` → no error
#[test]
fn a_having_reusing_a_result_aggregate_is_fine() {
    assert_eq!(
        check("SELECT count(*) FROM t GROUP BY a HAVING count(*)>2"),
        Ok(())
    );
}

// --- 11. the sub-expression entry point -----------------------------------

/// `count(*)` on its own, checked as a CHECK would be:
/// → `misuse of aggregate function count()`
#[test]
fn the_expression_entry_point_matches_a_check() {
    let e = first_column("SELECT count(*) FROM t");
    assert_eq!(
        Ctx::new().check_no_group(&e).map_err(|e| e.message),
        Err("misuse of aggregate function count()".to_string())
    );
}

/// A non-aggregate expression through the same entry point is not an error.
#[test]
fn the_expression_entry_point_accepts_a_non_aggregate() {
    let e = first_column("SELECT abs(a) FROM t");
    assert_eq!(Ctx::new().check_no_group(&e), Ok(()));
}

// --- 12. a zero-argument min/max -------------------------------------------

/// `SELECT min() FROM t` → `wrong number of arguments to function min()`
///
/// Zero arguments matches neither registration: the scalar `min` needs one or
/// more, and the aggregate takes exactly one. A parenthesised empty list is
/// therefore a zero-argument `min`, the same defect a bare `min(*)` is.
#[test]
fn min_with_no_arguments() {
    assert_eq!(
        check("SELECT min() FROM t"),
        Err("wrong number of arguments to function min()".to_string())
    );
}

/// `SELECT max() FROM t` → `wrong number of arguments to function max()`
#[test]
fn max_with_no_arguments() {
    assert_eq!(
        check("SELECT max() FROM t"),
        Err("wrong number of arguments to function max()".to_string())
    );
}

/// `SELECT min(DISTINCT) FROM t`
/// → `wrong number of arguments to function min()`
///
/// The arity is decided before anything about DISTINCT, so this is the arity
/// error and not `DISTINCT aggregates must have exactly one argument`.
#[test]
fn min_distinct_with_no_arguments() {
    assert_eq!(
        check("SELECT min(DISTINCT) FROM t"),
        Err("wrong number of arguments to function min()".to_string())
    );
}

/// `SELECT max(DISTINCT) FROM t`
/// → `wrong number of arguments to function max()`
#[test]
fn max_distinct_with_no_arguments() {
    assert_eq!(
        check("SELECT max(DISTINCT) FROM t"),
        Err("wrong number of arguments to function max()".to_string())
    );
}

/// `SELECT 1 FROM t WHERE min()>0`
/// → `wrong number of arguments to function min()`
///
/// The arity check precedes the scope check, so it wins even where there is no
/// group to fold over.
#[test]
fn a_zero_argument_min_in_a_where() {
    assert_eq!(
        check("SELECT 1 FROM t WHERE min()>0"),
        Err("wrong number of arguments to function min()".to_string())
    );
}

/// `CREATE TABLE q(a CHECK(min(DISTINCT)>0))`
/// → `wrong number of arguments to function min()`
#[test]
fn a_zero_argument_min_in_a_check() {
    assert_eq!(
        check_expr("CREATE TABLE q(a CHECK(min(DISTINCT)>0))"),
        Err("wrong number of arguments to function min()".to_string())
    );
}

/// `SELECT 1 FROM t GROUP BY a HAVING min()`
/// → `wrong number of arguments to function min()`
#[test]
fn a_zero_argument_min_in_a_having() {
    assert_eq!(
        check("SELECT 1 FROM t GROUP BY a HAVING min()"),
        Err("wrong number of arguments to function min()".to_string())
    );
}

/// `SELECT sum(min()) FROM t`
/// → `wrong number of arguments to function min()`
///
/// The inner call's own shape is decided before the outer call is found to be
/// a misuse, so the arity wins over the nesting.
#[test]
fn a_zero_argument_min_inside_sum() {
    assert_eq!(
        check("SELECT sum(min()) FROM t"),
        Err("wrong number of arguments to function min()".to_string())
    );
}

/// `SELECT min(a,b) FROM t` and friends → no error
///
/// Two arguments and up is the *scalar* `min`, which is a call on one row and
/// so has no arity to get wrong. This is the boundary that makes zero
/// arguments an arity error rather than a scalar call.
#[test]
fn the_scalar_min_starts_at_two_arguments() {
    for sql in [
        "SELECT min(a,b) FROM t",
        "SELECT min(a,b,a,b) FROM t",
        "SELECT min(a,b,c,d) FROM t",
        "SELECT max(a,b) FROM t",
        "SELECT min(DISTINCT a,b) FROM t",
    ] {
        assert_eq!(check(sql), Ok(()), "for {sql}");
    }
}

// --- 13. the DISTINCT check comes after the scope check --------------------

/// `SELECT 1 FROM t WHERE count(DISTINCT)>0`
/// → `misuse of aggregate function count()`
///
/// `DISTINCT aggregates must have exactly one argument` is raised by code
/// generation, so a place with no group reports the scope misuse instead. This
/// is the answer the suite's `count(DISTINCT)` cases want.
#[test]
fn count_distinct_with_no_arguments_in_a_where() {
    assert_eq!(
        check("SELECT 1 FROM t WHERE count(DISTINCT)>0"),
        Err("misuse of aggregate function count()".to_string())
    );
}

/// `SELECT 1 FROM t WHERE string_agg(DISTINCT a,b)>0`
/// → `misuse of aggregate function string_agg()`
#[test]
fn string_agg_distinct_in_a_where() {
    assert_eq!(
        check("SELECT 1 FROM t WHERE string_agg(DISTINCT a,b)>0"),
        Err("misuse of aggregate function string_agg()".to_string())
    );
}

/// `SELECT 1 FROM t WHERE group_concat(DISTINCT a,b)>0`
/// → `misuse of aggregate function group_concat()`
#[test]
fn group_concat_distinct_in_a_where() {
    assert_eq!(
        check("SELECT 1 FROM t WHERE group_concat(DISTINCT a,b)>0"),
        Err("misuse of aggregate function group_concat()".to_string())
    );
}

/// `CREATE TABLE q(a CHECK(count(DISTINCT)>0))`
/// → `misuse of aggregate function count()`
#[test]
fn count_distinct_in_a_check() {
    assert_eq!(
        check_expr("CREATE TABLE q(a CHECK(count(DISTINCT)>0))"),
        Err("misuse of aggregate function count()".to_string())
    );
}

/// `CREATE TABLE q(a,b CHECK(string_agg(DISTINCT a,b)>0))`
/// → `misuse of aggregate function string_agg()`
#[test]
fn string_agg_distinct_in_a_check() {
    assert_eq!(
        check_expr("CREATE TABLE q(a,b CHECK(string_agg(DISTINCT a,b)>0))"),
        Err("misuse of aggregate function string_agg()".to_string())
    );
}

/// `CREATE TABLE q(a,b CHECK(group_concat(DISTINCT a,b)>0))`
/// → `misuse of aggregate function group_concat()`
#[test]
fn group_concat_distinct_in_a_check() {
    assert_eq!(
        check_expr("CREATE TABLE q(a,b CHECK(group_concat(DISTINCT a,b)>0))"),
        Err("misuse of aggregate function group_concat()".to_string())
    );
}

/// `UPDATE t SET a=a WHERE count(DISTINCT)>0`
/// → `misuse of aggregate function count()`
#[test]
fn count_distinct_in_an_update_where() {
    assert_eq!(
        check("UPDATE t SET a=a WHERE count(DISTINCT)>0"),
        Err("misuse of aggregate function count()".to_string())
    );
}

/// `DELETE FROM t WHERE string_agg(DISTINCT a,b)>0`
/// → `misuse of aggregate function string_agg()`
#[test]
fn string_agg_distinct_in_a_delete_where() {
    assert_eq!(
        check("DELETE FROM t WHERE string_agg(DISTINCT a,b)>0"),
        Err("misuse of aggregate function string_agg()".to_string())
    );
}

/// `SELECT 1 FROM t WHERE sum(count(DISTINCT))>0`
/// → `misuse of aggregate function count()`
///
/// The nesting is reached before the DISTINCT check, and a nesting it is:
/// `count(DISTINCT)` is legal in a nested position, so the outer `sum` is
/// what is refused, by the inner name.
#[test]
fn count_distinct_nested_in_a_where() {
    assert_eq!(
        check("SELECT 1 FROM t WHERE sum(count(DISTINCT))>0"),
        Err("misuse of aggregate function count()".to_string())
    );
}

/// `SELECT 1 FROM t GROUP BY count(DISTINCT)`
/// → `aggregate functions are not allowed in the GROUP BY clause`
///
/// A GROUP BY is refused before any code generation, so the DISTINCT message is
/// not reached there at all.
#[test]
fn count_distinct_in_a_group_by() {
    assert_eq!(
        check("SELECT 1 FROM t GROUP BY count(DISTINCT)"),
        Err("aggregate functions are not allowed in the GROUP BY clause".to_string())
    );
}

// --- 14. GROUP BY: where the alias sits decides the message ----------------

/// `SELECT min(a) AS m FROM t GROUP BY max(m)` and its neighbours
/// → `misuse of aliased aggregate m`
///
/// SQLite resolves aliases inside a GROUP BY term, and the substitution
/// returns immediately, so an aggregate on the way to the alias means the
/// alias is reached with `NC_AllowAgg` already clear. These are the shapes the
/// bare-alias test does not reach.
#[test]
fn an_aggregate_on_the_way_to_an_alias() {
    for (sql, want) in [
        (
            "SELECT min(a) AS m FROM t GROUP BY max(m)",
            "misuse of aliased aggregate m",
        ),
        (
            "SELECT min(a) AS m FROM t GROUP BY a+max(m)",
            "misuse of aliased aggregate m",
        ),
        (
            "SELECT min(a) AS m FROM t GROUP BY abs(max(m))",
            "misuse of aliased aggregate m",
        ),
        (
            "SELECT min(a) AS m FROM t GROUP BY a, max(m)",
            "misuse of aliased aggregate m",
        ),
        (
            "SELECT min(a) AS m FROM t GROUP BY m, max(m)",
            "misuse of aliased aggregate m",
        ),
        (
            "SELECT min(a) AS m, min(b) AS n FROM t GROUP BY max(n)",
            "misuse of aliased aggregate n",
        ),
    ] {
        assert_eq!(check(sql), Err(want.to_string()), "for {sql}");
    }
}

/// `SELECT min(a) AS m FROM t GROUP BY m+0` and its neighbours
/// → `misuse of aggregate: min()`
///
/// Here the alias is reached through the term rather than through an
/// aggregate, so it really is substituted, and the aggregate that comes with it
/// is read off the copy. This is the opposite of what the bare `GROUP BY m`
/// form gives, and the difference is the whole shape of the rule.
#[test]
fn an_alias_substituted_into_a_group_by_term() {
    for (sql, want) in [
        (
            "SELECT min(a) AS m FROM t GROUP BY m+0",
            "misuse of aggregate: min()",
        ),
        (
            "SELECT min(a) AS m FROM t GROUP BY a+m",
            "misuse of aggregate: min()",
        ),
        (
            "SELECT min(a) AS m FROM t GROUP BY abs(m)",
            "misuse of aggregate: min()",
        ),
        (
            "SELECT min(a) AS m FROM t GROUP BY m COLLATE nocase",
            "misuse of aggregate: min()",
        ),
        (
            "SELECT min(a) AS m FROM t GROUP BY a, m+0",
            "misuse of aggregate: min()",
        ),
        (
            "SELECT min(a) AS m FROM t GROUP BY (m)+0",
            "misuse of aggregate: min()",
        ),
        (
            "SELECT min(a) AS m FROM t GROUP BY 0+m",
            "misuse of aggregate: min()",
        ),
        (
            "SELECT count(a) AS m FROM t GROUP BY m+0",
            "misuse of aggregate: count()",
        ),
        (
            "SELECT sum(a) AS m FROM t GROUP BY m+0",
            "misuse of aggregate: sum()",
        ),
        (
            "SELECT group_concat(a) AS m FROM t GROUP BY m+0",
            "misuse of aggregate: group_concat()",
        ),
    ] {
        assert_eq!(check(sql), Err(want.to_string()), "for {sql}");
    }
}

/// `SELECT min(a) AS m FROM t GROUP BY m+max(b)`
/// → `aggregate functions are not allowed in the GROUP BY clause`
///
/// A *written* aggregate in the term is the refusal, because substituting the
/// alias leaves that aggregate sitting in the clause. `GROUP BY m+0` has no
/// aggregate of its own and is the substitution; this one has and is not.
#[test]
fn a_written_aggregate_outranks_a_substitution() {
    assert_eq!(
        check("SELECT min(a) AS m FROM t GROUP BY m+max(b)"),
        Err("aggregate functions are not allowed in the GROUP BY clause".to_string())
    );
}

/// `SELECT 1 FROM t GROUP BY m+0, min(a)`
/// → `aggregate functions are not allowed in the GROUP BY clause`
///
/// The `EP_Agg` test is over the finished clause, so it outranks a
/// substitution from an earlier term.
#[test]
fn a_refusal_outranks_an_earlier_substitution() {
    assert_eq!(
        check("SELECT 1 FROM t GROUP BY m+0, min(a)"),
        Err("aggregate functions are not allowed in the GROUP BY clause".to_string())
    );
}

/// `SELECT min(a) AS m FROM t GROUP BY max(m), min()`
/// → `misuse of aliased aggregate m`
///
/// The alias returns immediately, so the later term's arity error is never
/// reached. The mirror, with the terms the other way round, is the arity error,
/// which is what makes this per-term rather than a whole-clause pass.
///
/// Both names in each statement resolve — `m` is the alias, `a` a real column —
/// so nothing here is a `no such column` in disguise. A shape that did need an
/// unresolvable name, such as `GROUP BY max(m), count(a,b)`, is left to name
/// resolution: `count(a,b)` needs `b` to exist, and this module has no schema.
#[test]
fn a_group_by_alias_beats_a_later_arity_error() {
    assert_eq!(
        check("SELECT min(a) AS m FROM t GROUP BY max(m), min()"),
        Err("misuse of aliased aggregate m".to_string())
    );
    assert_eq!(
        check("SELECT min(a) AS m FROM t GROUP BY min(), max(m)"),
        Err("wrong number of arguments to function min()".to_string())
    );
    assert_eq!(
        check("SELECT min(a) AS m FROM t GROUP BY max(m), string_agg(a)"),
        Err("misuse of aliased aggregate m".to_string())
    );
    assert_eq!(
        check("SELECT min(a) AS m FROM t GROUP BY string_agg(a), max(m)"),
        Err("wrong number of arguments to function string_agg()".to_string())
    );
}

/// `SELECT 1 FROM t GROUP BY min()`
/// → `wrong number of arguments to function min()`
///
/// A GROUP BY term is name-resolved like any other expression, so the arity
/// check comes first.
#[test]
fn a_group_by_arity_error() {
    assert_eq!(
        check("SELECT 1 FROM t GROUP BY min()"),
        Err("wrong number of arguments to function min()".to_string())
    );
    assert_eq!(
        check("SELECT 1 FROM t GROUP BY string_agg(a)"),
        Err("wrong number of arguments to function string_agg()".to_string())
    );
}

/// `SELECT min(a) AS m, min(b) AS n FROM t GROUP BY m+n+0`
/// → `misuse of aggregate: min()`
///
/// Either alias can bring its aggregate in, and either is named `min()`.
#[test]
fn a_term_with_two_aggregate_aliases() {
    assert_eq!(
        check("SELECT min(a) AS m, min(b) AS n FROM t GROUP BY m+n+0"),
        Err("misuse of aggregate: min()".to_string())
    );
}

/// An aggregate written into a term with no alias in it at all is the refusal
#[test]
fn a_written_aggregate_with_no_alias() {
    for sql in [
        "SELECT 1 FROM t GROUP BY abs(max(b))",
        "SELECT 1 FROM t GROUP BY max(b)+0",
        "SELECT 1 FROM t GROUP BY 0+max(b)",
        "SELECT 1 FROM t GROUP BY 0+count(*)",
    ] {
        assert_eq!(
            check(sql),
            Err("aggregate functions are not allowed in the GROUP BY clause".to_string()),
            "for {sql}"
        );
    }
}

// --- 15. the aggregates this build registers -------------------------------

/// `SELECT median(a) FROM t` → no error
///
/// `median`, `percentile`, `percentile_cont` and `percentile_disc` are real
/// aggregates in this `sqlite3` build (`testsuite/src/func.c:3485-3494`), so
/// every check applies to them.
#[test]
fn median_and_percentile_are_aggregates() {
    assert_eq!(check("SELECT median(a) FROM t"), Ok(()));
    assert_eq!(check("SELECT percentile(a,0.5) FROM t"), Ok(()));
    assert_eq!(check("SELECT percentile_cont(a,0.5) FROM t"), Ok(()));
    assert_eq!(check("SELECT percentile_disc(a,0.5) FROM t"), Ok(()));
}

/// `SELECT median(a,b) FROM t`
/// → `wrong number of arguments to function median()`
#[test]
fn median_takes_one_argument() {
    assert_eq!(
        check("SELECT median(a,b) FROM t"),
        Err("wrong number of arguments to function median()".to_string())
    );
}

/// `SELECT median() FROM t` → `wrong number of arguments to function median()`
#[test]
fn median_with_no_arguments() {
    assert_eq!(
        check("SELECT median() FROM t"),
        Err("wrong number of arguments to function median()".to_string())
    );
}

/// `SELECT percentile(a) FROM t` and its neighbours, each with the function's
/// own name in the message
#[test]
fn percentile_takes_two_arguments() {
    for (sql, want) in [
        (
            "SELECT percentile(a) FROM t",
            "wrong number of arguments to function percentile()",
        ),
        (
            "SELECT percentile() FROM t",
            "wrong number of arguments to function percentile()",
        ),
        (
            "SELECT percentile_cont(a) FROM t",
            "wrong number of arguments to function percentile_cont()",
        ),
        (
            "SELECT percentile_disc(a) FROM t",
            "wrong number of arguments to function percentile_disc()",
        ),
    ] {
        assert_eq!(check(sql), Err(want.to_string()), "for {sql}");
    }
}

/// `SELECT 1 FROM t WHERE median(a)>0` → `misuse of aggregate function median()`
#[test]
fn median_in_a_where() {
    assert_eq!(
        check("SELECT 1 FROM t WHERE median(a)>0"),
        Err("misuse of aggregate function median()".to_string())
    );
}

/// `SELECT 1 FROM t WHERE percentile(a,b)>0`
/// → `misuse of aggregate function percentile()`
#[test]
fn percentile_in_a_where() {
    assert_eq!(
        check("SELECT 1 FROM t WHERE percentile(a,b)>0"),
        Err("misuse of aggregate function percentile()".to_string())
    );
}

/// `SELECT SUM(median(a)) FROM t` → `misuse of aggregate function median()`
#[test]
fn median_nested_in_sum() {
    assert_eq!(
        check("SELECT SUM(median(a)) FROM t"),
        Err("misuse of aggregate function median()".to_string())
    );
}

/// `SELECT median(a) AS m FROM t GROUP BY a HAVING max(m)<1`
/// → `misuse of aliased aggregate m`
///
/// The alias rule covers these too, which it did not before.
#[test]
fn the_alias_rule_covers_median() {
    assert_eq!(
        check("SELECT median(a) AS m FROM t GROUP BY a HAVING max(m)<1"),
        Err("misuse of aliased aggregate m".to_string())
    );
    assert_eq!(
        check("SELECT median(a) AS m FROM t GROUP BY a ORDER BY max(m)"),
        Err("misuse of aliased aggregate m".to_string())
    );
}

/// `SELECT median(*) FROM t` → `wrong number of arguments to function median()`
#[test]
fn a_star_on_median() {
    assert_eq!(
        check("SELECT median(*) FROM t"),
        Err("wrong number of arguments to function median()".to_string())
    );
    assert_eq!(
        check("SELECT percentile(*) FROM t"),
        Err("wrong number of arguments to function percentile()".to_string())
    );
}

/// `CREATE TABLE q(a CHECK(median(a)>0))`
/// → `misuse of aggregate function median()`
#[test]
fn median_in_a_check() {
    assert_eq!(
        check_expr("CREATE TABLE q(a CHECK(median(a)>0))"),
        Err("misuse of aggregate function median()".to_string())
    );
}

/// `SELECT percentile(DISTINCT a,b) FROM t`
/// → `DISTINCT aggregates must have exactly one argument`
#[test]
fn percentile_distinct_with_two_arguments() {
    assert_eq!(
        check("SELECT percentile(DISTINCT a,b) FROM t"),
        Err("DISTINCT aggregates must have exactly one argument".to_string())
    );
}

/// `product` is not a function in this build, so it is not an aggregate either
///
/// `SELECT product(1,2)` is `no such function: product`, which is the
/// name-resolution answer and not one of the arity messages.
#[test]
fn product_is_not_an_aggregate() {
    assert_eq!(check("SELECT product(a) FROM t"), Ok(()));
    assert_eq!(check("SELECT product(a,b) FROM t"), Ok(()));
}

// --- 16. LIMIT and OFFSET --------------------------------------------------

/// `SELECT 1 FROM t LIMIT count(*)` and its neighbours, each refused
///
/// A LIMIT is resolved with an empty name context, so a fold that names no
/// column is still refused there.
#[test]
fn a_fold_naming_nothing_in_a_limit() {
    for (sql, want) in [
        (
            "SELECT 1 FROM t LIMIT count(*)",
            "misuse of aggregate function count()",
        ),
        (
            "SELECT 1 FROM t LIMIT count(1)",
            "misuse of aggregate function count()",
        ),
        (
            "SELECT 1 FROM t LIMIT count(1+1)",
            "misuse of aggregate function count()",
        ),
        (
            "SELECT 1 FROM t LIMIT min(1)",
            "misuse of aggregate function min()",
        ),
        (
            "SELECT 1 FROM t LIMIT sum(1)",
            "misuse of aggregate function sum()",
        ),
        (
            "SELECT 1 FROM t LIMIT string_agg(1,2)",
            "misuse of aggregate function string_agg()",
        ),
        (
            "SELECT 1 FROM t LIMIT abs(count(*))",
            "misuse of aggregate function count()",
        ),
        (
            "SELECT 1 FROM t LIMIT 2 OFFSET count(*)",
            "misuse of aggregate function count()",
        ),
    ] {
        assert_eq!(check(sql), Err(want.to_string()), "for {sql}");
    }
}

/// A LIMIT's arity check still applies: `LIMIT min()` is the arity error
#[test]
fn a_limit_arity_error() {
    for (sql, want) in [
        (
            "SELECT 1 FROM t LIMIT min()",
            "wrong number of arguments to function min()",
        ),
        (
            "SELECT 1 FROM t LIMIT group_concat()",
            "wrong number of arguments to function group_concat()",
        ),
        (
            "SELECT 1 FROM t LIMIT min(*)",
            "wrong number of arguments to function min()",
        ),
        (
            "SELECT 1 FROM t LIMIT 2 OFFSET count(1,2)",
            "wrong number of arguments to function count()",
        ),
    ] {
        assert_eq!(check(sql), Err(want.to_string()), "for {sql}");
    }
}

/// A fold in a LIMIT whose arguments name a column is `no such column: a`,
/// which is name resolution's answer, not one this module can give
///
/// `LIMIT count(a)` and `LIMIT min(a)` both defer, because no name resolves in
/// a LIMIT — not a column of the FROM, and not a result alias either. The
/// engine's own name resolution reports the column afterwards.
#[test]
fn a_fold_naming_a_column_in_a_limit_defers() {
    for sql in [
        "SELECT 1 FROM t LIMIT count(a)",
        "SELECT 1 FROM t LIMIT count(abs(a))",
        "SELECT 1 FROM t LIMIT min(a)",
        "SELECT 1 FROM t LIMIT sum(a)",
        "SELECT 1 FROM t LIMIT median(a)",
        "SELECT 1 FROM t LIMIT string_agg(a,1)",
        "SELECT min(a) AS m FROM t LIMIT max(m)",
    ] {
        assert_eq!(check(sql), Ok(()), "for {sql}");
    }
}

/// `SELECT 1 FROM t LIMIT sum(count(*))`
/// → `misuse of aggregate function count()`
///
/// The nesting is still found, because it needs no name.
#[test]
fn nesting_in_a_limit() {
    assert_eq!(
        check("SELECT 1 FROM t LIMIT sum(count(*))"),
        Err("misuse of aggregate function count()".to_string())
    );
}

/// `SELECT 1 FROM t LIMIT (SELECT 1 WHERE count(*)>0)`
/// → `misuse of aggregate function count()`
///
/// A subquery is its own scope and is checked on its own terms, even inside a
/// LIMIT.
#[test]
fn a_subquery_in_a_limit() {
    assert_eq!(
        check("SELECT 1 FROM t LIMIT (SELECT 1 WHERE count(*)>0)"),
        Err("misuse of aggregate function count()".to_string())
    );
}

// --- 17. a name that resolves to nothing, against the oracle ----------------
//
// Every expectation below was recorded by running the statement through the
// real `sqlite3` on the fixture at the top of this file, so `t` is `t(a,b)`
// and `u` is `u(m,x)`. These need a schema, so they go through
// `check_with_tables`: without one the walk cannot tell a name that resolves
// from one that does not, and answers with the aggregate message instead.
//
// The rule they pin down is SQLite's `sqlite3ErrorMsg`, which frees the old
// message and keeps the new one (`util.c:268`), combined with a resolution
// walk that stops at the first node which fails (`resolve.c:1505`). A name
// error is therefore not simply the more important message — it is the one
// the walk reached before giving up, and an aggregate defect the walk reached
// earlier has already written its own message over it.

/// `SELECT 1 FROM t GROUP BY nosuchcol, min()`
/// → `no such column: nosuchcol`
///
/// The name comes first in the clause, so the walk fails on it and the later
/// term's arity error is never reached. This is the case that made the module's
/// `refusal` flag wrong: the clause-level verdict was held back until the whole
/// clause had been walked, so it outranked a name the walk had already failed
/// on.
#[test]
fn a_bad_name_before_an_aggregate_defect_in_the_group_by() {
    for (sql, want) in [
        (
            "SELECT 1 FROM t GROUP BY nosuchcol, min()",
            "no such column: nosuchcol",
        ),
        (
            "SELECT 1 FROM t GROUP BY nosuchcol, min(a)",
            "no such column: nosuchcol",
        ),
        (
            "SELECT 1 FROM t GROUP BY nosuchcol, count(*)",
            "no such column: nosuchcol",
        ),
        (
            "SELECT 1 FROM t GROUP BY a, nosuchcol, min()",
            "no such column: nosuchcol",
        ),
        (
            "SELECT 1 FROM t GROUP BY a, min(a), nosuchcol",
            "no such column: nosuchcol",
        ),
        (
            "SELECT 1 FROM t GROUP BY nosuchcol+min(a)",
            "no such column: nosuchcol",
        ),
        (
            "SELECT 1 FROM t GROUP BY min(a)+nosuchcol",
            "no such column: nosuchcol",
        ),
        (
            "SELECT 1 FROM t GROUP BY min(nosuchcol)",
            "no such column: nosuchcol",
        ),
        (
            "SELECT 1 FROM t GROUP BY count(nosuchcol, a)",
            "no such column: nosuchcol",
        ),
    ] {
        assert_eq!(check_with_tables(sql), Err(want.to_string()), "for {sql}");
    }
}

/// `SELECT 1 FROM t GROUP BY m, count(*)` → `no such column: m`
///
/// The single-term form `GROUP BY m` is the same error, and it is the shape
/// the module already got right: `m` names nothing, so it is neither a column
/// nor a substitution for an aggregate alias.
#[test]
fn an_unresolvable_name_beside_an_aggregate_in_the_group_by() {
    for (sql, want) in [
        ("SELECT 1 FROM t GROUP BY m, count(*)", "no such column: m"),
        ("SELECT 1 FROM t GROUP BY m, min()", "no such column: m"),
        (
            "SELECT 1 FROM t GROUP BY a, m, count(*)",
            "no such column: m",
        ),
        ("SELECT 1 FROM t GROUP BY b, m", "no such column: m"),
        (
            "SELECT 1 FROM t GROUP BY b, m, count(*)",
            "no such column: m",
        ),
    ] {
        assert_eq!(check_with_tables(sql), Err(want.to_string()), "for {sql}");
    }
}

/// `SELECT 1 FROM u GROUP BY m, count(*)` where `m` *is* a column of the FROM
/// → `aggregate functions are not allowed in the GROUP BY clause`
///
/// The mirror of the case above, and the reason the name has to be resolved
/// against the schema rather than pattern-matched: the same statement is the
/// refusal when the schema has the column and `no such column` when it does
/// not. `u` is `u(m,x)`, so `m` resolves there and `a` does not.
#[test]
fn a_name_that_resolves_leaves_the_group_by_verdict_alone() {
    assert_eq!(
        check_with_tables("SELECT 1 FROM u GROUP BY m, count(*)"),
        Err("aggregate functions are not allowed in the GROUP BY clause".to_string())
    );
    assert_eq!(
        check_with_tables("SELECT 1 FROM u GROUP BY a, count(*)"),
        Err("no such column: a".to_string())
    );
    // And with a name that resolves in the fixture table, the clause is still
    // the refusal rather than anything about the name.
    assert_eq!(
        check_with_tables("SELECT 1 FROM t GROUP BY a, count(*)"),
        Err("aggregate functions are not allowed in the GROUP BY clause".to_string())
    );
}

/// `SELECT 1 FROM t GROUP BY t.nosuchcol, count(*)` → `no such column:
/// t.nosuchcol`
///
/// A qualified name is reported as it was written, qualifier and all
/// (`resolve.c:791-793`), and a qualifier naming no source in the FROM fails
/// like any other: `u.a` names a table that is not in this FROM, and `main.a`
/// names a schema rather than a table.
#[test]
fn a_qualified_name_reports_the_qualifier() {
    for (sql, want) in [
        (
            "SELECT 1 FROM t GROUP BY t.nosuchcol, count(*)",
            "no such column: t.nosuchcol",
        ),
        (
            "SELECT 1 FROM t GROUP BY nosuchtbl.a, count(*)",
            "no such column: nosuchtbl.a",
        ),
        (
            "SELECT 1 FROM t GROUP BY u.a, count(*)",
            "no such column: u.a",
        ),
        (
            "SELECT 1 FROM t GROUP BY main.a, count(*)",
            "no such column: main.a",
        ),
    ] {
        assert_eq!(check_with_tables(sql), Err(want.to_string()), "for {sql}");
    }
}

/// `SELECT 1 FROM t GROUP BY t.a, count(*)` and the rowid names
/// → `aggregate functions are not allowed in the GROUP BY clause`
///
/// A name that resolves leaves the clause's own verdict alone, whichever way
/// it resolves: through a real column, through a table's alias, or through one
/// of the three names a rowid table answers to besides its columns.
#[test]
fn a_name_that_resolves_in_any_of_its_ways_leaves_the_refusal() {
    for sql in [
        "SELECT 1 FROM t GROUP BY t.a, count(*)",
        "SELECT 1 FROM t AS q GROUP BY q.a, count(*)",
        "SELECT 1 FROM t AS q GROUP BY b, count(*)",
        "SELECT 1 FROM t GROUP BY rowid, count(*)",
        "SELECT 1 FROM t GROUP BY oid, count(*)",
        "SELECT 1 FROM t GROUP BY _rowid_, count(*)",
    ] {
        assert_eq!(
            check_with_tables(sql),
            Err("aggregate functions are not allowed in the GROUP BY clause".to_string()),
            "for {sql}"
        );
    }
}

/// `SELECT 1 FROM t WHERE nosuchcol AND count(*)>0` → `no such column:
/// nosuchcol`, and the mirror is the misuse
///
/// The pair that shows the rule is positional rather than a ranking: the same
/// two defects, in the same clause, and which one is reported depends only on
/// which the walk reaches first.
#[test]
fn a_bad_name_and_a_misuse_report_in_walk_order() {
    for (sql, want) in [
        (
            "SELECT 1 FROM t WHERE nosuchcol AND count(*)>0",
            "no such column: nosuchcol",
        ),
        (
            "SELECT 1 FROM t WHERE count(*)>0 AND nosuchcol",
            "misuse of aggregate function count()",
        ),
        (
            "SELECT 1 FROM t WHERE min() AND nosuchcol",
            "no such column: nosuchcol",
        ),
        (
            "SELECT 1 FROM t WHERE nosuchcol AND min()",
            "no such column: nosuchcol",
        ),
        (
            "SELECT 1 FROM t WHERE count(a,b) AND nosuchcol",
            "no such column: nosuchcol",
        ),
        (
            "SELECT 1 FROM t WHERE nosuchcol AND count(a,b)",
            "no such column: nosuchcol",
        ),
    ] {
        assert_eq!(check_with_tables(sql), Err(want.to_string()), "for {sql}");
    }
}

/// `SELECT 1 FROM t ORDER BY nosuchcol, count(*)` → `no such column:
/// nosuchcol`
///
/// The ORDER BY is resolved in a walk of its own (`resolve.c:2088`) and a
/// failure in it returns `WRC_Abort` for the whole SELECT, so the `EP_Agg` test
/// over the GROUP BY that follows it (`resolve.c:2109`) is never reached. The
/// aggregate message is not merely outranked here — it is never computed.
///
/// The one direction that does report the aggregate is where it comes first in
/// the same clause: `ORDER BY min(), nosuchcol` is the arity error, because
/// the walk stops there.
#[test]
fn a_bad_name_in_an_order_by_ends_the_statement() {
    for (sql, want) in [
        (
            "SELECT 1 FROM t ORDER BY nosuchcol, count(*)",
            "no such column: nosuchcol",
        ),
        (
            "SELECT 1 FROM t ORDER BY count(*), nosuchcol",
            "no such column: nosuchcol",
        ),
        (
            "SELECT 1 FROM t ORDER BY nosuchcol, min()",
            "no such column: nosuchcol",
        ),
        (
            "SELECT 1 FROM t ORDER BY min(), nosuchcol",
            "wrong number of arguments to function min()",
        ),
        (
            "SELECT 1 FROM t ORDER BY nosuchcol, max(m)",
            "no such column: nosuchcol",
        ),
    ] {
        assert_eq!(check_with_tables(sql), Err(want.to_string()), "for {sql}");
    }
}

/// A statement that names a column nothing has is the name's error
///
/// A name the walk reaches with nothing to resolve it to is
/// `no such column` on `sqlite3`, and this check now answers that itself rather
/// than leaving it to the resolution that runs after. That is the change the
/// schema bought: the statement is refused with the same message, from a phase
/// that can still see the aggregate defects a later clause would have
/// reported.
#[test]
fn a_bad_name_on_its_own_is_the_name_error() {
    for sql in [
        "SELECT nosuchcol FROM t",
        "SELECT 1 FROM t WHERE nosuchcol",
        "SELECT 1 FROM t ORDER BY nosuchcol",
        "SELECT 1 FROM t LIMIT nosuchcol",
        "SELECT 1 FROM t GROUP BY nosuchcol",
    ] {
        assert_eq!(
            check_with_tables(sql),
            Err("no such column: nosuchcol".to_string()),
            "for {sql}"
        );
    }
}

/// A statement with no bad name in it is untouched
///
/// The other half: giving the walk a schema must not make it report anything
/// for a statement that is merely legal.
#[test]
fn a_statement_with_no_bad_name_is_not_reported_at_all() {
    for sql in [
        "SELECT a, b FROM t",
        "SELECT 1 FROM t",
        "SELECT count(*) FROM t",
        "SELECT 1 FROM t GROUP BY a, b",
        "SELECT 1 FROM t WHERE b",
        "SELECT 1 FROM t ORDER BY b",
        "SELECT 1 FROM t LIMIT b",
    ] {
        assert_eq!(check_with_tables(sql), Ok(()), "for {sql}");
    }
}

/// A defect in an earlier clause outranks a bad name in a later one
///
/// The clauses are resolved in a fixed order, so a projection's arity error is
/// written before the GROUP BY is ever looked at and the name never overwrites
/// it. This is the `sqlite3ErrorMsg` overwrite rule again, at clause scale.
#[test]
fn a_defect_in_an_earlier_clause_outranks_a_later_bad_name() {
    for sql in [
        "SELECT count(a,b) FROM t WHERE nosuchcol",
        "SELECT count(a,b) FROM t GROUP BY nosuchcol",
        "SELECT count(a,b) FROM t ORDER BY nosuchcol",
    ] {
        assert_eq!(
            check_with_tables(sql),
            Err("wrong number of arguments to function count()".to_string()),
            "for {sql}"
        );
    }
}

/// A bad name in a subquery is the subquery's to report
///
/// `SELECT 1 FROM t WHERE a IN (SELECT nosuchcol FROM t)` is the same
/// `no such column: nosuchcol` as the subquery on its own, because a subquery
/// is resolved as a SELECT of its own with a FROM of its own — and the name is
/// judged against *that* FROM, not the enclosing one's. Here both are `t`, so
/// the two agree; the point is that the flag does not leak out of the subquery
/// and silence the enclosing statement's own checks.
#[test]
fn a_bad_name_in_a_subquery_is_reported_by_the_subquery() {
    for sql in [
        "SELECT 1 FROM t WHERE a IN (SELECT nosuchcol FROM t)",
        "SELECT 1 FROM t WHERE a IN (SELECT 1 FROM t GROUP BY nosuchcol, min())",
        "SELECT 1 FROM t WHERE a IN (SELECT 1 FROM t ORDER BY nosuchcol, min())",
        "SELECT 1 FROM t WHERE EXISTS (SELECT nosuchcol FROM t)",
        "SELECT (SELECT 1 FROM t GROUP BY nosuchcol, min()) FROM t",
        "SELECT 1 FROM t GROUP BY a, (SELECT nosuchcol FROM t)",
    ] {
        assert_eq!(
            check_with_tables(sql),
            Err("no such column: nosuchcol".to_string()),
            "for {sql}"
        );
    }
}

/// A FROM naming a table this connection does not have silences the check
///
/// `sqlite3` refuses the table before it looks at anything else, so the answer
/// is `no such table: nosuchtable` and never an aggregate message — however
/// wrong the aggregate verdict would be. Reporting one here would be reporting
/// an error for a statement `sqlite3` never got as far as checking.
#[test]
fn an_unknown_table_silences_the_aggregate_checks() {
    assert_eq!(
        check_with_tables("SELECT 1 FROM nosuchtable GROUP BY m, count(*)"),
        Ok(())
    );
    assert_eq!(
        check_with_tables("SELECT 1 FROM nosuchtable WHERE count(*)>0"),
        Ok(())
    );
}

/// With no schema the aggregate rules are still answered exactly
///
/// [`Ctx::check_without_names`] is what the rest of this file drives, and the
/// aggregate verdicts it gives are the oracle's for every statement whose only
/// defect is an aggregate one. The two calls agree wherever no name is
/// involved, which is what makes the schema an addition rather than a change.
#[test]
fn the_two_entry_points_agree_where_no_name_is_involved() {
    for (sql, want) in [
        (
            "SELECT count(a,b) FROM t",
            Err("wrong number of arguments to function count()".to_string()),
        ),
        (
            "SELECT 1 FROM t GROUP BY a, count(*)",
            Err("aggregate functions are not allowed in the GROUP BY clause".to_string()),
        ),
        (
            "SELECT 1 FROM t WHERE count(*)>0",
            Err("misuse of aggregate function count()".to_string()),
        ),
        (
            "SELECT min(a) AS m FROM t GROUP BY a HAVING max(m)<1",
            Err("misuse of aliased aggregate m".to_string()),
        ),
        ("SELECT count(*) FROM t", Ok(())),
        ("SELECT a, b, count(*) FROM t GROUP BY a+0", Ok(())),
    ] {
        assert_eq!(check(sql), want.clone(), "for {sql}");
        assert_eq!(check_with_tables(sql), want, "for {sql}");
    }
}

/// A defect in one clause ends the statement, so a later clause is not
/// resolved at all
///
/// The WHERE is resolved after the projection and before the ORDER BY and the
/// GROUP BY (`resolve.c:2001`, `:2036`, `:2088`, `:2105`), and a failure in any
/// of them returns `WRC_Abort` for the whole SELECT (`resolve.c:1505`). So which
/// of two defects is reported is decided by which clause each is in, and not by
/// any ranking between the messages.
#[test]
fn a_defect_in_an_earlier_clause_ends_the_statement() {
    for (sql, want) in [
        (
            "SELECT 1 FROM t WHERE min() GROUP BY count(*)",
            "wrong number of arguments to function min()",
        ),
        (
            "SELECT 1 FROM t WHERE min() ORDER BY count(*)",
            "wrong number of arguments to function min()",
        ),
        (
            "SELECT 1 FROM t WHERE count(*) ORDER BY min()",
            "misuse of aggregate function count()",
        ),
        (
            "SELECT 1 FROM t WHERE count(*) GROUP BY a",
            "misuse of aggregate: count()",
        ),
        (
            "SELECT 1 FROM t WHERE nosuchcol AND count(a,b)",
            "no such column: nosuchcol",
        ),
        (
            "SELECT 1 FROM t WHERE count(a,b) AND nosuchcol",
            "no such column: nosuchcol",
        ),
        (
            "SELECT 1 FROM t WHERE nosuchcol ORDER BY count(a,b)",
            "no such column: nosuchcol",
        ),
        (
            "SELECT 1 FROM t WHERE count(a,b) ORDER BY nosuchcol",
            "wrong number of arguments to function count()",
        ),
    ] {
        assert_eq!(check_with_tables(sql), Err(want.to_string()), "for {sql}");
    }
}

/// A misuse under an operator ends the clause, and a bare one does not
///
/// A misused aggregate is not an aggregate (`resolve.c:1299` clears `is_agg`),
/// so an operator holding one folds to a constant before the walk descends into
/// it and the rest of the clause is never walked. A bare call is a node of its
/// own, so the walk records the verdict and goes on to the next node — which is
/// how a name beside it becomes the later message and so the reported one.
///
/// The pair is the same two defects in the same clause, and only the shape of
/// the aggregate around them differs.
#[test]
fn a_misuse_under_an_operator_ends_the_clause() {
    for (sql, want) in [
        (
            "SELECT 1 FROM t WHERE count(*)>0 AND nosuchcol",
            "misuse of aggregate function count()",
        ),
        (
            "SELECT 1 FROM t WHERE count(*) AND nosuchcol",
            "no such column: nosuchcol",
        ),
        (
            "SELECT 1 FROM t WHERE min(a)>0 AND nosuchcol",
            "misuse of aggregate function min()",
        ),
        (
            "SELECT 1 FROM t WHERE min(a) AND nosuchcol",
            "no such column: nosuchcol",
        ),
        (
            "SELECT 1 FROM t WHERE count(*)+0 AND nosuchcol",
            "misuse of aggregate function count()",
        ),
        (
            "SELECT 1 FROM t WHERE abs(count(*)) AND nosuchcol",
            "no such column: nosuchcol",
        ),
        (
            "SELECT 1 FROM t WHERE a>0 AND count(*)>0 AND nosuchcol",
            "misuse of aggregate function count()",
        ),
        (
            "SELECT 1 FROM t WHERE a>0 AND nosuchcol AND count(*)>0",
            "no such column: nosuchcol",
        ),
    ] {
        assert_eq!(check_with_tables(sql), Err(want.to_string()), "for {sql}");
    }
}

// --- 18. what decides which of two defects is reported -----------------------
//
// The statements below each have exactly one defect on `sqlite3` -- a bad name,
// or an aggregate misuse -- and the module answers all of them. They are here
// because each one is a case where a plausible rule gets it wrong, and the
// oracle is the only thing that settles which.

// A name in a fold's argument list is resolved before the misuse is recorded.
//
// `resolveExprStep` records the call's own verdict and *then* walks the
// argument list (`resolve.c:1354`), so the name is the later message and
// `sqlite3ErrorMsg` keeps it. This holds in a WHERE and in an ORDER BY alike,
// and for every aggregate.
#[test]
fn a_name_in_a_folds_arguments_beats_the_misuse() {
    for (sql, want) in [
        (
            "SELECT 1 FROM t WHERE count(nosuchcol)",
            "no such column: nosuchcol",
        ),
        (
            "SELECT 1 FROM t ORDER BY count(nosuchcol)",
            "no such column: nosuchcol",
        ),
        (
            "SELECT 1 FROM t WHERE sum(nosuchcol)",
            "no such column: nosuchcol",
        ),
        (
            "SELECT 1 FROM t ORDER BY sum(nosuchcol)",
            "no such column: nosuchcol",
        ),
        (
            "SELECT 1 FROM t WHERE min(nosuchcol)",
            "no such column: nosuchcol",
        ),
        (
            "SELECT 1 FROM t GROUP BY count(nosuchcol)",
            "no such column: nosuchcol",
        ),
    ] {
        assert_eq!(check_with_tables(sql), Err(want.to_string()), "for {sql}");
    }
}

/// A LIMIT is resolved before every other clause of its SELECT
///
/// `resolve.c:1930` resolves a LIMIT and an OFFSET against an empty
/// `NameContext` before the body is walked at all, so a defect in one is
/// reached before a name anywhere else. That is what makes `ORDER BY nosuchcol
/// LIMIT min()` the arity error and not the name.
#[test]
fn a_limit_is_resolved_before_the_rest_of_the_select() {
    assert_eq!(
        check_with_tables("SELECT 1 FROM t ORDER BY nosuchcol LIMIT min()"),
        Err("wrong number of arguments to function min()".to_string())
    );
    // And a name in a LIMIT is still the name when nothing else is wrong.
    assert_eq!(
        check_with_tables("SELECT 1 FROM t LIMIT nosuchcol"),
        Err("no such column: nosuchcol".to_string())
    );
}

/// The first of two bad names is the one reported
///
/// `resolve.c:1505` returns `WRC_Abort` from the node that failed, so the walk
/// stops at the first name and the second is never reached. This is the one
/// place where a later message does *not* win, and it is the same walk order the
/// aggregate messages follow.
#[test]
fn the_first_of_two_bad_names_is_reported() {
    assert_eq!(
        check_with_tables("SELECT 1 FROM t WHERE nosuchcol AND nosuchcol2"),
        Err("no such column: nosuchcol".to_string())
    );
    assert_eq!(
        check_with_tables("SELECT 1 FROM t WHERE nosuchcol2 AND nosuchcol"),
        Err("no such column: nosuchcol2".to_string())
    );
    assert_eq!(
        check_with_tables("SELECT 1 FROM t WHERE nosuchcol AND nosuchcol2 AND min()"),
        Err("no such column: nosuchcol".to_string())
    );
}

/// A comparison folds, and an `AND` does not
///
/// A comparison is code-generated before the walk descends into it, and a
/// misused aggregate is not an aggregate (`resolve.c:1299` clears `is_agg`), so
/// `count(*)>0` is already a constant and the rest of the clause is never
/// walked. An `AND` is not code-generated during resolution, so its operands
/// are walked in the ordinary way and the walk continues past a bare verdict.
///
/// The pairs are the same two defects with the aggregate's shape the only
/// difference, which is what makes the shape the deciding thing.
#[test]
fn a_comparison_folds_its_operands_and_an_and_does_not() {
    for (sql, want) in [
        (
            "SELECT 1 FROM t WHERE count(*)>0 AND nosuchcol",
            "misuse of aggregate function count()",
        ),
        (
            "SELECT 1 FROM t WHERE count(*) AND nosuchcol",
            "no such column: nosuchcol",
        ),
        (
            "SELECT 1 FROM t WHERE nosuchcol AND count(*)>0",
            "no such column: nosuchcol",
        ),
        (
            "SELECT 1 FROM t WHERE min(a)>0 AND nosuchcol",
            "misuse of aggregate function min()",
        ),
        (
            "SELECT 1 FROM t WHERE min(a) AND nosuchcol",
            "no such column: nosuchcol",
        ),
        (
            "SELECT 1 FROM t WHERE count(*)=1 AND nosuchcol",
            "misuse of aggregate function count()",
        ),
        (
            "SELECT 1 FROM t WHERE count(*)+0 AND nosuchcol",
            "misuse of aggregate function count()",
        ),
        // A unary operator and an ordinary call are walked into as ordinary
        // nodes, so neither of them folds.
        (
            "SELECT 1 FROM t WHERE -count(*) AND nosuchcol",
            "no such column: nosuchcol",
        ),
        (
            "SELECT 1 FROM t WHERE abs(count(*)) AND nosuchcol",
            "no such column: nosuchcol",
        ),
    ] {
        assert_eq!(check_with_tables(sql), Err(want.to_string()), "for {sql}");
    }
}

/// A constant that decides an `AND` drops the other side
///
/// `sqlite3ExprSimplifiedAndOr` (`expr.c:2393`) rewrites `x AND false` to
/// `false` and `x AND true` to `x`, so the dropped side is never resolved and a
/// defect in it is never found. This is the only case where a name beside an
/// aggregate is *not* an error, and it is not a rule about names at all -- it is
/// a rule about the constant.
#[test]
fn a_constant_deciding_an_and_drops_the_other_side() {
    for sql in [
        "SELECT 1 FROM t WHERE nosuchcol AND 0",
        "SELECT 1 FROM t WHERE 0 AND nosuchcol",
    ] {
        assert_eq!(check_with_tables(sql), Ok(()), "for {sql}");
    }
    // A constant that does not decide it -- a true one -- leaves the other side
    // resolved, so the name is still reported.
    assert_eq!(
        check_with_tables("SELECT 1 FROM t WHERE nosuchcol AND 1"),
        Err("no such column: nosuchcol".to_string())
    );
    assert_eq!(
        check_with_tables("SELECT 1 FROM t WHERE 1 AND nosuchcol"),
        Err("no such column: nosuchcol".to_string())
    );
}

/// A comma separates terms, an `AND` does not
///
/// `ORDER BY` and `GROUP BY` are lists of terms resolved one after another
/// (`resolveOrderGroupBy`, `resolve.c:1841`), so a defect in one term leaves the
/// terms after it resolved. A WHERE is one expression, so a bare verdict inside
/// it does not stop the operands beside it from being reached. The same pair of
/// defects, in the two places, and the two answers.
#[test]
fn a_comma_separates_terms_but_an_and_does_not() {
    for (sql, want) in [
        (
            "SELECT 1 FROM t ORDER BY min(), nosuchcol",
            "wrong number of arguments to function min()",
        ),
        (
            "SELECT 1 FROM t ORDER BY nosuchcol, min()",
            "no such column: nosuchcol",
        ),
        (
            "SELECT 1 FROM t GROUP BY min(), nosuchcol",
            "wrong number of arguments to function min()",
        ),
        (
            "SELECT 1 FROM t GROUP BY nosuchcol, min()",
            "no such column: nosuchcol",
        ),
        (
            "SELECT 1 FROM t WHERE min() AND nosuchcol",
            "no such column: nosuchcol",
        ),
        (
            "SELECT 1 FROM t WHERE count(a,b) AND nosuchcol",
            "no such column: nosuchcol",
        ),
        (
            "SELECT 1 FROM t ORDER BY count(a,b) AND nosuchcol",
            "no such column: nosuchcol",
        ),
        // And a term that failed does end the terms after it.
        (
            "SELECT 1 FROM t GROUP BY a, min(), nosuchcol",
            "wrong number of arguments to function min()",
        ),
    ] {
        assert_eq!(check_with_tables(sql), Err(want.to_string()), "for {sql}");
    }
}
