//! `ORDER BY` with a descending term, and the arm of a merge.
//!
//! Two rules that only show up once a term asks for a direction, both measured
//! against the real `sqlite3` on `t1(a,b,c)` with `i1(b)`, `i2(b,c)` and `i3(c)`,
//! all declared ASC:
//!
//! * **A direction mismatch costs exactly one term.** An index delivers its
//!   keys one way, but a scan can be run backwards, so the mismatched term is
//!   still delivered -- and nothing behind it is, because those terms would come
//!   out reversed too. `ORDER BY b DESC` is a bare `SCAN t1 USING INDEX i1`;
//!   `ORDER BY b DESC, c` is the same index with `LAST TERM OF ORDER BY`.
//! * **A merge arm may be read through an index that covers the whole table.**
//!   The strict rule for a standalone scan refuses a covering index as wide as
//!   the table, because it gains nothing there. Inside a merge the arm is read
//!   anyway, so the same index counts as covering.

use nsqlite::affinity::Affinity;
use nsqlite::catalog::{Catalog, Column, Index, Table};
use nsqlite::connection::Outcome;
use nsqlite::explain::{self, Mode};
use nsqlite::value::Value;

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

fn index(name: &str, table: &str, columns: &[&str]) -> Index {
    Index {
        name: name.to_string(),
        table: table.to_string(),
        columns: columns.iter().map(|c| (*c).to_string()).collect(),
        ascending: columns.iter().map(|_| true).collect(),
        unique: false,
        root_page: 0,
    }
}

/// `t1(a,b,c)` with `i1(b)`, `i2(b,c)`, `i3(c)`, and `t2(x,y)` with `i4(x,y)`.
fn base() -> Catalog {
    let mut c = Catalog::new();
    c.put(table("t1", &["a", "b", "c"]));
    c.put(table("t2", &["x", "y"]));
    c.put_index(index("i1", "t1", &["b"]));
    c.put_index(index("i2", "t1", &["b", "c"]));
    c.put_index(index("i3", "t1", &["c"]));
    c.put_index(index("i4", "t2", &["x", "y"]));
    c
}

/// The `detail` column of every plan row, joined with ` ~ `.
#[track_caller]
fn plan(sql: &str) -> String {
    let e = explain::parse(&format!(" QUERY PLAN {sql}")).expect("the statement parses");
    assert_eq!(e.mode, Mode::QueryPlan, "{sql} asks for a query plan");
    let Outcome::Query { rows, .. } = explain::execute(&e, &base()).expect("the plan is built")
    else {
        panic!("EXPLAIN QUERY PLAN did not produce rows");
    };
    rows.iter()
        .map(|r| match &r.values[3] {
            Value::Text(s) => s.clone(),
            other => panic!("plan detail is not text: {other:?}"),
        })
        .collect::<Vec<_>>()
        .join(" ~ ")
}

#[track_caller]
fn eqp(sql: &str, expected: &str) {
    assert_eq!(plan(sql), expected, "plan for {sql}");
}

/// A single descending term is served by an ascending key: the scan runs
/// backwards and there is nothing behind it to disturb.
#[test]
fn one_descending_term_is_served_by_reversing_the_scan() {
    eqp("SELECT * FROM t1 ORDER BY b DESC", "SCAN t1 USING INDEX i1");
}

/// The same term followed by another costs the rest of the order, because a
/// reversed scan cannot also deliver the terms behind the reversed one.
#[test]
fn a_descending_term_costs_the_terms_behind_it() {
    eqp(
        "SELECT * FROM t1 ORDER BY b DESC, c",
        "SCAN t1 USING INDEX i1 ~ USE TEMP B-TREE FOR LAST TERM OF ORDER BY",
    );
}

/// The direction that mismatches does not have to be the first one.
#[test]
fn the_mismatch_can_be_in_any_position() {
    eqp(
        "SELECT * FROM t1 ORDER BY b, c DESC",
        "SCAN t1 USING INDEX i1 ~ USE TEMP B-TREE FOR LAST TERM OF ORDER BY",
    );
}

/// An all-ascending order of the same columns is served outright by the wider
/// index, with no sorter -- which is what makes the narrow choice above a
/// consequence of the direction rather than of the width.
#[test]
fn an_all_ascending_order_uses_the_wider_index_and_no_sorter() {
    eqp("SELECT * FROM t1 ORDER BY b, c", "SCAN t1 USING INDEX i2");
}

/// A leading term that no index can name falls back to the table, and the
/// sorter note is the plain one because nothing was served.
#[test]
fn a_term_no_index_serves_leaves_a_bare_scan() {
    eqp(
        "SELECT * FROM t1 ORDER BY a",
        "SCAN t1 ~ USE TEMP B-TREE FOR ORDER BY",
    );
}

/// A merge arm is read through a whole-table index, which a standalone scan of
/// the same table would refuse.
#[test]
fn a_merge_arm_may_be_read_through_a_whole_table_index() {
    eqp(
        "SELECT x FROM t2 UNION SELECT a FROM t1",
        "MERGE (UNION) ~ LEFT ~ SCAN t2 USING COVERING INDEX i4 ~ RIGHT ~ SCAN t1 ~ USE TEMP B-TREE FOR ORDER BY",
    );
}

/// The same index and the same columns, outside a merge, is a bare scan.
#[test]
fn outside_a_merge_the_same_index_is_refused() {
    eqp("SELECT x FROM t2", "SCAN t2");
}

/// A merge whose ordering names a column the arm does not have still keeps the
/// covering access, because coverage and ordering are separate questions.
#[test]
fn coverage_holds_even_when_the_order_cannot_be_served() {
    eqp(
        "SELECT a FROM t1 UNION ALL SELECT x FROM t2 ORDER BY 1",
        "MERGE (UNION ALL) ~ LEFT ~ SCAN t1 ~ USE TEMP B-TREE FOR ORDER BY ~ RIGHT ~ SCAN t2 USING COVERING INDEX i4",
    );
}
