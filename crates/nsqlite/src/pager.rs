//! Page-level access to a database file: a bounded page cache over a
//! `Read + Write` pair, with the size-change callback a b-tree needs to grow
//! the file and update the header's page count.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use super::error::{Error, Result, ResultCode};
use super::journal::{self, Journal, RecoveredPage};
use super::page::{DbHeader, HEADER_SIZE};

/// The number of cached pages held before the least recently used one is
/// written back and evicted.
const DEFAULT_CACHE_PAGES: usize = 512;

/// A page buffer owned by the cache.
type Buffer = Vec<u8>;

/// Where a page came from, so a clean read can be discarded without a write.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Origin {
    File,
    Dirty,
}

/// Reads and writes pages of one database file, caching them in memory.
///
/// The pager owns the file header: page 1 is a page like any other, except that
/// its first 100 bytes are the header, so callers get [`DbHeader`] from
/// [`Pager::header`] rather than parsing it out of the buffer.
pub struct Pager {
    file: File,
    /// Bytes per page, including the reserved region.
    page_size: u32,
    header: DbHeader,
    /// Pages currently resident, keyed by 1-based page number.
    cache: HashMap<u32, (Buffer, Origin, u64)>,
    /// Page numbers in eviction order, oldest first.
    lru: Vec<u32>,
    capacity: usize,
    /// Incremented on every cache hit, giving each page its recency stamp.
    clock: u64,
    /// Pages written since the file was opened, and their old lengths, so a
    /// rollback journal can restore them.
    dirty: Vec<u32>,
    /// Path of the temporary file backing an in-memory database, removed on drop.
    memory_path: Option<std::path::PathBuf>,
    /// The database's own path, which a journal is named after. A memory
    /// database has one too, since its backing file is a real path.
    path: Option<std::path::PathBuf>,
    /// The journal of the transaction in progress, if there is one.
    journal: Option<Journal>,
    /// The nonce the next journal uses, advanced per transaction so a record
    /// left by an earlier one cannot validate by accident.
    nonce: u32,
}

impl Pager {
    /// Opens `path`, reading and validating its header.
    ///
    /// A zero-length file is initialised in memory with a fresh header; the
    /// first page is written out on the first flush, which is what lets
    /// `CREATE TABLE` work on a path that did not exist yet.
    pub fn open(path: &Path) -> Result<Pager> {
        let mut file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(|e| {
                Error::new(
                    super::error::ResultCode::CantOpen,
                    format!("unable to open database file: {e}"),
                )
            })?;
        let len = file.metadata()?.len();
        let fresh = len == 0;
        let mut pager = if fresh {
            Pager {
                file,
                page_size: 4096,
                header: DbHeader::default(),
                cache: HashMap::new(),
                lru: Vec::new(),
                capacity: DEFAULT_CACHE_PAGES,
                clock: 0,
                dirty: Vec::new(),
                memory_path: None,
                path: None,
                journal: None,
                nonce: 1,
            }
        } else {
            if len < HEADER_SIZE as u64 {
                return Err(Error::not_a_db());
            }
            let mut first = vec![0u8; HEADER_SIZE];
            file.seek(SeekFrom::Start(0))?;
            file.read_exact(&mut first)?;
            let header = DbHeader::parse(&first)?;
            let page_size = header.page_size;
            let mut pager = Pager {
                file,
                page_size,
                header,
                cache: HashMap::new(),
                lru: Vec::new(),
                capacity: DEFAULT_CACHE_PAGES,
                clock: 0,
                dirty: Vec::new(),
                memory_path: None,
                path: None,
                journal: None,
                nonce: 1,
            };
            pager.path = Some(path.to_owned());
            // The header's page count is authoritative only when the change
            // counter matches version-valid-for; otherwise the file length is.
            let file_pages = (len / page_size as u64) as u32;
            if pager.header.db_size_pages == 0
                || pager.header.version_valid_for != pager.header.change_counter
            {
                pager.header.db_size_pages = file_pages;
            }
            pager
        };
        if !fresh {
            pager.replay_hot_journal()?;
        }
        if fresh {
            // A zero-length file has no page 1 yet, so there is nothing to
            // re-read: the in-memory header is already the authoritative one.
            // Writing it out now means every later open sees a valid database,
            // which is what makes CREATE TABLE work on a new path.
            pager.write_header()?;
            pager.flush()?;
        } else {
            // Prime the header from page 1 so callers always see current values.
            pager.load_header()?;
        }
        Ok(pager)
    }

