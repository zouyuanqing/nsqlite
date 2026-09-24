//! The rollback journal, in SQLite's `DELETE` mode.
//!
//! A transaction writes the pages it is about to change into a journal before
//! changing them, and copies them back on a rollback. The journal's existence
//! is what makes an interrupted write recoverable: a journal left behind by a
//! crash is a hot journal, and the next connection to open the database plays
//! it back before doing anything else.
//!
//! # Format
//!
//! The file opens with a 28-byte header, padded with zeroes to a whole sector so
//! that a write torn by a power loss cannot corrupt both the header and the
//! records that follow it:
//!
//! | Offset | Size | Field |
//! |---|---|---|
//! | 0  | 8 | the magic `d9 d5 05 f9 20 a1 63 d7` |
//! | 8  | 4 | pages in this segment, or -1 for "to the end of the file" |
//! | 12 | 4 | a random nonce for the checksum |
//! | 16 | 4 | the database's size in pages before the transaction |
//! | 20 | 4 | the sector size the writer assumed |
//! | 24 | 4 | the page size |
//!
//! Each record is a four-byte page number, the page's original contents, and a
//! four-byte checksum. The checksum is a sparse sample: starting from the
//! nonce, add every 200th byte of the page, counting down. It exists to catch a
//! record that was not written whole, not to be a strong digest, and the nonce
//! exists so that a record left over from a previous transaction does not
//! validate by accident.
//!
//! # Commit
//!
//! A commit is the deletion of the journal, which is the default mode. The
//! alternatives truncate the file or zero its header; all three are ways of
//! saying "what is here is not a journal any more", and a reader only has to
//! check the first byte is zero.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use super::error::{Error, Result, ResultCode};

/// The eight bytes every journal header starts with.
pub const MAGIC: [u8; 8] = [0xd9, 0xd5, 0x05, 0xf9, 0x20, 0xa1, 0x63, 0xd7];

/// The size of the header before it is padded out to a sector.
pub const HEADER_SIZE: u32 = 28;

/// The distance between the bytes the checksum samples.
pub const CHECKSUM_STRIDE: usize = 200;

/// A journal header, decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    /// Pages in this segment, or `None` for "to the end of the file".
    pub nrec: Option<u32>,
    /// The checksum nonce.
    pub nonce: u32,
    /// The database's size in pages before the transaction began.
    pub db_size: u32,
    pub sector_size: u32,
    pub page_size: u32,
}

impl Header {
    /// The number of bytes the header occupies, which is the 28 bytes padded
    /// out to a sector.
    pub fn padded_size(&self) -> u32 {
        let sector = self.sector_size.max(HEADER_SIZE) as usize;
        ((HEADER_SIZE as usize).div_ceil(sector) * sector) as u32
    }
}

/// The checksum SQLite uses for a page record.
///
/// It adds every `CHECKSUM_STRIDE`-th byte, counting down from the end of the
/// page. The sign is worth stating because it is easy to get backwards: the
/// nonce is added and the bytes are added, never subtracted.
pub fn checksum(page: &[u8], nonce: u32) -> u32 {
    let mut sum = nonce;
    let mut i = page.len() as i64 - CHECKSUM_STRIDE as i64;
    while i >= 0 {
        sum = sum.wrapping_add(page[i as usize] as u32);
        i -= CHECKSUM_STRIDE as i64;
    }
    sum
}

/// A journal being written for a transaction.
pub struct Journal {
    path: PathBuf,
    file: Option<File>,
    /// The header the journal was opened with, kept so a record's size is
    /// known without re-reading.
    header: Header,
    /// Pages already recorded, so a page changed twice is stored once. SQLite
    /// does the same, because the first copy is the one that has to come back.
    recorded: Vec<u32>,
    /// Where the next record goes.
    next_offset: u64,
    /// Whether a page has been recorded and the journal has since been closed.
    open: bool,
}

/// A journal that is present on disk and may or may not need replaying.
pub struct HotJournal {
    path: PathBuf,
    header: Header,
    /// The database the journal belongs to, which is its own name with the
    /// journal suffix removed.
    pub database: PathBuf,
}

/// The suffix SQLite appends for a rollback journal.
pub const SUFFIX: &str = "-journal";

/// The path a database's journal lives at.
pub fn journal_path(database: &Path) -> PathBuf {
    let mut s = database.as_os_str().to_os_string();
    s.push(SUFFIX);
    PathBuf::from(s)
}

