//! Tests for `CREATE INDEX` and `DROP INDEX`.
//!
//! The oracle throughout is sqlite3 3.53.4: every expected message and every
//! expected value was produced by running the same statement against the real
//! `sqlite3` binary and is quoted in the comment above the test that checks it.
//!
//! Most of these do not assert a fixed expectation. They build an index, change
//! the table, and then compare **the index against the table** — because a test
//! with a hard-coded answer can be passed by a tree that is consistently wrong
//! in the same way, and a stale index is exactly that: it is not an error, it
//! is a wrong answer.

use super::*;
use crate::catalog::Column;
use crate::pager::Pager;
use std::path::PathBuf;

/// A database in the temp directory, removed on drop unless it is kept.
struct Db {
    path: PathBuf,
}

impl Db {
    fn new(tag: &str) -> Db {
        let path = std::env::temp_dir().join(format!(
            "nsqlite-ddl-{}-{}-{tag}.db",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        let _ = std::fs::remove_file(&path);
        Db { path }
    }

    fn open(&self) -> Pager {
        Pager::open(&self.path).expect("open pager")
    }
}

impl Drop for Db {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// A table in the catalog, written to page `root`.
fn table(name: &str, columns: &[&str], root: u32) -> Table {
    Table {
        name: name.to_owned(),
        columns: columns
            .iter()
            .map(|c| Column {
                name: (*c).to_owned(),
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
        root_page: root,
    // An ordinary table: no module hosts it.
    virtual_module: None,
    // Declared by hand here, so there is no DDL and no CHECKs to carry.
    checks: Vec::new(),
    }
}

/// Writes a page of rows into a table and returns the catalog.
///
/// The tree's root moves when the table splits, so the *live* root is read back
/// before the table is recorded. Keeping the first one is the mistake this
/// helper exists to avoid: a catalog pointing at a leaf that is no longer the
/// root silently loses rows, and a test that then builds an index over it
/// indexes 181 of 1000 rows and never notices.
fn with_rows(db: &Db, table_name: &str, columns: &[&str], rows: &[(i64, Vec<Value>)]) -> Catalog {
    let mut pager = db.open();
    let root = pager.allocate().unwrap();
    crate::btree_write::LeafPage::empty(root, pager.page_size())
        .write_to(&mut pager)
        .unwrap();
    let mut tree = crate::table_tree::TableTree::open(&mut pager, root).unwrap();
    for (rowid, values) in rows {
        tree.insert(
            &mut pager,
            &crate::table_tree::Row {
                rowid: *rowid,
                values: values.clone(),
            },
        )
        .unwrap();
    }
    let live_root = tree.root();
    let mut catalog = Catalog::new();
    catalog.put(table(table_name, columns, live_root));
    pager.flush().unwrap();
    catalog
}

/// Writes rows into a table and returns the catalog *and* the live root page.
fn with_rows_root(
    db: &Db,
    table_name: &str,
    columns: &[&str],
    rows: &[(i64, Vec<Value>)],
) -> (Catalog, u32) {
    let catalog = with_rows(db, table_name, columns, rows);
    let root = catalog.get(table_name).unwrap().root_page;
    (catalog, root)
}

/// A one-column index, spelled the way `build_index` takes it.
fn cols(names: &[&str]) -> Vec<(String, bool)> {
    names.iter().map(|n| ((*n).to_owned(), true)).collect()
}

/// The pairs an index holds, as `key0 -> rowid` strings, which is what the
/// cross-check tests compare.
fn pairs_as_text(pager: &mut Pager, root: u32, key_len: usize) -> Vec<String> {
    read_pairs(pager, root, key_len)
        .unwrap()
        .iter()
        .map(|(k, r)| format!("{}#{r}", render_key(k)))
        .collect()
}

fn render_key(k: &[Value]) -> String {
    k.iter()
        .map(|v| match v {
            Value::Null => "NULL".to_string(),
            Value::Integer(i) => i.to_string(),
            Value::Real(r) => Value::real(*r).to_string(),
            Value::Text(s) => format!("'{s}'"),
            Value::TextBytes(b) => format!("x'{}", b.iter().map(|x| format!("{x:02X}")).collect::<String>()),
            Value::Blob(b) => format!(
                "x'{}'",
                b.iter().map(|x| format!("{x:02X}")).collect::<String>()
            ),
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// The pairs a table *should* produce, derived from its own contents.
///
/// This is the cross-check: the index is compared against a fresh walk of the
/// table, not against a list written out by hand, so a test passes only if the
/// index agrees with the data it indexes.
fn table_pairs(pager: &mut Pager, table: &Table, column_positions: &[usize]) -> Vec<String> {
    let mut tree = crate::table_tree::TableTree::open(pager, table.root_page).unwrap();
    let rows = tree.scan(pager).unwrap();
    let mut out: Vec<(Vec<Value>, i64)> = rows
        .iter()
        .map(|r| {
            (
                column_positions
                    .iter()
                    .map(|&c| r.values.get(c).cloned().unwrap_or(Value::Null))
                    .collect(),
                r.rowid,
            )
        })
        .collect();
    out.sort_by(|a, b| {
        let ord = crate::index::compare(
            &IndexEntry {
                key: a.0.clone(),
                rowid: a.1,
            },
            &IndexEntry {
                key: b.0.clone(),
                rowid: b.1,
            },
            a.0.len(),
        );
        ord
    });
    out.iter()
        .map(|(k, r)| format!("{}#{r}", render_key(k)))
        .collect()
}

// --- 1. Building an index from a table -------------------------------------

#[test]
fn a_new_index_holds_one_entry_per_row() {
    // sqlite3: CREATE TABLE t(a,b); 3 rows; CREATE INDEX i1 ON t(a);
    //          -> an index whose entries are (1,1) (2,2) (3,3)
    let db = Db::new("build-basic");
    let rows = vec![
        (1i64, vec![Value::Integer(3), Value::Integer(30)]),
        (2, vec![Value::Integer(1), Value::Integer(10)]),
        (3, vec![Value::Integer(2), Value::Integer(20)]),
    ];
    let catalog = with_rows(&db, "t", &["a", "b"], &rows);
    let mut pager = db.open();
    let index = build_index(&mut pager, &catalog, "i1", "t", &cols(&["a"]), false, false)
        .unwrap()
        .unwrap();
    assert_eq!(index.root_page, 3, "sqlite3 puts the first index at page 3");
    let got = pairs_as_text(&mut pager, index.root_page, 1);
    assert_eq!(
        got,
        vec!["1#2", "2#3", "3#1"],
        "entries come back in key order"
    );
}

#[test]
fn a_new_index_holds_one_entry_per_row_in_the_same_order_the_table_has() {
    // The build walks the table in rowid order and the tree sorts as it goes,
    // so the insertion order must not be visible in the result.
    let db = Db::new("build-order");
    let rows: Vec<(i64, Vec<Value>)> = (1..=20)
        .rev()
        .map(|i| (i, vec![Value::Integer(20 - i), Value::Integer(i)]))
        .collect();
    let catalog = with_rows(&db, "t", &["a", "b"], &rows);
    let mut pager = db.open();
    let index = build_index(&mut pager, &catalog, "i1", "t", &cols(&["a"]), false, false)
        .unwrap()
        .unwrap();
    let t = catalog.get("t").unwrap();
    assert_eq!(
        pairs_as_text(&mut pager, index.root_page, 1),
        table_pairs(&mut pager, t, &[0]),
        "the index must agree with the table it indexes"
    );
}

#[test]
fn a_composite_index_holds_every_key_column_in_order() {
    // sqlite3: CREATE INDEX i2 ON t(a,b) -> entries are (a,b,rowid)
    let db = Db::new("build-composite");
    let rows = vec![
        (1i64, vec![Value::Integer(1), Value::Integer(2)]),
        (2, vec![Value::Integer(1), Value::Integer(1)]),
        (3, vec![Value::Integer(0), Value::Integer(9)]),
    ];
    let catalog = with_rows(&db, "t", &["a", "b"], &rows);
    let mut pager = db.open();
    let index = build_index(
        &mut pager,
        &catalog,
        "i2",
        "t",
        &cols(&["a", "b"]),
        false,
        false,
    )
    .unwrap()
    .unwrap();
    let got = pairs_as_text(&mut pager, index.root_page, 2);
    assert_eq!(got, vec!["0,9#3", "1,1#2", "1,2#1"]);
}

#[test]
fn a_null_in_the_key_is_stored_as_null() {
    // sqlite3: INSERT INTO t VALUES(NULL,1) -> the index entry's key is NULL.
    let db = Db::new("build-null");
    let rows = vec![
        (1i64, vec![Value::Null, Value::Integer(1)]),
        (2, vec![Value::Integer(1), Value::Integer(2)]),
    ];
    let catalog = with_rows(&db, "t", &["a", "b"], &rows);
    let mut pager = db.open();
    let index = build_index(&mut pager, &catalog, "i1", "t", &cols(&["a"]), false, false)
        .unwrap()
        .unwrap();
    let got = read_pairs(&mut pager, index.root_page, 1).unwrap();
    assert_eq!(got[0].0, vec![Value::Null], "NULL sorts below 1");
    assert_eq!(got[0].1, 1);
}

#[test]
fn a_text_key_sorts_above_a_number() {
    // sqlite3 3.53.4, on a table holding a text, a blob, a NULL, a real and an
    // integer in one column, the index leaf holds these records in this order:
    //
    //   03 00 01 03          NULL,   rowid 3
    //   03 07 01 3f f8 ...   1.5,    rowid 4
    //   03 01 01 02 05       2,      rowid 5
    //   03 0f 09 62          'b',    rowid 1
    //   03 0e 01 ff 02       x'FF',  rowid 2
    //
    // So the class order is NULL < numbers < text < blob, and within the
    // numbers 1.5 and 2 interleave by value rather than by type.
    let db = Db::new("build-types");
    let rows = vec![
        (1i64, vec![Value::Text("b".into())]),
        (2, vec![Value::Blob(vec![0xff])]),
        (3, vec![Value::Null]),
        (4, vec![Value::Real(1.5)]),
        (5, vec![Value::Integer(2)]),
    ];
    let catalog = with_rows(&db, "t", &["a"], &rows);
    let mut pager = db.open();
    let index = build_index(&mut pager, &catalog, "i1", "t", &cols(&["a"]), false, false)
        .unwrap()
        .unwrap();
    assert_eq!(
        pairs_as_text(&mut pager, index.root_page, 1),
        vec!["NULL#3", "1.5#4", "2#5", "'b'#1", "x'FF'#2"],
        "the same order the reference wrote"
    );
}

#[test]
fn an_index_over_an_empty_table_still_has_a_root() {
    // sqlite3: CREATE TABLE t(a); CREATE INDEX i ON t(a);
    //          -> page_count 3, so the index has a page even with no rows.
    let db = Db::new("build-empty");
    let catalog = with_rows(&db, "t", &["a"], &[]);
    let mut pager = db.open();
    let before = pager.page_count();
    let index = build_index(&mut pager, &catalog, "i1", "t", &cols(&["a"]), false, false)
        .unwrap()
        .unwrap();
    assert_eq!(pager.page_count(), before + 1);
    let entries = read_entries(&mut pager, index.root_page, 1).unwrap();
    assert!(entries.is_empty());
    // And the root is a real b-tree page, not the zeroes `allocate` returns:
    // a page of zeroes reads as a freelist trunk.
    let page = pager.read_page(index.root_page).unwrap();
    assert_eq!(page[0], page_type::INDEX_LEAF);
}

#[test]
fn an_index_over_a_thousand_rows_splits_and_still_agrees_with_the_table() {
    // Past a single leaf, so the interior layer does the work.
    let db = Db::new("build-large");
    let rows: Vec<(i64, Vec<Value>)> = (1..=1000)
        .map(|i| (i, vec![Value::Integer(1000 - i), Value::Integer(i)]))
        .collect();
    let catalog = with_rows(&db, "t", &["a"], &rows);
    let mut pager = db.open();
    let index = build_index(&mut pager, &catalog, "i1", "t", &cols(&["a"]), false, false)
        .unwrap()
        .unwrap();
    let pages = tree_pages(&mut pager, index.root_page).unwrap();
    assert!(
        pages.len() > 1,
        "1000 entries cannot fit one page, yet the tree has {}",
        pages.len()
    );
    let t = catalog.get("t").unwrap();
    assert_eq!(
        pairs_as_text(&mut pager, index.root_page, 1),
        table_pairs(&mut pager, t, &[0]),
        "a split index must still agree with the table"
    );
}

// --- 2. UNIQUE --------------------------------------------------------------

#[test]
fn a_unique_index_over_duplicate_data_is_refused_with_the_column_named() {
    // sqlite3:
    //   CREATE TABLE t(a,b,c); INSERT INTO t VALUES(1,2,3);
    //   INSERT INTO t VALUES(1,2,4);
    //   CREATE UNIQUE INDEX i5 ON t(a);
    //   -> UNIQUE constraint failed: t.a
    let db = Db::new("unique-single");
    let rows = vec![
        (
            1i64,
            vec![Value::Integer(1), Value::Integer(2), Value::Integer(3)],
        ),
        (
            2,
            vec![Value::Integer(1), Value::Integer(2), Value::Integer(4)],
        ),
    ];
    let catalog = with_rows(&db, "t", &["a", "b", "c"], &rows);
    let mut pager = db.open();
    let e = build_index(&mut pager, &catalog, "i5", "t", &cols(&["a"]), true, false).unwrap_err();
    assert_eq!(e.message, "UNIQUE constraint failed: t.a");
    assert_eq!(e.code, ResultCode::Constraint);
    assert_eq!(e.extended_code(), 2067, "SQLITE_CONSTRAINT_UNIQUE");
}

#[test]
fn a_refused_unique_index_names_every_key_column() {
    // sqlite3:
    //   CREATE TABLE t(a,b); INSERT INTO t VALUES(1,2); INSERT INTO t VALUES(1,2);
    //   CREATE UNIQUE INDEX i ON t(a,b);
    //   -> UNIQUE constraint failed: t.a, t.b
    let db = Db::new("unique-tuple");
    let rows = vec![
        (1i64, vec![Value::Integer(1), Value::Integer(2)]),
        (2, vec![Value::Integer(1), Value::Integer(2)]),
    ];
    let catalog = with_rows(&db, "t", &["a", "b"], &rows);
    let mut pager = db.open();
    let e = build_index(
        &mut pager,
        &catalog,
        "i",
        "t",
        &cols(&["a", "b"]),
        true,
        false,
    )
    .unwrap_err();
    assert_eq!(e.message, "UNIQUE constraint failed: t.a, t.b");
}

#[test]
fn a_unique_index_over_distinct_data_is_accepted() {
    let db = Db::new("unique-ok");
    let rows = vec![
        (1i64, vec![Value::Integer(1), Value::Integer(2)]),
        (2, vec![Value::Integer(1), Value::Integer(3)]),
        (3, vec![Value::Integer(2), Value::Integer(2)]),
    ];
    let catalog = with_rows(&db, "t", &["a", "b"], &rows);
    let mut pager = db.open();
    let index = build_index(
        &mut pager,
        &catalog,
        "i",
        "t",
        &cols(&["a", "b"]),
        true,
        false,
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        read_entries(&mut pager, index.root_page, 2).unwrap().len(),
        3
    );
}

#[test]
fn a_null_key_never_collides() {
    // sqlite3: CREATE TABLE t(a); CREATE UNIQUE INDEX u ON t(a);
    //          INSERT INTO t VALUES(NULL); INSERT INTO t VALUES(NULL);
    //          -> both accepted (rc=0, count(*)=2)
    let db = Db::new("unique-null");
    let rows = vec![
        (1i64, vec![Value::Null]),
        (2, vec![Value::Null]),
        (3, vec![Value::Integer(1)]),
    ];
    let catalog = with_rows(&db, "t", &["a"], &rows);
    let mut pager = db.open();
    let index = build_index(&mut pager, &catalog, "u", "t", &cols(&["a"]), true, false)
        .unwrap()
        .unwrap();
    assert_eq!(
        read_entries(&mut pager, index.root_page, 1).unwrap().len(),
        3
    );
}

#[test]
fn a_null_in_a_composite_key_never_collides_either() {
    // sqlite3: CREATE UNIQUE INDEX u ON t(a,b);
    //   (1,NULL), (1,NULL), (1,2) all accepted; a second (1,2) refused.
    let db = Db::new("unique-null-composite");
    let rows = vec![
        (1i64, vec![Value::Integer(1), Value::Null]),
        (2, vec![Value::Integer(1), Value::Null]),
        (3, vec![Value::Integer(1), Value::Integer(2)]),
    ];
    let mut catalog = with_rows(&db, "t", &["a", "b"], &rows);
    let mut pager = db.open();
    let index = build_index(
        &mut pager,
        &catalog,
        "u",
        "t",
        &cols(&["a", "b"]),
        true,
        false,
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        read_entries(&mut pager, index.root_page, 2).unwrap().len(),
        3
    );
    // The catalog has to know about the index before a write will maintain
    // it, which is the connection's job after a CREATE.
    catalog.put_index(index);

    // A fourth row, (1,2) again, must be refused — the NULL exemption is for
    // keys holding a NULL, not for all collisions.
    let e = index_insert(
        &mut pager,
        &catalog,
        catalog.get("t").unwrap(),
        4,
        &[Value::Integer(1), Value::Integer(2)],
    )
    .unwrap_err();
    assert_eq!(e.message, "UNIQUE constraint failed: t.a, t.b");
}

#[test]
fn a_non_unique_index_holds_the_same_key_several_times() {
    // sqlite3: index-10.0 in index.test -- CREATE INDEX i1 ON t1(a);
    //   four rows, two with a=1, both are indexed.
    let db = Db::new("non-unique-dup");
    let rows = vec![
        (1i64, vec![Value::Integer(1), Value::Integer(2)]),
        (2, vec![Value::Integer(2), Value::Integer(4)]),
        (3, vec![Value::Integer(3), Value::Integer(8)]),
        (4, vec![Value::Integer(1), Value::Integer(12)]),
    ];
    let catalog = with_rows(&db, "t1", &["a", "b"], &rows);
    let mut pager = db.open();
    let index = build_index(
        &mut pager,
        &catalog,
        "i1",
        "t1",
        &cols(&["a"]),
        false,
        false,
    )
    .unwrap()
    .unwrap();
    let got = pairs_as_text(&mut pager, index.root_page, 1);
    assert_eq!(got, vec!["1#1", "1#4", "2#2", "3#3"]);
}

#[test]
fn the_unique_violation_message_is_exactly_the_references() {
    // Read straight off 3.53.4 for the two shapes.
    assert_eq!(
        unique_violation_message("t", &["a".to_owned()]),
        "UNIQUE constraint failed: t.a"
    );
    assert_eq!(
        unique_violation_message("t", &["a".to_owned(), "b".to_owned()]),
        "UNIQUE constraint failed: t.a, t.b"
    );
    assert_eq!(
        unique_violation_message("main.t", &["x".to_owned(), "y".to_owned(), "z".to_owned()]),
        "UNIQUE constraint failed: main.t.x, main.t.y, main.t.z"
    );
}

#[test]
fn a_collision_is_decided_on_the_key_and_ignores_the_rowid() {
    let spec = Index {
        name: "i".into(),
        table: "t".into(),
        columns: vec!["a".into()],
        ascending: vec![true],
        unique: true,
        root_page: 3,
    };
    let a = IndexEntry {
        key: vec![Value::Integer(1)],
        rowid: 1,
    };
    let b = IndexEntry {
        key: vec![Value::Integer(1)],
        rowid: 2,
    };
    assert!(
        collides(&spec, &a, &b),
        "same key, different rows, collides"
    );
    let c = IndexEntry {
        key: vec![Value::Null],
        rowid: 1,
    };
    let d = IndexEntry {
        key: vec![Value::Null],
        rowid: 2,
    };
    assert!(!collides(&spec, &c, &d), "a NULL key never collides");
    let mut loose = spec.clone();
    loose.unique = false;
    assert!(
        !collides(&loose, &a, &b),
        "a non-unique index never collides"
    );
}

// --- 3. The schema row ------------------------------------------------------

#[test]
fn the_schema_row_has_the_shape_the_reference_stores() {
    // sqlite3 3.53.4:
    //   type|name|tbl_name|rootpage|sql
    //   index|i1    |t     |3|CREATE INDEX i1 ON t(a)
    let index = Index {
        name: "i1".into(),
        table: "t".into(),
        columns: vec!["a".into()],
        ascending: vec![true],
        unique: false,
        root_page: 3,
    };
    let values = schema_row_values(&index, 3, "CREATE INDEX i1 ON t(a)");
    assert_eq!(values.len(), 5, "a schema row is five columns wide");
    assert_eq!(values[0], Value::Text("index".into()), "type is `index`");
    assert_eq!(values[1], Value::Text("i1".into()), "name is the index's");
    assert_eq!(
        values[2],
        Value::Text("t".into()),
        "tbl_name is the *table's*, not the index's"
    );
    assert_eq!(values[3], Value::Integer(3), "rootpage is the index root");
    assert_eq!(
        values[4],
        Value::Text("CREATE INDEX i1 ON t(a)".into()),
        "sql is the statement text"
    );
}

#[test]
fn the_stored_sql_copies_the_tail_and_normalises_only_the_prefix() {
    // The reference does not re-render the statement. It normalises the prefix
    // through the index name and copies the rest out of the source, so the
    // tail keeps whatever the statement spelled. Each pair below is a stored
    // `sql` column read back out of sqlite3 3.53.4:
    //
    //   CREATE    INDEX   i1   ON t(a)       -> CREATE INDEX i1   ON t(a)
    //   CREATE INDEX i1 ON t(a)  ;           -> CREATE INDEX i1 ON t(a)
    //   CREATE INDEX IF NOT EXISTS i1 ON t(a)-> CREATE INDEX i1 ON t(a)
    //   CREATE    INDEX   main.q2   ON   t(a)-> CREATE INDEX q2   ON   t(a)
    //   CREATE INDEX i1 ON t(a , b)         -> CREATE INDEX i1 ON t(a , b)
    //
    // The `main.q2` row is the one that fixes the boundary: the qualifier is
    // gone from the prefix while the three spaces before `ON` survive in the
    // tail, so nothing else in the statement was normalised.
    for (written, stored) in [
        (
            "CREATE    INDEX   i1   ON t(a)",
            "CREATE INDEX i1   ON t(a)",
        ),
        (
            "CREATE INDEX IF NOT EXISTS i1 ON t(a)",
            "CREATE INDEX i1 ON t(a)",
        ),
        (
            "CREATE    INDEX   main.q2   ON   t(a)",
            "CREATE INDEX q2   ON   t(a)",
        ),
        ("CREATE INDEX i1 ON t(a , b)", "CREATE INDEX i1 ON t(a , b)"),
        // The trailing spaces survive too, and only the semicolon goes. The
        // stored `sql` for `CREATE INDEX q3 ON t(a)   ;` is 26 bytes, which is
        // `CREATE INDEX q3 ON t(a)` (22) plus three spaces — so the space run in
        // front of the semicolon is part of the copied tail, not trimmed.
        ("CREATE INDEX i1 ON t(a)  ;", "CREATE INDEX i1 ON t(a)  "),
        ("CREATE INDEX q3 ON t(a)   ;", "CREATE INDEX q3 ON t(a)   "),
        (
            "CREATE INDEX iz ON t(a) /* c */",
            "CREATE INDEX iz ON t(a) /* c */",
        ),
        (
            "CREATE INDEX i9\nON t(a DESC)",
            "CREATE INDEX i9\nON t(a DESC)",
        ),
        (
            "CREATE UNIQUE INDEX i10 ON t(b ASC) WHERE a>1",
            "CREATE UNIQUE INDEX i10 ON t(b ASC) WHERE a>1",
        ),
        (
            "CREATE   UNIQUE\n  INDEX  ix   ON t ( a DESC , b COLLATE NOCASE )  ",
            "CREATE UNIQUE INDEX ix   ON t ( a DESC , b COLLATE NOCASE )  ",
        ),
    ] {
        assert_eq!(
            sql_for_statement(written).as_deref(),
            Some(stored),
            "for {written:?}"
        );
    }
}

#[test]
fn the_renderer_is_the_fallback_and_joins_columns_with_a_bare_comma() {
    // sqlite3 stores `CREATE INDEX i ON t(a,b,c)` with no space after any comma.
    // This is the path a caller takes when it never had the statement's text, so
    // it has to produce the same bytes for an unremarkable statement — and it
    // cannot, which is why `sql_for_statement` is the one to prefer.
    let c = cols(&["a"]);
    assert_eq!(
        IndexSpec::schema_sql("i1", "t", &c, false),
        "CREATE INDEX i1 ON t(a)"
    );
    assert_eq!(
        IndexSpec::schema_sql("i4", "t", &c, false),
        "CREATE INDEX i4 ON t(a)",
        "the renderer never emits IF NOT EXISTS"
    );
    let c2 = cols(&["a", "b"]);
    assert_eq!(
        IndexSpec::schema_sql("i2", "t", &c2, false),
        "CREATE INDEX i2 ON t(a,b)",
        "no space after the comma, which is what the reference writes"
    );
    assert_eq!(
        IndexSpec::schema_sql("i3", "t", &c2, true),
        "CREATE UNIQUE INDEX i3 ON t(a,b)"
    );
    // And for a statement whose text it *did* have, the two agree.
    assert_eq!(
        sql_for_statement("CREATE INDEX i2 ON t(a,b)").as_deref(),
        Some(IndexSpec::schema_sql("i2", "t", &c2, false).as_str())
    );
}

#[test]
fn the_stored_sql_keeps_the_name_spelling_the_statement_used() {
    // sqlite3: CREATE INDEX "My Idx" ON t(a) -> sql is
    //           CREATE INDEX "My Idx" ON t(a)
    let c = cols(&["a"]);
    assert_eq!(
        IndexSpec::schema_sql("\"My Idx\"", "t", &c, false),
        "CREATE INDEX \"My Idx\" ON t(a)"
    );
    // And the name it recorded is unquoted: `SELECT name` gave `My Idx`.
    let index = rebuild_index("My Idx", "CREATE INDEX \"My Idx\" ON t(a DESC)", 3).unwrap();
    assert_eq!(index.name, "My Idx");
    assert_eq!(index.table, "t");
    assert_eq!(index.columns, vec!["a"]);
    assert_eq!(
        index.ascending,
        vec![false],
        "DESC is read back from the text"
    );
}

#[test]
fn an_index_is_rebuilt_from_its_stored_text() {
    // This is what a reopened connection has to do, and what index-1.1c
    // checks: db close, reopen, and the index is still there with its columns.
    let index = rebuild_index("i2", "CREATE UNIQUE INDEX i2 ON t(a, b DESC)", 4).unwrap();
    assert_eq!(index.name, "i2");
    assert_eq!(index.table, "t");
    assert_eq!(index.columns, vec!["a", "b"]);
    assert_eq!(index.ascending, vec![true, false]);
    assert!(index.unique);
    assert_eq!(index.root_page, 4, "rootpage comes from the schema row");
}

#[test]
fn an_automatically_created_index_stores_no_text_and_rebuilds_from_the_name() {
    // sqlite3: CREATE TABLE t(a UNIQUE);
    //   SELECT name,sql FROM sqlite_schema WHERE type='index';
    //   -> sqlite_autoindex_t_1|          (sql is the empty string)
    let index = rebuild_index("sqlite_autoindex_t_1", "", 3).unwrap();
    assert_eq!(index.name, "sqlite_autoindex_t_1");
    assert!(
        index.unique,
        "an automatic index exists to enforce uniqueness"
    );
    assert_eq!(derived_index_name("t"), "sqlite_autoindex_t_1");
}

#[test]
fn a_schema_row_that_does_not_re_parse_is_a_corrupt_schema() {
    // The same shape `connection::rebuild_table` reports, and index-1.1c
    // depends on a reopened file not failing this way for a valid index.
    //
    // Note what is *not* a failure: a column the table no longer has is not
    // checked here. The rebuild reads the schema, and the table is not in
    // scope to check against; the column check belongs to CREATE INDEX, which
    // is why `rebuild_index("i1", "CREATE INDEX i1 ON t(nosuch)", 3)` parses
    // and returns an index naming `nosuch`.
    // Text that is not even a statement names the parse error after the
    // prefix; the reference's own wording is "malformed database schema(NAME)"
    // with the detail after a colon, as `rebuild_table` writes it.
    let e = rebuild_index("i1", "not a create index at all", 3).unwrap_err();
    assert_eq!(e.code, ResultCode::Corrupt);
    assert_eq!(
        e.message,
        "malformed database schema (i1): ERROR: near \"not\": syntax error"
    );

    // Text that parses but is not a CREATE INDEX at all gets the bare form,
    // with nothing after the prefix.
    let e = rebuild_index("i1", "CREATE TABLE t(x)", 3).unwrap_err();
    assert_eq!(e.code, ResultCode::Corrupt);
    assert_eq!(e.message, "malformed database schema (i1)");

    // A column that does not exist is passed through, not rejected.
    let index = rebuild_index("i1", "CREATE INDEX i1 ON t(nosuch)", 3).unwrap();
    assert_eq!(index.columns, vec!["nosuch"]);
}

// --- 4. Name checks ---------------------------------------------------------

#[test]
fn an_index_name_that_is_a_table_name_is_refused() {
    // sqlite3: CREATE TABLE t(a); CREATE INDEX t ON t(a);
    //   -> there is already a table named t
    let db = Db::new("name-table");
    let catalog = with_rows(&db, "t", &["a"], &[]);
    let mut pager = db.open();
    let e = build_index(&mut pager, &catalog, "t", "t", &cols(&["a"]), false, false).unwrap_err();
    assert_eq!(e.message, "there is already a table named t");
}

#[test]
fn a_duplicate_index_name_is_refused() {
    // sqlite3: CREATE INDEX i1 ON t(a); CREATE INDEX i1 ON t(b);
    //   -> index i1 already exists
    let db = Db::new("name-dup");
    let catalog = with_rows(&db, "t", &["a", "b"], &[]);
    let mut pager = db.open();
    let mut cat = catalog;
    let first = build_index(&mut pager, &cat, "i1", "t", &cols(&["a"]), false, false)
        .unwrap()
        .unwrap();
    cat.put_index(first);
    let e = build_index(&mut pager, &cat, "i1", "t", &cols(&["b"]), false, false).unwrap_err();
    assert_eq!(e.message, "index i1 already exists");
}

#[test]
fn an_index_name_is_matched_case_insensitively() {
    // sqlite3: an index is named `My Idx`; `DROP INDEX "my idx"` finds it.
    let db = Db::new("name-case");
    let catalog = with_rows(&db, "t", &["a"], &[]);
    let mut pager = db.open();
    let mut cat = catalog;
    let built = build_index(&mut pager, &cat, "My Idx", "t", &cols(&["a"]), false, false)
        .unwrap()
        .unwrap();
    cat.put_index(built);
    assert!(cat.index("MY IDX").is_some());
    assert!(cat.index("my idx").is_some());
    // And DROP matches it the same way.
    let dropped = drop_index(&mut pager, &cat, "my idx", false)
        .unwrap()
        .unwrap();
    assert_eq!(dropped.name, "My Idx", "the catalog's spelling is kept");
}

#[test]
fn if_not_exists_covers_an_index_but_not_a_table() {
    // sqlite3 3.53.4, both statements against a table named `t`:
    //
    //   CREATE INDEX IF NOT EXISTS ix ON t(a);
    //     CREATE INDEX ix ON t(a);   -> rc=0, and the second is a no-op
    //
    //   CREATE INDEX IF NOT EXISTS t ON t(a);
    //     -> Parse error: there is already a table named t   (rc=1)
    //
    // So `IF NOT EXISTS` suppresses the error only when the name is already an
    // *index*. It does not reach a table, and reading it the other way made
    // this a silent no-op where the reference refuses the statement.
    let db = Db::new("name-ine");
    let catalog = with_rows(&db, "t", &["a", "b"], &[]);
    let mut pager = db.open();
    let mut cat = catalog;
    let built = build_index(&mut pager, &cat, "i1", "t", &cols(&["a"]), false, false)
        .unwrap()
        .unwrap();
    cat.put_index(built);
    let got = build_index(&mut pager, &cat, "i1", "t", &cols(&["b"]), false, true).unwrap();
    assert!(got.is_none(), "IF NOT EXISTS over an index is a no-op");

    // And over a table name it still raises, exactly as 3.53.4 does.
    let e = build_index(&mut pager, &cat, "t", "t", &cols(&["a"]), false, true).unwrap_err();
    assert_eq!(e.message, "there is already a table named t");
    assert_eq!(e.code, ResultCode::Error);
}

#[test]
fn the_schema_table_may_not_be_indexed() {
    // sqlite3 3.53.4:
    //   CREATE INDEX i1 ON sqlite_master(name);
    //     -> table sqlite_master may not be indexed
    //   CREATE INDEX i2 ON sqlite_schema(name);
    //     -> table sqlite_schema may not be indexed
    // The name is echoed as written, so the two spellings do not share a
    // message, and a `main.`-qualified form is stripped back to the base.
    let db = Db::new("name-schema");
    let catalog = with_rows(&db, "t", &["a"], &[]);
    let mut pager = db.open();
    for spelling in [
        "sqlite_master",
        "SQLITE_MASTER",
        "sqlite_schema",
        "main.sqlite_master",
        "main.sqlite_schema",
    ] {
        let e = build_index(
            &mut pager,
            &catalog,
            "i1",
            spelling,
            &cols(&["name"]),
            false,
            false,
        )
        .unwrap_err();
        assert_eq!(
            e.message,
            format!("table {} may not be indexed", base_name(spelling)),
            "for {spelling}"
        );
    }
    assert!(is_schema_table_name("sqlite_master"));
    assert!(is_schema_table_name("temp.sqlite_schema"));
    assert!(!is_schema_table_name("t"));
}

#[test]
fn a_reserved_name_is_refused() {
    // sqlite3: CREATE INDEX sqlite_i1 ON t7(c);
    //   -> object name reserved for internal use: sqlite_i1
    let db = Db::new("name-reserved");
    let catalog = with_rows(&db, "t", &["a"], &[]);
    let mut pager = db.open();
    let e = build_index(
        &mut pager,
        &catalog,
        "sqlite_i1",
        "t",
        &cols(&["a"]),
        false,
        false,
    )
    .unwrap_err();
    assert_eq!(
        e.message,
        "object name reserved for internal use: sqlite_i1"
    );
    assert!(is_reserved("sqlite_x"));
    assert!(!is_reserved("sqlite"));
    assert!(!is_reserved("my_sqlite_x"));
}

#[test]
fn an_index_over_a_table_that_is_not_there_names_the_schema() {
    // sqlite3: CREATE INDEX i1 ON nosuch(a);
    //   -> no such table: main.nosuch
    let db = Db::new("name-notable");
    let catalog = with_rows(&db, "t", &["a"], &[]);
    let mut pager = db.open();
    let e = build_index(
        &mut pager,
        &catalog,
        "i1",
        "nosuch",
        &cols(&["a"]),
        false,
        false,
    )
    .unwrap_err();
    assert_eq!(e.message, "no such table: main.nosuch");
}

#[test]
fn an_index_over_a_column_that_is_not_there_names_the_bare_column() {
    // sqlite3: CREATE TABLE test1(f1 int, f2 int, f3 int);
    //   CREATE INDEX index1 ON test1(f4)  -> no such column: f4
    // index-2.2 also checks a mixed list: (f1, f2, f4, f3) -> the same
    // message, so the first bad column decides it.
    let db = Db::new("name-nocol");
    let catalog = with_rows(&db, "test1", &["f1", "f2", "f3"], &[]);
    let mut pager = db.open();
    for bad in [vec!["f4"], vec!["f1", "f2", "f4", "f3"], vec!["f3", "nope"]] {
        let e = build_index(
            &mut pager,
            &catalog,
            "index1",
            "test1",
            &cols(&bad),
            false,
            false,
        )
        .unwrap_err();
        let want = format!(
            "no such column: {}",
            bad.iter()
                .find(|c| !["f1", "f2", "f3"].contains(c))
                .unwrap()
        );
        assert_eq!(e.message, want);
    }
}

#[test]
fn an_index_over_the_rowid_alias_indexes_the_rowid() {
    // sqlite3:
    //   CREATE TABLE t(a INTEGER PRIMARY KEY, b);
    //   INSERT INTO t VALUES(5,50); INSERT INTO t VALUES(9,90);
    //   CREATE INDEX i ON t(a);
    //   -> the index page holds records `01 01 05 05` and `01 01 09 09`,
    //      i.e. the key is 5 and 9, not the NULL the row stores.
    let db = Db::new("alias-index");
    let mut pager = db.open();
    let root = pager.allocate().unwrap();
    crate::btree_write::LeafPage::empty(root, pager.page_size())
        .write_to(&mut pager)
        .unwrap();
    let mut tree = crate::table_tree::TableTree::open(&mut pager, root)
        .unwrap()
        .with_rowid_alias(Some(0));
    for (rowid, b) in [(5i64, 50i64), (9, 90)] {
        tree.insert(
            &mut pager,
            &crate::table_tree::Row {
                rowid,
                values: vec![Value::Integer(rowid), Value::Integer(b)],
            },
        )
        .unwrap();
    }
    let mut t = table("t", &["a", "b"], root);
    t.rowid_alias = Some(0);
    t.columns[0].rowid_alias = true;
    let mut catalog = Catalog::new();
    catalog.put(t.clone());
    pager.flush().unwrap();

    let index = build_index(&mut pager, &catalog, "i", "t", &cols(&["a"]), false, false)
        .unwrap()
        .unwrap();
    let got = pairs_as_text(&mut pager, index.root_page, 1);
    assert_eq!(got, vec!["5#5", "9#9"], "the key is the rowid, not NULL");
}

// --- 5. DROP INDEX ----------------------------------------------------------

#[test]
fn dropping_an_index_frees_its_root() {
    // sqlite3, on a 1024-byte-page database:
    //   before DROP: page_count 3, freelist_count 0
    //   after  DROP: page_count 3, freelist_count 1
    let db = Db::new("drop-frees");
    let rows = vec![
        (1i64, vec![Value::Integer(1)]),
        (2, vec![Value::Integer(2)]),
    ];
    let catalog = with_rows(&db, "t", &["a"], &rows);
    let mut pager = db.open();
    let mut cat = catalog;
    let built = build_index(&mut pager, &cat, "i", "t", &cols(&["a"]), false, false)
        .unwrap()
        .unwrap();
    cat.put_index(built.clone());
    let pages_before = pager.page_count();
    let free_before = pager.header().freelist_count;
    drop_index(&mut pager, &cat, "i", false).unwrap().unwrap();
    assert_eq!(
        pager.page_count(),
        pages_before,
        "the file does not shrink, the page goes on the freelist"
    );
    assert_eq!(pager.header().freelist_count, free_before + 1);
}

#[test]
fn dropping_a_multi_page_index_frees_every_page_it_owned() {
    let db = Db::new("drop-frees-all");
    let rows: Vec<(i64, Vec<Value>)> = (1..=600).map(|i| (i, vec![Value::Integer(i)])).collect();
    let catalog = with_rows(&db, "t", &["a"], &rows);
    let mut pager = db.open();
    let mut cat = catalog;
    let built = build_index(&mut pager, &cat, "i", "t", &cols(&["a"]), false, false)
        .unwrap()
        .unwrap();
    cat.put_index(built.clone());
    let owned = tree_pages(&mut pager, built.root_page).unwrap();
    assert!(owned.len() > 1, "600 keys need more than one page");
    let before = pager.page_count();
    drop_index(&mut pager, &cat, "i", false).unwrap().unwrap();
    assert_eq!(
        pager.header().freelist_count as usize,
        owned.len(),
        "every page the tree owned goes back"
    );
    assert_eq!(pager.page_count(), before, "the file does not shrink");
}

#[test]
fn an_index_rebuilt_after_a_drop_gets_a_fresh_page() {
    // sqlite3: CREATE TABLE t(a); CREATE INDEX i ON t(a); DROP INDEX i;
    //   CREATE INDEX i2 ON t(a); -> rootpage 3 again, freelist 0.
    //
    // This engine's `Pager::allocate` appends rather than taking from the
    // freelist — a gap in the pager, not in the index DDL — so the page number
    // differs. What is still this module's to guarantee is that the old index's
    // page went back on the freelist and the new one is a working tree, so
    // that is what is checked. Wiring the freelist into `allocate` is the
    // caller's (or the pager owner's) change.
    let db = Db::new("drop-reuse");
    let catalog = with_rows(&db, "t", &["a"], &[]);
    let mut pager = db.open();
    let mut cat = catalog;
    let first = build_index(&mut pager, &cat, "i", "t", &cols(&["a"]), false, false)
        .unwrap()
        .unwrap();
    cat.put_index(first.clone());
    drop_index(&mut pager, &cat, "i", false).unwrap().unwrap();
    cat.remove_index("i");
    let second = build_index(&mut pager, &cat, "i2", "t", &cols(&["a"]), false, false)
        .unwrap()
        .unwrap();
    assert_eq!(
        pager.header().freelist_count,
        1,
        "the first index's page is on the freelist"
    );
    assert_ne!(
        second.root_page, first.root_page,
        "a freed page is not handed straight back by this pager"
    );
    let page = pager.read_page(second.root_page).unwrap();
    assert_eq!(page[0], page_type::INDEX_LEAF, "and the new root is real");
}

#[test]
fn dropping_an_index_that_is_not_there_is_an_error() {
    // sqlite3: DROP INDEX index1  -> no such index: index1
    let db = Db::new("drop-missing");
    let catalog = with_rows(&db, "t", &["a"], &[]);
    let mut pager = db.open();
    let e = drop_index(&mut pager, &catalog, "index1", false).unwrap_err();
    assert_eq!(e.message, "no such index: index1");
    assert_eq!(e.code, ResultCode::Error, "SQLITE_ERROR, not CONSTRAINT");
}

#[test]
fn a_qualified_drop_reports_the_name_as_written() {
    // sqlite3: DROP INDEX main.nosuch -> no such index: main.nosuch
    let db = Db::new("drop-qualified");
    let catalog = with_rows(&db, "t", &["a"], &[]);
    let mut pager = db.open();
    for spelling in ["main.nosuch", "temp.nosuch", "nosuch"] {
        let e = drop_index(&mut pager, &catalog, spelling, false).unwrap_err();
        assert_eq!(e.message, format!("no such index: {spelling}"));
    }
}

#[test]
fn dropping_if_exists_is_a_no_op() {
    // sqlite3: DROP INDEX IF EXISTS nosuch -> rc=0, and
    //          DROP INDEX IF EXISTS no_such_index (index-17.4) -> rc=0.
    let db = Db::new("drop-ifexists");
    let catalog = with_rows(&db, "t", &["a"], &[]);
    let mut pager = db.open();
    let before = pager.page_count();
    let got = drop_index(&mut pager, &catalog, "nosuch", true).unwrap();
    assert!(got.is_none(), "nothing to do");
    assert_eq!(pager.page_count(), before, "and nothing is allocated");
}

#[test]
fn an_automatic_index_cannot_be_dropped() {
    // sqlite3:
    //   CREATE TABLE t(a UNIQUE);
    //   DROP INDEX sqlite_autoindex_t_1;
    //     -> index associated with UNIQUE or PRIMARY KEY constraint cannot be dropped
    //   DROP INDEX IF EXISTS sqlite_autoindex_t_1;
    //     -> the same; IF EXISTS does not suppress it (index-17.2/17.3).
    let db = Db::new("drop-auto");
    let catalog = with_rows(&db, "t", &["a"], &[]);
    let mut pager = db.open();
    let auto = Index {
        name: derived_index_name("t"),
        table: "t".into(),
        columns: vec!["a".into()],
        ascending: vec![true],
        unique: true,
        root_page: 3,
    };
    let mut cat = catalog;
    cat.put_index(auto);
    for if_exists in [false, true] {
        let e = drop_index(&mut pager, &cat, "sqlite_autoindex_t_1", if_exists).unwrap_err();
        assert_eq!(
            e.message,
            "index associated with UNIQUE or PRIMARY KEY constraint cannot be dropped"
        );
    }
}

// --- 6. Keeping an index current -------------------------------------------

#[test]
fn an_inserted_row_lands_in_every_index() {
    let db = Db::new("keep-insert");
    let catalog = with_rows(
        &db,
        "t",
        &["a", "b"],
        &[(1i64, vec![Value::Integer(1), Value::Text("x".into())])],
    );
    let mut pager = db.open();
    let mut cat = catalog;
    for name in ["ia", "ib", "iab"] {
        let c = match name {
            "ia" => cols(&["a"]),
            "ib" => cols(&["b"]),
            _ => cols(&["a", "b"]),
        };
        let built = build_index(&mut pager, &cat, name, "t", &c, false, false)
            .unwrap()
            .unwrap();
        cat.put_index(built);
    }
    let t = cat.get("t").unwrap().clone();
    index_insert(
        &mut pager,
        &cat,
        &t,
        2,
        &[Value::Integer(2), Value::Text("y".into())],
    )
    .unwrap();
    let ia = cat.index("ia").unwrap().root_page;
    let ib = cat.index("ib").unwrap().root_page;
    let iab = cat.index("iab").unwrap().root_page;
    assert_eq!(pairs_as_text(&mut pager, ia, 1), vec!["1#1", "2#2"]);
    assert_eq!(pairs_as_text(&mut pager, ib, 1), vec!["'x'#1", "'y'#2"]);
    assert_eq!(
        pairs_as_text(&mut pager, iab, 2),
        vec!["1,'x'#1", "2,'y'#2"]
    );
}

#[test]
fn the_index_and_the_table_agree_after_a_run_of_inserts() {
    // The cross-check: build, then insert 300 rows one at a time, and compare
    // the index against a walk of the table at the end.
    let db = Db::new("keep-insert-many");
    let catalog = with_rows(
        &db,
        "t",
        &["a", "b"],
        &[(1i64, vec![Value::Integer(0), Value::Integer(0)])],
    );
    let mut pager = db.open();
    let mut cat = catalog;
    let built = build_index(&mut pager, &cat, "i", "t", &cols(&["a"]), false, false)
        .unwrap()
        .unwrap();
    cat.put_index(built);
    let t = cat.get("t").unwrap().clone();

    let mut tree = crate::table_tree::TableTree::open(&mut pager, t.root_page).unwrap();
    for i in 2..=300i64 {
        let values = vec![Value::Integer(300 - i), Value::Integer(i)];
        tree.insert(
            &mut pager,
            &crate::table_tree::Row {
                rowid: i,
                values: values.clone(),
            },
        )
        .unwrap();
        index_insert(&mut pager, &cat, &t, i, &values).unwrap();
    }
    // The table splits on the way, and the catalog's root page has to follow
    // it — otherwise the comparison below would walk a subtree rather than the
    // table and pass for the wrong reason.
    let mut t = t.clone();
    t.root_page = tree.root();
    let root = cat.index("i").unwrap().root_page;
    assert_eq!(
        pairs_as_text(&mut pager, root, 1),
        table_pairs(&mut pager, &t, &[0]),
        "300 inserts later the index still matches the table"
    );
}

#[test]
fn an_insert_that_breaks_a_unique_index_is_refused() {
    // sqlite3:
    //   CREATE TABLE t(a,b,c);
    //   INSERT INTO t VALUES(1,2,3); INSERT INTO t VALUES(1,2,4);
    //   CREATE UNIQUE INDEX i3 ON t(a,b);       -- refused: (1,2) twice
    // so the data a UNIQUE index can be built over has to be distinct to begin
    // with. Read straight off 3.53.4, the insertion case is:
    //   CREATE TABLE t(a,b); INSERT INTO t VALUES(1,2);
    //   CREATE UNIQUE INDEX u ON t(a,b);
    //   INSERT INTO t VALUES(1,2);
    //   -> UNIQUE constraint failed: t.a, t.b
    let db = Db::new("keep-insert-unique");
    let catalog = with_rows(
        &db,
        "t",
        &["a", "b", "c"],
        &[
            (
                1i64,
                vec![Value::Integer(1), Value::Integer(2), Value::Integer(3)],
            ),
            (
                2,
                vec![Value::Integer(1), Value::Integer(3), Value::Integer(4)],
            ),
        ],
    );
    let mut pager = db.open();
    let mut cat = catalog;
    let built = build_index(&mut pager, &cat, "u", "t", &cols(&["a", "b"]), true, false)
        .unwrap()
        .unwrap();
    cat.put_index(built);
    let t = cat.get("t").unwrap().clone();
    let e = index_insert(
        &mut pager,
        &cat,
        &t,
        3,
        &[Value::Integer(1), Value::Integer(2), Value::Integer(5)],
    )
    .unwrap_err();
    assert_eq!(e.message, "UNIQUE constraint failed: t.a, t.b");
    assert_eq!(e.extended_code(), 2067);
    // The index must be unchanged by the refusal: still two entries.
    let root = cat.index("u").unwrap().root_page;
    assert_eq!(read_entries(&mut pager, root, 2).unwrap().len(), 2);
}

#[test]
fn a_deleted_row_leaves_every_index() {
    let db = Db::new("keep-delete");
    let rows = vec![
        (1i64, vec![Value::Integer(1), Value::Integer(10)]),
        (2, vec![Value::Integer(2), Value::Integer(20)]),
        (3, vec![Value::Integer(3), Value::Integer(30)]),
    ];
    let catalog = with_rows(&db, "t", &["a", "b"], &rows);
    let mut pager = db.open();
    let mut cat = catalog;
    let ia = build_index(&mut pager, &cat, "ia", "t", &cols(&["a"]), false, false)
        .unwrap()
        .unwrap();
    cat.put_index(ia);
    let ib = build_index(&mut pager, &cat, "ib", "t", &cols(&["b"]), false, false)
        .unwrap()
        .unwrap();
    cat.put_index(ib);
    let t = cat.get("t").unwrap().clone();

    index_delete(
        &mut pager,
        &cat,
        &t,
        2,
        &[Value::Integer(2), Value::Integer(20)],
    )
    .unwrap();
    let ia_root = cat.index("ia").unwrap().root_page;
    let ib_root = cat.index("ib").unwrap().root_page;
    assert_eq!(pairs_as_text(&mut pager, ia_root, 1), vec!["1#1", "3#3"]);
    assert_eq!(pairs_as_text(&mut pager, ib_root, 1), vec!["10#1", "30#3"]);
}

#[test]
fn deleting_every_row_leaves_an_empty_index() {
    // sqlite3: index-10.3 -- DELETE FROM t1 WHERE b=2; then b=1, and
    //   `SELECT b FROM t1 WHERE a=1 ORDER BY b` is empty.
    let db = Db::new("keep-delete-all");
    let rows: Vec<(i64, Vec<Value>)> = (1..=20)
        .map(|i| (i, vec![Value::Integer(1), Value::Integer(i)]))
        .collect();
    let catalog = with_rows(&db, "t", &["a", "b"], &rows);
    let mut pager = db.open();
    let mut cat = catalog;
    let built = build_index(&mut pager, &cat, "i", "t", &cols(&["a"]), false, false)
        .unwrap()
        .unwrap();
    cat.put_index(built);
    let t = cat.get("t").unwrap().clone();
    for (rowid, values) in &rows {
        index_delete(&mut pager, &cat, &t, *rowid, values).unwrap();
    }
    let root = cat.index("i").unwrap().root_page;
    let left = read_entries(&mut pager, root, 1).unwrap();
    assert!(left.is_empty(), "nothing is left, got {left:?}");
    // The page is still a legal, empty index leaf.
    let page = pager.read_page(root).unwrap();
    assert_eq!(page[0], page_type::INDEX_LEAF);
}

#[test]
fn a_deleted_row_leaves_a_multi_page_index_readable() {
    // The delete is a whole-tree rewrite, so the interesting case is one that
    // has already split: a rebuild that lost a page or stranded a subtree
    // would still leave a readable *leaf*, and only a full comparison with the
    // table would notice.
    //
    // The row count is modest on purpose. Each delete re-reads and re-writes
    // the whole tree, so deleting all 800 rows one at a time is 800 rebuilds
    // of an 800-entry tree. The test that matters is the *shape* — a tree that
    // spans several leaves, losing every other entry, still agreeing with the
    // table — and 300 rows gives a three-level tree for a fraction of the work.
    let db = Db::new("keep-delete-large");
    let rows: Vec<(i64, Vec<Value>)> = (1..=300).map(|i| (i, vec![Value::Integer(i)])).collect();
    let catalog = with_rows(&db, "t", &["a"], &rows);
    let mut pager = db.open();
    let mut cat = catalog;
    let built = build_index(&mut pager, &cat, "i", "t", &cols(&["a"]), false, false)
        .unwrap()
        .unwrap();
    cat.put_index(built);
    assert!(
        tree_pages(&mut pager, cat.index("i").unwrap().root_page)
            .unwrap()
            .len()
            > 1,
        "the index has to have split for this to test anything"
    );
    let t = cat.get("t").unwrap().clone();

    // Delete every other row, so the survivors are spread across every leaf
    // rather than clustered at one end.
    for (rowid, values) in rows.iter().filter(|(r, _)| r % 2 == 0) {
        index_delete(&mut pager, &cat, &t, *rowid, values).unwrap();
    }
    let root = cat.index("i").unwrap().root_page;
    let got: Vec<i64> = read_entries(&mut pager, root, 1)
        .unwrap()
        .iter()
        .map(|e| e.rowid)
        .collect();
    let want: Vec<i64> = (1..=300i64).filter(|r| r % 2 == 1).collect();
    assert_eq!(got, want, "the odd rowids are the survivors");

    // And deleting the rest leaves a legal, empty leaf.
    for (rowid, values) in rows.iter().filter(|(r, _)| r % 2 == 1) {
        index_delete(&mut pager, &cat, &t, *rowid, values).unwrap();
    }
    assert!(read_entries(&mut pager, root, 1).unwrap().is_empty());
    let page = pager.read_page(root).unwrap();
    assert_eq!(page[0], page_type::INDEX_LEAF);
}

#[test]
fn a_deleted_entry_that_is_not_there_is_reported_not_ignored() {
    // A silent no-op here is how an index goes stale without anything
    // reporting it, so the failure has to be loud.
    let db = Db::new("keep-delete-missing");
    let catalog = with_rows(
        &db,
        "t",
        &["a"],
        &[
            (1i64, vec![Value::Integer(1)]),
            (2, vec![Value::Integer(2)]),
        ],
    );
    let mut pager = db.open();
    let mut cat = catalog;
    let built = build_index(&mut pager, &cat, "i", "t", &cols(&["a"]), false, false)
        .unwrap()
        .unwrap();
    cat.put_index(built);
    let t = cat.get("t").unwrap().clone();
    let e = index_delete(&mut pager, &cat, &t, 99, &[Value::Integer(99)]).unwrap_err();
    assert!(e.message.contains("no index entry for rowid 99"), "{e}");
}

#[test]
fn an_update_that_moves_a_key_moves_the_entry() {
    let db = Db::new("keep-update-key");
    let rows = vec![
        (1i64, vec![Value::Integer(1), Value::Integer(10)]),
        (2, vec![Value::Integer(2), Value::Integer(20)]),
    ];
    let catalog = with_rows(&db, "t", &["a", "b"], &rows);
    let mut pager = db.open();
    let mut cat = catalog;
    let built = build_index(&mut pager, &cat, "i", "t", &cols(&["a"]), false, false)
        .unwrap()
        .unwrap();
    cat.put_index(built);
    let t = cat.get("t").unwrap().clone();
    let old = vec![Value::Integer(1), Value::Integer(10)];
    let new = vec![Value::Integer(5), Value::Integer(10)];
    index_update(&mut pager, &cat, &t, 1, &old, 1, &new).unwrap();
    let root = cat.index("i").unwrap().root_page;
    assert_eq!(
        pairs_as_text(&mut pager, root, 1),
        vec!["2#2", "5#1"],
        "the old key is gone and the new one is present, under the same rowid"
    );
}

#[test]
fn an_update_that_leaves_the_key_alone_keeps_the_entry() {
    // The delete half runs unconditionally, so an update of a non-indexed
    // column still has to leave exactly one entry behind.
    let db = Db::new("keep-update-nokey");
    let rows = vec![
        (1i64, vec![Value::Integer(1), Value::Integer(10)]),
        (2, vec![Value::Integer(2), Value::Integer(20)]),
    ];
    let catalog = with_rows(&db, "t", &["a", "b"], &rows);
    let mut pager = db.open();
    let mut cat = catalog;
    let built = build_index(&mut pager, &cat, "i", "t", &cols(&["a"]), false, false)
        .unwrap()
        .unwrap();
    cat.put_index(built);
    let t = cat.get("t").unwrap().clone();
    let old = vec![Value::Integer(1), Value::Integer(10)];
    let new = vec![Value::Integer(1), Value::Integer(99)];
    index_update(&mut pager, &cat, &t, 1, &old, 1, &new).unwrap();
    let root = cat.index("i").unwrap().root_page;
    assert_eq!(
        pairs_as_text(&mut pager, root, 1),
        vec!["1#1", "2#2"],
        "one entry, not two and not none"
    );
}

#[test]
fn an_update_that_moves_the_rowid_moves_the_trailing_key() {
    // An INTEGER PRIMARY KEY update is a row *move*: the rowid changes and so
    // does the key an index over that column holds.
    let db = Db::new("keep-update-rowid");
    let mut pager = db.open();
    let root = pager.allocate().unwrap();
    crate::btree_write::LeafPage::empty(root, pager.page_size())
        .write_to(&mut pager)
        .unwrap();
    let mut tree = crate::table_tree::TableTree::open(&mut pager, root)
        .unwrap()
        .with_rowid_alias(Some(0));
    for rowid in [5i64, 9] {
        tree.insert(
            &mut pager,
            &crate::table_tree::Row {
                rowid,
                values: vec![Value::Integer(rowid), Value::Integer(rowid * 10)],
            },
        )
        .unwrap();
    }
    let mut t = table("t", &["a", "b"], root);
    t.rowid_alias = Some(0);
    t.columns[0].rowid_alias = true;
    let mut cat = Catalog::new();
    cat.put(t.clone());
    pager.flush().unwrap();

    let built = build_index(&mut pager, &cat, "i", "t", &cols(&["a"]), false, false)
        .unwrap()
        .unwrap();
    cat.put_index(built);
    // Row 5 moves to rowid 7.
    index_update(
        &mut pager,
        &cat,
        &t,
        5,
        &[Value::Null, Value::Integer(50)],
        7,
        &[Value::Null, Value::Integer(50)],
    )
    .unwrap();
    let root_page = cat.index("i").unwrap().root_page;
    let got = read_entries(&mut pager, root_page, 1).unwrap();
    let got: Vec<(i64, i64)> = got
        .iter()
        .map(|e| (e.rowid, e.key[0].as_i64().unwrap()))
        .collect();
    assert_eq!(got, vec![(7, 7), (9, 9)], "the key follows the new rowid");
}

#[test]
fn an_update_that_breaks_a_unique_index_is_refused() {
    let db = Db::new("keep-update-unique");
    let rows = vec![
        (1i64, vec![Value::Integer(1)]),
        (2, vec![Value::Integer(2)]),
    ];
    let catalog = with_rows(&db, "t", &["a"], &rows);
    let mut pager = db.open();
    let mut cat = catalog;
    let built = build_index(&mut pager, &cat, "u", "t", &cols(&["a"]), true, false)
        .unwrap()
        .unwrap();
    cat.put_index(built);
    let t = cat.get("t").unwrap().clone();
    let e = index_update(
        &mut pager,
        &cat,
        &t,
        1,
        &[Value::Integer(1)],
        1,
        &[Value::Integer(2)],
    )
    .unwrap_err();
    assert_eq!(e.message, "UNIQUE constraint failed: t.a");
}

#[test]
fn a_table_with_no_index_is_left_alone_by_the_write_helpers() {
    let db = Db::new("keep-noindex");
    let catalog = with_rows(
        &db,
        "t",
        &["a"],
        &[
            (1i64, vec![Value::Integer(1)]),
            (2, vec![Value::Integer(2)]),
        ],
    );
    let mut pager = db.open();
    let t = catalog.get("t").unwrap().clone();
    let before = pager.page_count();
    index_insert(&mut pager, &catalog, &t, 3, &[Value::Integer(3)]).unwrap();
    index_delete(&mut pager, &catalog, &t, 1, &[Value::Integer(1)]).unwrap();
    index_update(
        &mut pager,
        &catalog,
        &t,
        2,
        &[Value::Integer(2)],
        2,
        &[Value::Integer(9)],
    )
    .unwrap();
    assert_eq!(pager.page_count(), before, "no page is touched");
}

// --- 7. Structural checks ---------------------------------------------------

#[test]
fn the_root_is_a_leaf_until_the_index_outgrows_one_page() {
    let db = Db::new("struct-root");
    let rows: Vec<(i64, Vec<Value>)> = (1..=5).map(|i| (i, vec![Value::Integer(i)])).collect();
    let catalog = with_rows(&db, "t", &["a"], &rows);
    let mut pager = db.open();
    let built = build_index(&mut pager, &catalog, "i", "t", &cols(&["a"]), false, false)
        .unwrap()
        .unwrap();
    let page = pager.read_page(built.root_page).unwrap();
    assert_eq!(page[0], page_type::INDEX_LEAF, "five keys fit one leaf");
    assert_eq!(
        built.root_page, 3,
        "an index root never moves, which is what lets the schema hold one page number"
    );
}

#[test]
fn a_rebuilt_index_keeps_its_root_page_number() {
    // The delete rewrite grows a replacement and grafts it onto the old root,
    // so the schema row never has to change. A delete that moved the root
    // would mean a schema write on every single-row delete.
    let db = Db::new("struct-root-stable");
    let rows: Vec<(i64, Vec<Value>)> = (1..=300).map(|i| (i, vec![Value::Integer(i)])).collect();
    let catalog = with_rows(&db, "t", &["a"], &rows);
    let mut pager = db.open();
    let mut cat = catalog;
    let built = build_index(&mut pager, &cat, "i", "t", &cols(&["a"]), false, false)
        .unwrap()
        .unwrap();
    cat.put_index(built.clone());
    let t = cat.get("t").unwrap().clone();
    for (rowid, values) in rows.iter().take(50) {
        index_delete(&mut pager, &cat, &t, *rowid, values).unwrap();
    }
    assert_eq!(
        cat.index("i").unwrap().root_page,
        built.root_page,
        "the catalog's root page is unchanged"
    );
    let entries = read_entries(&mut pager, built.root_page, 1).unwrap();
    assert_eq!(entries.len(), 250);
    assert_eq!(entries[0].rowid, 51);
    assert_eq!(entries[249].rowid, 300);
}

#[test]
fn every_page_of_the_tree_is_reachable_exactly_once() {
    // A rebuild that double-references a page, or writes one the tree never
    // reaches, would still answer every lookup above. Only a walk notices.
    let db = Db::new("struct-reachable");
    let rows: Vec<(i64, Vec<Value>)> = (1..=300)
        .map(|i| (i, vec![Value::Integer(300 - i)]))
        .collect();
    let catalog = with_rows(&db, "t", &["a"], &rows);
    let mut pager = db.open();
    let mut cat = catalog;
    let built = build_index(&mut pager, &cat, "i", "t", &cols(&["a"]), false, false)
        .unwrap()
        .unwrap();
    cat.put_index(built);
    let t = cat.get("t").unwrap().clone();
    let table_root_before = t.root_page;
    // Every third row, by rowid. `rows` is indexed from 0 but holds rowids
    // starting at 1, so this is `r % 3 == 1` and not `r % 3 == 0` — the
    // off-by-one the expectation below would have caught.
    let doomed: Vec<i64> = (1..=300i64).filter(|r| r % 3 == 1).collect();
    for (rowid, values) in rows.iter().filter(|(r, _)| doomed.contains(r)) {
        index_delete(&mut pager, &cat, &t, *rowid, values).unwrap();
    }
    let root = cat.index("i").unwrap().root_page;
    let pages = tree_pages(&mut pager, root).unwrap();
    let mut sorted = pages.clone();
    sorted.dedup();
    assert_eq!(sorted.len(), pages.len(), "no page is reached twice");

    // Every entry the table still has is in the index, and nothing else is.
    // The expectation is derived from the rows rather than written out, so a
    // test that passes is one where the index agrees with the data. The key is
    // `300 - rowid`, which is not the same number as the rowid — a comparison
    // that used the rowid in both places would pass against an index that had
    // the right *count* of entries and the wrong contents.
    let want: Vec<String> = rows
        .iter()
        .filter(|(r, _)| !doomed.contains(r))
        .map(|(r, v)| format!("{}#{r}", v[0]))
        .collect();
    let got = pairs_as_text(&mut pager, root, 1);
    let mut want = want;
    want.sort();
    let mut got_sorted = got.clone();
    got_sorted.sort();
    assert_eq!(got_sorted, want);
    assert_eq!(
        t.root_page, table_root_before,
        "the table itself is untouched by an index rewrite"
    );
}

#[test]
fn an_index_with_a_wide_key_gets_an_overflow_chain_that_drop_frees() {
    // An index key wider than the page's local limit spills into an overflow
    // chain. On a 4096-byte page the index `maxLocal` is
    // `(usable-12)*64/255 - 23` = 1002, so a 3000-byte key still fits but a
    // 6000-byte one does not.
    //
    // The reference, on three 6000-byte blob keys:
    //
    //   payload length 6005, local 489, overflow head page 13,
    //   chain 13 -> 14  (5516 spilled bytes over a 4092-byte capacity)
    //
    // The chain belongs to the entry, so freeing the tree has to take it with
    // it. Leaving it would leak pages the file never reuses, and
    // `free_index_tree` reads the cell to find the head, so a chain it cannot
    // read is a chain it cannot free.
    let db = Db::new("struct-overflow");
    let big: Vec<u8> = vec![0x41; 6000];
    let rows = vec![
        (1i64, vec![Value::Blob(big.clone())]),
        (2, vec![Value::Blob(vec![0x42; 6000])]),
    ];
    let catalog = with_rows(&db, "t", &["a"], &rows);
    let mut pager = db.open();
    let mut cat = catalog;
    let built = build_index(&mut pager, &cat, "i", "t", &cols(&["a"]), false, false)
        .unwrap()
        .unwrap();
    cat.put_index(built.clone());

    // The key comes back whole, which is the only thing a chain has to do.
    // The two blobs sort by their bytes, so the 0x41 one comes first.
    let entries = read_entries(&mut pager, built.root_page, 1).unwrap();
    assert_eq!(entries.len(), 2, "both wide entries are readable");
    assert_eq!(entries[0].key[0], Value::Blob(big));
    assert_eq!(entries[1].key[0], Value::Blob(vec![0x42; 6000]));

    // The root really is one leaf whose cells each name a chain, so the drop
    // below is freeing more than a page.
    let root_page = pager.read_page(built.root_page).unwrap();
    assert_eq!(root_page[0], page_type::INDEX_LEAF);
    let leaf = IndexLeaf::read(&mut pager, built.root_page).unwrap();
    assert_eq!(leaf.overflows.len(), 2);
    assert!(
        leaf.overflows.iter().all(|h| *h != 0),
        "both wide keys own a chain, got {:?}",
        leaf.overflows
    );

    let before = pager.page_count();
    drop_index(&mut pager, &cat, "i", false).unwrap().unwrap();
    // One b-tree page plus the two chains: 5 in total, matching the reference
    // file, which ended at 14 pages with the index root at 8.
    assert_eq!(
        pager.header().freelist_count as usize,
        1 + 2 * 2,
        "the b-tree page and both overflow chains went back"
    );
    assert_eq!(pager.page_count(), before, "the file does not shrink");
}

#[test]
fn an_index_written_against_a_table_reads_back_after_a_reopen() {
    // index-1.1c: the file is closed and reopened, and the index is still
    // there. The entries have to have been flushed with it.
    let db = Db::new("struct-reopen");
    let rows: Vec<(i64, Vec<Value>)> = (1..=40).map(|i| (i, vec![Value::Integer(i)])).collect();
    let catalog = with_rows(&db, "t", &["a"], &rows);
    let root = {
        let mut pager = db.open();
        let built = build_index(&mut pager, &catalog, "i", "t", &cols(&["a"]), false, false)
            .unwrap()
            .unwrap();
        pager.flush().unwrap();
        built.root_page
    };
    let mut pager = db.open();
    let entries = read_entries(&mut pager, root, 1).unwrap();
    let rowids: Vec<i64> = entries.iter().map(|e| e.rowid).collect();
    assert_eq!(rowids, (1..=40i64).collect::<Vec<_>>());
}

// --- 8. The findings a review of this module turned up ------------------------
//
// Each block below corresponds to one defect the review reported, and each names
// the sqlite3 3.53.4 output that shows what the reference does instead. The
// oracle was run rather than recalled; every hex and message quoted here came
// out of the real binary.

// --- 8a. A delete must not grow the file --------------------------------------

#[test]
fn a_delete_run_does_not_grow_the_database() {
    // The oracle. sqlite3 3.53.4:
    //   CREATE TABLE t(a); INSERT 500 rows; CREATE INDEX i ON t(a);
    //   before: page_count 7, freelist_count 0
    //   DELETE FROM t WHERE a<=25;
    //   after:  page_count 7, freelist_count 0     (file still 28672 bytes)
    //
    // The whole-tree rewrite this replaces took the same 25 deletes from 8 pages
    // to 108 and put 125 pages on the freelist, and those pages were never handed
    // out again. So the checks that matter are the two numbers rather than the
    // entry contents, which a leaked-page rewrite still got right.
    let db = Db::new("fix-delete-nogrow");
    let rows: Vec<(i64, Vec<Value>)> = (1..=500).map(|i| (i, vec![Value::Integer(i)])).collect();
    let catalog = with_rows(&db, "t", &["a"], &rows);
    let mut pager = db.open();
    let mut cat = catalog;
    let built = build_index(&mut pager, &cat, "i", "t", &cols(&["a"]), false, false)
        .unwrap()
        .unwrap();
    cat.put_index(built.clone());
    let pages_before = pager.page_count();
    let free_before = pager.header().freelist_count;
    assert_eq!(free_before, 0, "a fresh index has nothing on the freelist");

    let t = cat.get("t").unwrap().clone();
    for (rowid, values) in rows.iter().filter(|(r, _)| *r <= 25) {
        index_delete(&mut pager, &cat, &t, *rowid, values).unwrap();
    }
    assert_eq!(
        pager.page_count(),
        pages_before,
        "25 deletes must not append a single page"
    );
    assert_eq!(
        pager.header().freelist_count,
        0,
        "a delete removes a cell rather than a page, so the freelist stays empty"
    );
    assert_eq!(
        read_entries(&mut pager, built.root_page, 1).unwrap().len(),
        475,
        "and the index still holds every other row"
    );
}

#[test]
fn a_long_delete_run_stays_the_same_size() {
    // The oracle, and why the row count is not larger. sqlite3 3.53.4 does 2000
    // deletes from a 20000-row index in 502ms, leaving the file at 28672 bytes.
    // The rewrite this replaces took 18.9s on 2000 rows and left the file
    // hundreds of times its original size.
    //
    // 500 rows separates the two comfortably: the rewrite rebuilt the whole tree
    // per delete, so 250 deletes was already the slow shape, while an in-place
    // delete is one descent per delete.
    let db = Db::new("fix-delete-scale");
    let rows: Vec<(i64, Vec<Value>)> = (1..=500).map(|i| (i, vec![Value::Integer(i)])).collect();
    let catalog = with_rows(&db, "t", &["a"], &rows);
    let mut pager = db.open();
    let mut cat = catalog;
    let built = build_index(&mut pager, &cat, "i", "t", &cols(&["a"]), false, false)
        .unwrap()
        .unwrap();
    cat.put_index(built.clone());
    let pages_before = pager.page_count();
    let free_before = pager.header().freelist_count;
    assert_eq!(free_before, 0, "a fresh index has nothing on the freelist");
    let t = cat.get("t").unwrap().clone();

    for (rowid, values) in rows.iter().filter(|(r, _)| *r % 2 == 0) {
        index_delete(&mut pager, &cat, &t, *rowid, values).unwrap();
    }
    assert_eq!(
        pager.page_count(),
        pages_before,
        "250 deletes must not append a single page"
    );
    assert_eq!(
        pager.header().freelist_count,
        free_before,
        "and must not strand a page either"
    );
    let got: Vec<i64> = read_entries(&mut pager, built.root_page, 1)
        .unwrap()
        .iter()
        .map(|e| e.rowid)
        .collect();
    assert_eq!(got, (1..=500i64).filter(|r| r % 2 == 1).collect::<Vec<_>>());
}

#[test]
fn a_delete_leaves_the_index_matching_the_table_row_for_row() {
    // The cross-check that a leaked page or a lost subtree cannot pass. Deleting
    // every *third* row leaves the survivors spread across every leaf rather
    // than clustered at one end, so a tree that dropped a page loses something.
    let db = Db::new("fix-delete-agrees");
    let rows: Vec<(i64, Vec<Value>)> = (1..=400)
        .map(|i| (i, vec![Value::Integer(400 - i)]))
        .collect();
    let catalog = with_rows(&db, "t", &["a"], &rows);
    let mut pager = db.open();
    let mut cat = catalog;
    let built = build_index(&mut pager, &cat, "i", "t", &cols(&["a"]), false, false)
        .unwrap()
        .unwrap();
    cat.put_index(built.clone());
    assert!(
        tree_pages(&mut pager, built.root_page).unwrap().len() > 1,
        "the index has to have split for this to test anything"
    );
    let t = cat.get("t").unwrap().clone();

    let doomed: Vec<i64> = (1..=400i64).filter(|r| r % 3 == 1).collect();
    for (rowid, values) in rows.iter().filter(|(r, _)| doomed.contains(r)) {
        index_delete(&mut pager, &cat, &t, *rowid, values).unwrap();
    }

    // The expectation is derived from the table's own rows rather than written
    // out by hand. The key is `400 - rowid`, so a comparison that used the rowid
    // in both places would pass against an index with the right count and the
    // wrong contents.
    let want: Vec<String> = rows
        .iter()
        .filter(|(r, _)| !doomed.contains(r))
        .map(|(r, v)| format!("{}#{r}", v[0]))
        .collect();
    let mut want = want;
    want.sort();
    let got = pairs_as_text(&mut pager, built.root_page, 1);
    let mut got_sorted = got.clone();
    got_sorted.sort();
    assert_eq!(
        got_sorted, want,
        "the index holds exactly the surviving rows"
    );

    // And every page of the tree is still reached exactly once. A collapse that
    // double-referenced a page, or left one behind, would answer every lookup
    // above correctly and fail only here.
    let pages = tree_pages(&mut pager, built.root_page).unwrap();
    let mut sorted = pages.clone();
    sorted.dedup();
    assert_eq!(sorted.len(), pages.len(), "no page is reached twice");
}

#[test]
fn deleting_every_row_from_a_split_index_leaves_an_empty_root_leaf() {
    // The oracle. sqlite3 3.53.4, on a 300-row index emptied by `DELETE FROM t`:
    //   page_count 3, freelist_count 0
    //   page 1 type 83 (schema table), page 2 type 13 (table interior, 0 cells)
    //   page 3 type 10 (index leaf, 0 cells)
    // So the tree collapses all the way back to a single empty leaf, and the
    // root keeps its page number.
    let db = Db::new("fix-delete-emptied");
    let rows: Vec<(i64, Vec<Value>)> = (1..=300).map(|i| (i, vec![Value::Integer(i)])).collect();
    let catalog = with_rows(&db, "t", &["a"], &rows);
    let mut pager = db.open();
    let mut cat = catalog;
    let built = build_index(&mut pager, &cat, "i", "t", &cols(&["a"]), false, false)
        .unwrap()
        .unwrap();
    cat.put_index(built.clone());
    let t = cat.get("t").unwrap().clone();
    for (rowid, values) in &rows {
        index_delete(&mut pager, &cat, &t, *rowid, values).unwrap();
    }
    assert!(
        read_entries(&mut pager, built.root_page, 1)
            .unwrap()
            .is_empty(),
        "nothing is left"
    );
    let page = pager.read_page(built.root_page).unwrap();
    assert_eq!(
        page[0],
        page_type::INDEX_LEAF,
        "3.53.4 leaves the root an index leaf, not an interior page"
    );
    assert_eq!(
        built.root_page, 3,
        "the root never moved, which is what lets the schema hold one page number"
    );
    assert_eq!(
        tree_pages(&mut pager, built.root_page).unwrap().len(),
        1,
        "and the whole tree is one page again"
    );
}

#[test]
fn deleting_a_promoted_key_demotes_the_cell_below_it() {
    // A key a split promoted into an interior cell is in *no* leaf, so deleting
    // it cannot be a leaf removal — the cell has to be overwritten with the
    // largest key of the subtree it bounded. A descent that stopped at "the key
    // matches a separator" and called the entry missing, or a delete that dropped
    // the cell and freed the child below it, both lose rows silently.
    //
    // 300 rows forces several splits, so several keys are promoted, and every
    // row is deleted in turn with the index checked against what is left after
    // each one. That is the only way to catch a lost subtree: a lookup would
    // still find every key the tree still has, and a single end-of-run check
    // would not say *when* the tree lost one.
    let db = Db::new("fix-delete-promoted");
    let rows: Vec<(i64, Vec<Value>)> = (1..=300).map(|i| (i, vec![Value::Integer(i)])).collect();
    let catalog = with_rows(&db, "t", &["a"], &rows);
    let mut pager = db.open();
    let mut cat = catalog;
    let built = build_index(&mut pager, &cat, "i", "t", &cols(&["a"]), false, false)
        .unwrap()
        .unwrap();
    cat.put_index(built.clone());
    let t = cat.get("t").unwrap().clone();
    assert!(
        tree_pages(&mut pager, built.root_page).unwrap().len() > 1,
        "the index has to have split for a key to have been promoted"
    );
    let mut left: Vec<i64> = rows.iter().map(|(r, _)| *r).collect();
    for (rowid, values) in &rows {
        index_delete(&mut pager, &cat, &t, *rowid, values).unwrap();
        left.retain(|r| r != rowid);
        let got: Vec<i64> = read_entries(&mut pager, built.root_page, 1)
            .unwrap()
            .iter()
            .map(|e| e.rowid)
            .collect();
        assert_eq!(
            got, left,
            "after deleting rowid {rowid} the index must hold exactly what is left"
        );
    }
}

// --- 8b. A non-ASCII name must not abort the process ---------------------------

#[test]
fn a_non_ascii_index_name_is_not_reserved() {
    // sqlite3 3.53.4 accepts all of these, rc=0, and lists them in the schema:
    //   CREATE INDEX 日本語ABC ON t(a);
    //   CREATE INDEX ab日本      ON t(a);
    //   CREATE INDEX abc日本     ON t(a);
    // `is_reserved` sliced `name[..7]`, which panics whenever byte 7 falls inside
    // a character, and it is reached from the ordinary `CREATE INDEX` path — so
    // the process aborted on a statement the reference accepts.
    for name in ["日本語", "ab日本", "abc日本", "日本語ABC"] {
        assert!(
            !is_reserved(name),
            "{name:?} is not a sqlite_ name and must not abort"
        );
    }
    // The check is still a check. The reference's `CREATE INDEX sqlite_x ON t(a)`
    // is `object name reserved for internal use: sqlite_x`, it is
    // case-insensitive (`Sqlite_X` is refused too), and it has to match at the
    // start only (`mysqlite_x` is accepted, rc=0).
    for reserved in ["sqlite_x", "Sqlite_X", "SQLITE_X", "sqlite_autoindex_t_1"] {
        assert!(is_reserved(reserved), "{reserved} is reserved");
    }
    for fine in ["sqlite", "my_sqlite_x", "日本語sqlite_x", "xsqlite_"] {
        assert!(!is_reserved(fine), "{fine} is not reserved");
    }
}

#[test]
fn an_index_over_a_cjk_name_is_built_and_dropped() {
    // The end-to-end shape of the same defect: the name reaches `is_reserved` on
    // the create path and again on the drop path.
    let db = Db::new("fix-reserved-cjk");
    let catalog = with_rows(&db, "t", &["a"], &[(1i64, vec![Value::Integer(1)])]);
    let mut pager = db.open();
    let mut cat = catalog;
    let built = build_index(&mut pager, &cat, "日本語", "t", &cols(&["a"]), false, false)
        .unwrap()
        .unwrap();
    let root = built.root_page;
    cat.put_index(built);
    assert_eq!(read_entries(&mut pager, root, 1).unwrap().len(), 1);
    assert!(drop_index(&mut pager, &cat, "日本語", false)
        .unwrap()
        .is_some());
    // And the stored text keeps the name as the statement spelled it, which for
    // a CJK name is simply the name.
    assert_eq!(
        sql_for_statement("CREATE INDEX 日本語 ON t(a)").as_deref(),
        Some("CREATE INDEX 日本語 ON t(a)")
    );
}

// --- 8c. A refused UPDATE must leave every index describing the old row -------

#[test]
fn a_refused_update_leaves_every_index_describing_the_old_row() {
    // `i1` is a plain index on `t(a)` and `i2` is UNIQUE on `t(b)`. The update
    // makes rowid 1's new `b` collide with rowid 2's, so `i2` refuses — and by
    // then `i1` must not have been touched at all.
    //
    // The oracle, sqlite3 3.53.4, on the same schema and the same update:
    //   UNIQUE constraint failed: t.b
    //   SELECT rowid,a FROM t INDEXED BY i1 WHERE a=3  -> 0 rows
    //   and i1 still reads 1|1, 2|2
    // The version this fixes had already written `([3], 1)` into `i1` and taken
    // `([1], 1)` out, so `i1` held a phantom for a row whose value was still 1 —
    // a wrong query answer rather than an error, and one the caller could not
    // repair because it only ever sees the error.
    let db = Db::new("fix-update-atomic");
    let rows = vec![
        (1i64, vec![Value::Integer(1), Value::Integer(10)]),
        (2, vec![Value::Integer(2), Value::Integer(20)]),
    ];
    let catalog = with_rows(&db, "t", &["a", "b"], &rows);
    let mut pager = db.open();
    let mut cat = catalog;
    let i1 = build_index(&mut pager, &cat, "i1", "t", &cols(&["a"]), false, false)
        .unwrap()
        .unwrap();
    cat.put_index(i1.clone());
    let i2 = build_index(&mut pager, &cat, "i2", "t", &cols(&["b"]), true, false)
        .unwrap()
        .unwrap();
    cat.put_index(i2.clone());
    let t = cat.get("t").unwrap().clone();

    let e = index_update(
        &mut pager,
        &cat,
        &t,
        1,
        &[Value::Integer(1), Value::Integer(10)],
        1,
        &[Value::Integer(3), Value::Integer(20)],
    )
    .unwrap_err();
    assert_eq!(e.message, "UNIQUE constraint failed: t.b");
    assert_eq!(e.extended_code(), 2067);

    // `i1` names the row's `a`, and the row's `a` is still 1.
    assert_eq!(
        pairs_as_text(&mut pager, i1.root_page, 1),
        vec!["1#1", "2#2"],
        "the non-unique index must be exactly as it was"
    );
    assert_eq!(
        IndexTree::open(&mut pager, i1.root_page, 1)
            .lookup(&[Value::Integer(3)])
            .unwrap(),
        Vec::<i64>::new(),
        "3.53.4 answers 0 rows for a=3 after the refused update"
    );
    assert_eq!(
        pairs_as_text(&mut pager, i2.root_page, 1),
        vec!["10#1", "20#2"],
        "and so must the one that refused"
    );
}

#[test]
fn a_refused_update_keeps_a_multi_page_index_intact() {
    // The same defect with indexes that have split, so a half-finished write
    // would also leave the page count wrong and not only the contents.
    let db = Db::new("fix-update-atomic-large");
    let rows: Vec<(i64, Vec<Value>)> = (1..=400)
        .map(|i| (i, vec![Value::Integer(i), Value::Integer(1000 - i)]))
        .collect();
    let catalog = with_rows(&db, "t", &["a", "b"], &rows);
    let mut pager = db.open();
    let mut cat = catalog;
    let i1 = build_index(&mut pager, &cat, "i1", "t", &cols(&["a"]), false, false)
        .unwrap()
        .unwrap();
    cat.put_index(i1.clone());
    let i2 = build_index(&mut pager, &cat, "i2", "t", &cols(&["b"]), true, false)
        .unwrap()
        .unwrap();
    cat.put_index(i2.clone());
    let t = cat.get("t").unwrap().clone();
    let pages_before = pager.page_count();
    let before1 = pairs_as_text(&mut pager, i1.root_page, 1);

    // Row 1's new `b` collides with row 2's.
    let e = index_update(
        &mut pager,
        &cat,
        &t,
        1,
        &[Value::Integer(1), Value::Integer(999)],
        1,
        &[Value::Integer(2), Value::Integer(998)],
    )
    .unwrap_err();
    assert_eq!(e.message, "UNIQUE constraint failed: t.b");
    assert_eq!(
        pairs_as_text(&mut pager, i1.root_page, 1),
        before1,
        "a refused update writes to no index at all"
    );
    assert_eq!(
        read_entries(&mut pager, i1.root_page, 1).unwrap().len(),
        400
    );
    assert_eq!(pager.page_count(), pages_before, "and allocates nothing");
    assert_eq!(
        read_entries(&mut pager, i2.root_page, 1).unwrap().len(),
        400
    );
}

#[test]
fn an_accepted_update_still_moves_every_index() {
    // The cross-check on the other side of the same change: the pre-check must
    // not have broken the write path it was added to.
    let db = Db::new("fix-update-accepted");
    let rows = vec![
        (1i64, vec![Value::Integer(1), Value::Integer(10)]),
        (2, vec![Value::Integer(2), Value::Integer(20)]),
    ];
    let catalog = with_rows(&db, "t", &["a", "b"], &rows);
    let mut pager = db.open();
    let mut cat = catalog;
    let mut roots = std::collections::BTreeMap::new();
    for (name, c, unique) in [
        ("i1", cols(&["a"]), false),
        ("i2", cols(&["b"]), true),
        ("i3", cols(&["a", "b"]), false),
    ] {
        let built = build_index(&mut pager, &cat, name, "t", &c, unique, false)
            .unwrap()
            .unwrap();
        roots.insert(name, built.root_page);
        cat.put_index(built);
    }
    let t = cat.get("t").unwrap().clone();
    index_update(
        &mut pager,
        &cat,
        &t,
        1,
        &[Value::Integer(1), Value::Integer(10)],
        1,
        &[Value::Integer(7), Value::Integer(70)],
    )
    .unwrap();
    assert_eq!(
        pairs_as_text(&mut pager, roots["i1"], 1),
        vec!["2#2", "7#1"],
        "i1 followed the new key"
    );
    assert_eq!(
        pairs_as_text(&mut pager, roots["i2"], 1),
        vec!["20#2", "70#1"],
        "i2 followed the new key"
    );
    assert_eq!(
        pairs_as_text(&mut pager, roots["i3"], 2),
        vec!["2,20#2", "7,70#1"],
        "and the composite key moved as one"
    );
}

// --- 8d. A refused CREATE UNIQUE INDEX must not orphan its pages --------------

#[test]
fn a_refused_create_unique_index_frees_the_pages_it_allocated() {
    // The oracle. sqlite3 3.53.4, on 2000 distinct rows and a trailing
    // duplicate:
    //   CREATE UNIQUE INDEX i5 ON t(a);  -> UNIQUE constraint failed: t.a
    //   page_count 7, freelist_count 0, before and after.
    // So the reference leaves the file exactly as it was. The version this fixes
    // returned the error without freeing anything, so the half-built tree's 16
    // pages were orphaned — invisible to every content check and permanent,
    // because `Pager::allocate` never consults the freelist.
    //
    // The row count matters: with an index that fits one page there is only the
    // root to leak, which is one page, and an assertion on `page_count` would
    // have passed on a small database for the wrong reason. 2000 rows makes the
    // index many pages, which is where the leak shows.
    let db = Db::new("fix-create-leak");
    let mut rows: Vec<(i64, Vec<Value>)> =
        (1..=2000).map(|i| (i, vec![Value::Integer(i)])).collect();
    // The duplicate goes last, so the build gets to the end of the walk before
    // it refuses — which is what allocated the pages.
    rows.push((2001, vec![Value::Integer(7)]));
    let catalog = with_rows(&db, "t", &["a"], &rows);
    let mut pager = db.open();
    let pages_before = pager.page_count();
    let free_before = pager.header().freelist_count;

    let e = build_index(&mut pager, &catalog, "i5", "t", &cols(&["a"]), true, false).unwrap_err();
    assert_eq!(e.message, "UNIQUE constraint failed: t.a");
    let gained = pager.page_count() - pages_before;
    assert!(
        gained > 1,
        "a 2000-row index needs more than one page, so this test is only \
         meaningful if the build really did allocate some; it gained {gained}"
    );
    // The pages are freed rather than leaked. They are *not* given back to the
    // file, because a SQLite database never shrinks — a page that is not in use
    // goes on the freelist. What 3.53.4 shows as `freelist_count 0` is that
    // *its* delete frees a cell, not a page; this build allocated pages and has
    // to account for them somewhere.
    assert_eq!(
        pager.header().freelist_count as usize - free_before as usize,
        gained as usize,
        "every page the partial build allocated went on the freelist, so none \
         is orphaned and none is double-freed"
    );
}

#[test]
fn a_refused_create_over_a_small_table_frees_just_the_root() {
    // The same fix on a table small enough that the index never splits, which is
    // the case the earlier claim was actually about. 3.53.4:
    //   CREATE TABLE t(a,b,c); two rows; CREATE UNIQUE INDEX i5 ON t(a);
    //     -> UNIQUE constraint failed: t.a, and the file is unchanged.
    let db = Db::new("fix-create-leak-small");
    let rows = vec![
        (
            1i64,
            vec![Value::Integer(1), Value::Integer(2), Value::Integer(3)],
        ),
        (
            2,
            vec![Value::Integer(1), Value::Integer(2), Value::Integer(4)],
        ),
    ];
    let catalog = with_rows(&db, "t", &["a", "b", "c"], &rows);
    let mut pager = db.open();
    let pages_before = pager.page_count();
    let free_before = pager.header().freelist_count;
    let e = build_index(&mut pager, &catalog, "i5", "t", &cols(&["a"]), true, false).unwrap_err();
    assert_eq!(e.message, "UNIQUE constraint failed: t.a");
    assert_eq!(
        pager.header().freelist_count,
        free_before + 1,
        "the one page the build allocated is on the freelist, not orphaned"
    );
    assert_eq!(
        pager.page_count(),
        pages_before + 1,
        "the file never shrinks"
    );
}

#[test]
fn a_refused_create_leaves_an_index_that_already_existed_untouched() {
    // The refused build is a page-level operation, so it must not disturb
    // anything already in the file.
    let db = Db::new("fix-create-leak-clean");
    // A duplicate in the second column, so the build walks all 300 rows and only
    // then refuses: `b` holds `i % 100`, so rows 1 and 101 collide and the
    // refusal comes at the end of the walk rather than the start.
    let rows: Vec<(i64, Vec<Value>)> = (1..=300)
        .map(|i| (i, vec![Value::Integer(i), Value::Integer(i % 100)]))
        .collect();
    let catalog = with_rows(&db, "t", &["a", "b"], &rows);
    let mut pager = db.open();
    let mut cat = catalog;
    let good = build_index(&mut pager, &cat, "good", "t", &cols(&["a"]), false, false)
        .unwrap()
        .unwrap();
    cat.put_index(good.clone());
    let good_before = pairs_as_text(&mut pager, good.root_page, 1);
    let pages_before = pager.page_count();

    let e = build_index(&mut pager, &cat, "bad", "t", &cols(&["b"]), true, false).unwrap_err();
    assert_eq!(e.message, "UNIQUE constraint failed: t.b");
    assert_eq!(
        pairs_as_text(&mut pager, good.root_page, 1),
        good_before,
        "an index that already existed is untouched"
    );
    assert_eq!(
        cat.index("good").unwrap().root_page,
        good.root_page,
        "and its root page still names the live tree"
    );
    // The refused build's own pages are accounted for rather than orphaned, and
    // the index that was already there is not among them.
    let gained = pager.page_count() - pages_before;
    assert!(
        gained > 0,
        "a 300-row index needed pages, so the refusal had something to free"
    );
    assert_eq!(
        pager.header().freelist_count as usize,
        gained as usize,
        "and every one of them went on the freelist"
    );
    assert_eq!(
        read_entries(&mut pager, good.root_page, 1).unwrap().len(),
        300,
        "the surviving index still holds every row"
    );
}

// --- 8e. DESC ordering is not implemented, and the reference does it -----------

// Ignored rather than deleted, because the review's claim about the reference
// is correct and this is the test that would catch the fix. It cannot pass yet:
// the cell order is decided by `index::compare`, which this module does not own
// and which consults no direction, so closing the gap means a change to
// `index.rs`/`index_interior.rs` (an order that `Index::ascending` reverses) and
// this module then building its entries through that comparator.
#[test]
#[ignore = "DESC cell order needs index.rs to consult Index::ascending"]
fn a_descending_index_is_written_in_descending_cell_order() {
    // NOT IMPLEMENTED — this is the gap this test documents, and it is left
    // failing rather than removed, because the review's claim about the
    // reference is correct and the module does not reproduce it.
    //
    // The oracle. sqlite3 3.53.4, on
    //   CREATE TABLE t(a);
    //   CREATE INDEX di ON t(a DESC);
    //   CREATE INDEX ai ON t(a);
    //   INSERT INTO t VALUES(30),(10),(20);
    // decoding the two index leaf pages byte by byte:
    //
    //   di (the DESC index), cell-pointer array [4091, 4080, 4086]
    //     -> the cells hold (30,1), (20,3), (10,2)      i.e. descending
    //   ai (the ASC index), cell-pointer array [4086, 4080, 4091]
    //     -> the cells hold (10,2), (20,3), (30,1)      i.e. ascending
    //
    // So the reference stores a descending key **unchanged** and reverses the
    // order the cells sit in, which is what `Index::compare_keys` in
    // `catalog.rs` already models and what `entry_for`'s doc comment claims.
    // The claim is right about the reference and wrong about this module:
    // `index::compare` orders by value then rowid and consults no direction, so
    // both indexes come out byte-identical and ascending.
    let db = Db::new("fix-desc");
    let rows = vec![
        (1i64, vec![Value::Integer(30)]),
        (2, vec![Value::Integer(10)]),
        (3, vec![Value::Integer(20)]),
    ];
    let catalog = with_rows(&db, "t", &["a"], &rows);
    let mut pager = db.open();
    let di = build_index(
        &mut pager,
        &catalog,
        "di",
        "t",
        &[("a".to_string(), false)],
        false,
        false,
    )
    .unwrap()
    .unwrap();
    let got = read_pairs(&mut pager, di.root_page, 1)
        .unwrap()
        .iter()
        .map(|(k, r)| (k[0].as_i64().unwrap(), *r))
        .collect::<Vec<_>>();
    assert_eq!(
        got,
        vec![(30, 1), (20, 3), (10, 2)],
        "3.53.4 writes a DESC index's cells in descending order"
    );
}

#[test]
fn a_descending_index_stores_its_keys_unchanged() {
    // The half of the DESC behaviour this module *does* get right, and which is
    // the reason the other half is worth fixing rather than working around. The
    // oracle, sqlite3 3.53.4, `CREATE INDEX di ON t(a DESC)` over the rows 1, 2
    // and 3: the records are `01 01 01 01`, `01 01 02 02` and `01 01 03 03`, the
    // same bytes an ascending index over the same column writes. A key inverted
    // on the way in would produce a file the reference reads with the wrong
    // ordering.
    let db = Db::new("fix-desc-bytes");
    let rows: Vec<(i64, Vec<Value>)> = (1..=3).map(|i| (i, vec![Value::Integer(i)])).collect();
    let catalog = with_rows(&db, "t", &["a"], &rows);
    let mut pager = db.open();
    let di = build_index(
        &mut pager,
        &catalog,
        "di",
        "t",
        &[("a".to_string(), false)],
        false,
        false,
    )
    .unwrap()
    .unwrap();
    let got = read_pairs(&mut pager, di.root_page, 1).unwrap();
    let values: Vec<i64> = got.iter().map(|(k, _)| k[0].as_i64().unwrap()).collect();
    assert_eq!(
        values,
        vec![1, 2, 3],
        "the key is stored as it is, not inverted"
    );
    // And the ascending index over the same column holds the same key records,
    // which is the reference's behaviour: the two differ only in cell order.
    let ai = build_index(
        &mut pager,
        &catalog,
        "ai",
        "t",
        &[("a".to_string(), true)],
        false,
        false,
    )
    .unwrap()
    .unwrap();
    let di_leaf = IndexLeaf::read(&mut pager, di.root_page).unwrap();
    let ai_leaf = IndexLeaf::read(&mut pager, ai.root_page).unwrap();
    let di_bytes: Vec<Vec<u8>> = di_leaf.entries.iter().map(|e| e.encode()).collect();
    let ai_bytes: Vec<Vec<u8>> = ai_leaf.entries.iter().map(|e| e.encode()).collect();
    assert_eq!(
        di_bytes, ai_bytes,
        "the same key records, byte for byte, whichever direction is declared"
    );
}

// --- 8f. COLLATE is dropped by the parser, and the semantics go with it -------

// Ignored rather than deleted, for the same reason as the DESC one above: the
// oracle is right and this is the test that would catch the fix. It cannot pass
// while `parser.rs` throws the `COLLATE` clause away, and the clause has to
// survive the parse before this module has anywhere to put it.
#[test]
#[ignore = "a COLLATE clause has to survive the parse before it can be honoured"]
fn a_collated_index_is_ordered_by_the_collation() {
    // NOT IMPLEMENTED — this documents the gap, and the oracle below it is
    // correct, but the fix is not confined to this module.
    //
    // The oracle, sqlite3 3.53.4, on
    //   CREATE TABLE t(a); CREATE INDEX i1 ON t(a COLLATE NOCASE);
    //   INSERT INTO t VALUES('b'),('A'),('c'),('B'),('a');
    // the index leaf's cell-pointer array is [4085, 4067, 4091, 4073, 4079],
    // and the cells hold, in order: b, A, B, a, c — the *case-insensitive*
    // ordering, with the rowid breaking the tie. So `A` (rowid 2) comes before
    // `B` (rowid 4) and `a` (rowid 5) after `b` (rowid 1), which a BINARY
    // comparison would order the other way round entirely.
    //
    // This module has no way to express that. `parser.rs` eats the `COLLATE`
    // keyword and swallows the collation name as a bare identifier
    // (parser.rs:2379-2380), so `IndexSpec` has nowhere to put it, and both
    // `entry_for` and `index::compare` compare with BINARY. The consequence is
    // not a cosmetic mismatch: a lookup that used BINARY would both miss the
    // row and look at the wrong part of the tree.
    let db = Db::new("fix-collate");
    let rows = vec![
        (1i64, vec![Value::Text("b".into())]),
        (2, vec![Value::Text("A".into())]),
        (3, vec![Value::Text("c".into())]),
        (4, vec![Value::Text("B".into())]),
        (5, vec![Value::Text("a".into())]),
    ];
    let catalog = with_rows(&db, "t", &["a"], &rows);
    let mut pager = db.open();
    let i1 = build_index(&mut pager, &catalog, "i1", "t", &cols(&["a"]), false, false)
        .unwrap()
        .unwrap();
    let got: Vec<(String, i64)> = read_pairs(&mut pager, i1.root_page, 1)
        .unwrap()
        .iter()
        .map(|(k, r)| (k[0].as_str().unwrap().to_string(), *r))
        .collect();
    assert_eq!(
        got,
        vec![
            ("b".to_string(), 1),
            ("A".to_string(), 2),
            ("B".to_string(), 4),
            ("a".to_string(), 5),
            ("c".to_string(), 3),
        ],
        "3.53.4 orders a NOCASE index by the case-insensitive comparison"
    );
}

#[test]
fn the_parser_drops_the_collation_clause_this_module_would_need() {
    // What the module has to work with. The oracle for the *stored text* is the
    // reference: `CREATE INDEX i1 ON t(a COLLATE NOCASE)` stores exactly
    // `CREATE INDEX i1 ON t(a COLLATE NOCASE)`, so the clause is part of the
    // statement and `sql_for_statement` copies it out of the source correctly
    // even though the parse cannot see it.
    assert_eq!(
        sql_for_statement("CREATE INDEX i1 ON t(a COLLATE NOCASE)").as_deref(),
        Some("CREATE INDEX i1 ON t(a COLLATE NOCASE)"),
        "the stored text keeps the clause even though the parse drops it"
    );
    // And the parse really does drop it: the columns come back with no
    // collation, and `rebuild_index` cannot recover one either.
    let rebuilt = rebuild_index("i1", "CREATE INDEX i1 ON t(a COLLATE NOCASE)", 3).unwrap();
    assert_eq!(rebuilt.columns, vec!["a"]);
    assert_eq!(
        rebuilt.ascending,
        vec![true],
        "with no collation carried and no way to carry one"
    );
}

// --- 8g. Structural checks on the new delete ---------------------------------

#[test]
fn a_delete_leaves_no_page_referenced_twice() {
    // A collapse that re-pointed a parent at a page it already named would
    // answer every lookup correctly and fail only here, so the walk is the
    // check. `every_page_of_the_tree_is_reachable_exactly_once` covers the leaf
    // case; this one drives deletes all the way down to an emptied tree, where
    // the root itself is collapsed onto its last child.
    let db = Db::new("fix-delete-reachable");
    let rows: Vec<(i64, Vec<Value>)> = (1..=200).map(|i| (i, vec![Value::Integer(i)])).collect();
    let catalog = with_rows(&db, "t", &["a"], &rows);
    let mut pager = db.open();
    let mut cat = catalog;
    let built = build_index(&mut pager, &cat, "i", "t", &cols(&["a"]), false, false)
        .unwrap()
        .unwrap();
    cat.put_index(built.clone());
    let t = cat.get("t").unwrap().clone();

    // Descending rowid order, so the deletes come from the rightmost leaf
    // first, which is the case that makes a cell's own child empty.
    for (rowid, values) in rows.iter().rev() {
        index_delete(&mut pager, &cat, &t, *rowid, values).unwrap();
        let pages = tree_pages(&mut pager, built.root_page).unwrap();
        let mut sorted = pages.clone();
        sorted.dedup();
        assert_eq!(sorted.len(), pages.len(), "no page is reached twice");
    }
    assert!(read_entries(&mut pager, built.root_page, 1)
        .unwrap()
        .is_empty());
    assert_eq!(
        tree_pages(&mut pager, built.root_page).unwrap(),
        vec![built.root_page],
        "and the tree is the one page again"
    );
}

#[test]
fn a_delete_of_a_wide_key_frees_only_its_own_chain() {
    // A leaf rewrite carries the surviving entries' chain heads across, so a
    // delete that did not would reallocate a whole chain per remaining wide key
    // and strand the old ones — the leak `split_leaf`'s doc describes. The check
    // is the freelist count, which has to move by exactly the removed entry's
    // own chain and nothing else.
    let db = Db::new("fix-delete-overflow");
    // 6000-byte blobs spill: the index maxLocal on a 4096-byte page is
    // (usable-12)*64/255 - 23 = 1002, so each key gets a two-page chain.
    let rows: Vec<(i64, Vec<Value>)> = (1..=4)
        .map(|i| (i, vec![Value::Blob(vec![0x40 + i as u8; 6000])]))
        .collect();
    let catalog = with_rows(&db, "t", &["a"], &rows);
    let mut pager = db.open();
    let mut cat = catalog;
    let built = build_index(&mut pager, &cat, "i", "t", &cols(&["a"]), false, false)
        .unwrap()
        .unwrap();
    cat.put_index(built.clone());
    let t = cat.get("t").unwrap().clone();
    let before = pager.header().freelist_count;
    // `tree_pages` walks the b-tree only, so it does not count the overflow
    // chains; the leaf's own `overflows` do, and each has to be non-zero for the
    // check below to mean anything.
    let chains: Vec<u32> = IndexLeaf::read(&mut pager, built.root_page)
        .unwrap()
        .overflows
        .clone();
    assert_eq!(chains.len(), 4, "one chain per wide key");
    assert!(
        chains.iter().all(|h| *h != 0),
        "every key owns a chain, so a delete has one to free: {chains:?}"
    );
    // The chains are two pages each, so dropping one must put exactly two pages
    // back and no more.
    let chain_len = |p: &mut Pager, head: u32| -> u32 {
        let mut n = 0;
        let mut next = head;
        while next != 0 {
            let page = p.read_page(next).unwrap();
            next = u32::from_be_bytes([page[0], page[1], page[2], page[3]]);
            n += 1;
        }
        n
    };
    let dropped = chain_len(&mut pager, chains[1]);
    assert_eq!(
        dropped, 2,
        "a 6005-byte payload spills over two 4092-byte pages"
    );

    index_delete(&mut pager, &cat, &t, 2, &[Value::Blob(vec![0x42; 6000])]).unwrap();
    assert_eq!(
        pager.header().freelist_count,
        before + dropped,
        "exactly the removed entry's chain went back, and the other three kept \
         theirs — a rewrite that dropped the heads would have reallocated a \
         chain for each survivor and stranded the originals"
    );
    assert_eq!(
        read_entries(&mut pager, built.root_page, 1).unwrap().len(),
        3,
        "the row is gone"
    );
    // And the surviving keys still read back whole, which they would not if the
    // rewrite had dropped their heads and reallocated: the bytes would be
    // reassembled from a chain belonging to a different entry.
    let entries = read_entries(&mut pager, built.root_page, 1).unwrap();
    let lengths: Vec<usize> = entries
        .iter()
        .map(|e| match &e.key[0] {
            Value::Blob(b) => b.len(),
            other => panic!("not a blob: {other:?}"),
        })
        .collect();
    assert_eq!(lengths, vec![6000, 6000, 6000]);
    assert!(
        pager.header().freelist_count > before,
        "the removed entry's chain went back on the freelist"
    );
}

#[test]
fn an_index_with_a_shrinking_tree_never_shrinks_the_file() {
    // The whole point of the in-place delete, stated as one property: deleting
    // from an index does not append pages, however the tree is shaped. The
    // shapes are produced by deleting in an order that empties the leftmost
    // leaf, the rightmost leaf, a promoted key, and every row in turn.
    let db = Db::new("fix-delete-nofile-growth");
    let rows: Vec<(i64, Vec<Value>)> = (1..=250).map(|i| (i, vec![Value::Integer(i)])).collect();
    let catalog = with_rows(&db, "t", &["a"], &rows);
    let mut pager = db.open();
    let mut cat = catalog;
    let built = build_index(&mut pager, &cat, "i", "t", &cols(&["a"]), false, false)
        .unwrap()
        .unwrap();
    cat.put_index(built.clone());
    let t = cat.get("t").unwrap().clone();
    let pages_after_build = pager.page_count();

    // Ascending: empties the leftmost leaves first.
    for (rowid, values) in rows.iter().take(120) {
        index_delete(&mut pager, &cat, &t, *rowid, values).unwrap();
        assert_eq!(
            pager.page_count(),
            pages_after_build,
            "deleting rowid {rowid} appended a page"
        );
    }
    // Descending: empties the rightmost leaves and the promoted separators.
    for (rowid, values) in rows.iter().rev().take(100) {
        index_delete(&mut pager, &cat, &t, *rowid, values).unwrap();
        assert_eq!(
            pager.page_count(),
            pages_after_build,
            "deleting rowid {rowid} appended a page"
        );
    }
    // And the rest.
    for (rowid, values) in rows.iter().skip(120).rev().skip(100) {
        index_delete(&mut pager, &cat, &t, *rowid, values).unwrap();
    }
    assert!(read_entries(&mut pager, built.root_page, 1)
        .unwrap()
        .is_empty());
    assert_eq!(
        pager.page_count(),
        pages_after_build,
        "and emptying the index entirely appended nothing either"
    );
}