    /// Opens an in-memory database with the given page size.
    ///
    /// The backing store is a temporary file that is removed when the pager is
    /// dropped, which keeps one code path for on-disk and memory databases.
    pub fn open_memory(page_size: u32) -> Result<Pager> {
        let path = std::env::temp_dir().join(format!(
            "nsqlite-mem-{}-{:p}.db",
            std::process::id(),
            &page_size as *const u32
        ));
        let _ = std::fs::remove_file(&path);
        let mut pager = Pager::open(&path)?;
        pager.header.page_size = page_size;
        pager.memory_path = Some(path.clone());
        pager.path = Some(path);
        Ok(pager)
    }

    pub fn page_size(&self) -> u32 {
        self.page_size
    }

    pub fn header(&self) -> &DbHeader {
        &self.header
    }

    pub fn header_mut(&mut self) -> &mut DbHeader {
        &mut self.header
    }

    /// The number of pages the file currently holds.
    /// Records that page `n` is part of the file.
    ///
    /// Page 1 holds the file header and is never allocated, so a caller that
    /// puts a b-tree there has to say the file is at least that long or the
    /// page is never flushed.
    pub fn claim_page(&mut self, n: u32) -> Result<()> {
        if n > self.header.db_size_pages {
            self.header.db_size_pages = n;
        }
        Ok(())
    }

    pub fn page_count(&self) -> u32 {
        self.header.db_size_pages
    }

    /// Bytes available for b-tree content, excluding the reserved region.
    pub fn usable_size(&self) -> u32 {
        self.header.usable_size()
    }

    /// Re-reads the file header from the cached page 1.
    fn load_header(&mut self) -> Result<()> {
        let page = self.read_page(1)?;
        self.header = DbHeader::parse(&page[..HEADER_SIZE])?;
        Ok(())
    }

    /// Returns a mutable reference to page `n`, reading it in if needed.
    ///
    /// The returned borrow points into the cache, so it is invalidated by the
    /// next call to this method; callers must not hold it across a write.
    pub fn page(&mut self, n: u32) -> Result<&mut [u8]> {
        debug_assert!(n > 0, "page numbers are 1-based");
        self.clock += 1;
        if !self.cache.contains_key(&n) {
            self.fetch(n)?;
        }
        self.touch(n);
        Ok(&mut self.cache.get_mut(&n).expect("just ensured").0)
    }

    /// Returns the contents of page `n` without marking it dirty.
    pub fn read_page(&mut self, n: u32) -> Result<Vec<u8>> {
        if let Some((buf, _, _)) = self.cache.get(&n) {
            return Ok(buf.clone());
        }
        self.clock += 1;
        self.fetch(n)?;
        self.touch(n);
        Ok(self.cache.get(&n).expect("just fetched").0.clone())
    }

    /// Marks page `n` dirty so the next [`Pager::flush`] writes it out.
    ///
    /// Inside a transaction the page's current contents go into the journal
    /// first, which is the only point where the pre-image still exists: once
    /// the page has been modified, what it held before is gone.
    pub fn mark_dirty(&mut self, n: u32) {
        if self.journal.is_some() && n > 0 {
            let _ = self.journal_page(n);
        }
        if let Some((_, origin, _)) = self.cache.get_mut(&n) {
            *origin = Origin::Dirty;
        }
        if !self.dirty.contains(&n) {
            self.dirty.push(n);
        }
        self.bump_change_counter();
    }

    fn bump_change_counter(&mut self) {
        self.header.change_counter = self.header.change_counter.wrapping_add(1);
        self.header.version_valid_for = self.header.change_counter;
    }

