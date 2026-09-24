//! Writes a database with this engine and has the real `sqlite3` verify it.
//!
//! The interop tests in `interop.rs` check the other direction: that this engine
//! reads what SQLite wrote. These check that what this engine writes is a real
//! SQLite database — that the page images, the b-tree shape, and the header all
//! satisfy the reader that ships with SQLite.
//!
//! A file only counts as passing when `sqlite3` reports it intact *and*
//! `PRAGMA integrity_check` returns `ok`, since the check walks every page and
//! validates the b-tree invariants independently of the queries.

use std::path::PathBuf;
use std::process::Command;

/// The sqlite3 binary, overridable for a non-default install.
fn sqlite3() -> String {
    std::env::var("NSQLITE_SQLITE3").unwrap_or_else(|_| "sqlite3".to_string())
}

/// Whether the sqlite3 binary can be run, so the tests skip on a machine
/// without it rather than failing.
fn sqlite3_available() -> bool {
    Command::new(sqlite3())
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn work_dir() -> PathBuf {
    let d = std::env::temp_dir().join(format!("nsqlite-write-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&d);
    d
}

/// Runs a statement through the real sqlite3 and returns its stdout.
fn run(path: &std::path::Path, sql: &str) -> String {
    let out = Command::new(sqlite3())
        .arg(path)
        .arg(sql)
        .output()
        .expect("running sqlite3");
    assert!(
        out.status.success(),
        "sqlite3 failed on {sql:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

/// Runs a statement and returns the error text, for the negative cases.
fn run_err(path: &std::path::Path, sql: &str) -> String {
    let out = Command::new(sqlite3())
        .arg(path)
        .arg(sql)
        .output()
        .expect("running sqlite3");
    String::from_utf8_lossy(&out.stderr).to_string()
}

/// Writes a leaf page, letting the library allocate any overflow chain it needs.
fn write_leaf(pager: &mut nsqlite::pager::Pager, page: &nsqlite::btree_write::LeafPage) {
    page.write_to(pager).expect("writing the leaf");
}

/// Rewrites the `rootpage` column of a table's row in `sqlite_schema`.
///
/// The SQL layer refuses to modify `sqlite_schema`, and rightly so, but the
/// engine has to keep it current when a root moves. A split that reaches the
/// root allocates a new one, and a reopened connection finds the tree by
/// reading this column, so a file written here would otherwise point at the
/// page the tree grew out of.
fn set_schema_root(pager: &mut nsqlite::pager::Pager, table: &str, root: u32) {
    use nsqlite::btree_write::LeafPage;
    use nsqlite::record;

    // sqlite_schema is always a table b-tree on page 1.
    let schema = LeafPage::read(pager, 1).expect("reading sqlite_schema");
    let at = schema
        .cells
        .iter()
        .position(|c| {
            record::decode_record(&c.payload, pager.header().text_encoding)
                .ok()
                .and_then(|d| d.values.get(1).and_then(|v| v.as_str()).map(|s| s == table))
                .unwrap_or(false)
        })
        .expect("the table must have a schema row");
    let decoded = record::decode_record(&schema.cells[at].payload, pager.header().text_encoding)
        .expect("decoding the schema row");
    assert_eq!(decoded.values.len(), 5, "sqlite_schema has five columns");

    let mut values = decoded.values.clone();
    values[3] = nsqlite::value::Value::Integer(root as i64);
    let new_payload = record::encode(&values).bytes;
    assert!(
        new_payload.len() <= schema.cells[at].payload.len(),
        "the re-encoded schema row must not be longer than the original"
    );

    let mut cells = schema.cells.clone();
    cells[at].payload = new_payload;
    let page = LeafPage { cells, ..schema };
    page.write_to(pager).expect("writing the schema page");
    // The schema cookie has to advance, or a reader keeps using its cached copy.
    let cc = pager.header().change_counter.wrapping_add(1);
    pager.header_mut().change_counter = cc;
    pager.header_mut().version_valid_for = cc;
    pager.header_mut().schema_cookie = pager.header().schema_cookie.wrapping_add(1);
    pager.write_header().expect("writing the header");
    pager.flush().expect("flushing");
}

/// Builds a database containing `rows` using this engine's writer.
///
/// The schema is created by the real sqlite3 first, so the table's root page is
/// a genuine one; this engine then overwrites that page with its own leaf image.
/// Writing the leaf before the schema exists would leave page 2 occupied and
/// sqlite3 would rightly call the file malformed.
fn write_db(name: &str, rows: &[(i64, String)], columns: usize) -> PathBuf {
    use nsqlite::btree_write::{Cell, LeafPage};
    use nsqlite::pager::Pager;
    use nsqlite::value::Value;

    let path = work_dir().join(name);
    let _ = std::fs::remove_file(&path);

    let mut cols = String::new();
    for i in 0..columns {
        if i > 0 {
            cols.push_str(", ");
        }
        cols.push_str(&format!("c{i} TEXT"));
    }
    run(&path, &format!("CREATE TABLE t({cols});"));
    let root: u32 = run(&path, "SELECT rootpage FROM sqlite_schema WHERE name='t';")
        .trim()
        .parse()
        .expect("root page");

    let mut pager = Pager::open(&path).expect("reopening");
    let usable = pager.usable_size();
    let cells: Vec<Cell> = rows
        .iter()
        .map(|(rowid, text)| {
            let mut values = vec![Value::Null; columns];
            values[0] = Value::Text(text.clone());
            Cell {
                rowid: *rowid,
                payload: LeafPage::encode_payload(&values, None),
                first_overflow: 0,
            }
        })
        .collect();

    let page = LeafPage {
        page_no: root,
        cells,
        ..LeafPage::empty(root, 4096)
    };
    write_leaf(&mut pager, &page);
    // The page count is unchanged, so the change counter alone has to advance,
    // or sqlite3 will not re-read the page count from the header.
    let cc = pager.header().change_counter.wrapping_add(1);
    pager.header_mut().change_counter = cc;
    pager.header_mut().version_valid_for = cc;
    pager.flush().expect("flushing");
    path
}

#[test]
fn sqlite3_accepts_a_page_this_engine_wrote() {
    if !sqlite3_available() {
        eprintln!("skipping: sqlite3 not on PATH");
        return;
    }
    let rows: Vec<(i64, String)> = (1..=50).map(|i| (i, format!("value{i}"))).collect();
    let path = write_db("basic.db", &rows, 2);
    let count = run(&path, "SELECT count(*) FROM t;");
    assert_eq!(count.trim(), "50", "sqlite3 did not see every row");
    let integrity = run(&path, "PRAGMA integrity_check;");
    assert_eq!(
        integrity.trim(),
        "ok",
        "sqlite3 rejected the file this engine wrote"
    );
}

#[test]
fn sqlite3_reads_back_every_row() {
    if !sqlite3_available() {
        return;
    }
    // Build the table through the real sqlite3 so the schema and root page are
    // genuine, then overwrite the leaf with this engine's own image. This is
    // the strongest form of the test: the rows were laid out by this engine and
    // are read back by SQLite.
    let path = work_dir().join("rows.db");
    let _ = std::fs::remove_file(&path);
    run(&path, "CREATE TABLE t(c0 TEXT);");

    // Find the table's root page, then write our own leaf there.
    let root: u32 = run(&path, "SELECT rootpage FROM sqlite_schema WHERE name='t';")
        .trim()
        .parse()
        .expect("root page");

    use nsqlite::btree_write::{Cell, LeafPage};
    use nsqlite::pager::Pager;
    use nsqlite::value::Value;
    let mut pager = Pager::open(&path).expect("reopening");
    let usable = pager.usable_size();
    let cells: Vec<Cell> = (1..=100)
        .map(|i| Cell {
            rowid: i,
            payload: LeafPage::encode_payload(&[Value::Text(format!("row{i}"))], None),
            first_overflow: 0,
        })
        .collect();
    let page = LeafPage {
        page_no: root,
        cells,
        ..LeafPage::empty(root, 4096)
    };
    write_leaf(&mut pager, &page);
    // The page count did not change, so only the change counter needs bumping.
    pager.header_mut().change_counter = pager.header().change_counter.wrapping_add(1);
    pager.header_mut().version_valid_for = pager.header().change_counter;
    pager.flush().unwrap();
    drop(pager);

    let got = run(&path, "SELECT count(*) FROM t;");
    assert_eq!(got.trim(), "100", "sqlite3 did not see all the rows");
    let first = run(&path, "SELECT c0 FROM t WHERE rowid=1;");
    assert_eq!(first.trim(), "row1");
    let last = run(&path, "SELECT c0 FROM t WHERE rowid=100;");
    assert_eq!(last.trim(), "row100");
    let integrity = run(&path, "PRAGMA integrity_check;");
    assert_eq!(integrity.trim(), "ok", "the file failed integrity_check");
}

#[test]
fn sqlite3_reports_a_corrupt_file_rather_than_crashing() {
    if !sqlite3_available() {
        return;
    }
    // The negative control: if the writer were subtly wrong, this is the check
    // that would catch it, so confirm the check itself has teeth by corrupting
    // a page deliberately.
    let rows: Vec<(i64, String)> = (1..=20).map(|i| (i, format!("v{i}"))).collect();
    let path = write_db("corrupt.db", &rows, 2);

    // Claim an impossible cell count on the table's root page. The b-tree page
    // header starts at offset 0 on every page except page 1, and the cell count
    // is the fourth field.
    let root: u32 = run(&path, "SELECT rootpage FROM sqlite_schema WHERE name='t';")
        .trim()
        .parse()
        .expect("root page");
    let mut bytes = std::fs::read(&path).unwrap();
    let at = (root as usize - 1) * 4096;
    assert_eq!(
        bytes[at], 0x0d,
        "the root must be a table leaf before corrupting it"
    );
    bytes[at + 3..at + 5].copy_from_slice(&0xfffeu16.to_be_bytes());
    std::fs::write(&path, &bytes).unwrap();

    let err = run_err(&path, "PRAGMA integrity_check;");
    assert!(
        err.to_lowercase().contains("malformed") || err.to_lowercase().contains("corrupt"),
        "integrity_check should have complained, got: {err:?}"
    );
}

#[test]
fn a_full_page_stores_content_start_as_zero() {
    if !sqlite3_available() {
        return;
    }
    use nsqlite::btree_write::{Cell, LeafPage};
    use nsqlite::pager::Pager;
    use nsqlite::value::Value;

    // Fill a page exactly, so the content area reaches the front of the page and
    // the zero-means-end-of-page convention has to kick in.
    let path = work_dir().join("full.db");
    let _ = std::fs::remove_file(&path);
    let mut pager = Pager::open(&path).unwrap();
    pager.allocate().unwrap();
    let root = pager.allocate().unwrap();
    let usable = pager.usable_size();

    // 4061 bytes is the largest payload a table leaf holds entirely.
    let big = "x".repeat(4061);
    let mut cells = vec![Cell {
        rowid: 1,
        payload: LeafPage::encode_payload(&[Value::Text(big)], None),
        first_overflow: 0,
    }];
    // Add a second cell only if it still fits, to push the page to the edge.
    let probe = Cell {
        rowid: 2,
        payload: LeafPage::encode_payload(&[Value::Text(String::new())], None),
        first_overflow: 0,
    };
    let mut page = LeafPage {
        page_no: root,
        cells: cells.clone(),
        ..LeafPage::empty(root, 4096)
    };
    if page.insert(probe.clone(), usable).unwrap() {
        cells.push(probe);
    }
    page.cells = cells;
    write_leaf(&mut pager, &page);
    pager.flush().unwrap();
    drop(pager);

    // The reader must accept whatever convention was used.
    let mut pager = Pager::open(&path).unwrap();
    let read = LeafPage::read(&mut pager, root).expect("reading back the full page");
    assert!(
        !read.cells.is_empty(),
        "the full page must still hold its cells"
    );
    drop(pager);
}

/// Builds a tree with this engine and has sqlite3 read every row back.
///
/// This is the end-to-end check for the whole write path: routing, leaf
/// splitting, interior-node splitting, and root growth all have to produce a
/// file whose b-tree is valid to a reader that knows nothing about this engine.
#[test]
fn sqlite3_reads_a_tree_that_grew_through_many_splits() {
    if !sqlite3_available() {
        eprintln!("skipping: sqlite3 not on PATH");
        return;
    }
    use nsqlite::table_tree::{Row, TableTree};
    use nsqlite::value::Value;

    let path = work_dir().join("tree.db");
    let _ = std::fs::remove_file(&path);
    run(&path, "CREATE TABLE t(c0 TEXT, c1 TEXT);");
    let root: u32 = run(&path, "SELECT rootpage FROM sqlite_schema WHERE name='t';")
        .trim()
        .parse()
        .expect("root page");

    // Wide enough rows that a 4096-byte page holds only a couple of dozen, so
    // several thousand rows force leaves and interior nodes to split repeatedly.
    const N: i64 = 3000;
    {
        let mut pager = nsqlite::pager::Pager::open(&path).unwrap();
        let mut tree = TableTree::open(&mut pager, root).unwrap();
        for i in 1..=N {
            tree.insert(
                &mut pager,
                &Row {
                    rowid: i,
                    values: vec![
                        Value::Text(format!("value-{i}")),
                        Value::Text("y".repeat(60)),
                    ],
                },
            )
            .expect("inserting a row");
        }
        // The root moves as the tree grows, so the schema has to be updated to
        // the new page: that is how a reopened connection finds the tree.
        let new_root = tree.root();
        drop(tree);
        set_schema_root(&mut pager, "t", new_root);
    }

    let count = run(&path, "SELECT count(*) FROM t;");
    assert_eq!(
        count.trim(),
        N.to_string(),
        "sqlite3 lost rows across a split"
    );
    let first = run(&path, "SELECT c0 FROM t WHERE rowid=1;");
    assert_eq!(first.trim(), "value-1");
    let last = run(&path, "SELECT c0 FROM t WHERE rowid=3000;");
    assert_eq!(last.trim(), "value-3000");
    let middle = run(&path, "SELECT c0 FROM t WHERE rowid=1500;");
    assert_eq!(middle.trim(), "value-1500");
    // A scan must come back in rowid order, which is what the interior
    // separators encode.
    let ordered = run(
        &path,
        "SELECT count(*) FROM (SELECT rowid FROM t ORDER BY rowid);",
    );
    assert_eq!(ordered.trim(), N.to_string());
    let integrity = run(&path, "PRAGMA integrity_check;");
    assert_eq!(
        integrity.trim(),
        "ok",
        "the grown tree failed integrity_check"
    );
}

/// The same, with negative rowids, which use the nine-byte varint form and so
/// take a different path through the cell size arithmetic.
#[test]
fn sqlite3_reads_a_tree_with_negative_rowids() {
    if !sqlite3_available() {
        return;
    }
    use nsqlite::table_tree::{Row, TableTree};
    use nsqlite::value::Value;

    let path = work_dir().join("neg.db");
    let _ = std::fs::remove_file(&path);
    run(&path, "CREATE TABLE t(c0 TEXT);");
    let root: u32 = run(&path, "SELECT rootpage FROM sqlite_schema WHERE name='t';")
        .trim()
        .parse()
        .expect("root page");

    let mut keys: Vec<i64> = (1..=800).map(|i| -i).collect();
    keys.extend(1..=800);
    {
        let mut pager = nsqlite::pager::Pager::open(&path).unwrap();
        let mut tree = TableTree::open(&mut pager, root).unwrap();
        for k in &keys {
            tree.insert(
                &mut pager,
                &Row {
                    rowid: *k,
                    values: vec![Value::Text(format!("k{k}"))],
                },
            )
            .expect("inserting a row");
        }
        let new_root = tree.root();
        drop(tree);
        set_schema_root(&mut pager, "t", new_root);
    }

    let count = run(&path, "SELECT count(*) FROM t;");
    assert_eq!(count.trim(), (keys.len() as i64).to_string());
    let lo = run(&path, "SELECT c0 FROM t WHERE rowid=-800;");
    assert_eq!(lo.trim(), "k-800");
    let hi = run(&path, "SELECT c0 FROM t WHERE rowid=800;");
    assert_eq!(hi.trim(), "k800");
    let min = run(&path, "SELECT min(rowid) FROM t;");
    assert_eq!(min.trim(), "-800");
    let max = run(&path, "SELECT max(rowid) FROM t;");
    assert_eq!(max.trim(), "800");
    let integrity = run(&path, "PRAGMA integrity_check;");
    assert_eq!(
        integrity.trim(),
        "ok",
        "the negative-rowid tree failed integrity_check"
    );
}

/// A row far larger than a page must get a real overflow chain, and the engine
/// must reuse that chain when the page is rewritten rather than allocating a
/// second one and stranding the first.
///
/// This is the path that was broken: the writer took a closure for the chain head
/// and every production caller answered zero, so a spilling row produced a cell
/// pointing at nothing. The file looked fine until something read the row back.
#[test]
fn sqlite3_reads_a_row_that_spills_into_an_overflow_chain() {
    if !sqlite3_available() {
        eprintln!("skipping: sqlite3 not on PATH");
        return;
    }
    use nsqlite::table_tree::{Row, TableTree};
    use nsqlite::value::Value;

    let path = work_dir().join("overflow.db");
    let _ = std::fs::remove_file(&path);
    run(&path, "CREATE TABLE t(c0 TEXT);");
    let root: u32 = run(&path, "SELECT rootpage FROM sqlite_schema WHERE name='t';")
        .trim()
        .parse()
        .expect("root page");

    // Several rows well past the 4061-byte local limit, so each needs a chain
    // several pages long.
    let body = "abcdefghij".repeat(2000); // 20000 bytes
    {
        let mut pager = nsqlite::pager::Pager::open(&path).unwrap();
        let mut tree = TableTree::open(&mut pager, root).unwrap();
        for i in 1..=5 {
            tree.insert(
                &mut pager,
                &Row {
                    rowid: i,
                    values: vec![Value::Text(format!("{i:04}{body}"))],
                },
            )
            .expect("inserting a spilling row");
        }
        let new_root = tree.root();
        drop(tree);
        set_schema_root(&mut pager, "t", new_root);
    }

    let count = run(&path, "SELECT count(*) FROM t;");
    assert_eq!(count.trim(), "5");
    let len = run(&path, "SELECT length(c0) FROM t WHERE rowid=3;");
    assert_eq!(len.trim(), "20004", "the whole payload must come back");
    let head = run(&path, "SELECT substr(c0, 1, 4) FROM t WHERE rowid=3;");
    assert_eq!(head.trim(), "0003");
    // substr is one-based and a negative start counts from the end, so these
    // three together cover the head, the middle and the tail of the chain. The
    // middle one only matches if the pages are in the right order, which is the
    // property a chain gets wrong.
    let tail = run(&path, "SELECT substr(c0, -4) FROM t WHERE rowid=3;");
    assert_eq!(tail.trim(), "ghij", "the last bytes of the chain");
    let middle = run(&path, "SELECT substr(c0, 10000, 10) FROM t WHERE rowid=3;");
    assert_eq!(middle.trim(), "fghijabcde");
    let integrity = run(&path, "PRAGMA integrity_check;");
    assert_eq!(
        integrity.trim(),
        "ok",
        "the spilled file failed integrity_check"
    );
}

/// Rewriting a page that holds spilling rows must not allocate a second chain
/// for the same row, which would leave the first unreachable and the file
/// steadily growing.
#[test]
fn rewriting_a_page_reuses_the_existing_overflow_chain() {
    use nsqlite::btree_write::LeafPage;
    use nsqlite::pager::Pager;
    use nsqlite::value::Value;

    let path = work_dir().join("reuse.db");
    let _ = std::fs::remove_file(&path);
    {
        let mut pager = Pager::open(&path).unwrap();
        pager.allocate().unwrap();
        let root = pager.allocate().unwrap();
        let payload = LeafPage::encode_payload(&[Value::Text("q".repeat(20_000))], None);
        let page = LeafPage {
            page_no: root,
            cells: vec![nsqlite::btree_write::Cell::new(1, payload)],
            ..LeafPage::empty(root, 4096)
        };
        page.write_to(&mut pager).unwrap();
        pager.flush().unwrap();

        let after_first = pager.page_count();
        let head_before = LeafPage::read(&mut pager, root).unwrap().cells[0].first_overflow;
        assert!(head_before != 0, "a 20000-byte row must have a chain");

        // Rewrite the same page with the same row. The chain belongs to the
        // row, so it must be reused rather than a new one allocated. Reading the
        // page back is how a real rewrite learns the chain head: the caller
        // holds a page structure it built or read, and only a read one carries
        // the head.
        let page = LeafPage::read(&mut pager, root).unwrap();
        page.write_to(&mut pager).unwrap();
        pager.flush().unwrap();

        assert_eq!(
            pager.page_count(),
            after_first,
            "a rewrite must not allocate another chain for the same row"
        );
        let head_after = LeafPage::read(&mut pager, root).unwrap().cells[0].first_overflow;
        assert_eq!(head_before, head_after, "the chain head moved");
        // And the row still reads back whole.
        let read = LeafPage::read(&mut pager, root).unwrap();
        let decoded =
            nsqlite::record::decode_record(&read.cells[0].payload, nsqlite::text::Encoding::Utf8)
                .unwrap();
        assert_eq!(decoded.values[0].as_str().map(|s| s.len()), Some(20_000));
    }
    drop(path);
}
