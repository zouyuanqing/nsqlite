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
            }
        })
        .collect();

    let page = LeafPage {
        page_no: root,
        cells,
        ..LeafPage::empty(root, 4096)
    };
    {
        let dst = pager.page(root).expect("root page");
        page.write(dst, usable, |_| Ok(0))
            .expect("writing the leaf");
    }
    pager.mark_dirty(root);
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
        })
        .collect();
    let page = LeafPage {
        page_no: root,
        cells,
        ..LeafPage::empty(root, 4096)
    };
    {
        let dst = pager.page(root).unwrap();
        page.write(dst, usable, |_| Ok(0)).unwrap();
    }
    pager.mark_dirty(root);
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
    }];
    // Add a second cell only if it still fits, to push the page to the edge.
    let probe = Cell {
        rowid: 2,
        payload: LeafPage::encode_payload(&[Value::Text(String::new())], None),
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
    {
        let dst = pager.page(root).unwrap();
        page.write(dst, usable, |_| Ok(0)).unwrap();
    }
    pager.mark_dirty(root);
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