impl Journal {
    /// Opens a journal for a transaction, truncating any that was there.
    ///
    /// The nonce is drawn from the caller rather than from a random source, so
    /// a test can predict it; production code passes a value that changes.
    pub fn create(
        database: &Path,
        page_size: u32,
        sector_size: u32,
        nonce: u32,
    ) -> Result<Journal> {
        let path = journal_path(database);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .map_err(|e| {
                Error::new(
                    ResultCode::CantOpen,
                    format!("unable to open journal {}: {e}", path.display()),
                )
            })?;
        let header = Header {
            nrec: None,
            nonce,
            db_size: 0,
            sector_size,
            page_size,
        };
        let mut j = Journal {
            path,
            file: Some(file),
            header,
            recorded: Vec::new(),
            next_offset: header.padded_size() as u64,
            open: true,
        };
        j.write_header()?;
        Ok(j)
    }

    /// Records that the database had `pages` pages before the transaction.
    pub fn set_original_size(&mut self, pages: u32) -> Result<()> {
        self.header.db_size = pages;
        self.write_header()
    }

    fn write_header(&mut self) -> Result<()> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&MAGIC);
        // -1 means "records run to the end of the file", which is what a
        // journal of unbounded length says.
        buf.extend_from_slice(&(-1i32).to_be_bytes());
        buf.extend_from_slice(&self.header.nonce.to_be_bytes());
        buf.extend_from_slice(&self.header.db_size.to_be_bytes());
        buf.extend_from_slice(&self.header.sector_size.to_be_bytes());
        buf.extend_from_slice(&self.header.page_size.to_be_bytes());
        // The rest of the first sector is zero, so a torn write cannot leave a
        // valid header next to a damaged record.
        buf.resize(self.header.padded_size() as usize, 0);
        let f = self.file.as_mut().expect("an open journal has a file");
        f.seek(SeekFrom::Start(0))?;
        f.write_all(&buf)?;
        Ok(())
    }

    /// Saves a page's current contents, so a rollback can put it back.
    ///
    /// A page already recorded is not recorded again: the first copy is the
    /// one that predates every change in the transaction, and recording the
    /// later one would restore the wrong contents.
    pub fn record(&mut self, page_no: u32, contents: &[u8]) -> Result<()> {
        if self.recorded.contains(&page_no) {
            return Ok(());
        }
        let size = self.header.page_size as usize;
        if contents.len() != size {
            return Err(Error::corrupt(format!(
                "a journal record must be a whole page: got {} bytes for page {page_no}",
                contents.len()
            )));
        }
        let f = self.file.as_mut().expect("an open journal has a file");
        f.seek(SeekFrom::Start(self.next_offset))?;
        f.write_all(&page_no.to_be_bytes())?;
        f.write_all(contents)?;
        f.write_all(&checksum(contents, self.header.nonce).to_be_bytes())?;
        self.next_offset += (4 + size + 4) as u64;
        self.recorded.push(page_no);
        Ok(())
    }

    /// Reads back a page this journal recorded, for a rollback.
    pub fn page_contents(&mut self, page_no: u32) -> Result<Vec<u8>> {
        let size = self.header.page_size as usize;
        let f = self
            .file
            .as_mut()
            .ok_or_else(|| Error::new(ResultCode::Error, "the journal is closed"))?;
        f.seek(SeekFrom::Start(self.header.padded_size() as u64))?;
        for _ in 0..=self.recorded.len() {
            let mut at = [0u8; 4];
            f.read_exact(&mut at)?;
            let mut contents = vec![0u8; size];
            f.read_exact(&mut contents)?;
            let mut stored = [0u8; 4];
            f.read_exact(&mut stored)?;
            if u32::from_be_bytes(at) == page_no {
                if checksum(&contents, self.header.nonce) != u32::from_be_bytes(stored) {
                    return Err(Error::corrupt(format!(
                        "the journal record for page {page_no} is damaged"
                    )));
                }
                return Ok(contents);
            }
            // Reading the record moved the position on to the next one.
        }
        Err(Error::corrupt(format!(
            "the journal has no record of page {page_no}"
        )))
    }

    /// The pages this journal holds a copy of.
    pub fn recorded_pages(&self) -> &[u32] {
        &self.recorded
    }

    /// Writes the journal's contents to disk.
    pub fn sync(&mut self) -> Result<()> {
        if let Some(f) = self.file.as_mut() {
            f.flush()?;
            f.sync_data()?;
        }
        Ok(())
    }

    /// Throws the journal away without a rollback, which is what a caller
    /// wanting the unlink-on-drop behaviour asks for.
    pub fn discard(&mut self) -> Result<()> {
        self.file = None;
        let _ = std::fs::remove_file(&self.path);
        self.open = false;
        Ok(())
    }

    /// Commits by making the journal unrecognisable.
    ///
    /// Deleting the file is what the default mode does. The truncate and
    /// zero-the-header forms are the other two documented ways to commit, and
    /// a reader only checks that the first byte is zero, so all three are
    /// equivalent as far as recovery is concerned.
    pub fn commit(&mut self) -> Result<()> {
        self.file = None;
        let _ = std::fs::remove_file(&self.path);
        self.open = false;
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn header(&self) -> &Header {
        &self.header
    }

    /// Whether the journal is still able to take records.
    pub fn is_open(&self) -> bool {
        self.open
    }
}

