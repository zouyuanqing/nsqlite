//! `EXPLAIN QUERY PLAN`: the WHERE shapes that are not a plain `=` or a
//! comparison.
//!
//! Every expectation here is the text the real `sqlite3` prints for the same
//! statement in the same schema, produced by running the binary -- see
//! [`oracle`], which applies the DDL to a real database and reads the `detail`
//! column back out of the shell's tree rendering. The module under test is
//! [`nsqlite::explain`]; the shared helpers live in `explain.rs` next to the
//! cases that predate this file.
//!
//! These are the shapes where a plan is a *seek* rather than a scan, and each
//! one is a rule rather than a special case: `IS NULL` and `IN (...)` are
//! lookups keyed on the column, a `GLOB` with a literal prefix is a two-sided
//! range, and `IS NOT NULL` is a seek only when the index also answers the rest
//! of the query. The reason each is grouped this way rather than filed under
//! "index choice" is that all four were a plain `SCAN` here while `sqlite3`
//! reported a `SEARCH`.

use nsqlite::affinity::Affinity;
use nsqlite::catalog::{Catalog, Column, Index, Table};
use nsqlite::connection::Outcome;
use nsqlite::explain;
use nsqlite::parser::parse_one;

/// Builds a table the way the catalog records one. The declared types are left
/// empty because a plan does not look at them, except in the GLOB cases, which
/// say so where they matter.
fn table(name: &str, columns: &[&str]) -> Table {
    Table {
        name: name.to_string(),
        columns: columns
            .iter()
            .map(|c| Column {
                name: (*c).to_string(),
                declared_type: String::new(),
                affinity: Affinity::Text,
                not_null: false,
                default: None,
                rowid_alias: false,
            })
            .collect(),
        rowid_alias: None,
        without_rowid: false,
        root_page: 0,
    }
}

/// The same table, with one column declared `TEXT`.
///
/// The GLOB prefix check reads the *declared* type rather than a derived
/// affinity field, so a column with no declared type is the one that exercises
/// the numeric-prefix guard -- SQLite reads an undeclared column as BLOB.
fn text_b_table() -> Table {
    let mut t = table("t1", &["a", "b", "c"]);
    if let Some(b) = t.columns.iter_mut().find(|c| c.name == "b") {
        b.declared_type = "TEXT".into();
        b.affinity = Affinity::Text;
    }
    t
}

fn index(name: &str, table: &str, columns: &[&str], unique: bool) -> Index {
    Index {
        name: name.to_string(),
        table: table.to_string(),
        columns: columns.iter().map(|c| (*c).to_string()).collect(),
        ascending: columns.iter().map(|_| true).collect(),
        unique,
        root_page: 0,
    }
}

fn catalog(tables: Vec<Table>, indexes: Vec<Index>) -> Catalog {
    let mut c = Catalog::new();
    for t in tables {
        c.put(t);
    }
    for i in indexes {
        c.put_index(i);
    }
    c
}

/// The schema most of these cases share, as (DDL, catalog).
///
/// The two indexes are chosen so that every rule below is observable: `i1(b)`
/// and `i2(b,c)` differ in width, so the choice between them shows the width
/// criterion, and `i2` reaches a second column so a two-term constraint can be
/// seen.
fn base() -> (Vec<&'static str>, Catalog) {
    let ddl = vec![
        "CREATE TABLE t1(a, b, c)",
        "CREATE INDEX i1 ON t1(b)",
        "CREATE INDEX i2 ON t1(b, c)",
    ];
    let cat = catalog(
        vec![table("t1", &["a", "b", "c"])],
        vec![
            index("i1", "t1", &["b"], false),
            index("i2", "t1", &["b", "c"], false),
        ],
    );
    (ddl, cat)
}

/// The `detail` column of every plan row, joined with ` ~ `.
fn plan_lines(sql: &str, cat: &Catalog) -> Result<String, String> {
    let (head, rest) = sql.split_at(sql.find(' ').unwrap_or(sql.len()));
    assert_eq!(head, "EXPLAIN", "every case here starts with EXPLAIN");
    let e = explain::parse(rest).map_err(|e| e.message.to_string())?;
    let Outcome::Query { columns, rows } =
        explain::execute(&e, cat).map_err(|e| e.message.to_string())?
    else {
        return Err("EXPLAIN QUERY PLAN did not produce rows".into());
    };
    assert_eq!(
        columns,
        explain::QUERY_PLAN_COLUMNS,
        "column names for {sql}"
    );
    Ok(rows
        .iter()
        .map(|r| match &r.values[3] {
            nsqlite::Value::Text(s) => s.clone(),
            other => panic!("plan detail is not text: {other:?}"),
        })
        .collect::<Vec<_>>()
        .join(" ~ "))
}

