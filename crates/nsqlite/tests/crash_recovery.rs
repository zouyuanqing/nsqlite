//! Crash recovery: does replaying a hot journal actually undo a transaction?
//!
//! The journal exists for exactly one reason, and nothing else in the tree
//! tests that reason: a transaction whose changes are already in the database
//! file, but which never committed, has to be *undone* by the next connection
//! to open the file. A rollback that restores the right number of wrong rows is
//! a rollback that does not work, so every assertion here compares row
//! *values*, not counts.
//!
//! # How a crash is simulated
//!
//! Not by calling the rollback path, and not by a clean `ROLLBACK` — both would
//! pass with a broken replay. A crash is made by doing exactly what a crash
//! leaves behind, through the public SQL API:
//!
//! 1. `BEGIN` creates the journal and records the database's current size.
//! 2. The statements are executed. This engine flushes after every INSERT, so
//!    the uncommitted bytes really are in the file while the transaction is
//!    still open.
//! 3. The `Connection` is *dropped* with the transaction open. `Pager` has no
//!    `Drop` that flushes or rolls back, and `Journal::drop` deliberately keeps
//!    its file, so this is byte-for-byte the state a power loss leaves: a
//!    database with an uncommitted change on it and a journal next to it.
//!
//! Recovery is then triggered the only way it ever happens in practice — by
//! opening the database, which makes the pager find the journal and replay it
//! before the connection sees a single row.
//!
//! # What real sqlite3 does
//!
//! Every expectation below was checked against sqlite3 3.53.4, not recalled.
//! The results, and how to reproduce them, are in `tools/crash_test.sh`; the
//! short version:
//!
//! | state left on disk | what sqlite3 does |
//! |---|---|
//! | hot journal, uncommitted pages on disk | rolls back, restores the file **byte for byte**, deletes the journal |
//! | journal grew the file | truncates the file back to the journalled size |
//! | journal torn mid-record | replays the whole records, ignores the torn one |
//! | a record's bytes damaged | replays up to it, then stops and deletes the journal |
//! | journal header zeroed (`PRAGMA journal_mode=PERSIST`) | ignores it entirely, leaves the file alone |
//! | no journal at all | leaves the file alone |
//!
//! The last two matter most: they are what stops a *committed* transaction from
//! being rolled back by a stale journal.
//!
//! The first one is the strongest expectation in the file. Real sqlite3 was
//! asked to recover journals of this shape and restores the file exactly —
//! differing offsets: `[]`. So the tests below do not merely ask whether the
//! rows came back, which a replay that re-applies the crashed *file header*
//! would satisfy while leaving a freelist naming a page that has been given a
//! b-tree page image; they ask whether the file is the file that was there
//! before, and they ask the real sqlite3 whether it considers it sound.

use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};

use nsqlite::connection::{Connection, Outcome};
use nsqlite::journal;

/// The page size every database in this file uses.
const PAGE_SIZE: u32 = 4096;

/// The schema the tests share. `id` is an `INTEGER PRIMARY KEY`, so it is a
/// rowid alias: it comes back from the cell key, which makes the row values in
/// every assertion a genuine check of the restored page image.
const SCHEMA: &str = "CREATE TABLE t(id INTEGER PRIMARY KEY, name TEXT, score INTEGER, tag TEXT);";

/// The query every test reads with.
const SELECT_ALL: &str = "SELECT id, name, score, tag FROM t;";