impl Drop for Journal {
    fn drop(&mut self) {
        // Dropping the handle does not remove the journal. A journal left on
        // disk is what makes an interrupted transaction recoverable, and the
        // caller decides its fate: commit removes it, and a caller that wants
        // the unlink-on-drop behaviour can call discard.
        self.file = None;
    }
}

/// Looks for a journal left behind by an interrupted transaction.
///
/// A journal is hot when it exists, begins with the magic, and belongs to a
/// database that still exists. The first byte alone decides the common case,
/// which is a journal that was zeroed or truncated by a commit, and the rest of
/// the magic is checked here so a caller does not have to.
pub fn find_hot(database: &Path) -> Result<Option<HotJournal>> {
    let path = journal_path(database);
    let mut f = match File::open(&path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(Error::new(
                ResultCode::CantOpen,
                format!("unable to read journal {}: {e}", path.display()),
            ))
        }
    };
    let mut magic = [0u8; 8];
    match f.read_exact(&mut magic) {
        Ok(()) => {}
        // A journal shorter than its magic cannot be one; the commit that
        // truncated it leaves exactly this.
        Err(_) => return Ok(None),
    }
    if magic != MAGIC {
        // Zeroed by a commit, or never a journal.
        return Ok(None);
    }
    let mut rest = [0u8; 20];
    f.read_exact(&mut rest)?;
    let u32_at = |b: &[u8], at: usize| u32::from_be_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]]);
    let raw_nrec = u32_at(&rest, 0);
    let header = Header {
        nrec: if raw_nrec == u32::MAX {
            None
        } else {
            Some(raw_nrec)
        },
        nonce: u32_at(&rest, 4),
        db_size: u32_at(&rest, 8),
        sector_size: u32_at(&rest, 12),
        page_size: u32_at(&rest, 16),
    };
    Ok(Some(HotJournal {
        path,
        header,
        database: database.to_owned(),
    }))
}

/// One page recovered from a journal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveredPage {
    pub page_no: u32,
    pub contents: Vec<u8>,
}

impl HotJournal {
    /// Reads every valid record, stopping at the first that fails its checksum.
    ///
    /// A torn tail is expected rather than exceptional: a crash during a write
    /// leaves a journal whose last record is incomplete, and the records before
    /// it are still the ones that have to be restored.
    pub fn recover(&self) -> Result<Vec<RecoveredPage>> {
        let mut f = File::open(&self.path)?;
        let page_size = self.header.page_size as usize;
        let record_size = 4 + page_size + 4;
        let mut out = Vec::new();
        let mut offset = self.header.padded_size() as u64;
        loop {
            f.seek(SeekFrom::Start(offset))?;
            let mut page_no = [0u8; 4];
            if f.read_exact(&mut page_no).is_err() {
                break;
            }
            let mut contents = vec![0u8; page_size];
            if f.read_exact(&mut contents).is_err() {
                break;
            }
            let mut stored = [0u8; 4];
            if f.read_exact(&mut stored).is_err() {
                break;
            }
            let want = u32::from_be_bytes(stored);
            if checksum(&contents, self.header.nonce) != want {
                // The record is damaged, so this and everything after it is not
                // trustworthy.
                break;
            }
            out.push(RecoveredPage {
                page_no: u32::from_be_bytes(page_no),
                contents,
            });
            offset += record_size as u64;
            if let Some(n) = self.header.nrec {
                if out.len() as u32 >= n {
                    break;
                }
            }
        }
        Ok(out)
    }

    /// The database's size in pages before the interrupted transaction, which
    /// is how a rollback knows whether the file grew.
    pub fn original_size(&self) -> u32 {
        self.header.db_size
    }