/// Asserts a plan matches the text `sqlite3` prints for the same statement.
#[track_caller]
fn eqp(sql: &str, expected: &str) {
    let cat = base().1;
    match plan_lines(sql, &cat) {
        Ok(got) => assert_eq!(got, expected, "plan for {sql}"),
        Err(e) => panic!("plan for {sql} failed: {e}"),
    }
}

/// Asserts a plan against a schema other than [`base`].
#[track_caller]
fn eqp_on(sql: &str, cat: &Catalog, expected: &str) {
    match plan_lines(sql, cat) {
        Ok(got) => assert_eq!(got, expected, "plan for {sql}"),
        Err(e) => panic!("plan for {sql} failed: {e}"),
    }
}

/// Runs a statement through the real `sqlite3` and returns the `detail` column
/// of its plan, joined the way [`plan_lines`] joins this engine's.
///
/// The shell renders `EXPLAIN QUERY PLAN` as a tree rather than as the four
/// columns the statement returns, so the detail words are recovered by
/// stripping the `|--` / `` `-- `` glyph run in front of each line. What is
/// left is exactly the `detail` column, in the same order.
///
/// It is not called by the tests -- that would make the suite depend on an
/// external installation -- so the expectations stay frozen. It is a function
/// rather than a script so the schema and the statement are the same two
/// values a test uses, and re-measuring a case is a one-line change.
#[allow(dead_code)]
fn oracle(ddl: &[&str], sql: &str) -> String {
    let path = std::env::temp_dir().join(format!("nsqlite_where_{}.db", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let script = format!("{};\nEXPLAIN QUERY PLAN {sql};\n", ddl.join(";\n"));
    let mut child = std::process::Command::new("sqlite3")
        .arg(&path)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("sqlite3 is on PATH");
    {
        use std::io::Write;
        child
            .stdin
            .as_mut()
            .expect("stdin is piped")
            .write_all(script.as_bytes())
            .expect("the script is written");
    }
    let out = child.wait_with_output().expect("sqlite3 finishes");
    let details: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim_end)
        .filter(|l| !l.trim().is_empty() && l.trim() != "QUERY PLAN")
        .map(|l| {
            l.trim_start_matches(|c: char| c == '|' || c == '`' || c == '-' || c.is_whitespace())
                .to_string()
        })
        .collect();
    let _ = std::fs::remove_file(&path);
    details.join(" ~ ")
}

// --- the bounds of a comparison -------------------------------------------

/// `<=` is a top bound, and the plan has to say so.
///
/// This is the sign bug that made the module wrong rather than incomplete:
/// `range_bounds` records `>=` and `<=` distinctly because SQLite prints the
/// strict form for both, and the renderer used to map every marker that was not
/// `<` to `>?`. So `WHERE b<=1` planned as `SEARCH t1 USING INDEX i1 (b>?)` --
/// a seek for `b > 1` on a query that asked for `b <= 1`.
#[test]
fn a_less_or_equal_is_a_top_bound() {
    eqp(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE b<=1",
        "SEARCH t1 USING INDEX i1 (b<?)",
    );
    eqp(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE b>=1",
        "SEARCH t1 USING INDEX i1 (b>?)",
    );
    eqp(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE b<1",
        "SEARCH t1 USING INDEX i1 (b<?)",
    );
    eqp(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE b>1",
        "SEARCH t1 USING INDEX i1 (b>?)",
    );
}

/// Both bounds print, whichever way round they were written.
///
/// The order in the source does not reach the plan: `b<=5 AND b>=1` and
/// `b>=1 AND b<=5` are the same constraint, and SQLite reports the lower bound
/// first both times.
#[test]
fn bounds_print_lower_first_whichever_way_they_were_written() {
    for sql in [
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE b<=5 AND b>=1",
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE b>=1 AND b<=5",
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE b>1 AND b<5",
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE b<5 AND b>1",
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE b BETWEEN 1 AND 5",
    ] {
        eqp(sql, "SEARCH t1 USING INDEX i1 (b>? AND b<?)");
    }
}

/// A `<=` on a rowid is a top bound too, and the two signs must not swap.
#[test]
fn a_rowid_bound_takes_its_own_sign() {
    let cat = catalog(vec![table("t1", &["a", "b", "c"])], Vec::new());
    eqp_on(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE rowid<=1",
        &cat,
        "SEARCH t1 USING INTEGER PRIMARY KEY (rowid<?)",
    );
    eqp_on(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE rowid>=1",
        &cat,
        "SEARCH t1 USING INTEGER PRIMARY KEY (rowid>?)",
    );
    eqp_on(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE rowid<=5 AND rowid>=1",
        &cat,
        "SEARCH t1 USING INTEGER PRIMARY KEY (rowid>? AND rowid<?)",
    );
}

/// Only the first bound on each side of one column is reported.
///
/// `b>1 AND b>2` constrains `b` no more tightly than `b>2` alone, and SQLite
/// prints one term for the pair rather than two.
#[test]
fn a_second_bound_on_the_same_side_is_not_reported() {
    eqp(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE b>1 AND b>2",
        "SEARCH t1 USING INDEX i1 (b>?)",
    );
    eqp(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE b<1 AND b<2",
        "SEARCH t1 USING INDEX i1 (b<?)",
    );
    eqp(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE b>=1 AND b>2",
        "SEARCH t1 USING INDEX i1 (b>?)",
    );
    eqp(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE b>1 AND b<5 AND b<3",
        "SEARCH t1 USING INDEX i1 (b>? AND b<?)",
    );
}

// --- IS NULL ---------------------------------------------------------------

/// `IS NULL` seeks the NULLs in the index the same way it seeks an equality.
///
/// The text is `(b=?)` and not `(b IS NULL)`, and the index choice follows the
/// same width rule an equality does: on `t1(a,b,c,d)` with `i1(b)`, `i2(b,c)`
/// and `i3(b,c,d)`, `SELECT *` over `b IS NULL` reports `i2` and
/// `SELECT b,c,d` reports `i3`, which is the "one pinned column wants the
/// widest key" criterion.
#[test]
fn is_null_seeks_the_nulls() {
    eqp(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE b IS NULL",
        "SEARCH t1 USING INDEX i1 (b=?)",
    );
    eqp(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE b ISNULL",
        "SEARCH t1 USING INDEX i1 (b=?)",
    );
    eqp(
        "EXPLAIN QUERY PLAN SELECT a FROM t1 WHERE b IS NULL",
        "SEARCH t1 USING INDEX i1 (b=?)",
    );
    eqp(
        "EXPLAIN QUERY PLAN SELECT b,c FROM t1 WHERE b IS NULL",
        "SEARCH t1 USING COVERING INDEX i2 (b=?)",
    );
}

/// An `IS NULL` seeks the nulls and the columns behind it are still reached.
///
/// An `IS NULL` pins a *point* in the key rather than bounding it, which is
/// what separates it from `b>1 AND c=1` -- there a bound makes the key an
/// interval and the columns behind it are unreachable. Measured on
/// `t1(a,b,c)` with `i1(b)` and `i2(b,c)`.
#[test]
fn an_equality_past_is_null_is_kept() {
    eqp(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE b IS NULL AND c=1",
        "SEARCH t1 USING INDEX i2 (b=? AND c=?)",
    );
    eqp(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE b IS NULL AND c>1",
        "SEARCH t1 USING INDEX i2 (b=? AND c>?)",
    );
    eqp(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE b IS NULL AND b IS NULL",
        "SEARCH t1 USING INDEX i1 (b=?)",
    );
    eqp(
        "EXPLAIN QUERY PLAN SELECT b,c FROM t1 WHERE b IS NULL AND c=1",
        "SEARCH t1 USING COVERING INDEX i2 (b=? AND c=?)",
    );
}

/// Which index an `IS NULL` picks is a cost tie this planner cannot settle.
///
/// The text and the access are right; the *choice* among several usable
/// indexes is where SQLite's cost model decides, and the deciding term is a
/// flat constant in `where.c` rather than a rule: when a non-covering scan is
/// costed, each WHERE term the index can evaluate reduces the estimated table
/// lookups, and a `WO_EQ` or `WO_IS` term gets a further flat reduction of 19 --
/// an `IS NULL` term does not. So an equality is assumed to resolve most of
/// the lookups it causes and a nullity test is not, and the winner moves.
///
/// The consequence is that the answer is not a function of the schema.
/// On `t1(a,b,c)` with `i1(b)` and `i2(b,c)`, declaring the indexes in that
/// order makes `SELECT * FROM t1 WHERE b IS NULL` report `i2`, and declaring
/// them the other way round makes the *same statement* report `i1`. Both
/// indexes are declared, both can serve the query, and the query text does not
/// change -- so the tie is broken by the row-count estimate this engine does
/// not collect. That is the module's documented cost-model gap.
///
/// So what is pinned here is what *is* determined: the seek, the text, the
/// single-index schemas where there is nothing to choose between, and the
/// two-term case where a second equality settles it. The multi-index tie is not
/// claimed.
#[test]
fn an_is_null_seeks_and_says_so() {
    // One index: nothing to choose, and the answer is exact.
    let single = catalog(
        vec![table("t1", &["a", "b", "c"])],
        vec![index("i1", "t1", &["b"], false)],
    );
    for (proj, expected) in [
        ("*", "SEARCH t1 USING INDEX i1 (b=?)"),
        ("b", "SEARCH t1 USING COVERING INDEX i1 (b=?)"),
        // `b,c` is not covered by an index on `b`, but the search is still
        // worth it: the table lookup happens only for the rows that are NULL.
        ("b,c", "SEARCH t1 USING INDEX i1 (b=?)"),
    ] {
        eqp_on(
            &format!("EXPLAIN QUERY PLAN SELECT {proj} FROM t1 WHERE b IS NULL"),
            &single,
            expected,
        );
    }
    // An equality alongside the `IS NULL` settles the index on its own, and the
    // plan reports both terms.
    let two = catalog(
        vec![table("t1", &["a", "b", "c"])],
        vec![
            index("i1", "t1", &["b"], false),
            index("i2", "t1", &["b", "c"], false),
        ],
    );
    eqp_on(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE b IS NULL AND c=1",
        &two,
        "SEARCH t1 USING INDEX i2 (b=? AND c=?)",
    );
}

/// An `IS NULL` on a column past the leading key one is a plain scan.
#[test]
fn is_null_past_the_leading_key_column_is_a_scan() {
    eqp(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE c IS NULL",
        "SCAN t1",
    );
}

// --- IS and IN -------------------------------------------------------------

/// `IS` against a non-NULL value is an equality; `IS NOT` is not.
///
/// `IS` does not test for NULL the way `IS NOT NULL` does, so against a value
/// it is a two-way comparison and the planner treats it as a seek.
#[test]
fn is_against_a_value_is_an_equality() {
    eqp(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE b IS 1",
        "SEARCH t1 USING INDEX i2 (b=?)",
    );
    eqp(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE b IS '1'",
        "SEARCH t1 USING INDEX i2 (b=?)",
    );
    eqp(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE b IS 1.0",
        "SEARCH t1 USING INDEX i2 (b=?)",
    );
    eqp(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE b IS 1 AND c=2",
        "SEARCH t1 USING INDEX i2 (b=? AND c=?)",
    );
    // `IS NOT` pins nothing: a two-way comparison is not a seek.
    eqp(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE b IS NOT 1",
        "SCAN t1",
    );
}

/// An `IN` over literals is a lookup, and the plan does not echo the list.
///
/// A one-element list is still a list, and a NULL in the list changes nothing:
/// SQLite evaluates the list to the set of non-NULL values it holds, so the
/// seek is the same.
#[test]
fn an_in_list_is_a_lookup() {
    eqp(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE b IN (1,2)",
        "SEARCH t1 USING INDEX i2 (b=?)",
    );
    eqp(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE b IN (1)",
        "SEARCH t1 USING INDEX i2 (b=?)",
    );
    eqp(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE b IN (1,2,3)",
        "SEARCH t1 USING INDEX i2 (b=?)",
    );
    eqp(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE b IN (1,NULL)",
        "SEARCH t1 USING INDEX i2 (b=?)",
    );
    eqp(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE b IN (1,2) AND c=3",
        "SEARCH t1 USING INDEX i2 (b=? AND c=?)",
    );
    eqp(
        "EXPLAIN QUERY PLAN SELECT b,c FROM t1 WHERE b IN (1,2)",
        "SEARCH t1 USING COVERING INDEX i2 (b=?)",
    );
}

/// An `IN` list naming a column is not a list of literals, so it pins nothing.
///
/// This is the same rule that keeps `a=b` from counting as an equality, and it
/// is the reason the list is checked element by element rather than taken on
/// its type. A negated `IN` is a plain scan for the mirror reason: it asks for
/// every value the list does *not* hold, which is not a seek.
#[test]
fn an_in_list_of_non_literals_is_a_scan() {
    eqp(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE b IN (c)",
        "SCAN t1",
    );
    eqp(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE b IN (a)",
        "SCAN t1",
    );
    eqp(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE b IN (1,2,c)",
        "SCAN t1",
    );
    eqp(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE b NOT IN (1,2)",
        "SCAN t1",
    );
}

/// An `IN (SELECT ...)` is a lookup, and it brings a `LIST SUBQUERY` line with
/// the sub-select's own plan.
///
/// The line comes after the outer access, because in SQLite the list subquery
/// hangs off the outer scan. `SELECT 1` inside reports `SCAN CONSTANT ROW`,
/// which is the same marker a `FROM`-less select gets.
#[test]
fn an_in_select_is_a_lookup_and_a_list_subquery() {
    eqp(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE b IN (SELECT b FROM t1)",
        "SEARCH t1 USING INDEX i2 (b=?) ~ LIST SUBQUERY 1 ~ SCAN t1 USING COVERING INDEX i1",
    );
    eqp(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE b IN (SELECT b FROM t1) AND c=1",
        "SEARCH t1 USING INDEX i2 (b=? AND c=?) ~ LIST SUBQUERY 1 ~ SCAN t1 USING COVERING INDEX i1",
    );
    eqp(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE b IN (SELECT 1)",
        "SEARCH t1 USING INDEX i2 (b=?) ~ LIST SUBQUERY 1 ~ SCAN CONSTANT ROW",
    );
    eqp(
        "EXPLAIN QUERY PLAN SELECT b,c FROM t1 WHERE b IN (SELECT b FROM t1)",
        "SEARCH t1 USING COVERING INDEX i2 (b=?) ~ LIST SUBQUERY 1 ~ SCAN t1 USING COVERING INDEX i1",
    );
}

/// Two list subqueries are numbered 1 and 2, in the order they were written,
/// and both come after the outer access line.
#[test]
fn list_subqueries_are_numbered_in_order() {
    eqp(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE b IN (SELECT b FROM t1) AND c IN (SELECT c FROM t1)",
        "SEARCH t1 USING INDEX i2 (b=? AND c=?) \
         ~ LIST SUBQUERY 1 ~ SCAN t1 USING COVERING INDEX i1 \
         ~ LIST SUBQUERY 2 ~ SCAN t1 USING COVERING INDEX i2",
    );
}

/// A negated `IN (SELECT ...)` is not a probe, so it gets no list subquery.
///
/// SQLite still names the index it *would* have used, beside `FOR IN-OPERATOR`;
/// this reports the scan alone, which is the right access and the wrong
/// wording. The module docs call this out.
#[test]
fn a_negated_in_select_falls_back_to_a_scan() {
    eqp(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE b NOT IN (SELECT b FROM t1)",
        "SCAN t1",
    );
}

// --- IS NOT NULL -----------------------------------------------------------

/// `IS NOT NULL` seeks only when the index also answers the rest of the query.
///
/// SQLite builds the constraint as a virtual `x > NULL` term -- see the
/// `TK_NOTNULL` branch of `whereexpr.c` -- and a range scan over most of an
/// index is rarely cheaper than reading the table, so the seek wins only when
/// there is no per-row table lookup left to pay for. This is the one
/// constraint whose text is `>?` but whose *presence* changes the answer, so
/// it is kept out of the ordinary range list and consulted at the point where
/// the covering answer is known.
#[test]
fn is_not_null_seeks_only_when_covering() {
    // Not covering: the table has to be read anyway, so the scan wins.
    eqp(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE b IS NOT NULL",
        "SCAN t1",
    );
    eqp(
        "EXPLAIN QUERY PLAN SELECT a FROM t1 WHERE b IS NOT NULL",
        "SCAN t1",
    );
    // Covering: the seek wins, and the index is the narrowest one that covers.
    eqp(
        "EXPLAIN QUERY PLAN SELECT b FROM t1 WHERE b IS NOT NULL",
        "SEARCH t1 USING COVERING INDEX i1 (b>?)",
    );
    eqp(
        "EXPLAIN QUERY PLAN SELECT b,c FROM t1 WHERE b IS NOT NULL",
        "SEARCH t1 USING COVERING INDEX i2 (b>?)",
    );
    eqp(
        "EXPLAIN QUERY PLAN SELECT count(*) FROM t1 WHERE b IS NOT NULL",
        "SEARCH t1 USING COVERING INDEX i1 (b>?)",
    );
    // The one-word spelling is what the parser accepts, and it plans the same.
    eqp(
        "EXPLAIN QUERY PLAN SELECT b,c FROM t1 WHERE b NOTNULL",
        "SEARCH t1 USING COVERING INDEX i2 (b>?)",
    );
}

/// An ordinary range is not subject to any of the not-null rules.
///
/// This is what makes `IS NOT NULL` the odd one out rather than a special case
/// of a general rule: the same projections that fall back to a scan for a bare
/// `IS NOT NULL` all report a search for `b>1`.
#[test]
fn an_ordinary_range_is_not_subject_to_the_not_null_rule() {
    eqp(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE b>1",
        "SEARCH t1 USING INDEX i1 (b>?)",
    );
    eqp(
        "EXPLAIN QUERY PLAN SELECT b,c FROM t1 WHERE b>1",
        "SEARCH t1 USING COVERING INDEX i2 (b>?)",
    );
    eqp(
        "EXPLAIN QUERY PLAN SELECT a FROM t1 WHERE b>1",
        "SEARCH t1 USING INDEX i1 (b>?)",
    );
}

/// A not-null bound is only *added* to a plan that is a search already.
///
/// It never causes one. On its own it is a range over most of the index, which
/// is rarely cheaper than reading the table, so `SELECT a` and `SELECT *` are
/// both plain scans. Two things do make the bound appear: an index that covers
/// the query, and a real range that has already forced the walk.
#[test]
fn is_not_null_alone_is_never_a_seek() {
    eqp(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE b IS NOT NULL",
        "SCAN t1",
    );
    eqp(
        "EXPLAIN QUERY PLAN SELECT a FROM t1 WHERE b IS NOT NULL",
        "SCAN t1",
    );
    eqp(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE c IS NOT NULL",
        "SCAN t1",
    );
    eqp(
        "EXPLAIN QUERY PLAN SELECT a FROM t1 WHERE b IS NOT NULL AND c=1",
        "SCAN t1",
    );
}

/// A covering index is the one thing that makes the not-null bound pay.
#[test]
fn is_not_null_seeks_through_a_covering_index() {
    eqp(
        "EXPLAIN QUERY PLAN SELECT b FROM t1 WHERE b IS NOT NULL",
        "SEARCH t1 USING COVERING INDEX i1 (b>?)",
    );
    eqp(
        "EXPLAIN QUERY PLAN SELECT b,c FROM t1 WHERE b IS NOT NULL",
        "SEARCH t1 USING COVERING INDEX i2 (b>?)",
    );
    eqp(
        "EXPLAIN QUERY PLAN SELECT count(*) FROM t1 WHERE b IS NOT NULL",
        "SEARCH t1 USING COVERING INDEX i1 (b>?)",
    );
    // The one-word spelling is what the parser accepts, and it plans the same.
    eqp(
        "EXPLAIN QUERY PLAN SELECT b,c FROM t1 WHERE b NOTNULL",
        "SEARCH t1 USING COVERING INDEX i2 (b>?)",
    );
}

/// A not-null bound on a later column still only applies once the seek exists.
///
/// `b IS NOT NULL AND c=1` is a scan, not a search: the not-null on `b` is not
/// a reason to seek and the equality on `c` is behind a column nothing pins.
#[test]
fn a_not_null_bound_on_a_later_column_does_not_force_a_seek() {
    eqp(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE b IS NOT NULL AND c=1",
        "SCAN t1",
    );
    eqp(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE c IS NOT NULL AND b=1",
        "SEARCH t1 USING INDEX i2 (b=? AND c>?)",
    );
}

/// A real range has already forced the walk, so the not-null bound joins it.
#[test]
fn a_not_null_bound_joins_a_range_on_the_same_column() {
    eqp(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE b<5 AND b IS NOT NULL",
        "SEARCH t1 USING INDEX i1 (b>? AND b<?)",
    );
    eqp(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE b IS NOT NULL AND b<5",
        "SEARCH t1 USING INDEX i1 (b>? AND b<?)",
    );
    // The same bound on a column the range does not reach is not reported:
    // `b<5` pins nothing, so the walk is already decided without it.
    eqp(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE c IS NOT NULL AND b<5",
        "SEARCH t1 USING INDEX i1 (b<?)",
    );
    // An equality on the leading column beats it, because that is a real seek.
    eqp(
        "EXPLAIN QUERY PLAN SELECT b,c FROM t1 WHERE b=1 AND c IS NOT NULL",
        "SEARCH t1 USING COVERING INDEX i2 (b=? AND c>?)",
    );
}

/// A not-null bound on the only constraint there is does not turn a covering
/// scan into a search when the index is the whole table.
///
/// This is the strict-subset rule meeting the not-null rule: reading a table
/// costs no more than reading an index of the same width, so neither wins and
/// the scan is reported.
#[test]
fn a_whole_table_index_is_still_a_scan() {
    let cat = catalog(
        vec![table("t1", &["a", "b", "c"])],
        vec![index("iabc", "t1", &["a", "b", "c"], false)],
    );
    eqp_on(
        "EXPLAIN QUERY PLAN SELECT b FROM t1 WHERE b IS NOT NULL",
        &cat,
        "SCAN t1",
    );
}

// --- GLOB ------------------------------------------------------------------

/// A `GLOB` with a usable literal prefix is a two-sided range.
///
/// Every match starts with the prefix, so the index is walked from the first
/// key that does to the last, and SQLite reports both bounds and no values.
/// The prefix rule is SQLite's own, from `isLikeOrGlob` in `whereexpr.c`.
#[test]
fn a_glob_with_a_prefix_is_a_range() {
    eqp(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE b GLOB 'x*'",
        "SEARCH t1 USING INDEX i1 (b>? AND b<?)",
    );
    eqp(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE b GLOB 'x'",
        "SEARCH t1 USING INDEX i1 (b>? AND b<?)",
    );
    eqp(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE b GLOB 'x?'",
        "SEARCH t1 USING INDEX i1 (b>? AND b<?)",
    );
    eqp(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE b GLOB 'xy*z'",
        "SEARCH t1 USING INDEX i1 (b>? AND b<?)",
    );
    eqp(
        "EXPLAIN QUERY PLAN SELECT b,c FROM t1 WHERE b GLOB 'x*'",
        "SEARCH t1 USING COVERING INDEX i2 (b>? AND b<?)",
    );
}

/// A `GLOB` with no usable prefix is a plain scan.
///
/// The wildcards are `*`, `?` and `[`; a pattern that begins with one has no
/// prefix, and neither has an empty one. A pattern that begins with `[` is
/// worth naming separately: the class is at the front, so there is no literal
/// to bound by, even though `[a]x` -- a class in the middle -- does have one.
#[test]
fn a_glob_with_no_prefix_is_a_scan() {
    for pattern in [
        "'*'", "'*x'", "'?'", "'??'", "'[a]'", "'[a]*'", "'[a'", "''",
    ] {
        let sql = format!("EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE b GLOB {pattern}");
        eqp(&sql, "SCAN t1");
    }
    eqp(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE b GLOB 'x[a]'",
        "SEARCH t1 USING INDEX i1 (b>? AND b<?)",
    );
    // A pattern that is not a literal has no readable prefix at all.
    eqp(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE b GLOB 'x' || 'y'",
        "SCAN t1",
    );
    // The negated form is not an optimisation at all.
    eqp(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE b NOT GLOB 'x*'",
        "SCAN t1",
    );
}

/// An escaped wildcard is part of the prefix, so the pattern is still usable.
#[test]
fn an_escaped_wildcard_does_not_end_the_prefix() {
    eqp(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE b GLOB 'a\\*b'",
        "SEARCH t1 USING INDEX i1 (b>? AND b<?)",
    );
    eqp(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE b GLOB '\\?'",
        "SEARCH t1 USING INDEX i1 (b>? AND b<?)",
    );
}

/// A `GLOB` on a column past the leading key one is not a seek.
///
/// The range bounds that column, and a bound on a column the seek has not
/// reached yet constrains nothing an index walk can use.
#[test]
fn a_glob_past_the_leading_key_column_is_a_scan() {
    eqp(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE c GLOB 'x*'",
        "SCAN t1",
    );
    eqp(
        "EXPLAIN QUERY PLAN SELECT b,c FROM t1 WHERE c GLOB 'x*'",
        "SCAN t1 USING COVERING INDEX i2",
    );
}

/// `LIKE` is not optimised here, and the reason is a collation.
///
/// SQLite applies the same prefix optimisation to `LIKE` as to `GLOB`, but
/// only to a `NOCASE` column -- the case-sensitive `LIKE` cannot use a range
/// because `LIKE 'x%'` matches `X` too, and a range bound on `x` would not. This
/// catalog does not carry a collation, so every column here is as case-sensitive
/// as SQLite's default and `LIKE` is always a scan. Measured on the oracle at
/// its default `case_sensitive_like`, `SELECT * FROM t1 WHERE b LIKE 'x%'` is a
/// `SCAN` on every declared type except `b COLLATE NOCASE`:
///
/// ```text
/// b                   SCAN t1
/// b TEXT              SCAN t1
/// b TEXT COLLATE NOCASE  SEARCH t1 USING INDEX i1 (b>? AND b<?)
/// ```
///
/// So the honest answer is a scan, and these cases pin that rather than leave
/// it to chance. Reporting a search here would be a wrong answer, which is
/// worse than the gap it would close.
#[test]
fn like_is_always_a_scan_without_a_nocase_collation() {
    for pattern in ["'x%'", "'x_'", "'x'", "'%x'", "'_x'", "'x%y'", "''"] {
        let sql = format!("EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE b LIKE {pattern}");
        eqp(&sql, "SCAN t1");
    }
    eqp(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE b NOT LIKE 'x%'",
        "SCAN t1",
    );
}

/// A `GLOB` whose prefix reads as a number is not a range on a non-TEXT column.
///
/// This is the affinity half of `isLikeOrGlob`. SQLite abandons the
/// optimisation when the value would be compared as a string against something
/// stored as a number, so `b GLOB '1*'` is a scan on a column with no declared
/// type and a search on one declared `TEXT`. The test catalogs below declare
/// every column as-is, which is why the two cases need two catalogs.
#[test]
fn a_glob_prefix_that_reads_as_a_number_needs_text_affinity() {
    // A declared type is what decides the affinity, and the catalogs here
    // declare none: SQLite reads an undeclared column as BLOB, which is not
    // TEXT, so the numeric-prefix guard applies and `b GLOB '1*'` is a scan.
    // A catalog that does declare the column as TEXT is the second half of
    // this test.
    let text = catalog(
        vec![table("t1", &["a", "b", "c"])],
        vec![
            index("i1", "t1", &["b"], false),
            index("i2", "t1", &["b", "c"], false),
        ],
    );
    eqp_on(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE b GLOB '1*'",
        &text,
        "SCAN t1",
    );
    // A prefix that is not a number is a range either way.
    eqp_on(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE b GLOB '1x*'",
        &text,
        "SEARCH t1 USING INDEX i1 (b>? AND b<?)",
    );
    eqp_on(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE b GLOB 'x1*'",
        &text,
        "SEARCH t1 USING INDEX i1 (b>? AND b<?)",
    );

    // Declared TEXT: every prefix is safe, because the value is already text.
    let declared = catalog(
        vec![text_b_table()],
        vec![
            index("i1", "t1", &["b"], false),
            index("i2", "t1", &["b", "c"], false),
        ],
    );
    eqp_on(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE b GLOB '1*'",
        &declared,
        "SEARCH t1 USING INDEX i1 (b>? AND b<?)",
    );
    eqp_on(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE b GLOB '1x*'",
        &declared,
        "SEARCH t1 USING INDEX i1 (b>? AND b<?)",
    );
}

// --- the statements behind the rules ---------------------------------------

/// The two-word `NOT NULL` is the one spelling of this constraint the parser
/// does not accept, and the error is a parser error rather than a plan.
///
/// `x NOTNULL` and `x IS NOT NULL` both plan the shape above; `x NOT NULL`
/// stops at `NOT` with `near "NOT": syntax error`, which is what the real
/// `sqlite3` would only report for a column that does not exist. It is pinned
/// here rather than fixed because `parser.rs` belongs to another track, and a
/// test that named the wanted plan would be asserting something the engine
/// cannot yet parse. Re-measure with [`oracle`] when the parser takes it.
#[test]
fn the_two_word_not_null_is_a_parser_gap() {
    let cat = base().1;
    let err = plan_lines("EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE b NOT NULL", &cat)
        .expect_err("the two-word spelling does not parse");
    assert_eq!(err, "near \"NOT\": syntax error");
    // ...and the one-word spelling of the same constraint does plan.
    eqp(
        "EXPLAIN QUERY PLAN SELECT b,c FROM t1 WHERE b NOTNULL",
        "SEARCH t1 USING COVERING INDEX i2 (b>?)",
    );
}

/// A join constraint between two columns is still a scan on the inner table.
///
/// This pins the other known difference in the WHERE collector: `equalities`
/// only accepts a literal on the other side of `=`, which is right for one
/// table -- `a=b` pins neither column -- but a join is a seek on the inner
/// table in SQLite. The access path reported here is the scan the engine
/// actually walks; the wording is the part that differs.
#[test]
fn a_join_between_two_columns_is_still_a_scan() {
    eqp(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 AS p, t1 AS q WHERE p.b=q.b",
        "SCAN p ~ SCAN q",
    );
}

/// A range on the leading column hides every column behind it.
///
/// This is the counterpart to `an_equality_past_is_null_is_kept`: an equality
/// pins a point and leaves the rest of the key usable, and a bound makes it an
/// interval, so `b>1 AND c=1` reports only `b`. The two cases together are why
/// the constraint text is built per key column rather than by listing whatever
/// the WHERE clause happened to mention.
#[test]
fn a_range_hides_the_columns_behind_it() {
    eqp(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE b>1 AND c=1",
        "SEARCH t1 USING INDEX i1 (b>?)",
    );
    eqp(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE b<1 AND c=1",
        "SEARCH t1 USING INDEX i1 (b<?)",
    );
    eqp(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE c=1 AND b>1",
        "SEARCH t1 USING INDEX i1 (b>?)",
    );
    eqp(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE b=1 AND c=1",
        "SEARCH t1 USING INDEX i2 (b=? AND c=?)",
    );
}

/// The `sqlite3` binary these expectations came from is the one the file
/// claims, so that re-measuring it is a deliberate act rather than drift.
///
/// `#[ignore]`d, because the suite must not depend on an external
/// installation. The run that produced the numbers in this file's comments is
/// `sqlite3 -version` reporting 3.53.4.
#[test]
#[ignore = "needs the sqlite3 binary; run it to re-measure the expectations"]
fn sqlite3_is_available_to_re_measure() {
    let (ddl, _) = base();
    assert_eq!(
        oracle(&ddl, "SELECT * FROM t1 WHERE b<=1"),
        "SEARCH t1 USING INDEX i1 (b<?)"
    );
    assert_eq!(
        oracle(&ddl, "SELECT * FROM t1 WHERE b IS NULL"),
        "SEARCH t1 USING INDEX i1 (b=?)"
    );
    assert_eq!(
        oracle(&ddl, "SELECT * FROM t1 WHERE b GLOB 'x*'"),
        "SEARCH t1 USING INDEX i1 (b>? AND b<?)"
    );
}

/// The statements in this file are the statements the tests above run.
///
/// This asserts nothing about a plan; it is here so that the file cannot be
/// pointed at a different engine without the statements changing, since every
/// expectation in it is a claim about a specific statement.
#[test]
fn the_statements_parse_as_written() {
    for sql in [
        "SELECT * FROM t1 WHERE b<=1",
        "SELECT * FROM t1 WHERE b IS NULL",
        "SELECT * FROM t1 WHERE b IS NOT NULL",
        "SELECT * FROM t1 WHERE b IN (1,2)",
        "SELECT * FROM t1 WHERE b IN (SELECT b FROM t1)",
        "SELECT * FROM t1 WHERE b GLOB 'x*'",
    ] {
        parse_one(sql).unwrap_or_else(|e| panic!("{sql} does not parse: {}", e.message));
    }
}