    fn touch(&mut self, n: u32) {
        let clock = self.clock;
        if let Some(entry) = self.cache.get_mut(&n) {
            entry.2 = clock;
        }
        if let Some(pos) = self.lru.iter().position(|&p| p == n) {
            self.lru.remove(pos);
        }
        self.lru.push(n);
    }

    /// Reads page `n` off disk into the cache.
    fn fetch(&mut self, n: u32) -> Result<()> {
        self.evict_if_needed();
        let offset = (n as u64 - 1) * self.page_size as u64;
        let mut buf = vec![0u8; self.page_size as usize];
        self.file.seek(SeekFrom::Start(offset))?;
        match self.file.read_exact(&mut buf) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                // Reading past the end yields zeroes: SQLite grows a file by
                // writing a page before anything refers to it, so a short read
                // here means the file shrank underneath us.
                let _ = self.file.seek(SeekFrom::Start(offset))?;
                let mut got = 0;
                while got < buf.len() {
                    match self.file.read(&mut buf[got..])? {
                        0 => break,
                        n => got += n,
                    }
                }
            }
            Err(e) => return Err(e.into()),
        }
        self.cache.insert(n, (buf, Origin::File, self.clock));
        self.lru.push(n);
        Ok(())
    }

    fn evict_if_needed(&mut self) {
        while self.cache.len() >= self.capacity {
            // Find the resident page with the oldest stamp, preferring clean
            // pages so a dirty one is not written out needlessly.
            let victim = self
                .lru
                .iter()
                .copied()
                .min_by_key(|&n| (self.cache[&n].1 == Origin::Dirty, self.cache[&n].2))
                .expect("cache is non-empty");
            self.evict(victim);
        }
    }

    fn evict(&mut self, n: u32) {
        if let Some((buf, origin, _)) = self.cache.remove(&n) {
            if origin == Origin::Dirty {
                let _ = self.write_page(&buf, n);
            }
        }
        self.lru.retain(|&p| p != n);
    }

    fn write_page(&mut self, buf: &[u8], n: u32) -> Result<()> {
        let offset = (n as u64 - 1) * self.page_size as u64;
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.write_all(buf)?;
        Ok(())
    }

    /// Appends a fresh page of zeroes to the file and returns its number.
    ///
    /// Page 1 holds the file header and is never handed out here: a fresh
    /// database starts with a page count of zero, so the next page would come
    /// back as page 1 and overwrite the header `open` just wrote, leaving an
    /// all-zero file that no later open can read.
    pub fn allocate(&mut self) -> Result<u32> {
        let n = (self.header.db_size_pages + 1).max(2);
        let buf = vec![0u8; self.page_size as usize];
        self.write_page(&buf, n)?;
        self.header.db_size_pages = n;
        self.cache.insert(n, (buf, Origin::Dirty, self.clock));
        self.lru.push(n);
        if !self.dirty.contains(&n) {
            self.dirty.push(n);
        }
        self.bump_change_counter();
        Ok(n)
    }

    /// Releases page `n` back to the freelist.
    ///
    /// A freelist trunk page is an array of 32-bit page numbers filling the
    /// usable space: the first is the next trunk page or zero, the second is the
    /// number of leaf pages that follow, and the rest are those pages. A single
    /// freed page therefore becomes a trunk with no successor and one leaf,
    /// and the page itself is zeroed apart from that array so a stale read finds
    /// no old contents.
    pub fn free(&mut self, n: u32) -> Result<()> {
        if n == 0 || n > self.header.db_size_pages {
            return Ok(());
        }
        let trunk = self.header.freelist_trunk;
        {
            let page = self.page(n)?;
            for b in page.iter_mut() {
                *b = 0;
            }
            // With no existing trunk this page is the first of a chain, so its
            // successor is zero. It carries one leaf, itself.
            page[0..4].copy_from_slice(&trunk.to_be_bytes());
            page[4..8].copy_from_slice(&1u32.to_be_bytes());
            page[8..12].copy_from_slice(&n.to_be_bytes());
        }
        self.mark_dirty(n);
        self.header.freelist_trunk = n;
        self.header.freelist_count += 1;
        self.bump_change_counter();
        Ok(())
    }

    /// Writes the current header into the cached page 1.
    ///
    /// The page count, change counter and freelist fields are the parts that
    /// must be current for another process to see a consistent file.
    pub fn write_header(&mut self) -> Result<()> {
        let bytes = self.header.to_bytes();
        {
            let page = self.page(1)?;
            page[..HEADER_SIZE].copy_from_slice(&bytes);
        }
        self.mark_dirty(1);
        Ok(())
    }

    /// Writes every dirty page and the header to the file.
    pub fn flush(&mut self) -> Result<()> {
        self.write_header()?;
        let pages: Vec<u32> = self.dirty.clone();
        for n in pages {
            if let Some((buf, origin, _)) = self.cache.get(&n) {
                if *origin == Origin::Dirty {
                    let buf = buf.clone();
                    self.write_page(&buf, n)?;
                    if let Some(entry) = self.cache.get_mut(&n) {
                        entry.1 = Origin::File;
                    }
                }
            }
        }
        self.dirty.clear();
        self.file.flush()?;
        Ok(())
    }

    /// Starts journalling, so the pages a transaction changes can be put back.
    ///
    /// The page count is recorded in the header, because a rollback has to know
    /// whether the file grew and needs shortening.
    pub fn begin_journal(&mut self) -> Result<()> {
        if self.journal.is_some() {
            return Ok(());
        }
        let path = self.path.clone().ok_or_else(|| {
            Error::new(
                ResultCode::Error,
                "an in-memory database cannot be journalled",
            )
        })?;
        self.nonce = self.nonce.wrapping_add(1);
        let mut j = Journal::create(&path, self.page_size, 512, self.nonce)?;
        j.set_original_size(self.header.db_size_pages)?;
        self.journal = Some(j);
        Ok(())
    }

    /// Whether a journal is open.
    pub fn journalling(&self) -> bool {
        self.journal.is_some()
    }

    /// Writes the journal's pre-image of page `n` if it has not been written
    /// yet.
    pub fn journal_page(&mut self, n: u32) -> Result<()> {
        if n == 0 {
            return Ok(());
        }
        if let Some(j) = &self.journal {
            if j.recorded_pages().contains(&n) {
                return Ok(());
            }
        }
        let current = self.read_page(n)?;
        if let Some(j) = self.journal.as_mut() {
            j.record(n, &current)?;
        }
        Ok(())
    }

    /// The commit: the journal's deletion is what makes the change permanent.
    pub fn commit_journal(&mut self) -> Result<()> {
        if let Some(mut j) = self.journal.take() {
            j.sync()?;
            j.commit()?;
        }
        Ok(())
    }

    /// Puts back every page the journal holds, and shortens the file if the
    /// transaction grew it.
    pub fn rollback_journal(&mut self) -> Result<()> {
        let Some(mut j) = self.journal.take() else {
            return Ok(());
        };
        j.sync()?;
        let original = j.header().db_size;
        // The cache holds the newer versions, so it is dropped before the
        // journal's come back; otherwise a later read would see the cache.
        self.cache.clear();
        self.lru.clear();
        self.dirty.clear();
        for page in j.recorded_pages().to_vec() {
            let contents = j.page_contents(page)?;
            self.write_page_at(page, &contents)?;
        }
        self.truncate(original)?;
        j.discard()?;
        // The rollback undid the change counter's progress, so the next write
        // has to advance it rather than reuse a value already on disk.
        self.header.change_counter = self.header.change_counter.wrapping_add(1);
        self.header.version_valid_for = self.header.change_counter;
        self.write_header()?;
        self.flush()?;
        Ok(())
    }

    /// Puts back the pages of a journal left behind by an interrupted
    /// transaction.
    ///
    /// This runs before the caller sees the database at all, because until it
    /// has, the file is mid-transaction and reading it would mix the committed
    /// and uncommitted states.
    fn replay_hot_journal(&mut self) -> Result<()> {
        let Some(path) = self.path.clone() else {
            return Ok(());
        };
        let Some(hot) = journal::find_hot(&path)? else {
            return Ok(());
        };
        self.cache.clear();
        self.lru.clear();
        self.dirty.clear();
        for RecoveredPage { page_no, contents } in hot.recover()? {
            if page_no == 0 || page_no > hot.original_size() {
                // A page the database did not have before the transaction was
                // never written, and one past the original size is past the
                // file as it was.
                continue;
            }
            self.write_page_at(page_no, &contents)?;
        }
        if hot.original_size() > 0 {
            self.truncate(hot.original_size())?;
        }
        hot.finish()?;
        self.write_header()?;
        self.flush()?;
        Ok(())
    }

    /// Writes a page's bytes without going through the cache.
    fn write_page_at(&mut self, n: u32, contents: &[u8]) -> Result<()> {
        let offset = (n as u64 - 1) * self.page_size as u64;
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.write_all(contents)?;
        Ok(())
    }

    /// Shortens the file to `pages` pages, or extends it with zeroes.
    fn truncate(&mut self, pages: u32) -> Result<()> {
        let len = pages as u64 * self.page_size as u64;
        if self.file.metadata()?.len() != len {
            self.file.set_len(len)?;
        }
        self.header.db_size_pages = pages;
        Ok(())
    }

    /// The page numbers written since the last [`Pager::flush`], for a journal
    /// to capture before the change lands.
    pub fn dirty_pages(&self) -> &[u32] {
        &self.dirty
    }
}