/// A row as strings, so an assertion reads like the table it describes.
type Row = Vec<String>;

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// Gives each test its own directory, so a journal left by one cannot be
/// mistaken for a hot journal by another.
fn work_dir(tag: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let d = std::env::temp_dir().join(format!("nsqlite-crash-{}-{tag}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).expect("creating the work directory");
    d
}

/// A database built by committing `rows` in order, so a test has a known
/// starting state that is genuinely on disk.
fn committed_db(tag: &str, rows: &[(i64, &str, i64, &str)]) -> PathBuf {
    let path = work_dir(tag).join("crash.db");
    let mut conn = Connection::open(&path).expect("creating the database");
    conn.execute_script(SCHEMA).expect("creating the table");
    for (id, name, score, t) in rows {
        conn.execute_script(&format!(
            "INSERT INTO t VALUES({id},'{name}',{score},'{t}');"
        ))
        .expect("committing a row");
    }
    drop(conn);
    assert!(
        !journal::journal_path(&path).exists(),
        "a committed database has no journal"
    );
    path
}

/// The file's length in pages, read without opening the pager — opening it
/// would trigger the very recovery a test is in the middle of setting up.
fn pages_on_disk(path: &Path) -> u64 {
    std::fs::metadata(path).expect("the database file").len() / PAGE_SIZE as u64
}

/// The first 100 bytes of the file: the header, and nothing else.
///
/// This is the part of a page 1 that `DbHeader` owns, so it is the part a
/// recovered file can be wrong about without the rows looking wrong.
fn file_header(path: &Path) -> Vec<u8> {
    let raw = std::fs::read(path).expect("reading the database file");
    assert!(raw.len() >= 100, "the file is too short to hold a header");
    raw[..100].to_vec()
}

/// A database on disk, the whole file.
fn file_bytes(path: &Path) -> Vec<u8> {
    std::fs::read(path).expect("reading the database file")
}

/// Asserts that the real sqlite3 calls the file at `path` undamaged, and says
/// what it said when it does not.
///
/// The pager can put every row back and still leave a file sqlite3 calls
/// corrupt, so this is the only check that catches it: a row-level assertion
/// cannot see a freelist that names a page now holding something else. Skips
/// when sqlite3 is not on PATH rather than failing, since the rest of this file
/// does not need it.
fn assert_sqlite3_agrees(path: &Path) {
    if !sqlite3_available() {
        eprintln!("skipping the integrity_check: sqlite3 is not on PATH");
        return;
    }
    let out = Command::new(sqlite3())
        .arg(path)
        .arg("PRAGMA integrity_check;")
        .output()
        .expect("running sqlite3");
    let said = String::from_utf8_lossy(&out.stdout).to_string();
    assert_eq!(
        said.trim(),
        "ok",
        "sqlite3 does not consider the recovered file sound, at {}. It said: {said:?}",
        path.display()
    );
}

/// Reads a table through a fresh connection.
///
/// Opening the pager is what replays a hot journal, so this *is* the recovery.
fn read_rows(path: &Path) -> Vec<Row> {
    let mut conn =
        Connection::open(path).unwrap_or_else(|e| panic!("opening {}: {e}", path.display()));
    let outcomes = conn
        .execute_script(SELECT_ALL)
        .unwrap_or_else(|e| panic!("reading t: {e}"));
    let Outcome::Query { rows, .. } = &outcomes[0] else {
        panic!("expected a query result, got {:?}", outcomes[0]);
    };
    let mut out: Vec<Row> = rows
        .iter()
        .map(|r| r.values.iter().map(|v| v.to_string()).collect())
        .collect();
    // The table has no ORDER BY here, so the comparison would depend on scan
    // order. Sorting by the id, which is the leading column, makes the
    // comparison about the values rather than about the order they came out in.
    out.sort();
    out
}

/// Runs `script` in a transaction that is never committed, then drops the
/// connection with the transaction still open.
///
/// Returns the page count the crash left behind, which is more than the original
/// whenever the transaction grew the file.
fn crash(path: &Path, script: &str) -> u64 {
    let mut conn = Connection::open(path).expect("reopening for the crash");
    conn.execute_script("BEGIN;").expect("BEGIN");
    assert!(conn.in_transaction(), "the transaction has to be open");
    conn.execute_script(script)
        .expect("the interrupted transaction's statements");
    // The statements above flushed as they went, so the uncommitted bytes are
    // already in the file; only the journal says they should not be there.
    let pages = pages_on_disk(path);
    drop(conn);
    assert!(
        journal::journal_path(path).exists(),
        "abandoning an open transaction must leave its journal on disk"
    );
    pages
}

/// The journal's records, as `(page number, contents)`.
fn records(path: &Path) -> Vec<(u32, Vec<u8>)> {
    let hot = journal::find_hot(path)
        .unwrap_or_else(|e| panic!("probing for a hot journal: {e}"))
        .expect("the journal is hot");
    let original_size = hot.original_size();
    let recovered = hot.recover().expect("recovering the journal");
    for r in &recovered {
        assert!(
            r.page_no > 0 && r.page_no <= original_size,
            "a journal record for page {} is outside the file's original \
             {original_size} pages",
            r.page_no
        );
    }
    recovered
        .into_iter()
        .map(|r| (r.page_no, r.contents))
        .collect()
}

/// The offset of record `n`'s first byte.
///
/// The header is 28 bytes padded out to the sector size, and each record is a
/// four-byte page number, a whole page, and a four-byte checksum.
fn record_offset(n: usize, sector: u32) -> u64 {
    let padded = journal::HEADER_SIZE as usize;
    let padded = padded.div_ceil(sector as usize) * sector as usize;
    padded as u64 + n as u64 * (4 + PAGE_SIZE as usize + 4) as u64
}

/// The sector size the pager writes journals with.
const SECTOR: u32 = 512;

/// A byte offset inside a page image that the record checksum actually covers.
///
/// The checksum is deliberately sparse — every `CHECKSUM_STRIDE`-th byte,
/// counting down from the end — so it catches a record that was not written
/// whole without being a digest over the whole page. A test that damages a
/// record has to flip one of *these* bytes: flipping a byte the sample misses
/// leaves the checksum valid, and the record is then correctly recovered. That
/// is the behaviour the real format has, and it is why a "corrupted" record in
/// practice means a torn or scrambled one, not an arbitrary single-byte edit.
fn sampled_offset() -> usize {
    PAGE_SIZE as usize - journal::CHECKSUM_STRIDE
}

// ---------------------------------------------------------------------------
// 1. The basic case.
// ---------------------------------------------------------------------------

#[test]
fn replay_undoes_an_interrupted_transaction() {
    let path = committed_db("basic", &[(1, "alpha", 10, "a"), (2, "beta", 20, "b")]);
    let before = read_rows(&path);
    assert_eq!(before.len(), 2);

    let _ = crash(
        &path,
        "INSERT INTO t VALUES(3,'gamma',30,'c'); INSERT INTO t VALUES(4,'delta',40,'d');",
    );

    let after = read_rows(&path);
    assert_eq!(
        after, before,
        "every row that was there before is still there with the same values, \
         and every row the interrupted transaction added is gone"
    );
    assert!(
        !journal::journal_path(&path).exists(),
        "a replayed journal is removed, or the next open would roll back again"
    );
}

#[test]
fn the_interrupted_rows_are_really_on_disk_before_the_reopen() {
    // Without this, "the new rows are gone afterwards" could pass because they
    // were never written. The test has to show the database is mid-transaction
    // first.
    let path = committed_db("ondisk", &[(1, "alpha", 10, "a")]);
    let _ = crash(&path, "INSERT INTO t VALUES(2,'ghost',20,'g');");

    // Read the pages directly, without the pager replaying anything.
    let raw = std::fs::read(&path).expect("reading the database file");
    let hot = journal::find_hot(&path).unwrap().expect("hot");
    let recs = hot.recover().expect("recovering");
    assert!(
        !recs.is_empty(),
        "the transaction journalled the pages it changed"
    );
    for r in &recs {
        let at = (r.page_no as usize - 1) * PAGE_SIZE as usize;
        assert_ne!(
            &raw[at..at + PAGE_SIZE as usize],
            &r.contents[..],
            "page {} was journalled but is unchanged on disk, so the \
             transaction's write never landed and the test would prove nothing",
            r.page_no
        );
    }
}

// ---------------------------------------------------------------------------
// 2. The transaction changed existing rows, not just added new ones.
// ---------------------------------------------------------------------------

#[test]
fn replay_undoes_an_interrupted_update() {
    let path = committed_db(
        "update",
        &[
            (1, "alpha", 10, "a"),
            (2, "beta", 20, "b"),
            (3, "gamma", 30, "c"),
        ],
    );
    let before = read_rows(&path);
    let _ = crash(
        &path,
        "UPDATE t SET score = 999, name = 'clobbered' WHERE id = 2;",
    );

    let after = read_rows(&path);
    assert_eq!(
        after, before,
        "an interrupted UPDATE is undone: restoring the old page image has to \
         bring back the old value, not merely make the row parseable"
    );
    assert!(
        after.iter().any(|r| r[1] == "beta" && r[2] == "20"),
        "row 2 is back to its committed values, got {after:?}"
    );
}

#[test]
fn replay_undoes_an_interrupted_delete() {
    let path = committed_db(
        "delete",
        &[
            (1, "alpha", 10, "a"),
            (2, "beta", 20, "b"),
            (3, "gamma", 30, "c"),
        ],
    );
    let before = read_rows(&path);
    let _ = crash(&path, "DELETE FROM t WHERE id = 3;");

    let after = read_rows(&path);
    assert_eq!(
        after, before,
        "an interrupted DELETE has to be undone, not just an interrupted INSERT: \
         the row is absent from the file and must come back with its values"
    );
    assert!(
        after.iter().any(|r| r[1] == "gamma" && r[3] == "c"),
        "the deleted row is back, got {after:?}"
    );
}

#[test]
fn replay_undoes_a_mix_of_changes() {
    let path = committed_db(
        "mixed",
        &[
            (1, "alpha", 10, "a"),
            (2, "beta", 20, "b"),
            (3, "gamma", 30, "c"),
        ],
    );
    let before = read_rows(&path);
    let _ = crash(
        &path,
        "INSERT INTO t VALUES(4,'delta',40,'d');
         UPDATE t SET score = 555 WHERE id = 1;
         DELETE FROM t WHERE id = 2;",
    );

    let after = read_rows(&path);
    assert_eq!(after, before, "insert, update and delete are all undone");
    assert!(
        after.iter().any(|r| r[1] == "beta"),
        "the deleted row is back, got {after:?}"
    );
    assert!(
        after.iter().any(|r| r[2] == "10"),
        "the updated row is back to its old score, got {after:?}"
    );
}

// ---------------------------------------------------------------------------
// 2b. A transaction that freed a page.
//
// This is the case the row-level assertions above cannot see. A DROP TABLE puts
// the table's root on the freelist, which is *only* recorded in the first 100
// bytes of the file — page 1's header. Every test in section 1 and 2 inserts
// rows and leaves the freelist empty, so the recovered pages are right and the
// tests pass whether or not replay gets the header right.
//
// The failure it guards against was real. `replay_hot_journal` restored the
// journalled pages and then called `write_header()`, which writes the pager's
// in-memory header — parsed from the *crashed* file — back over the restored
// page 1. That re-applied the interrupted transaction's freelist_trunk and
// freelist_count, so the recovered file named a page that the journal had just
// put a b-tree page image back into. The rows all came back, and the file was
// corrupt.
//
// Real sqlite3 restores the file byte for byte from a journal of this shape, so
// that is the expectation asserted here: not "the rows are right" but "the file
// is the file that was there before the crash".
// ---------------------------------------------------------------------------

/// The two freelist fields, which are the ones a freed page shows up in.
fn freelist_fields(path: &Path) -> (u32, u32) {
    let h = file_header(path);
    (
        u32::from_be_bytes([h[32], h[33], h[34], h[35]]),
        u32::from_be_bytes([h[36], h[37], h[38], h[39]]),
    )
}

#[test]
fn replay_undoes_an_interrupted_drop_table_and_the_freelist_it_took() {
    let path = work_dir("drop").join("crash.db");
    let mut conn = Connection::open(&path).expect("creating the database");
    conn.execute_script(SCHEMA).expect("creating the table");
    conn.execute_script(
        "INSERT INTO t VALUES(1,'alpha',10,'a'); INSERT INTO t VALUES(2,'beta',20,'b');",
    )
    .expect("inserting");
    // A second table gives the DROP something to free, so the freelist moves
    // off zero. Without it the interrupted transaction changes no freelist
    // field and the header cannot be wrong.
    conn.execute_script("CREATE TABLE gone(x); INSERT INTO gone VALUES(1);")
        .expect("creating the table to drop");
    drop(conn);

    let committed = file_bytes(&path);
    assert_eq!(
        freelist_fields(&path),
        (0, 0),
        "the committed database has an empty freelist, which is what makes the \
         freelist fields after the crash attributable to the DROP"
    );
    let before = read_rows(&path);

    let _ = crash(&path, "DROP TABLE gone; DELETE FROM t WHERE id=1;");

    let crashed = file_bytes(&path);
    let (trunk, count) = freelist_fields(&path);
    assert_eq!(
        (trunk, count),
        (3, 1),
        "the interrupted transaction really did free a page and record it, so \
         this test can tell whether replay undoes it"
    );

    let after = read_rows(&path);
    assert_eq!(after, before, "the rows are all back");

    // The rows being right is not the point. This is.
    assert_eq!(
        file_header(&path),
        committed[..100],
        "the recovered file header is byte for byte the header the crash \
         interrupted, so the freed page is not still named by the freelist"
    );
    assert_eq!(
        file_bytes(&path),
        committed,
        "and the whole recovered file is byte for byte the file that was there \
         before the crash, which is what sqlite3 produces from the same journal"
    );
    assert_eq!(
        freelist_fields(&path),
        (0, 0),
        "the freelist is empty again"
    );
    assert_sqlite3_agrees(&path);
    assert_ne!(
        file_header(&path),
        crashed[..100],
        "the recovered header is not the crashed one: this is the check that \
         catches write_header() re-applying the interrupted transaction's state"
    );
}

#[test]
fn replay_undoes_an_interrupted_transaction_that_split_a_btree_page() {
    // The other structural change a transaction can make that no test above
    // makes: enough inserts to split the table's leaf page, which allocates a
    // new page and rewrites the root. The killed-process test inserts a few
    // small rows, which never splits anything, so without this the file-growth
    // path in `replay_hot_journal` is only exercised by rows that overflow.
    let mut rows: Vec<(i64, &str, i64, &str)> = (1..=60).map(|i| (i, "row", i, "t")).collect();
    rows[0] = (1, "alpha", 10, "a");
    let path = committed_db("split", &rows);
    let committed = file_bytes(&path);
    let before = read_rows(&path);
    assert_eq!(before.len(), 60);

    // Insert enough rows, in one transaction, to split the root and allocate
    // the next page.
    let mut script = String::new();
    for i in 61..160 {
        script.push_str(&format!(
            "INSERT INTO t VALUES({i},'split-{i}',{i},'{}');",
            "q".repeat(200)
        ));
    }
    let _ = crash(&path, &script);
    let crashed = file_bytes(&path);
    assert!(
        crashed.len() > committed.len(),
        "the interrupted transaction has to have allocated pages for this test \
         to mean anything: {} bytes -> {}",
        committed.len(),
        crashed.len()
    );

    let after = read_rows(&path);
    assert_eq!(
        after, before,
        "and the rows are the ones that were committed"
    );
    assert_eq!(
        file_bytes(&path),
        committed,
        "a b-tree page split is undone, not just the rows that went into the \
         new page: the root page and the page it pointed at are both restored"
    );
    assert_eq!(
        pages_on_disk(&path),
        committed.len() as u64 / PAGE_SIZE as u64
    );
    assert_sqlite3_agrees(&path);
}

// ---------------------------------------------------------------------------
// 3. A transaction that grew the file must shorten it again.
// ---------------------------------------------------------------------------

#[test]
fn replay_shortens_a_file_the_interrupted_transaction_grew() {
    let path = committed_db("grow", &[(1, "alpha", 10, "a"), (2, "beta", 20, "b")]);
    let pages_before = pages_on_disk(&path);
    let len_before = std::fs::metadata(&path).unwrap().len();
    let before = read_rows(&path);

    // Rows long enough to spill into overflow pages, so the file has to grow.
    // `value()` does not exist yet in this engine, so the text is built here.
    let mut script = String::new();
    for i in 3..40 {
        let filler = "x".repeat(3000);
        script.push_str(&format!(
            "INSERT INTO t VALUES({i},'row-{i}',{i},'{filler}');"
        ));
    }
    let pages_crashed = crash(&path, &script);

    assert!(
        pages_crashed > pages_before,
        "the interrupted transaction has to have grown the file for this test \
         to mean anything: {pages_before} -> {pages_crashed}"
    );
    assert_eq!(
        pages_on_disk(&path),
        pages_crashed,
        "before the reopen the file is still the grown one"
    );

    let after = read_rows(&path);
    assert_eq!(
        pages_on_disk(&path),
        pages_before,
        "recovery shortens the file back to the page count the header recorded \
         before the interrupted transaction"
    );
    assert_eq!(
        std::fs::metadata(&path).unwrap().len(),
        len_before,
        "the file is back to its original length in bytes"
    );
    assert_eq!(after, before, "and back to the rows it had");
    assert_eq!(
        pages_on_disk(&path) * PAGE_SIZE as u64,
        std::fs::metadata(&path).unwrap().len(),
        "the file is a whole number of pages, so nothing was left dangling"
    );
}

#[test]
fn a_committed_transaction_that_grew_the_file_is_not_shortened() {
    // The other side of the same code path: a journal that is *not* hot must
    // leave the file exactly as long as the commit made it. If recovery ran
    // anyway, a committed database would lose the pages it just added.
    let path = committed_db("nogrow", &[(1, "alpha", 10, "a")]);
    let pages_before = pages_on_disk(&path);

    let mut conn = Connection::open(&path).expect("reopening");
    let mut script = String::from("BEGIN;");
    for i in 2..40 {
        let filler = "x".repeat(3000);
        script.push_str(&format!(
            "INSERT INTO t VALUES({i},'row-{i}',{i},'{filler}');"
        ));
    }
    script.push_str("COMMIT;");
    conn.execute_script(&script).expect("committing");
    drop(conn);

    let pages_committed = pages_on_disk(&path);
    assert!(
        pages_committed > pages_before,
        "the committed transaction grew the file, which is what makes the \
         stale-journal case below meaningful"
    );

    let rows = read_rows(&path);
    assert_eq!(
        pages_on_disk(&path),
        pages_committed,
        "opening a database whose journal was deleted by its commit must not \
         shorten it"
    );
    assert_eq!(
        rows.len(),
        39,
        "every committed row is still there, got {} rows",
        rows.len()
    );
}

// ---------------------------------------------------------------------------
// 4. A journal torn mid-write.
// ---------------------------------------------------------------------------

/// Cuts a journal short, as a crash part way through writing a record does.
/// `keep` is how many bytes of the file survive.
fn tear_journal(path: &Path, keep: u64) {
    truncate_journal_to(
        path,
        std::fs::metadata(journal::journal_path(path))
            .unwrap()
            .len()
            - keep,
    );
}

/// Shortens a journal to `len` bytes.
fn truncate_journal_to(path: &Path, len: u64) {
    let jp = journal::journal_path(path);
    let f = OpenOptions::new()
        .write(true)
        .open(&jp)
        .expect("opening the journal to tear it");
    f.set_len(len).expect("truncating the journal");
    f.sync_all().expect("syncing the torn journal");
}

#[test]
fn a_torn_journal_recovers_the_records_before_the_tear() {
    let path = committed_db("torn", &[(1, "alpha", 10, "a"), (2, "beta", 20, "b")]);
    let before = read_rows(&path);

    // Page 1 carries the file header, which every write bumps, and the table's
    // root carries the new row, so the journal holds at least two records.
    let _ = crash(&path, "INSERT INTO t VALUES(3,'gamma',30,'c');");
    let recs = records(&path);
    assert!(
        recs.len() >= 2,
        "the crash has to have journalled at least two pages, got {}",
        recs.len()
    );
    // A page journalled twice keeps the copy that predates the change, so each
    // record's page is genuinely in a modified state on disk.
    let first_page = recs[0].0;
    let first_contents = recs[0].1.clone();
    let second_page = recs[1].0;

    // Tear the file in the middle of the second record, which is what a crash
    // part way through writing it leaves.
    let record_size = 4 + PAGE_SIZE as usize + 4;
    tear_journal(&path, (record_size / 2) as u64);

    // The torn record is not recovered, and the whole one before it is.
    let torn_recs = records(&path);
    assert_eq!(
        torn_recs.len(),
        recs.len() - 1,
        "recovery stops at the torn record and keeps the rest"
    );
    assert_eq!(torn_recs[0].0, first_page);
    assert_eq!(
        torn_recs[0].1, first_contents,
        "the intact record comes back byte for byte"
    );
    assert!(
        !torn_recs.iter().any(|&(p, _)| p == second_page),
        "the torn record is not treated as recovered"
    );

    let after = read_rows(&path);
    assert_eq!(
        after, before,
        "the page whose record was whole is restored, so the committed rows \
         are back even though the journal was torn"
    );
}

#[test]
fn a_journal_torn_inside_its_first_record_recovers_nothing() {
    // The degenerate case: the crash happened while the very first record was
    // being written, so the journal has a header and a fragment of a record.
    // Recovery must find nothing and report nothing, rather than failing — the
    // records before the tear are the ones that get restored, and here there are
    // none.
    let path = committed_db("torn0", &[(1, "alpha", 10, "a"), (2, "beta", 20, "b")]);
    let _ = crash(&path, "INSERT INTO t VALUES(3,'gamma',30,'c');");

    // Left with the 28-byte header and its sector padding, so not even the
    // first record's page number is on disk.
    truncate_journal_to(&path, record_offset(0, SECTOR));
    assert_eq!(
        std::fs::metadata(journal::journal_path(&path))
            .unwrap()
            .len(),
        record_offset(0, SECTOR),
        "only the header survives"
    );

    assert!(
        records(&path).is_empty(),
        "a journal with no complete record recovers nothing"
    );
    // Opening the database must still work, which is the whole point: a reader
    // cannot tell how much of a journal a crash destroyed, so a torn one has to
    // be handled as an ordinary hot journal with no usable records.
    let _ = read_rows(&path);
    assert!(
        !journal::journal_path(&path).exists(),
        "the journal is removed either way, so the next open does not try again"
    );
}

// ---------------------------------------------------------------------------
// 5. A damaged record.
// ---------------------------------------------------------------------------

#[test]
fn a_damaged_record_stops_recovery_and_keeps_what_came_before() {
    let path = committed_db("damaged", &[(1, "alpha", 10, "a"), (2, "beta", 20, "b")]);
    let before = read_rows(&path);
    let _ = crash(&path, "INSERT INTO t VALUES(3,'gamma',30,'c');");
    let recs = records(&path);
    assert!(
        recs.len() >= 2,
        "expected several records, got {}",
        recs.len()
    );
    let first_page = recs[0].0;

    // Flip a byte inside the *second* record's page image, at an offset the
    // checksum samples. Its checksum no longer matches, so recovery has to stop
    // there — the first record is still good and has to be replayed.
    let at = record_offset(1, SECTOR) + 4 + sampled_offset() as u64;
    let jp = journal::journal_path(&path);
    let mut data = std::fs::read(&jp).expect("reading the journal");
    assert!(
        at < data.len() as u64,
        "the offset has to land inside the second record, which ends at {}",
        record_offset(2, SECTOR)
    );
    data[at as usize] ^= 0xff;
    std::fs::write(&jp, &data).expect("writing the damaged journal");

    let damaged = records(&path);
    assert_eq!(
        damaged.len(),
        recs.len() - 1,
        "recovery stops at the damaged record"
    );
    assert_eq!(
        damaged[0].0, first_page,
        "and keeps the records that came before it"
    );

    let after = read_rows(&path);
    assert!(
        !after.iter().any(|r| r[1] == "gamma"),
        "the row the interrupted transaction added is not there, got {after:?}"
    );
    assert_eq!(
        after, before,
        "the first record was still replayed, so the committed rows are back"
    );
}

#[test]
fn a_damaged_first_record_recovers_nothing_and_still_cleans_up() {
    // The table root is the first page the transaction touches. Damaging its
    // record means nothing is replayed, so the file keeps the interrupted
    // transaction's page image. That is the one case a rollback cannot repair:
    // the pre-image is gone. What is well defined, and what is asserted here, is
    // that recovery finds nothing, opens the file without error, and clears the
    // journal so the next open does not try again.
    let path = committed_db("damaged0", &[(1, "alpha", 10, "a"), (2, "beta", 20, "b")]);
    let _ = crash(&path, "INSERT INTO t VALUES(3,'gamma',30,'c');");
    let jp = journal::journal_path(&path);
    let mut data = std::fs::read(&jp).expect("reading the journal");
    let at = record_offset(0, SECTOR) + 4 + sampled_offset() as u64;
    data[at as usize] ^= 0xff;
    std::fs::write(&jp, &data).expect("writing the damaged journal");

    assert!(
        records(&path).is_empty(),
        "the damaged first record stops recovery"
    );
    // The connection must still be able to open the file; whether the rows are
    // readable depends on what the damaged page now says, which is the crash's
    // own doing rather than something recovery can repair.
    let _ = Connection::open(&path).expect("the database still opens");
    assert!(
        !journal::journal_path(&path).exists(),
        "the journal is cleaned up even when nothing could be recovered from it"
    );
}

#[test]
fn damage_to_a_byte_the_checksum_does_not_sample_is_still_recovered() {
    // The other half of the checksum's design, and the reason a "corrupted
    // record" in practice means a torn or scrambled one rather than an arbitrary
    // single-byte edit. The sample is every 200th byte, so a byte outside it can
    // be wrong and the record still validates. Asserting this pins the format
    // down instead of leaving it as a surprise.
    let path = committed_db("unsampled", &[(1, "alpha", 10, "a"), (2, "beta", 20, "b")]);
    let before = read_rows(&path);
    let _ = crash(&path, "INSERT INTO t VALUES(3,'gamma',30,'c');");
    let recs = records(&path);
    assert!(
        recs.len() >= 2,
        "expected several records, got {}",
        recs.len()
    );
    // The damage has to land in a record for a b-tree page, and in a byte the
    // page does not otherwise mean something by. The journal's first record is
    // the table's root and its second is page 1, so "the second record" is the
    // file header, whose offset 0 is the magic string: flipping that makes the
    // restored file unreadable whatever the checksum says. Real sqlite3 was
    // asked about exactly that -- a journal of this shape with one flipped byte
    // at the second record's offset 0 -- and it prints
    // `Parse error ...: file is not a database (26)` and leaves no journal.
    let target = recs
        .iter()
        .position(|&(p, _)| p != 1)
        .expect("a record for a page other than the file header");
    let at = record_offset(target, SECTOR) + 4 + free_byte(&recs[target].1);
    let jp = journal::journal_path(&path);
    let mut data = std::fs::read(&jp).expect("reading the journal");
    let page = recs[target].0;
    data[at as usize] ^= 0xff;
    std::fs::write(&jp, &data).expect("writing the journal");

    assert_eq!(
        records(&path).len(),
        recs.len(),
        "a byte outside the checksum's sample does not invalidate the record"
    );
    assert_eq!(
        read_rows(&path),
        before,
        "and recovery still restores the committed rows"
    );
    // Nothing else depends on which page it was, but pinning the choice down
    // keeps the test from silently drifting back onto page 1.
    assert!(
        page != 1,
        "the damaged record is a b-tree page, not the header"
    );
}

/// The first byte of a leaf page's own unused gap, which no part of the page
/// reads.
///
/// The cell pointer array ends at `8 + 2*ncells` and the lowest cell body
/// starts at the smallest pointer, so everything between them is free space
/// this engine does not write and the b-tree does not read. That is the only
/// kind of byte an "arbitrary edit" can be harmless in: offset 0 is the page
/// type, the pointers are the pointers, and the tail is cell content -- a row
/// whose text runs to the end of the page will change value. sqlite3 was asked
/// about offset 12 of the table root and answers `ok` with both rows intact.
fn free_byte(page: &[u8]) -> u64 {
    let ncells = u16::from_be_bytes([page[3], page[4]]) as usize;
    let first_cell = ncells
        .min(256)
        .checked_sub(1)
        .map(|i| u16::from_be_bytes([page[8 + 2 * i], page[9 + 2 * i]]) as usize)
        .unwrap_or(page.len());
    let gap_start = 8 + 2 * ncells;
    assert!(
        gap_start < first_cell,
        "the leaf page has no unused gap to damage: {gap_start} >= {first_cell}"
    );
    assert!(
        !is_sampled(gap_start as u32),
        "the byte chosen for the damage is one the checksum samples, so the \
         test would pass for the wrong reason"
    );
    gap_start as u64
}

/// Whether a page offset is one the record checksum reads.
///
/// The sample starts at `PAGE_SIZE - CHECKSUM_STRIDE` and steps down by
/// `CHECKSUM_STRIDE`, so it covers the tail of the page and misses the head.
fn is_sampled(offset: u32) -> bool {
    let stride = journal::CHECKSUM_STRIDE as u32;
    offset >= PAGE_SIZE - stride && (PAGE_SIZE - offset) % stride == 0
}

// ---------------------------------------------------------------------------
// 6. A journal whose header was zeroed is not a journal.
// ---------------------------------------------------------------------------

#[test]
fn a_zeroed_journal_header_is_not_treated_as_a_journal() {
    let path = committed_db("zeroed", &[(1, "alpha", 10, "a"), (2, "beta", 20, "b")]);
    let _ = crash(&path, "INSERT INTO t VALUES(3,'gamma',30,'c');");
    let jp = journal::journal_path(&path);
    assert!(jp.exists(), "the crash left a journal");

    // Zeroing the header is what `PRAGMA journal_mode=PERSIST` does at commit:
    // the file stays on disk but the first byte is zero, which is how a reader
    // tells it apart from a journal a transaction is still using. Those records
    // describe a transaction that *committed*, so replaying them would undo
    // committed work.
    let mut data = std::fs::read(&jp).expect("reading the journal");
    for b in data.iter_mut().take(8) {
        *b = 0;
    }
    std::fs::write(&jp, &data).expect("zeroing the header");

    assert!(
        journal::find_hot(&path)
            .expect("probing for a hot journal")
            .is_none(),
        "a zeroed first byte is what marks a committed journal"
    );

    // The observable consequence: the row the transaction added is still part
    // of the database, because the journal was correctly ignored.
    let after = read_rows(&path);
    assert!(
        after.iter().any(|r| r[1] == "gamma"),
        "a committed journal's records must not be replayed: the row it added \
         is still in the database, got {after:?}"
    );
    assert_eq!(after.len(), 3, "and the committed rows are untouched too");
    assert!(
        jp.exists(),
        "a journal that is not hot is left alone, which is what PERSIST mode \
         relies on"
    );
}

#[test]
fn a_truncated_journal_is_not_treated_as_a_journal() {
    // The other documented way a commit retires a journal is to leave it zero
    // length. A file that cannot hold the magic is not a journal either.
    let path = committed_db("zerolen", &[(1, "alpha", 10, "a")]);
    let _ = crash(&path, "INSERT INTO t VALUES(2,'gamma',20,'g');");
    let jp = journal::journal_path(&path);
    let f = OpenOptions::new()
        .write(true)
        .open(&jp)
        .expect("opening the journal");
    f.set_len(0).expect("truncating the journal");
    drop(f);

    assert!(
        journal::find_hot(&path).expect("probing").is_none(),
        "a journal shorter than its magic cannot be one"
    );
    let after = read_rows(&path);
    assert!(
        after.iter().any(|r| r[1] == "gamma"),
        "the committed row is still there, got {after:?}"
    );
    assert!(
        journal::journal_path(&path).exists(),
        "and the retired journal is not deleted by a reader that ignored it"
    );
}

#[test]
fn a_database_with_no_journal_is_left_alone() {
    let path = committed_db("nojournal", &[(1, "alpha", 10, "a")]);
    let before = read_rows(&path);
    let pages = pages_on_disk(&path);
    let after = read_rows(&path);
    assert_eq!(after, before);
    assert_eq!(pages_on_disk(&path), pages);
    assert!(!journal::journal_path(&path).exists());
}

// ---------------------------------------------------------------------------
// 7. A real killed process.
// ---------------------------------------------------------------------------

/// The name of this test binary, re-invoked to run the child half.
fn current_exe() -> PathBuf {
    std::env::current_exe().expect("the test binary's own path")
}

#[test]
fn a_killed_process_leaves_a_journal_that_replays() {
    let path = work_dir("kill").join("crash.db");
    let mut conn = Connection::open(&path).expect("creating the database");
    conn.execute_script(SCHEMA).expect("creating the table");
    conn.execute_script(
        "INSERT INTO t VALUES(1,'alpha',10,'a'); INSERT INTO t VALUES(2,'beta',20,'b');",
    )
    .expect("inserting");
    // A second table, for the child to drop. Giving the killed process a DROP
    // is what makes this test cover the file header and not just the pages: a
    // freed page is recorded *only* in the first 100 bytes of page 1, so a
    // killed process that only inserts small rows never exercises the part of
    // recovery that gets the header right. (The child used to insert three
    // rows, none of which split a page or freed one.)
    conn.execute_script("CREATE TABLE gone(x); INSERT INTO gone VALUES(1);")
        .expect("creating the table the child drops");
    drop(conn);

    let committed = file_bytes(&path);
    let before = read_rows(&path);
    let pages_before = pages_on_disk(&path);
    assert_eq!(before.len(), 2);
    assert_eq!(
        freelist_fields(&path),
        (0, 0),
        "the committed database has an empty freelist"
    );

    // The child writes part of a transaction, flushes it, and then aborts. It
    // is a separate process, so the file handles are torn down by the operating
    // system rather than by Rust's unwinding, which is the part this test exists
    // for: every other test in this file drops a `Pager`, and a drop runs
    // destructors.
    let mut child = Command::new(current_exe())
        .arg("--exact")
        .arg("crash_child_writes_then_dies")
        .arg("--ignored")
        .arg("--nocapture")
        .env("NSQLITE_CRASH_CHILD", &path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawning the child test process");
    let status = child.wait().expect("waiting for the child");
    assert!(
        !status.success(),
        "the child is supposed to die without a clean exit, got {status:?}"
    );

    // Whatever the child left behind has to be a hot journal, or the test has
    // not simulated a crash at all.
    let jp = journal::journal_path(&path);
    assert!(
        jp.exists(),
        "a killed process leaves its journal on disk; the child got as far as \
         {}",
        std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0)
    );
    assert!(
        journal::find_hot(&path).expect("probing").is_some(),
        "and it is still recognisable as a journal"
    );
    // The child really did free a page, or the byte-for-byte check below would
    // pass without ever having covered the header.
    let (trunk, count) = freelist_fields(&path);
    assert!(
        count > 0,
        "the killed process freed a page, so the header is part of what \
         recovery has to get right: freelist_trunk={trunk} count={count}"
    );

    // Recovery happens by opening the database, exactly as it would after a real
    // power loss.
    let after = read_rows(&path);
    assert_eq!(
        after, before,
        "reopening after a killed process restores the committed rows and \
         removes the interrupted transaction's"
    );
    assert_eq!(
        pages_on_disk(&path),
        pages_before,
        "the file is back to its size"
    );
    assert_eq!(
        file_bytes(&path),
        committed,
        "and byte for byte the file that was there before the process died, \
         which is what sqlite3 produces from the same journal"
    );
    assert_sqlite3_agrees(&path);
    assert!(!jp.exists(), "the replayed journal is removed");
}

/// The child half of [`a_killed_process_leaves_a_journal_that_replays`].
///
/// Ignored by default and run only when the parent sets `NSQLITE_CRASH_CHILD`.
/// It begins a transaction, writes rows, frees a page, flushes them, and then
/// aborts the process, so no destructor runs on the way out and the journal is
/// left exactly as a crash leaves it.
#[test]
#[ignore]
fn crash_child_writes_then_dies() {
    let Some(db) = std::env::var_os("NSQLITE_CRASH_CHILD") else {
        return;
    };
    let path = PathBuf::from(db);
    let mut conn = Connection::open(&path).expect("the child opening the database");
    conn.execute_script("BEGIN;").expect("BEGIN in the child");
    // A row long enough to spill, so the file grows and the recovery has to
    // shorten it as well as restore the page images.
    let filler = "y".repeat(4000);
    conn.execute_script("INSERT INTO t VALUES(3,'ghost-a',30,'g');")
        .expect("the child inserting");
    conn.execute_script(&format!("INSERT INTO t VALUES(4,'ghost-b',40,'{filler}');"))
        .expect("the child inserting a spilling row");
    conn.execute_script("INSERT INTO t VALUES(5,'ghost-c',50,'g');")
        .expect("the child inserting");
    // A DROP TABLE, which is the change the pre-images have to undo in the
    // file header as well as in the b-tree pages. The child used to only
    // insert, which leaves the freelist untouched.
    conn.execute_script("DROP TABLE gone;")
        .expect("the child dropping a table");
    // No COMMIT, no ROLLBACK, and no drop: the process just stops.
    std::process::abort();
}

// ---------------------------------------------------------------------------
// 8. The real sqlite3.
// ---------------------------------------------------------------------------

fn sqlite3() -> String {
    std::env::var("NSQLITE_SQLITE3").unwrap_or_else(|_| "sqlite3".to_string())
}

fn sqlite3_available() -> bool {
    Command::new(sqlite3())
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn sqlite3_query(path: &Path, sql: &str) -> String {
    let out = Command::new(sqlite3())
        .arg(path)
        .arg(sql)
        .output()
        .expect("running sqlite3");
    String::from_utf8_lossy(&out.stdout).to_string()
}

#[test]
fn the_real_sqlite3_recovers_this_engines_journal() {
    if !sqlite3_available() {
        eprintln!("skipping: sqlite3 is not on PATH");
        return;
    }
    let path = committed_db("sqlite3", &[(1, "alpha", 10, "a"), (2, "beta", 20, "b")]);
    let _ = crash(&path, "INSERT INTO t VALUES(3,'gamma',30,'c');");
    assert!(journal::journal_path(&path).exists());

    // sqlite3 replays the journal on open. This is the only proof that the
    // format is compatible in the direction that matters: a crash this engine
    // suffers has to be recoverable by every other SQLite too.
    let out = sqlite3_query(&path, SELECT_ALL);
    assert_eq!(
        out.lines().filter(|l| !l.trim().is_empty()).count(),
        2,
        "sqlite3 recovered something other than the two committed rows: {out:?}"
    );
    assert!(out.contains("alpha"), "sqlite3 said: {out:?}");
    assert!(out.contains("beta"), "sqlite3 said: {out:?}");
    assert!(
        !out.contains("gamma"),
        "sqlite3 replayed a committed row away: {out:?}"
    );
    assert!(
        !journal::journal_path(&path).exists(),
        "sqlite3 deletes the journal it replayed"
    );
}

#[test]
fn sqlite3_leaves_this_engines_committed_database_alone() {
    if !sqlite3_available() {
        eprintln!("skipping: sqlite3 is not on PATH");
        return;
    }
    // The other direction: what this engine commits, the real one reads. A
    // recovery test that only ever looked at recovery could pass on a file no
    // other implementation can open.
    let path = committed_db(
        "sqlite3-committed",
        &[
            (1, "alpha", 10, "a"),
            (2, "beta", 20, "b"),
            (3, "gamma", 30, "c"),
        ],
    );
    let out = sqlite3_query(&path, "PRAGMA integrity_check;");
    assert_eq!(out.trim(), "ok", "sqlite3 said: {out:?}");
    let out = sqlite3_query(&path, SELECT_ALL);
    assert_eq!(
        out.lines().filter(|l| !l.trim().is_empty()).count(),
        3,
        "sqlite3 read: {out:?}"
    );
}
