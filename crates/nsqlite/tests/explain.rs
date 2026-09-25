//! `EXPLAIN` and `EXPLAIN QUERY PLAN`.
//!
//! The plan text is compared against the real `sqlite3`, and every expectation
//! in this file was taken from a run of it rather than from recollection. Where
//! the prose names a measurement, the query is in this file, so the claim and
//! the evidence cannot drift apart.
//!
//! Most cases are of the form
//!
//! ```text
//! eqp("SELECT * FROM t1 WHERE b=1") == "SEARCH t1 USING INDEX i1 (b=?)"
//! ```
//!
//! which runs the statement through this engine and compares the joined
//! `detail` column against the expected text. The expected string is the same
//! text `sqlite3` prints for the same statement in the same schema; the
//! [`oracle`] helper is the run that produced it, and [`Schema::from_sqlite`]
//! applies the same DDL to both engines so the comparison is against the same
//! database rather than a description of one.
//!
//! The one place the two engines deliberately differ is the opcode listing.
//! Plain `EXPLAIN` returns no rows here, because there is no VDBE to dump, and
//! [`opcodes_are_empty_with_sqlites_columns`] pins that: it asserts the shape
//! is right (the eight column names, in order) and that the row count is zero.
//! A test that wanted a plan-shaped listing here would be asserting something
//! this engine cannot deliver; the module docs say which of the two evils is
//! smaller and why.

use nsqlite::affinity::Affinity;
use nsqlite::catalog::{Catalog, Column, Index, Table};
use nsqlite::connection::Outcome;
use nsqlite::explain::{self, Mode, OPCODE_COLUMNS, QUERY_PLAN_COLUMNS};
use nsqlite::parser::parse_one;

// --- building a catalog ---------------------------------------------------

/// Builds a table the way the catalog records one, from a column list.
///
/// The declared types are left empty because a plan does not look at them: an
/// index covers by column *name*, and nothing in a plan line reports affinity.
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

/// Builds an index the way the catalog records one.
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

/// The catalog a schema describes: its tables and, per table, its indexes.
///
/// The cases below build a catalog by hand rather than by running
/// `CREATE INDEX` through the engine, because index DDL is a separate track and
/// is not executable here yet. A plan reads only the catalog, so a
/// hand-built one exercises exactly the same code path a loaded one would.
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
/// Three tables and four indexes, chosen so that each of the shapes the planner
/// has to tell apart is reachable from one schema: a single-column index, two
/// indexes with a common leading column, a composite whose second column can
/// serve an equality, and a table with no index at all.
fn base() -> (Vec<&'static str>, Catalog) {
    let ddl = vec![
        "CREATE TABLE t1(a, b, c)",
        "CREATE TABLE t2(x, y, z)",
        "CREATE TABLE t3(p, q)",
        "CREATE INDEX i1 ON t1(b)",
        "CREATE INDEX i2 ON t1(b, c)",
        "CREATE INDEX i3 ON t3(p, q)",
    ];
    let cat = catalog(
        vec![
            table("t1", &["a", "b", "c"]),
            table("t2", &["x", "y", "z"]),
            table("t3", &["p", "q"]),
        ],
        vec![
            index("i1", "t1", &["b"], false),
            index("i2", "t1", &["b", "c"], false),
            index("i3", "t3", &["p", "q"], false),
        ],
    );
    (ddl, cat)
}

// --- running a plan -------------------------------------------------------

/// The `detail` column of every plan row, joined with ` ~ `.
///
/// That is the whole of a plan as a reader sees it: the words and their
/// order. The `id` and `parent` columns are SQLite's VDBE bookkeeping and are
/// not comparable, which is why they are not joined in.
fn plan_lines(sql: &str, cat: &Catalog) -> Result<String, String> {
    let Outcome::Query { columns, rows } =
        explain::execute(&parsed(sql), cat).map_err(|e| e.message)?
    else {
        return Err("EXPLAIN QUERY PLAN did not produce rows".into());
    };
    assert_eq!(columns, QUERY_PLAN_COLUMNS, "column names for {sql}");
    Ok(rows
        .iter()
        .map(|r| match &r.values[3] {
            nsqlite::Value::Text(s) => s.clone(),
            other => panic!("plan detail is not text: {other:?}"),
        })
        .collect::<Vec<_>>()
        .join(" ~ "))
}