impl std::fmt::Debug for Pager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pager")
            .field("page_size", &self.page_size)
            .field("page_count", &self.header.db_size_pages)
            .field("cached", &self.cache.len())
            .field("dirty", &self.dirty.len())
            .field("in_memory", &self.memory_path.is_some())
            .finish()
    }
}

impl Drop for Pager {
    fn drop(&mut self) {
        // A memory database must not leave a file behind, whether or not the
        // caller flushed.
        if let Some(path) = self.memory_path.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::page::page_type;

    fn temp_path(tag: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("nsqlite-pager-{}-{tag}.db", std::process::id()));
        let _ = std::fs::remove_file(&p);
        p
    }

    #[test]
    fn a_new_file_gets_a_valid_header() {
        let path = temp_path("new");
        let mut p = Pager::open(&path).unwrap();
        assert_eq!(p.page_size(), 4096);
        assert_eq!(p.header().text_encoding, crate::text::Encoding::Utf8);
        p.write_header().unwrap();
        p.flush().unwrap();
        drop(p);

        // Reopening must see the same header, which is what makes the file a
        // valid SQLite database.
        let p = Pager::open(&path).unwrap();
        assert_eq!(p.page_size(), 4096);
        drop(p);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn allocating_extends_the_file() {
        let path = temp_path("alloc");
        let mut p = Pager::open(&path).unwrap();
        p.write_header().unwrap();
        let a = p.allocate().unwrap();
        let b = p.allocate().unwrap();
        // Page 1 holds the file header, so allocation starts at 2 and keeps
        // counting up from there.
        assert_eq!((a, b), (2, 3));
        assert_eq!(p.page_count(), 3);
        p.flush().unwrap();
        drop(p);

        let p = Pager::open(&path).unwrap();
        assert_eq!(p.page_count(), 3);
        drop(p);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn dirty_pages_survive_a_reopen() {
        let path = temp_path("dirty");
        let mut p = Pager::open(&path).unwrap();
        p.write_header().unwrap();
        // Page 1 holds the file header, so start the payload at page 2.
        p.allocate().unwrap();
        let n = p.allocate().unwrap();
        assert!(n > 1);
        {
            let page = p.page(n).unwrap();
            page[0] = 0xab;
            page[4095] = 0xcd;
        }
        p.mark_dirty(n);
        p.flush().unwrap();
        drop(p);

        let mut p = Pager::open(&path).unwrap();
        let page = p.read_page(n).unwrap();
        assert_eq!((page[0], page[4095]), (0xab, 0xcd));
        drop(p);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_page_one_header_is_readable_through_the_pager() {
        let path = temp_path("page1");
        let mut p = Pager::open(&path).unwrap();
        // Page 1 is the file header and is never allocated, so the b-tree that
        // shares it is placed there directly.
        let n = 1u32;
        p.claim_page(n).unwrap();
        p.write_header().unwrap();
        p.flush().unwrap();
        drop(p);

        // A b-tree root on page 1 must not be mistaken for file-header bytes.
        let mut p = Pager::open(&path).unwrap();
        {
            let page = p.page(1).unwrap();
            page[HEADER_SIZE] = page_type::TABLE_LEAF;
        }
        p.mark_dirty(1);
        p.flush().unwrap();
        drop(p);

        let mut p = Pager::open(&path).unwrap();
        let page = p.read_page(1).unwrap();
        assert_eq!(page[HEADER_SIZE], page_type::TABLE_LEAF);
        assert_eq!(p.header().page_size, 4096);
        drop(p);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_truncated_file_is_not_a_database() {
        let path = temp_path("trunc");
        std::fs::write(&path, b"not a database").unwrap();
        assert_eq!(Pager::open(&path).unwrap_err().code.name(), "NOTADB");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn the_cache_evicts_without_losing_dirty_data() {
        let path = temp_path("evict");
        let mut p = Pager::open(&path).unwrap();
        p.capacity = 4;
        p.write_header().unwrap();
        // Touch more pages than the cache holds, dirtying each in turn, so an
        // eviction has to write a page back rather than drop it.
        // Mark every allocated page with its own page number, so a read-back
        // that mixes up two pages is caught rather than passing on coincidence.
        let mut marked = Vec::new();
        for _ in 0..32u32 {
            let n = p.allocate().unwrap();
            {
                let page = p.page(n).unwrap();
                page[0] = n as u8;
            }
            p.mark_dirty(n);
            marked.push(n);
        }
        p.flush().unwrap();
        drop(p);

        let mut p = Pager::open(&path).unwrap();
        // Page 1's first bytes are the file header, so check it separately;
        // the rest were marked with their own page number.
        for &n in &marked {
            if n == 1 {
                continue;
            }
            let page = p.read_page(n).unwrap();
            assert_eq!(page[0], n as u8, "page {n} lost its byte through eviction");
        }
        drop(p);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn freeing_a_page_clears_it_and_records_it() {
        let path = temp_path("free");
        let mut p = Pager::open(&path).unwrap();
        p.write_header().unwrap();
        p.allocate().unwrap();
        let n = p.allocate().unwrap();
        {
            let page = p.page(n).unwrap();
            page[7] = 0xff;
        }
        p.mark_dirty(n);
        p.free(n).unwrap();
        assert_eq!(p.header().freelist_trunk, n);
        assert_eq!(p.header().freelist_count, 1);
        p.flush().unwrap();
        drop(p);

        let mut p = Pager::open(&path).unwrap();
        let page = p.read_page(n).unwrap();
        // A trunk page is an array: the next trunk (zero when there is none),
        // the leaf count, then the leaves. Getting this wrong points the chain
        // at page 1, which is the schema, and the file stops being readable.
        let next = u32::from_be_bytes([page[0], page[1], page[2], page[3]]);
        let leaves = u32::from_be_bytes([page[4], page[5], page[6], page[7]]);
        let first_leaf = u32::from_be_bytes([page[8], page[9], page[10], page[11]]);
        assert_eq!(next, 0, "a first trunk has no successor");
        assert_eq!(leaves, 1, "one page was freed");
        assert_eq!(first_leaf, n, "the freed page is its own first leaf");
        assert_eq!(page[12], 0, "a freed page must not keep its old contents");
        drop(p);
        let _ = std::fs::remove_file(&path);
    }
}