    /// Removes the journal, which is the last step of a rollback.
    pub fn finish(&self) -> Result<()> {
        let _ = std::fs::remove_file(&self.path);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("nsqlite-journal-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d.join("test.db")
    }

    /// A page of recognisable contents, so a mix-up is visible.
    fn page(fill: u8, size: usize) -> Vec<u8> {
        let mut v = vec![fill; size];
        for (i, b) in v.iter_mut().enumerate().take(16) {
            *b = (i as u8).wrapping_add(fill);
        }
        v
    }

    #[test]
    fn the_header_round_trips_through_a_file() {
        let db = temp("hdr");
        {
            let mut j = Journal::create(&db, 4096, 512, 0xdead_beef).unwrap();
            j.set_original_size(7).unwrap();
            j.sync().unwrap();
        }
        let hot = find_hot(&db).unwrap().expect("the journal is hot");
        assert_eq!(hot.header.page_size, 4096);
        assert_eq!(hot.header.sector_size, 512);
        assert_eq!(hot.header.nonce, 0xdead_beef);
        assert_eq!(hot.original_size(), 7);
        assert_eq!(hot.header.nrec, None, "-1 means to the end of the file");
        let _ = std::fs::remove_dir_all(db.parent().unwrap());
    }

    #[test]
    fn the_header_is_padded_out_to_a_sector() {
        let db = temp("pad");
        {
            let mut j = Journal::create(&db, 4096, 512, 1).unwrap();
            j.record(1, &page(0, 4096)).unwrap();
            j.sync().unwrap();
        }
        // 28 bytes of header padded to 512, so the first record starts there.
        assert_eq!(
            std::fs::metadata(journal_path(&db)).unwrap().len(),
            512 + 4 + 4096 + 4
        );
        let _ = std::fs::remove_dir_all(db.parent().unwrap());
    }

    #[test]
    fn a_recorded_page_comes_back_byte_for_byte() {
        let db = temp("rec");
        let original = page(0x11, 4096);
        {
            let mut j = Journal::create(&db, 4096, 512, 0x1234).unwrap();
            j.record(3, &original).unwrap();
            j.sync().unwrap();
        }
        let hot = find_hot(&db).unwrap().unwrap();
        let pages = hot.recover().unwrap();
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].page_no, 3);
        assert_eq!(pages[0].contents, original);
        let _ = std::fs::remove_dir_all(db.parent().unwrap());
    }

    #[test]
    fn a_page_recorded_twice_keeps_its_first_copy() {
        let db = temp("twice");
        let first = page(0x11, 4096);
        let second = page(0x22, 4096);
        {
            let mut j = Journal::create(&db, 4096, 512, 7).unwrap();
            j.record(5, &first).unwrap();
            j.record(5, &second).unwrap();
            assert_eq!(j.recorded_pages(), &[5], "the second copy is not recorded");
            j.sync().unwrap();
        }
        let hot = find_hot(&db).unwrap().unwrap();
        let pages = hot.recover().unwrap();
        assert_eq!(pages.len(), 1);
        assert_eq!(
            pages[0].contents, first,
            "a rollback must restore what the page held before the transaction, \
             not its later contents"
        );
        let _ = std::fs::remove_dir_all(db.parent().unwrap());
    }

    #[test]
    fn a_record_that_is_not_a_whole_page_is_refused() {
        let db = temp("short");
        let mut j = Journal::create(&db, 4096, 512, 1).unwrap();
        let e = j.record(1, &page(0, 100)).unwrap_err();
        assert!(e.message.contains("whole page"), "got: {}", e.message);
        drop(j);
        let _ = std::fs::remove_dir_all(db.parent().unwrap());
    }

    #[test]
    fn a_committed_journal_is_not_hot() {
        let db = temp("commit");
        let mut j = Journal::create(&db, 4096, 512, 1).unwrap();
        j.record(1, &page(0, 4096)).unwrap();
        j.sync().unwrap();
        j.commit().unwrap();
        // A commit is the deletion, so nothing is left to replay.
        assert!(find_hot(&db).unwrap().is_none());
        assert!(!journal_path(&db).exists());
        let _ = std::fs::remove_dir_all(db.parent().unwrap());
    }

    #[test]
    fn a_dropped_journal_stays_for_recovery() {
        let db = temp("drop");
        {
            let mut j = Journal::create(&db, 4096, 512, 1).unwrap();
            j.record(1, &page(0, 4096)).unwrap();
            j.sync().unwrap();
            // Dropped without a commit, which is what a crash looks like. The
            // journal has to survive: this is the state the next connection
            // finds, and rolling it back is the only way to reach a consistent
            // file.
        }
        let hot = find_hot(&db)
            .unwrap()
            .expect("an uncommitted journal is hot");
        assert_eq!(hot.recover().unwrap().len(), 1);
        hot.finish().unwrap();
        let _ = std::fs::remove_dir_all(db.parent().unwrap());
    }

    #[test]
    fn discarding_a_journal_removes_it() {
        let db = temp("discard");
        let mut j = Journal::create(&db, 4096, 512, 1).unwrap();
        j.record(1, &page(0, 4096)).unwrap();
        j.sync().unwrap();
        j.discard().unwrap();
        assert!(find_hot(&db).unwrap().is_none());
        let _ = std::fs::remove_dir_all(db.parent().unwrap());
    }

    #[test]
    fn a_torn_tail_stops_the_recovery_without_losing_earlier_records() {
        let db = temp("torn");
        let p1 = page(0x11, 4096);
        let p2 = page(0x22, 4096);
        {
            let mut j = Journal::create(&db, 4096, 512, 99).unwrap();
            j.record(1, &p1).unwrap();
            j.record(2, &p2).unwrap();
            j.sync().unwrap();
        }
        // Truncate inside the second record, which is what a crash mid-write
        // leaves.
        let path = journal_path(&db);
        let len = std::fs::metadata(&path).unwrap().len();
        let f = OpenOptions::new().write(true).open(&path).unwrap();
        f.set_len(len - 100).unwrap();
        drop(f);

        let hot = find_hot(&db).unwrap().unwrap();
        let pages = hot.recover().unwrap();
        assert_eq!(pages.len(), 1, "the complete record survives");
        assert_eq!(pages[0].page_no, 1);
        assert_eq!(pages[0].contents, p1);
        let _ = std::fs::remove_dir_all(db.parent().unwrap());
    }

    #[test]
    fn a_damaged_record_stops_the_recovery() {
        let db = temp("dmg");
        {
            let mut j = Journal::create(&db, 4096, 512, 99).unwrap();
            j.record(1, &page(0x11, 4096)).unwrap();
            j.record(2, &page(0x22, 4096)).unwrap();
            j.sync().unwrap();
        }
        // Flip a byte inside the second record's contents, so its checksum no
        // longer matches and recovery stops there.
        let path = journal_path(&db);
        let mut data = std::fs::read(&path).unwrap();
        let record = 4 + 4096 + 4;
        let at = 512 + record + 100;
        assert!(
            at < data.len(),
            "the offset has to land inside the second record"
        );
        data[at] ^= 0xff;
        std::fs::write(&path, &data).unwrap();

        let hot = find_hot(&db).unwrap().unwrap();
        let pages = hot.recover().unwrap();
        assert_eq!(pages.len(), 1, "recovery stops at the damaged record");
        assert_eq!(pages[0].page_no, 1);
        let _ = std::fs::remove_dir_all(db.parent().unwrap());
    }

    #[test]
    fn the_checksum_is_a_sparse_sample_from_the_end() {
        // A page of zeroes with one byte set, so the sample either includes it
        // or does not, and the stride says which.
        let mut p = vec![0u8; 4096];
        let nonce = 5;
        // The sampled offsets are len-200, len-400, and so on, so byte 3896 of a
        // 4096-byte page is the first one sampled.
        p[3896] = 10;
        assert_eq!(checksum(&p, nonce), nonce + 10);
        p[3896] = 0;
        assert_eq!(checksum(&p, nonce), nonce);
    }

    #[test]
    fn the_nonce_keeps_a_stale_record_from_validating() {
        let p = page(0x33, 4096);
        let with = checksum(&p, 1);
        assert_ne!(
            with,
            checksum(&p, 2),
            "a different nonce gives a different sum"
        );
    }

    #[test]
    fn a_zeroed_or_absent_journal_is_not_hot() {
        let db = temp("zero");
        std::fs::write(journal_path(&db), b"").unwrap();
        assert!(find_hot(&db).unwrap().is_none());
        // Zeroing the header is one of the three documented ways to commit.
        std::fs::write(journal_path(&db), vec![0u8; 600]).unwrap();
        assert!(find_hot(&db).unwrap().is_none());
        // And no journal at all is not hot either.
        let _ = std::fs::remove_file(journal_path(&db));
        assert!(find_hot(&db).unwrap().is_none());
        let _ = std::fs::remove_dir_all(db.parent().unwrap());
    }

    #[test]
    fn a_short_file_is_not_mistaken_for_a_journal() {
        let db = temp("tiny");
        std::fs::write(journal_path(&db), b"abc").unwrap();
        assert!(find_hot(&db).unwrap().is_none());
        let _ = std::fs::remove_dir_all(db.parent().unwrap());
    }
}