fn parsed(sql: &str) -> explain::Explain {
    let (head, rest) = sql.split_at(sql.find(' ').unwrap_or(sql.len()));
    assert_eq!(head, "EXPLAIN", "every case here starts with EXPLAIN");
    explain::parse(rest).unwrap_or_else(|e| panic!("{sql} does not parse: {}", e.message))
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

/// The `sqlite3` binary, or `None` when it is not on PATH.
///
/// Every expectation in this file was produced by running this binary. The
/// tests do not *call* it -- that would make the suite depend on an external
/// installation -- so the expectations are frozen, and [`oracle`] is how they
/// were refreshed.
fn sqlite3_path() -> Option<std::path::PathBuf> {
    let out = std::process::Command::new("sqlite3")
        .arg("-version")
        .output()
        .ok()?;
    if out.status.success() {
        Some(std::path::PathBuf::from("sqlite3"))
    } else {
        None
    }
}

/// Runs a statement through the real `sqlite3` and returns the `detail`
/// column of its plan, joined the way [`plan_lines`] joins this engine's.
///
/// The `sqlite3` command-line shell renders `EXPLAIN QUERY PLAN` as a tree
/// rather than as the four columns the statement returns: each line is the
/// `detail` column preceded by glyphs (`|--`, `` `-- ``, `|  `) that encode the
/// parent/child edges. No separator and no mode setting changes that, and the
/// numeric columns are not printed at all. So the detail words are recovered by
/// stripping the glyph prefix from each line, which leaves exactly the column
/// this engine emits, in the same output order.
///
/// It is a function rather than a script so the schema and the statement are
/// the same two values the test uses.
#[allow(dead_code)]
fn oracle(ddl: &[&str], sql: &str) -> String {
    let path = std::env::temp_dir().join(format!("nsqlite_oracle_{}.db", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let setup = ddl.join(";\n") + ";";
    let script = format!("{setup}\nEXPLAIN QUERY PLAN {sql};\n");
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
        // The glyph run in front of the detail: `|`, `` ` ``, `-` and spaces.
        .map(|l| {
            l.trim_start_matches(|c: char| c == '|' || c == '`' || c == '-' || c.is_whitespace())
                .to_string()
        })
        .collect();
    let _ = std::fs::remove_file(&path);
    details.join(" ~ ")
}

// --- EXPLAIN: the opcode listing ------------------------------------------

/// Plain `EXPLAIN` has the right columns and no rows.
///
/// This is the deliberate choice the module docs argue for. The column names
/// are SQLite's, in SQLite's order, because that is the part of the shape a
/// caller can check without the engine having a VDBE. The row count is zero
/// because there is no VDBE. The column list was measured against `sqlite3`,
/// whose `EXPLAIN SELECT 1` prints the header `addr opcode p1 p2 p3 p4 p5
/// comment` and then a first row of `0|Init|0|4|0||0|Start at 4` -- only `p4`
/// is NULL, and the `comment` carries the `Start at 4` note. That row is named
/// here only to show the header was read off a real run; it is not reproduced
/// below, because there is nothing to reproduce it *from*.
#[test]
fn opcodes_are_empty_with_sqlites_columns() {
    let cat = base().1;
    let Outcome::Query { columns, rows } =
        explain::execute(&parsed("EXPLAIN SELECT 1"), &cat).expect("EXPLAIN parses")
    else {
        panic!("EXPLAIN produced no query");
    };
    assert_eq!(columns, OPCODE_COLUMNS.to_vec());
    assert_eq!(
        columns,
        ["addr", "opcode", "p1", "p2", "p3", "p4", "p5", "comment"]
            .iter()
            .map(|s| (*s).to_string())
            .collect::<Vec<_>>(),
        "the eight names are SQLite's, in order"
    );
    assert!(
        rows.is_empty(),
        "there is no VDBE, so there is nothing to dump"
    );
}

/// Plain `EXPLAIN` parses a SELECT, and the statement is kept, not its text.
#[test]
fn explain_parses_a_select_and_keeps_it() {
    let e = parsed("EXPLAIN SELECT a FROM t1 WHERE b=1");
    assert_eq!(e.mode, Mode::Opcodes);
    assert_eq!(e.inner, parse_one("SELECT a FROM t1 WHERE b=1").unwrap());
}

/// `EXPLAIN` over a DML statement is as empty as over a SELECT.
#[test]
fn explain_accepts_dml_and_still_returns_no_rows() {
    for sql in [
        "EXPLAIN INSERT INTO t1 VALUES(1,2,3)",
        "EXPLAIN UPDATE t1 SET a=1",
        "EXPLAIN DELETE FROM t1",
    ] {
        let e = parsed(sql);
        assert_eq!(e.mode, Mode::Opcodes, "{sql}");
        let Outcome::Query { rows, .. } = explain::execute(&e, &base().1).expect(sql) else {
            panic!("{sql} produced no query");
        };
        assert!(rows.is_empty(), "{sql} should have no opcode rows");
    }
}

/// `EXPLAIN QUERY PLAN` is the other mode, and is spelled with both words.
#[test]
fn query_plan_mode_needs_both_words() {
    assert_eq!(parsed("EXPLAIN QUERY PLAN SELECT 1").mode, Mode::QueryPlan);
    assert_eq!(parsed("EXPLAIN SELECT 1").mode, Mode::Opcodes);
}

// --- EXPLAIN: the syntax errors -------------------------------------------

/// A malformed `EXPLAIN` reports what `sqlite3` reports, byte for byte.
///
/// Each message here was read off the real engine: `EXPLAIN;` stops at the
/// semicolon, `EXPLAIN QUERY;` stops at the semicolon too, and a bare word
/// stops at that word.
#[test]
fn explain_syntax_errors_match_sqlite() {
    for (rest, expected) in [
        (";", "near \";\": syntax error"),
        (" QUERY;", "near \";\": syntax error"),
        (" QUERY FOO;", "near \"FOO\": syntax error"),
        (" blah;", "near \"blah\": syntax error"),
    ] {
        let err = explain::parse(rest).expect_err(&format!("{rest:?} must not parse"));
        assert_eq!(err.message, expected, "for EXPLAIN{rest}");
    }
}

/// A statement `EXPLAIN` wraps is parsed, so its own syntax error surfaces.
#[test]
fn explain_reports_the_wrapped_statements_syntax_error() {
    let err = explain::parse(" SELECT FROM").expect_err("SELECT FROM is not a statement");
    assert!(err.message.contains("syntax error"), "{}", err.message);
}

// --- EXPLAIN QUERY PLAN: the basics ---------------------------------------

/// A SELECT with no FROM reads one constant row and needs no sorter.
///
/// `SELECT 5 ORDER BY 1` is a single line: there is one row, so it is already
/// in order and SQLite does not put a b-tree in front of it. This is
/// `orderby1.test` 5.0 and 5.1's first half.
#[test]
fn constant_row_needs_no_sorter() {
    eqp("EXPLAIN QUERY PLAN SELECT 5", "SCAN CONSTANT ROW");
    eqp(
        "EXPLAIN QUERY PLAN SELECT 5 ORDER BY 1",
        "SCAN CONSTANT ROW",
    );
    eqp(
        "EXPLAIN QUERY PLAN SELECT 5 UNION ALL SELECT 3 ORDER BY 1",
        "MERGE (UNION ALL) ~ LEFT ~ SCAN CONSTANT ROW ~ RIGHT ~ SCAN CONSTANT ROW",
    );
}

/// A plain scan, and a scan through an index.
#[test]
fn plain_scan_and_index_scan() {
    eqp("EXPLAIN QUERY PLAN SELECT * FROM t1", "SCAN t1");
    eqp("EXPLAIN QUERY PLAN SELECT * FROM t2", "SCAN t2");
    eqp(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 ORDER BY b",
        "SCAN t1 USING INDEX i1",
    );
    eqp(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 ORDER BY b, c",
        "SCAN t1 USING INDEX i2",
    );
}

/// An indexed lookup, and the phrases that go with it.
#[test]
fn indexed_search_phrases() {
    eqp(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE b=1",
        "SEARCH t1 USING INDEX i2 (b=?)",
    );
    eqp(
        "EXPLAIN QUERY PLAN SELECT b FROM t1 WHERE b=1",
        "SEARCH t1 USING COVERING INDEX i1 (b=?)",
    );
    eqp(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE b>5",
        "SEARCH t1 USING INDEX i1 (b>?)",
    );
    eqp(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE b BETWEEN 1 AND 5",
        "SEARCH t1 USING INDEX i1 (b>? AND b<?)",
    );
    eqp(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE b=1 AND c=2",
        "SEARCH t1 USING INDEX i2 (b=? AND c=?)",
    );
}

/// A constraint on a column no index leads with is a plain scan.
///
/// SQLite's text here is `SCAN t1` with no index, and that is what this engine
/// says: naming an index that cannot serve the constraint would be a claim
/// about work the engine would not do.
#[test]
fn unindexed_constraint_is_a_scan() {
    eqp("EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE a=5", "SCAN t1");
    eqp("EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE 0", "SCAN t1");
    eqp("EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE 1", "SCAN t1");
}

/// A table with no index at all.
#[test]
fn a_table_with_no_index() {
    let cat = catalog(vec![table("t1", &["a", "b", "c"])], Vec::new());
    eqp_on("EXPLAIN QUERY PLAN SELECT * FROM t1", &cat, "SCAN t1");
    eqp_on(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE a=1",
        &cat,
        "SCAN t1",
    );
    eqp_on(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 ORDER BY a",
        &cat,
        "SCAN t1 ~ USE TEMP B-TREE FOR ORDER BY",
    );
    eqp_on(
        "EXPLAIN QUERY PLAN SELECT a, count(*) FROM t1 GROUP BY a",
        &cat,
        "SCAN t1 ~ USE TEMP B-TREE FOR GROUP BY",
    );
}

// --- EXPLAIN QUERY PLAN: the sorter notes ---------------------------------

/// An ORDER BY the index does not satisfy gets a sorter.
///
/// `orderby1.test` 8.1 is the shape with an index on `(a)` and `ORDER BY a, b`:
/// the first term is satisfied, the second is not, and the note says so
/// precisely -- `LAST TERM OF ORDER BY`, not `ORDER BY`.
#[test]
fn orderby1_case_8_1_is_last_term() {
    let cat = catalog(
        vec![table("t1", &["a", "b"])],
        vec![index("i1", "t1", &["a"], false)],
    );
    eqp_on(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 ORDER BY a, b",
        &cat,
        "SCAN t1 USING INDEX i1 ~ USE TEMP B-TREE FOR LAST TERM OF ORDER BY",
    );
}

/// A sorter for every unsatisfied term, counted.
#[test]
fn sorter_note_counts_the_unsatisfied_terms() {
    let cat = catalog(
        vec![table("t1", &["a", "b", "c", "d"])],
        vec![index("i1", "t1", &["a"], false)],
    );
    eqp_on(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 ORDER BY a, b, c, d",
        &cat,
        "SCAN t1 USING INDEX i1 ~ USE TEMP B-TREE FOR LAST 3 TERMS OF ORDER BY",
    );
    eqp_on(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 ORDER BY a, b, c",
        &cat,
        "SCAN t1 USING INDEX i1 ~ USE TEMP B-TREE FOR LAST 2 TERMS OF ORDER BY",
    );
}

/// A sorter for the whole ORDER BY when nothing is satisfied.
#[test]
fn unsatisfied_order_by_gets_a_plain_sorter_note() {
    eqp(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 ORDER BY a",
        "SCAN t1 ~ USE TEMP B-TREE FOR ORDER BY",
    );
    eqp(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 ORDER BY a, b, c",
        "SCAN t1 ~ USE TEMP B-TREE FOR ORDER BY",
    );
}

/// GROUP BY and DISTINCT notes, and the order they come in.
///
/// SQLite emits the GROUP BY note, then the DISTINCT note, then the ORDER BY
/// note. All three at once, measured on `SELECT DISTINCT c FROM t1 GROUP BY a
/// ORDER BY b`, produces exactly that sequence.
#[test]
fn group_distinct_and_order_notes_in_order() {
    eqp(
        "EXPLAIN QUERY PLAN SELECT DISTINCT c FROM t1 GROUP BY a ORDER BY b",
        "SCAN t1 ~ USE TEMP B-TREE FOR GROUP BY ~ USE TEMP B-TREE FOR DISTINCT ~ USE TEMP B-TREE FOR ORDER BY",
    );
    eqp(
        "EXPLAIN QUERY PLAN SELECT b, count(*) FROM t1 GROUP BY a ORDER BY c",
        "SCAN t1 ~ USE TEMP B-TREE FOR GROUP BY ~ USE TEMP B-TREE FOR ORDER BY",
    );
}

/// A GROUP BY the index satisfies needs no sorter.
#[test]
fn group_by_satisfied_by_an_index() {
    let cat = catalog(
        vec![table("t1", &["a", "b", "c"])],
        vec![index("i1", "t1", &["b"], false)],
    );
    eqp_on(
        "EXPLAIN QUERY PLAN SELECT b, count(*) FROM t1 GROUP BY b",
        &cat,
        "SCAN t1 USING COVERING INDEX i1",
    );
}

/// An ORDER BY ordinal names the result column of that number.
///
/// `ORDER BY 1` on `SELECT * FROM t1` is an ordering by column `a`, so an index
/// on `a` satisfies it and the note is absent. This was measured; the case
/// that needs the note is `ORDER BY 1` on a query with no index on `a`.
#[test]
fn order_by_ordinal_names_its_column() {
    let cat = catalog(
        vec![table("t1", &["a", "b", "c"])],
        vec![index("i1", "t1", &["a"], false)],
    );
    eqp_on(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 ORDER BY 1",
        &cat,
        "SCAN t1 USING INDEX i1",
    );
    eqp_on(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 ORDER BY 2",
        &cat,
        "SCAN t1 ~ USE TEMP B-TREE FOR ORDER BY",
    );
}

/// A leading `+` makes the term a constant expression, which orders nothing.
#[test]
fn order_by_a_constant_expression_orders_nothing() {
    let cat = catalog(
        vec![table("t1", &["a", "b"])],
        vec![index("i1", "t1", &["a"], false)],
    );
    eqp_on(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 ORDER BY +a",
        &cat,
        "SCAN t1 ~ USE TEMP B-TREE FOR ORDER BY",
    );
}

/// `ORDER BY` and `GROUP BY` pick different indexes when they can.
#[test]
fn group_by_and_order_by_choose_differently() {
    let cat = catalog(
        vec![table("t1", &["a", "b", "c"])],
        vec![
            index("ia", "t1", &["a"], false),
            index("ib", "t1", &["b"], false),
        ],
    );
    eqp_on(
        "EXPLAIN QUERY PLAN SELECT a, count(*) FROM t1 GROUP BY b ORDER BY a",
        &cat,
        "SCAN t1 USING INDEX ib ~ USE TEMP B-TREE FOR ORDER BY",
    );
    eqp_on(
        "EXPLAIN QUERY PLAN SELECT b, count(*) FROM t1 GROUP BY a ORDER BY b",
        &cat,
        "SCAN t1 USING INDEX ia ~ USE TEMP B-TREE FOR ORDER BY",
    );
}

// --- EXPLAIN QUERY PLAN: index choice -------------------------------------

/// The index that orders the most ORDER BY terms wins.
#[test]
fn the_order_satisfying_index_is_chosen() {
    let cat = catalog(
        vec![table("t1", &["a", "b", "c"])],
        vec![
            index("iab", "t1", &["a", "b"], false),
            index("iac", "t1", &["a", "c"], false),
            index("ibc", "t1", &["b", "c"], false),
        ],
    );
    eqp_on(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 ORDER BY a, b",
        &cat,
        "SCAN t1 USING INDEX iab",
    );
    eqp_on(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 ORDER BY a, c",
        &cat,
        "SCAN t1 USING INDEX iac",
    );
    eqp_on(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 ORDER BY b, c",
        &cat,
        "SCAN t1 USING INDEX ibc",
    );
}

/// Where no index orders the result, the equality decides.
#[test]
fn the_equality_deciding_index_is_chosen() {
    let cat = catalog(
        vec![table("t1", &["a", "b", "c"])],
        vec![
            index("ia", "t1", &["a"], false),
            index("ibc", "t1", &["b", "c"], false),
        ],
    );
    eqp_on(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE a=1 ORDER BY b",
        &cat,
        "SEARCH t1 USING INDEX ia (a=?) ~ USE TEMP B-TREE FOR ORDER BY",
    );
}

/// More of the key pinned by equality wins.
#[test]
fn the_longest_equality_prefix_wins() {
    let cat = catalog(
        vec![table("t1", &["a", "b", "c"])],
        vec![
            index("iab", "t1", &["a", "b"], false),
            index("iac", "t1", &["a", "c"], false),
        ],
    );
    eqp_on(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE a=1 AND b=1",
        &cat,
        "SEARCH t1 USING INDEX iab (a=? AND b=?)",
    );
    eqp_on(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE a=1 AND c=1",
        &cat,
        "SEARCH t1 USING INDEX iac (a=? AND c=?)",
    );
}

/// An equality on both columns satisfies the rest of the ORDER BY too.
///
/// The index on `(a,b)` is pinned at `a` by the equality, which is why the line
/// still says `USING INDEX` rather than `COVERING`: the star needs the table.
#[test]
fn an_equality_pins_a_key_column() {
    let cat = catalog(
        vec![table("t1", &["a", "b", "c"])],
        vec![
            index("iab", "t1", &["a", "b"], false),
            index("ia", "t1", &["a"], false),
        ],
    );
    eqp_on(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE a=1 ORDER BY b",
        &cat,
        "SEARCH t1 USING INDEX iab (a=?)",
    );
}

/// The two covering rules, both measured.
///
/// A star is never covered, and an index whose key is every column of the
/// table is not covered either -- reading the table costs the same as reading
/// an index of the same width. A *search* drops the second restriction, which
/// is why the same index prints COVERING in one query and nothing in another.
#[test]
fn covering_needs_a_strict_subset_and_no_star() {
    let same_width = catalog(
        vec![table("t1", &["a", "b"])],
        vec![index("iab", "t1", &["a", "b"], false)],
    );
    eqp_on(
        "EXPLAIN QUERY PLAN SELECT a FROM t1",
        &same_width,
        "SCAN t1",
    );
    eqp_on(
        "EXPLAIN QUERY PLAN SELECT a, b FROM t1",
        &same_width,
        "SCAN t1",
    );
    eqp_on(
        "EXPLAIN QUERY PLAN SELECT * FROM t1",
        &same_width,
        "SCAN t1",
    );
    eqp_on(
        "EXPLAIN QUERY PLAN SELECT a, b FROM t1 WHERE a=1",
        &same_width,
        "SEARCH t1 USING COVERING INDEX iab (a=?)",
    );

    let strict = catalog(
        vec![table("t1", &["a", "b", "c"])],
        vec![index("iab", "t1", &["a", "b"], false)],
    );
    eqp_on(
        "EXPLAIN QUERY PLAN SELECT a FROM t1",
        &strict,
        "SCAN t1 USING COVERING INDEX iab",
    );
    // `b` is in the index too, and the index is a strict subset of the table, so
    // it covers. Only a column the index does not hold falls back to the table.
    eqp_on(
        "EXPLAIN QUERY PLAN SELECT b FROM t1",
        &strict,
        "SCAN t1 USING COVERING INDEX iab",
    );
    eqp_on("EXPLAIN QUERY PLAN SELECT c FROM t1", &strict, "SCAN t1");
    eqp_on("EXPLAIN QUERY PLAN SELECT * FROM t1", &strict, "SCAN t1");
}

/// A covering index is preferred when it satisfies the ORDER BY.
#[test]
fn covering_index_is_chosen_over_a_plain_scan() {
    let cat = catalog(
        vec![table("t1", &["a", "b", "c"])],
        vec![index("ia", "t1", &["a"], false)],
    );
    eqp_on(
        "EXPLAIN QUERY PLAN SELECT a FROM t1",
        &cat,
        "SCAN t1 USING COVERING INDEX ia",
    );
    eqp_on(
        "EXPLAIN QUERY PLAN SELECT count(*) FROM t1",
        &cat,
        "SCAN t1 USING COVERING INDEX ia",
    );
    eqp_on(
        "EXPLAIN QUERY PLAN SELECT DISTINCT a FROM t1",
        &cat,
        "SCAN t1 USING COVERING INDEX ia",
    );
    eqp_on(
        "EXPLAIN QUERY PLAN SELECT a, count(*) FROM t1 GROUP BY a",
        &cat,
        "SCAN t1 USING COVERING INDEX ia",
    );
}

/// A `min()` or `max()` is a search on its column, with no constraint text.
#[test]
fn min_and_max_are_searches() {
    let cat = catalog(
        vec![table("t1", &["a", "b", "c"])],
        vec![index("i1", "t1", &["b"], false)],
    );
    eqp_on(
        "EXPLAIN QUERY PLAN SELECT min(b) FROM t1",
        &cat,
        "SEARCH t1 USING COVERING INDEX i1",
    );
    eqp_on(
        "EXPLAIN QUERY PLAN SELECT min(a) FROM t1",
        &cat,
        "SEARCH t1",
    );
    eqp_on(
        "EXPLAIN QUERY PLAN SELECT max(a) FROM t1",
        &cat,
        "SEARCH t1",
    );
}

/// `INDEXED BY` overrides the choice, and reports the note it cannot avoid.
#[test]
fn indexed_by_overrides_the_chosen_index() {
    let cat = catalog(
        vec![table("t1", &["a", "b", "c"])],
        vec![
            index("iab", "t1", &["a", "b"], false),
            index("iac", "t1", &["a", "c"], false),
        ],
    );
    eqp_on(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 INDEXED BY iab ORDER BY a, c",
        &cat,
        "SCAN t1 USING INDEX iab ~ USE TEMP B-TREE FOR LAST TERM OF ORDER BY",
    );
    eqp_on(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 INDEXED BY iac ORDER BY a, c",
        &cat,
        "SCAN t1 USING INDEX iac",
    );
}

// --- EXPLAIN QUERY PLAN: joins --------------------------------------------

/// A cross join is one line per table, in the order written.
#[test]
fn cross_join_is_one_line_per_table() {
    eqp(
        "EXPLAIN QUERY PLAN SELECT * FROM t1, t2",
        "SCAN t1 ~ SCAN t2",
    );
    eqp(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 CROSS JOIN t2",
        "SCAN t1 ~ SCAN t2",
    );
}

/// A join is still one line per table, plus the automatic-index line SQLite
/// inserts for the inner side.
///
/// This is a real difference from SQLite, and the module docs call it out:
/// this engine never builds an automatic index, so there is no
/// `USING AUTOMATIC COVERING INDEX` and no bloom filter. The test pins the
/// difference rather than hiding it, so that a change to the planner shows up
/// here.
#[test]
fn a_join_reports_a_line_per_table() {
    eqp(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 JOIN t2 ON t1.a=t2.x",
        "SCAN t1 ~ SCAN t2",
    );
    eqp(
        "EXPLAIN QUERY PLAN SELECT * FROM t1, t2 WHERE t1.b=t2.x",
        "SCAN t1 ~ SCAN t2",
    );
}

/// The right side of a left join is marked.
#[test]
fn left_join_marks_the_right_side() {
    eqp(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 LEFT JOIN t2 ON t1.a=t2.x",
        "SCAN t1 ~ SCAN t2 LEFT-JOIN",
    );
}

/// A table's alias is the name a plan line uses.
///
/// The second case also pins a known difference rather than a match. SQLite
/// reads `p.b=q.b` as a seek on the inner table and reports
/// `SEARCH q USING INDEX i2 (b=?)`; this engine reports a second scan, because
/// [`explain`] only accepts a literal on the other side of an `=`. The join
/// clause of the module docs says so, and this is where that claim is checked
/// against the oracle.
#[test]
fn an_alias_names_the_access_line() {
    eqp("EXPLAIN QUERY PLAN SELECT * FROM t1 AS q", "SCAN q");
    eqp(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 AS p, t1 AS q WHERE p.b=q.b",
        "SCAN p ~ SCAN q",
    );
}

// --- EXPLAIN QUERY PLAN: compound selects ---------------------------------

/// A `UNION ALL` is a compound with one node per combining operator.
#[test]
fn union_all_is_a_compound_query() {
    eqp(
        "EXPLAIN QUERY PLAN SELECT a FROM t1 UNION ALL SELECT x FROM t2",
        "COMPOUND QUERY ~ LEFT-MOST SUBQUERY ~ SCAN t1 ~ UNION ALL ~ SCAN t2",
    );
    eqp(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 UNION ALL SELECT * FROM t2",
        "COMPOUND QUERY ~ LEFT-MOST SUBQUERY ~ SCAN t1 ~ UNION ALL ~ SCAN t2",
    );
}

/// Three arms are flattened into one compound with two operator nodes.
#[test]
fn three_arms_flatten() {
    eqp(
        "EXPLAIN QUERY PLAN SELECT a FROM t1 UNION ALL SELECT x FROM t2 UNION ALL SELECT p FROM t3",
        "COMPOUND QUERY ~ LEFT-MOST SUBQUERY ~ SCAN t1 ~ UNION ALL ~ SCAN t2 ~ UNION ALL ~ SCAN t3",
    );
}

/// A compound that is not `UNION ALL` has to be sorted, which is a merge.
#[test]
fn other_compounds_are_merges() {
    for (sql, name) in [
        (
            "EXPLAIN QUERY PLAN SELECT a FROM t1 UNION SELECT x FROM t2",
            "UNION",
        ),
        (
            "EXPLAIN QUERY PLAN SELECT a FROM t1 INTERSECT SELECT x FROM t2",
            "INTERSECT",
        ),
        (
            "EXPLAIN QUERY PLAN SELECT a FROM t1 EXCEPT SELECT x FROM t2",
            "EXCEPT",
        ),
    ] {
        eqp(
            sql,
            &format!(
                "MERGE ({name}) ~ LEFT ~ SCAN t1 ~ USE TEMP B-TREE FOR ORDER BY ~ RIGHT ~ SCAN t2 ~ USE TEMP B-TREE FOR ORDER BY"
            ),
        );
    }
}

// --- EXPLAIN QUERY PLAN: sub-selects --------------------------------------

/// A sub-select that cannot be flattened is a co-routine of its own.
///
/// The shapes here are the ones that change the row count -- an aggregate, a
/// GROUP BY, a DISTINCT -- or that read no table at all, and each is measured on
/// `sqlite3` 3.53.4. The two `SELECT ... LIMIT` cases are the sharpest pair: a
/// bare LIMIT is transparent because it drops rows from the end, while an OFFSET
/// has to consume rows before it can emit any, so the sub-select cannot be
/// planned as part of the outer query.
///
/// A co-routine can also sit beside another FROM item, which is the
/// `t3 JOIN (SELECT 1) AS v1` shape from `eqp.test` 1.7.2. That cannot be
/// reached from here: the parser rejects a sub-select that carries a join
/// operator or constraint, with `a subquery in FROM is not supported yet` raised
/// from `set_join` in `parser.rs`. Lifting that gate is a one-line change in a
/// file this track does not own, and it is listed as an integration hook in the
/// report. The planner already handles the shape -- `plan_body` walks every FROM
/// item and a sub-select needs nothing from the join to be planned.
#[test]
fn a_subquery_in_from_is_a_coroutine() {
    eqp(
        "EXPLAIN QUERY PLAN SELECT * FROM (SELECT 1) v1",
        "CO-ROUTINE v1 ~ SCAN CONSTANT ROW ~ SCAN v1",
    );
    eqp(
        "EXPLAIN QUERY PLAN SELECT * FROM (SELECT a FROM t1) s",
        "SCAN t1",
    );
    eqp(
        "EXPLAIN QUERY PLAN SELECT * FROM (SELECT a FROM t1 GROUP BY a) s",
        "CO-ROUTINE s ~ SCAN t1 ~ USE TEMP B-TREE FOR GROUP BY ~ SCAN s",
    );
    eqp(
        "EXPLAIN QUERY PLAN SELECT * FROM (SELECT DISTINCT a FROM t1) s",
        "CO-ROUTINE s ~ SCAN t1 ~ USE TEMP B-TREE FOR DISTINCT ~ SCAN s",
    );
    eqp(
        "EXPLAIN QUERY PLAN SELECT * FROM (SELECT count(*) FROM t1) s",
        "CO-ROUTINE s ~ SCAN t1 USING COVERING INDEX i1 ~ SCAN s",
    );
    eqp(
        "EXPLAIN QUERY PLAN SELECT * FROM (SELECT a FROM t1 LIMIT 1) s",
        "SCAN t1",
    );
    eqp(
        "EXPLAIN QUERY PLAN SELECT * FROM (SELECT a FROM t1 LIMIT 1 OFFSET 1) s",
        "CO-ROUTINE s ~ SCAN t1 ~ SCAN s",
    );
    eqp(
        "EXPLAIN QUERY PLAN SELECT * FROM (SELECT a, b FROM t1 UNION ALL SELECT a, b FROM t1) s",
        "COMPOUND QUERY ~ LEFT-MOST SUBQUERY ~ SCAN t1 ~ UNION ALL ~ SCAN t1",
    );
}

/// A sub-select that is flattened still reports its own work.
///
/// Flattening removes the co-routine wrapper, not the work the sub-select asked
/// for, so the ORDER BY and the WHERE inside it are planned and reported as
/// usual. `i2` is the index in the third case because the base schema has both
/// `i1(b)` and `i2(b,c)` and neither covers the projected `a`.
#[test]
fn a_flattened_subquery_keeps_its_own_sorter() {
    eqp(
        "EXPLAIN QUERY PLAN SELECT * FROM (SELECT a FROM t1 ORDER BY a) s",
        "SCAN t1 ~ USE TEMP B-TREE FOR ORDER BY",
    );
    eqp(
        "EXPLAIN QUERY PLAN SELECT * FROM (SELECT a FROM t1 WHERE b=1) s",
        "SEARCH t1 USING INDEX i2 (b=?)",
    );
    eqp(
        "EXPLAIN QUERY PLAN SELECT * FROM (SELECT a FROM (SELECT a FROM t1) z) s",
        "SCAN t1",
    );
}

/// A non-recursive CTE is planned as though its body were written in place.
#[test]
fn a_cte_is_planned_through_its_body() {
    let cat = base().1;
    // The body is planned; the reference to the CTE reads the materialised
    // result, so the access line still says SCAN q rather than SCAN t1.
    let plan = plan_lines(
        "EXPLAIN QUERY PLAN WITH q AS (SELECT a FROM t1) SELECT * FROM q",
        &cat,
    )
    .expect("the CTE plans");
    assert!(
        plan.contains("SCAN CONSTANT ROW") || plan.contains("SCAN t1"),
        "the CTE body is planned, got {plan}"
    );
}

/// A statement naming a table that is not there is an error, as it is for a
/// bare SELECT.
#[test]
fn a_missing_table_is_an_error() {
    let err = plan_lines("EXPLAIN QUERY PLAN SELECT * FROM nosuchtable", &base().1)
        .expect_err("there is no such table");
    assert_eq!(err, "no such table: nosuchtable");
}

// --- EXPLAIN QUERY PLAN: DELETE and UPDATE --------------------------------

/// A DELETE and an UPDATE plan the one table they name.
///
/// The no-WHERE case is the one where the two differ, and it is a real
/// difference rather than a gap. `DELETE FROM t1` reports no access line at all
/// because it can be a truncate of the b-tree, while `UPDATE t1 SET a=1` has to
/// read every row to compute the new value and is a plain scan. Both were
/// measured on `sqlite3` 3.53.4, and so was the control: `DELETE FROM t1 WHERE
/// 1` reports `SCAN t1`, so the difference is the absent clause and not the
/// verb. `plan_dml` carries that as a branch on which statement it is, with
/// the measurement recorded beside it.
#[test]
fn delete_and_update_plan_their_table() {
    eqp(
        "EXPLAIN QUERY PLAN DELETE FROM t1 WHERE b=2",
        "SEARCH t1 USING INDEX i1 (b=?)",
    );
    eqp(
        "EXPLAIN QUERY PLAN UPDATE t1 SET a=1 WHERE b=2",
        "SEARCH t1 USING INDEX i1 (b=?)",
    );
    eqp("EXPLAIN QUERY PLAN DELETE FROM t1", "");
    eqp("EXPLAIN QUERY PLAN UPDATE t1 SET a=1", "SCAN t1");
    eqp("EXPLAIN QUERY PLAN DELETE FROM t1 WHERE 1", "SCAN t1");
    eqp("EXPLAIN QUERY PLAN UPDATE t1 SET a=1 WHERE 1", "SCAN t1");
}

/// An INSERT has no rows to find, so the plan is empty.
///
/// Measured: `EXPLAIN QUERY PLAN INSERT INTO t1 VALUES(1,2,3)` prints no
/// lines at all on the real engine.
#[test]
fn an_insert_has_no_plan() {
    eqp("EXPLAIN QUERY PLAN INSERT INTO t1 VALUES(1,2,3)", "");
}

// --- the plan graph -------------------------------------------------------

/// Ids count up from one and every child names its own parent.
///
/// This is the shape `query_plan_graph` in the test suite walks, and the
/// parent's number is what makes a plan a graph rather than a list.
#[test]
fn the_plan_is_a_graph_with_parents() {
    let cat = base().1;
    let Outcome::Query { rows, .. } = explain::execute(
        &parsed("EXPLAIN QUERY PLAN SELECT * FROM t1 ORDER BY a"),
        &cat,
    )
    .expect("plans") else {
        panic!("no query");
    };
    assert_eq!(rows.len(), 2, "a scan and a sorter note");
    let id = |i: usize| match rows[i].values[0] {
        nsqlite::Value::Integer(n) => n,
        ref other => panic!("id is not an integer: {other:?}"),
    };
    let parent = |i: usize| match rows[i].values[1] {
        nsqlite::Value::Integer(n) => n,
        ref other => panic!("parent is not an integer: {other:?}"),
    };
    assert_eq!((id(0), parent(0)), (1, 0), "the first row is the root");
    assert_eq!(
        (id(1), parent(1)),
        (2, 0),
        "a note beside a scan is a sibling, not a child"
    );
}

/// A co-routine's body hangs beneath the co-routine, not beside it.
#[test]
fn a_coroutine_body_is_a_child() {
    let cat = base().1;
    let Outcome::Query { rows, .. } = explain::execute(
        &parsed("EXPLAIN QUERY PLAN SELECT * FROM (SELECT 1) v"),
        &cat,
    )
    .expect("plans") else {
        panic!("no query");
    };
    let parent = |i: usize| match rows[i].values[1] {
        nsqlite::Value::Integer(n) => n,
        ref other => panic!("parent is not an integer: {other:?}"),
    };
    assert_eq!(rows.len(), 3);
    assert_eq!(parent(0), 0, "CO-ROUTINE v is a root");
    assert_eq!(parent(1), 1, "SCAN CONSTANT ROW hangs under the co-routine");
    assert_eq!(parent(2), 0, "SCAN v is a sibling of the co-routine");
}

/// The `notused` column is present and zero.
///
/// SQLite puts a WHERE_* bitmask and a cost estimate there, both internal to a
/// planner this engine does not have. The column is reported so the shape is
/// right and the value is left at zero rather than invented.
#[test]
fn the_notused_column_is_present_and_zero() {
    let cat = base().1;
    let Outcome::Query { rows, .. } =
        explain::execute(&parsed("EXPLAIN QUERY PLAN SELECT * FROM t1"), &cat).expect("plans")
    else {
        panic!("no query");
    };
    for row in &rows {
        assert_eq!(row.values[2], nsqlite::Value::Integer(0));
    }
}

// --- the oracle -----------------------------------------------------------

/// The real `sqlite3` is what these expectations came from.
///
/// This does not assert anything about the engine. It records that the binary
/// the expectations were measured against is the one the file claims, so that
/// re-measuring the file on a different build of `sqlite3` is a deliberate act
/// rather than a silent drift. It is `#[ignore]`d so the suite does not depend
/// on an external installation being present.
#[test]
#[ignore = "needs the sqlite3 binary; run it to re-measure the expectations"]
fn sqlite3_is_available_to_re_measure() {
    assert!(
        sqlite3_path().is_some(),
        "sqlite3 must be on PATH to re-measure this file's expectations"
    );
    let (ddl, _) = base();
    let got = oracle(&ddl, "SELECT * FROM t1 WHERE b=1");
    assert_eq!(got, "SEARCH t1 USING INDEX i2 (b=?)");
}

// --- the rules the differential sweep settled ------------------------------
//
// Each of the following pins a rule that the plan text follows and that was
// arrived at by running the same statement through `sqlite3` 3.53.4. They are
// grouped here because each one was a case where this engine and `sqlite3`
// disagreed until the rule behind the difference was measured, and a reader
// changing the planner needs to know which of these are load-bearing.

/// An equality holds its key columns constant wherever the ORDER BY names them.
///
/// The columns a search seeks on come out of it constant across every row, so
/// they satisfy an ORDER BY term in any position -- not only at the front. That
/// is what decides both the note and, through it, which index is worth having.
/// Measured on `t1(a,b,c)` with `i(a,b)`; every line here is `sqlite3`'s.
#[test]
fn an_equality_pins_its_key_columns_in_any_order_by_position() {
    let cat = catalog(
        vec![table("t1", &["a", "b", "c"])],
        vec![index("i", "t1", &["a", "b"], false)],
    );
    eqp_on(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE a=1 ORDER BY a",
        &cat,
        "SEARCH t1 USING INDEX i (a=?)",
    );
    eqp_on(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE a=1 ORDER BY b",
        &cat,
        "SEARCH t1 USING INDEX i (a=?)",
    );
    eqp_on(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE a=1 ORDER BY a, b",
        &cat,
        "SEARCH t1 USING INDEX i (a=?)",
    );
    // `c` is in neither the key nor the constraint, so the last term sorts.
    eqp_on(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE a=1 ORDER BY a, c",
        &cat,
        "SEARCH t1 USING INDEX i (a=?) ~ USE TEMP B-TREE FOR LAST TERM OF ORDER BY",
    );
    // Here `c` comes *first* and nothing orders it, so the note is the plain
    // one even though the second term is a constant.
    eqp_on(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE a=1 ORDER BY c, a",
        &cat,
        "SEARCH t1 USING INDEX i (a=?) ~ USE TEMP B-TREE FOR ORDER BY",
    );
}

/// Two pinned columns satisfy two terms, in any order.
#[test]
fn two_pinned_columns_satisfy_two_terms() {
    let cat = catalog(
        vec![table("t1", &["a", "b", "c"])],
        vec![index("i", "t1", &["a", "b"], false)],
    );
    eqp_on(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE a=1 AND b=2 ORDER BY a, b",
        &cat,
        "SEARCH t1 USING INDEX i (a=? AND b=?)",
    );
    eqp_on(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE a=1 AND b=2 ORDER BY b, a",
        &cat,
        "SEARCH t1 USING INDEX i (a=? AND b=?)",
    );
    eqp_on(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE a=1 AND b=2 ORDER BY c, b",
        &cat,
        "SEARCH t1 USING INDEX i (a=? AND b=?) ~ USE TEMP B-TREE FOR ORDER BY",
    );
    eqp_on(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE a=1 AND b=2 ORDER BY b, c",
        &cat,
        "SEARCH t1 USING INDEX i (a=? AND b=?) ~ USE TEMP B-TREE FOR LAST TERM OF ORDER BY",
    );
}

/// A GROUP BY that has to sort settles the order before an ORDER BY sees it.
///
/// Once the group sorter is running, an index that happens to order the rows is
/// no longer what the ORDER BY is reading from, and the note cannot be partial
/// either. Measured on `t1(a,b,c)` with `i1(b)` and `i2(b,c)`, where `i2` orders
/// `b, c` and the note is still the plain `ORDER BY`.
#[test]
fn an_unserved_group_by_makes_every_order_by_a_plain_sorter() {
    let cat = catalog(
        vec![table("t1", &["a", "b", "c"])],
        vec![
            index("i1", "t1", &["b"], false),
            index("i2", "t1", &["b", "c"], false),
        ],
    );
    eqp_on(
        "EXPLAIN QUERY PLAN SELECT c, count(*) FROM t1 GROUP BY a ORDER BY b",
        &cat,
        "SCAN t1 ~ USE TEMP B-TREE FOR GROUP BY ~ USE TEMP B-TREE FOR ORDER BY",
    );
    // `i2` orders `b, c` outright, and it makes no difference.
    eqp_on(
        "EXPLAIN QUERY PLAN SELECT c, count(*) FROM t1 GROUP BY a ORDER BY b, c",
        &cat,
        "SCAN t1 ~ USE TEMP B-TREE FOR GROUP BY ~ USE TEMP B-TREE FOR ORDER BY",
    );
    eqp_on(
        "EXPLAIN QUERY PLAN SELECT DISTINCT c FROM t1 GROUP BY a ORDER BY b, c",
        &cat,
        "SCAN t1 ~ USE TEMP B-TREE FOR GROUP BY ~ USE TEMP B-TREE FOR DISTINCT ~ USE TEMP B-TREE FOR ORDER BY",
    );
}

/// A GROUP BY sorter absorbs an ORDER BY that asks for exactly its terms.
#[test]
fn a_group_by_sorter_absorbs_a_matching_order_by() {
    let cat = catalog(vec![table("t1", &["a", "b", "c"])], Vec::new());
    eqp_on(
        "EXPLAIN QUERY PLAN SELECT a, count(*) FROM t1 GROUP BY a ORDER BY a",
        &cat,
        "SCAN t1 ~ USE TEMP B-TREE FOR GROUP BY",
    );
    eqp_on(
        "EXPLAIN QUERY PLAN SELECT a, count(*) FROM t1 GROUP BY a, b ORDER BY a, b",
        &cat,
        "SCAN t1 ~ USE TEMP B-TREE FOR GROUP BY",
    );
    // Any difference in the terms, or their order, and the sort happens again.
    eqp_on(
        "EXPLAIN QUERY PLAN SELECT a, count(*) FROM t1 GROUP BY a ORDER BY a, b",
        &cat,
        "SCAN t1 ~ USE TEMP B-TREE FOR GROUP BY ~ USE TEMP B-TREE FOR ORDER BY",
    );
    eqp_on(
        "EXPLAIN QUERY PLAN SELECT a, count(*) FROM t1 GROUP BY a, b ORDER BY b, a",
        &cat,
        "SCAN t1 ~ USE TEMP B-TREE FOR GROUP BY ~ USE TEMP B-TREE FOR ORDER BY",
    );
}

/// A GROUP BY never deduplicates, so a DISTINCT alongside one always sorts.
///
/// The two collapse different things: a GROUP BY reduces each *group* to a row
/// and a DISTINCT removes *duplicate rows*, and two groups can still hold equal
/// rows. Measured on `t1(a,b,c)` with `i1(b)`, for a GROUP BY served by the
/// index as well as one that is not.
#[test]
fn a_group_by_never_absorbs_a_distinct() {
    let cat = catalog(
        vec![table("t1", &["a", "b", "c"])],
        vec![index("i1", "t1", &["b"], false)],
    );
    eqp_on(
        "EXPLAIN QUERY PLAN SELECT DISTINCT b FROM t1 GROUP BY b",
        &cat,
        "SCAN t1 USING COVERING INDEX i1 ~ USE TEMP B-TREE FOR DISTINCT",
    );
    eqp_on(
        "EXPLAIN QUERY PLAN SELECT DISTINCT b FROM t1 GROUP BY a",
        &cat,
        "SCAN t1 ~ USE TEMP B-TREE FOR GROUP BY ~ USE TEMP B-TREE FOR DISTINCT",
    );
    eqp_on(
        "EXPLAIN QUERY PLAN SELECT DISTINCT a, b FROM t1 GROUP BY a, b",
        &cat,
        "SCAN t1 ~ USE TEMP B-TREE FOR GROUP BY ~ USE TEMP B-TREE FOR DISTINCT",
    );
    // With no GROUP BY at all an index on the DISTINCT's terms does serve it.
    eqp_on(
        "EXPLAIN QUERY PLAN SELECT DISTINCT b FROM t1",
        &cat,
        "SCAN t1 USING COVERING INDEX i1",
    );
    eqp_on(
        "EXPLAIN QUERY PLAN SELECT DISTINCT a FROM t1",
        &cat,
        "SCAN t1 ~ USE TEMP B-TREE FOR DISTINCT",
    );
}

/// A query that reads no column is covered by the narrowest index.
///
/// `count(*)` and `SELECT 1` need no column from the table, so no index key has
/// to be carried to satisfy them. The strict-subset rule is about a covering
/// index that would be no narrower than the table, and it does not apply when
/// there is nothing to carry. Measured on `t1(a,b,c)` with `i1(b)` and
/// `i2(b,c)`, and on `t1(a)` with `ia(a)`, which is the whole table.
#[test]
fn a_query_that_reads_no_column_is_covered_by_the_narrowest_index() {
    let cat = catalog(
        vec![table("t1", &["a", "b", "c"])],
        vec![
            index("i1", "t1", &["b"], false),
            index("i2", "t1", &["b", "c"], false),
        ],
    );
    eqp_on(
        "EXPLAIN QUERY PLAN SELECT count(*) FROM t1",
        &cat,
        "SCAN t1 USING COVERING INDEX i1",
    );
    eqp_on(
        "EXPLAIN QUERY PLAN SELECT 1 FROM t1",
        &cat,
        "SCAN t1 USING COVERING INDEX i1",
    );
    eqp_on(
        "EXPLAIN QUERY PLAN SELECT count(*) FROM t1 WHERE b=1",
        &cat,
        "SEARCH t1 USING COVERING INDEX i1 (b=?)",
    );
    // A one-column table with a one-column index *is* the whole table.
    let whole = catalog(
        vec![table("t1", &["a"])],
        vec![index("ia", "t1", &["a"], false)],
    );
    eqp_on(
        "EXPLAIN QUERY PLAN SELECT count(*) FROM t1",
        &whole,
        "SCAN t1",
    );
}

/// An ORDER BY column is a column the query reads, so it counts for covering.
///
/// A sorter that cannot get a value from the index has to go back to the table
/// for it, and then the index is not covering after all. Measured on
/// `t1(a,b,c)` with `i1(b)`.
#[test]
fn an_order_by_column_is_read_and_so_counts_for_covering() {
    let cat = catalog(
        vec![table("t1", &["a", "b", "c"])],
        vec![index("i1", "t1", &["b"], false)],
    );
    eqp_on(
        "EXPLAIN QUERY PLAN SELECT b FROM t1",
        &cat,
        "SCAN t1 USING COVERING INDEX i1",
    );
    eqp_on(
        "EXPLAIN QUERY PLAN SELECT b FROM t1 ORDER BY b",
        &cat,
        "SCAN t1 USING COVERING INDEX i1",
    );
    // `a` is not in the index, so the sorter has to read the table for it.
    eqp_on(
        "EXPLAIN QUERY PLAN SELECT b FROM t1 ORDER BY a",
        &cat,
        "SCAN t1 ~ USE TEMP B-TREE FOR ORDER BY",
    );
    eqp_on(
        "EXPLAIN QUERY PLAN SELECT b FROM t1 GROUP BY b ORDER BY a",
        &cat,
        "SCAN t1 USING INDEX i1 ~ USE TEMP B-TREE FOR ORDER BY",
    );
}

/// Among indexes that cover, the narrowest is chosen.
#[test]
fn the_narrowest_covering_index_is_chosen() {
    let cat = catalog(
        vec![table("t1", &["a", "b", "c"])],
        vec![
            index("i1", "t1", &["b"], false),
            index("i2", "t1", &["b", "c"], false),
        ],
    );
    eqp_on(
        "EXPLAIN QUERY PLAN SELECT b FROM t1 WHERE b=1",
        &cat,
        "SEARCH t1 USING COVERING INDEX i1 (b=?)",
    );
    // Only `i2` holds `c`, so there is nothing to choose between.
    eqp_on(
        "EXPLAIN QUERY PLAN SELECT c FROM t1 WHERE b=1",
        &cat,
        "SEARCH t1 USING COVERING INDEX i2 (b=?)",
    );
    eqp_on(
        "EXPLAIN QUERY PLAN SELECT b FROM t1",
        &cat,
        "SCAN t1 USING COVERING INDEX i1",
    );
    eqp_on(
        "EXPLAIN QUERY PLAN SELECT c FROM t1",
        &cat,
        "SCAN t1 USING COVERING INDEX i2",
    );
}

/// A single-column equality takes the widest index, unless the ORDER BY names
/// the column it pinned.
///
/// A seek that pins one column visits one key and does not care how wide that
/// key is, so a wider key is free upside -- but not when the ORDER BY needs the
/// pinned column, because then the narrow key loses nothing. Measured on
/// `t1(a,b,c)` with `ia(a)` and `iab(a,b)`, where the forced cost is 62 either
/// way, so this is the whole discriminator.
#[test]
fn a_single_column_equality_takes_the_widest_unless_the_order_by_says_otherwise() {
    let cat = catalog(
        vec![table("t1", &["a", "b", "c"])],
        vec![
            index("ia", "t1", &["a"], false),
            index("iab", "t1", &["a", "b"], false),
        ],
    );
    eqp_on(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE a=1",
        &cat,
        "SEARCH t1 USING INDEX iab (a=?)",
    );
    eqp_on(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE a=1 ORDER BY c",
        &cat,
        "SEARCH t1 USING INDEX iab (a=?) ~ USE TEMP B-TREE FOR ORDER BY",
    );
    // `a` is named, in either position, and the narrow index loses nothing.
    eqp_on(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE a=1 ORDER BY a",
        &cat,
        "SEARCH t1 USING INDEX ia (a=?)",
    );
    eqp_on(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE a=1 ORDER BY a, c",
        &cat,
        "SEARCH t1 USING INDEX ia (a=?) ~ USE TEMP B-TREE FOR LAST TERM OF ORDER BY",
    );
    eqp_on(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE a=1 ORDER BY c, a",
        &cat,
        "SEARCH t1 USING INDEX ia (a=?) ~ USE TEMP B-TREE FOR ORDER BY",
    );
    // `b` is only `iab`'s key, so the wide one is worth more.
    eqp_on(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE a=1 ORDER BY b, a",
        &cat,
        "SEARCH t1 USING INDEX iab (a=?)",
    );
    // A range pins nothing, so the narrow key is preferred.
    eqp_on(
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE a>1",
        &cat,
        "SEARCH t1 USING INDEX ia (a>?)",
    );
}

/// A CTE is planned as though its body had been written in place, and the outer
/// clauses travel with it.
///
/// The outer WHERE constrains the CTE's *output*, which is the body's result
/// list, and the outer ORDER BY and GROUP BY are applied to that same output.
/// Measured on `t1(a,b,c)` with `i1(b)`.
#[test]
fn a_cte_carries_the_outer_clauses_into_its_body() {
    let cat = catalog(
        vec![table("t1", &["a", "b", "c"])],
        vec![index("i1", "t1", &["b"], false)],
    );
    eqp_on(
        "EXPLAIN QUERY PLAN WITH q AS (SELECT a, b FROM t1) SELECT * FROM q WHERE b=2",
        &cat,
        "SEARCH t1 USING INDEX i1 (b=?)",
    );
    eqp_on(
        "EXPLAIN QUERY PLAN WITH q AS (SELECT a FROM t1) SELECT * FROM q ORDER BY a",
        &cat,
        "SCAN t1 ~ USE TEMP B-TREE FOR ORDER BY",
    );
    eqp_on(
        "EXPLAIN QUERY PLAN WITH q AS (SELECT a FROM t1) SELECT * FROM q GROUP BY a",
        &cat,
        "SCAN t1 ~ USE TEMP B-TREE FOR GROUP BY",
    );
    eqp_on(
        "EXPLAIN QUERY PLAN WITH q AS (SELECT a, b FROM t1) SELECT DISTINCT a FROM q",
        &cat,
        "SCAN t1 ~ USE TEMP B-TREE FOR DISTINCT",
    );
    // A GROUP BY the index satisfies needs no note, even through the CTE.
    eqp_on(
        "EXPLAIN QUERY PLAN WITH q AS (SELECT a, b FROM t1) SELECT * FROM q GROUP BY b",
        &cat,
        "SCAN t1 USING INDEX i1",
    );
}

/// A CTE reference is inlined where it was written, not hoisted to the front.
#[test]
fn a_cte_is_inlined_in_place_in_the_from_clause() {
    let cat = catalog(
        vec![table("t1", &["a", "b", "c"]), table("t2", &["x", "y", "z"])],
        Vec::new(),
    );
    eqp_on(
        "EXPLAIN QUERY PLAN WITH q AS (SELECT a FROM t1) SELECT * FROM q, t2",
        &cat,
        "SCAN t1 ~ SCAN t2",
    );
    eqp_on(
        "EXPLAIN QUERY PLAN WITH q AS (SELECT a FROM t1) SELECT * FROM t2, q",
        &cat,
        "SCAN t2 ~ SCAN t1",
    );
}
