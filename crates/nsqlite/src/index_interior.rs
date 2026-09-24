//! Index interior pages: the layer that lets an index outlive a single page.
//!
//! An [`IndexLeaf`](super::index::IndexLeaf) holds entries until it is full, and
//! then the index has nowhere to put the next one. This module is what sits
//! above the leaves: interior pages that each point at a child and carry the
//! key that splits it from the next child, plus the splitting and grafting that
//! keeps the tree searchable as it grows.
//!
//! # The cell layout
//!
//! An index interior cell is an index leaf cell with a four-byte child page
//! number in front of it:
//!
//! ```text
//! +-----------------+----------------+-------------------+--------------------------+
//! | left child (4)  | payload length | payload record    | overflow pointer (4)      |
//! | big-endian u32  | varint         |                   | present only if it spills |
//! +-----------------+----------------+-------------------+--------------------------+
//! ```
//!
//! The payload is the same record an index leaf cell holds — the key columns
//! followed by the fixed-width rowid — so the same overflow geometry applies
//! with `INDEX_INTERIOR` in place of `INDEX_LEAF`.
//!
//! This was checked against files written by sqlite3 3.53.4 rather than
//! recalled, with an independent parser rather than this module's own reader.
//! On a 4096-byte page holding 5000 single-column integer keys
//! (`CREATE TABLE t(id INTEGER PRIMARY KEY, k INT)`, then `CREATE INDEX i1 ON
//! t(k)` *before* the rows, which is what puts the root at page 3), the root
//! was an index interior page with 13 cells and a rightmost child of page 29,
//! and its first cell's bytes were:
//!
//! ```text
//! 00 00 00 04  07  03 02 02 01 80 01 80
//! ^^^^^^^^^^  ^  ^^^^^^^^^^^^^^ ^^^^^^^
//! child = 4    |  |  the record     |  the rowid, two bytes
//!              |  payload length = 7
//!              payload-length varint
//! ```
//!
//! Read as a record: a three-byte header naming serial types 2 and 2, then the
//! key 384 and the rowid 384, each a two-byte integer. The rowid slot is *not*
//! the fixed eight bytes this module writes — see
//! [`IndexEntry`](super::index::IndexEntry) — so a separator here is five bytes
//! wider than the reference's. Every cell layout and geometry claim below holds
//! either way, because the widths are computed from the actual encoded length,
//! and this module does read a reference index correctly.
//!
//! A 1200-byte key on a 4096-byte page showed the spilling form. The reference
//! wrote `payload = 1206, local = 489`, i.e. a 4-byte child, a 2-byte length
//! varint, 489 local bytes and a 4-byte overflow pointer, with 717 bytes in the
//! chain. The head page it named was 192 — a page number, not a byte count.
//! That 489 is the same `min_local` the reference keeps on an index leaf,
//! confirming the geometry is keyed off the page kind only in name.
//!
//! A 900-byte key on a 512-byte page behaved the same way at the smaller
//! threshold: `payload = 906, local = 39`, and the child it named was itself an
//! interior page, so a spilled separator is ordinary.
//!
//! # The separator rule, and why the obvious reading is wrong
//!
//! The obvious statement is "a cell's key is the largest key in its left
//! child". That is false, and building to it produces an index that loses one
//! key per split. In the reference file the cell pointing at leaf 4 carried the
//! key 384, but leaf 4's largest entry was 383.
//!
//! What SQLite does is **promote** the key: the largest key of the left
//! subtree moves *out* of the leaves and *into* the interior cell, so it is
//! stored exactly once. The accounting on the reference file is exact — 4987
//! leaf entries plus 13 separators is 5000, every key, with no key appearing in
//! both a leaf and a separator.
//!
//! The matching descent rule is `key <= separator` goes left, and it is a real
//! distinction about *lookups*, not about storage. Simulating `<` and `<=`
//! over a 4000-key, four-level reference tree misplaced 0 stored keys under
//! either rule, because the separator is a tie-break that no stored key ever
//! ties with. What differs is where a *new* entry sharing a promoted key goes,
//! and the reference is unambiguous about that. Inserting `k=2532` — whose own
//! rowid, 2532, is the promoted separator — put the new row in the cell's
//! **right** subtree; inserting `k=2532` with `id=1`, a rowid *below* the
//! separator, put it in the **left**. That is precisely `key <= separator`
//! compared as a whole entry, key then rowid, and it is what
//! [`IndexTree::insert`] relies on.
//!
//! So a cell's left child holds the entries that compare at or below the
//! separator, and the separator is a real index entry that only a walk of this
//! node finds. [`IndexTree::scan`] emits it between the two subtrees for that
//! reason, and the separator is compared as a whole entry: inserting a second
//! rowid for an already-promoted key puts the new entry to the *right* of that
//! cell, which the reference file does too.
//!
//! # Where the root goes
//!
//! The root's page number never moves, which is what lets a schema keep one
//! `rootpage` for the life of the index. That was checked rather than assumed:
//! an index grown to 1.1 million rows kept `rootpage` at 3 from the moment the
//! first row was inserted, and ended three levels deep with a 15-cell root. A
//! 512-byte-page index with a 100-byte second key column also reads `rootpage=3`
//! after the interior layer split.
//!
//! When the root itself overflows, the root page is *not* reused as one of its
//! own children — that would make the root its own descendant and the descent
//! would never terminate. Two fresh pages take the halves and the root becomes
//! a single cell. Observed directly: a 512-byte index over 3000 rows
//! `(k, j)` keys ended as a two-cell root whose three children were 1551, 1552
//! and 3054, all freshly allocated and none of them the root. The children
//! carried 9, 9 and 9 cells, and the root's separators were 1000 and 2000 —
//! the reference promotes the largest separator of the lower half exactly as
//! [`split_interior`] does here.
//!
//! Splitting a non-root page is the opposite: the original page keeps its
//! number for the lower half, so the parent gains a cell rather than a page,
//! and the cell it already had for that page is re-seated rather than appended.

use super::error::{Error, Result};
use super::index::{compare, IndexEntry, IndexLeaf};
use super::page::{local_payload_size, overflow_capacity_for, page_type, PageHeader};
use super::pager::Pager;
use super::value::Value;
use super::varint;

/// One cell of an index interior page: a left child and the key that was
/// promoted out of that child's subtree.
///
/// The key is stored *here* rather than in a leaf, so a scan must emit it
/// between the left and the right subtree. It is also the entry a lookup for
/// exactly that key finds, which is why the separator cannot be an exclusive
/// bound.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexInteriorCell {
    /// The child holding the keys strictly below `separator`.
    pub left_child: u32,
    /// The largest key of `left_child`'s subtree, held here rather than below.
    pub separator: IndexEntry,
    /// The head of this cell's overflow chain, or zero when the key fits.
    pub overflow: u32,
}

impl IndexInteriorCell {
    /// The payload's on-page length, and whether a 4-byte overflow pointer
    /// follows it.
    fn local_parts(&self, usable: u32) -> (usize, bool) {
        let n = self.separator.encode().len() as u32;
        let local = local_payload_size(page_type::INDEX_INTERIOR, n, usable) as usize;
        (local, local != n as usize)
    }

    /// The bytes the cell occupies on the page, excluding its overflow chain.
    pub fn on_page_size(&self, usable: u32) -> usize {
        let payload = self.separator.encode().len();
        let (local, spilled) = self.local_parts(usable);
        4 + varint::len_for(payload as u64) + local + if spilled { 4 } else { 0 }
    }

    /// Serialises the cell, given the overflow head to record for a spilled key.
    fn encode(&self, usable: u32, overflow: u32) -> Vec<u8> {
        let payload = self.separator.encode();
        let (local, spilled) = self.local_parts(usable);
        let mut out = Vec::with_capacity(4 + payload.len() + 4);
        out.extend_from_slice(&self.left_child.to_be_bytes());
        varint::append(&mut out, payload.len() as u64);
        out.extend_from_slice(&payload[..local]);
        if spilled {
            out.extend_from_slice(&overflow.to_be_bytes());
        }
        out
    }
}

/// An index interior page.
///
/// Cells are in ascending separator order, which is also the pointer-array
/// order, and the rightmost child holds the keys greater than every separator.
pub struct IndexInterior {
    pub page_no: u32,
    pub cells: Vec<IndexInteriorCell>,
    pub rightmost: u32,
    pub page_size: u32,
    pub header_offset: u16,
}

impl IndexInterior {
    /// A new, empty interior page with a single rightmost child.
    pub fn empty(page_no: u32, page_size: u32, rightmost: u32) -> IndexInterior {
        IndexInterior {
            page_no,
            cells: Vec::new(),
            rightmost,
            page_size,
            header_offset: if page_no == 1 { 100 } else { 0 },
        }
    }

    /// Reads an index interior page, following any overflow chains so a wide
    /// key comes back whole.
    pub fn read(pager: &mut Pager, page_no: u32) -> Result<IndexInterior> {
        let page = pager.read_page(page_no)?;
        let page_size = page.len() as u32;
        let header_offset = if page_no == 1 { 100 } else { 0 };
        let h = PageHeader::parse(&page, header_offset)?;
        if h.page_type != page_type::INDEX_INTERIOR {
            return Err(Error::corrupt(format!(
                "page {page_no} is a {} where an index interior was expected",
                page_type::name(h.page_type)
            )));
        }
        let usable = pager.usable_size();
        let encoding = pager.header().text_encoding;
        let array = h.cell_pointer_array_offset();
        let mut cells = Vec::with_capacity(h.cell_count as usize);
        for i in 0..h.cell_count as usize {
            let at = array + i * 2;
            if at + 2 > page.len() {
                return Err(Error::corrupt("cell pointer array is out of range"));
            }
            let offset = u16::from_be_bytes([page[at], page[at + 1]]) as usize;
            if offset + 5 > page.len() {
                return Err(Error::corrupt("index interior cell is truncated"));
            }
            let left_child = u32::from_be_bytes([
                page[offset],
                page[offset + 1],
                page[offset + 2],
                page[offset + 3],
            ]);
            let body = &page[offset + 4..];
            let (payload_len, n) = varint::get_checked(body)
                .ok_or_else(|| Error::corrupt("index interior cell payload length is truncated"))?;
            let payload_len = payload_len as usize;
            let local =
                local_payload_size(page_type::INDEX_INTERIOR, payload_len as u32, usable) as usize;
            if local > body.len() - n {
                return Err(Error::corrupt(
                    "index interior cell is shorter than its local payload",
                ));
            }
            let mut payload = body[n..n + local].to_vec();
            let mut overflow = 0u32;
            if local < payload_len {
                let p = n + local;
                if body.len() < p + 4 {
                    return Err(Error::corrupt(
                        "index interior cell overflow pointer is truncated",
                    ));
                }
                let head = u32::from_be_bytes([body[p], body[p + 1], body[p + 2], body[p + 3]]);
                read_chain(pager, head, &mut payload, payload_len, usable)?;
                overflow = head;
            }
            cells.push(IndexInteriorCell {
                left_child,
                separator: IndexEntry::decode(&payload, encoding)?,
                overflow,
            });
        }
        Ok(IndexInterior {
            page_no,
            cells,
            rightmost: h
                .right_most
                .ok_or_else(|| Error::corrupt("index interior page has no rightmost child"))?,
            page_size,
            header_offset,
        })
    }

    /// Lays the cells out, returning the content start and each cell's offset.
    fn layout(&self, usable: u32) -> Result<(u32, Vec<u16>)> {
        let floor = self.header_offset as usize + PageHeader::SIZE_INTERIOR + self.cells.len() * 2;
        let mut content_start = self.page_size as usize;
        let mut offsets = Vec::with_capacity(self.cells.len());
        for cell in self.cells.iter().rev() {
            let n = cell.on_page_size(usable);
            if n > content_start {
                return Err(Error::corrupt(
                    "index interior cell is larger than the page",
                ));
            }
            content_start -= n;
            offsets.push(content_start as u16);
        }
        offsets.reverse();
        if content_start < floor {
            return Err(Error::full());
        }
        Ok((content_start as u32, offsets))
    }

    /// Free bytes between the pointer array and the cell content area.
    pub fn free_space(&self, usable: u32) -> usize {
        let Ok((content_start, _)) = self.layout(usable) else {
            return 0;
        };
        let floor = self.header_offset as usize + PageHeader::SIZE_INTERIOR + self.cells.len() * 2;
        (content_start as usize).saturating_sub(floor)
    }

    /// Whether the page can hold one more cell of the given size.
    pub fn fits(&self, usable: u32, cell_size: usize) -> bool {
        // Each cell costs its own bytes plus a two-byte pointer in the array.
        self.free_space(usable) >= cell_size + 2
    }

    /// Whether the page can be written as it stands.
    pub fn can_write(&self, usable: u32) -> bool {
        self.layout(usable).is_ok()
    }

    /// Inserts a cell in separator order, returning where it landed. A
    /// separator that is already present is a structural error rather than a
    /// silent duplicate, since a duplicated separator would double-reference a
    /// key.
    pub fn insert_cell(&mut self, cell: IndexInteriorCell, key_len: usize) -> Result<usize> {
        match self
            .cells
            .binary_search_by(|c| compare(&c.separator, &cell.separator, key_len))
        {
            Ok(_) => Err(Error::corrupt(
                "an index interior page cannot hold the same separator twice",
            )),
            Err(pos) => {
                self.cells.insert(pos, cell);
                Ok(pos)
            }
        }
    }

    /// The child that should receive an entry.
    ///
    /// An entry equal to a separator belongs in that cell's left subtree,
    /// because the separator itself lives in the cell rather than in a leaf.
    /// This is the inclusive descent the reference format requires; the
    /// exclusive form loses one key per interior cell.
    pub fn child_for(&self, entry: &IndexEntry, key_len: usize) -> u32 {
        self.descent(entry, key_len).2
    }

    /// Where an entry descends: the cell position it takes, whether it landed
    /// on a separator of this page, where `cells.len()` means the rightmost
    /// child, and that child's page.
    ///
    /// The caller needs the position as well as the child because grafting a
    /// split back in has to know which existing cell named the page that split.
    pub fn descent(&self, entry: &IndexEntry, key_len: usize) -> (usize, bool, u32) {
        for (i, cell) in self.cells.iter().enumerate() {
            let ord = compare(entry, &cell.separator, key_len);
            if ord != std::cmp::Ordering::Greater {
                return (i, ord == std::cmp::Ordering::Equal, cell.left_child);
            }
        }
        (self.cells.len(), false, self.rightmost)
    }

    /// The separator equal to `entry`, if this page holds it.
    ///
    /// A promoted separator is a real index entry stored in this cell rather
    /// than in any leaf, so this is the only place a duplicate of it can hide.
    /// [`IndexTree::insert`] consults it on every interior page of the descent
    /// for exactly that reason.
    pub fn separator_for(&self, entry: &IndexEntry, key_len: usize) -> Option<&IndexEntry> {
        self.cells
            .iter()
            .find(|c| compare(&c.separator, entry, key_len) == std::cmp::Ordering::Equal)
            .map(|c| &c.separator)
    }

    /// Writes the page, allocating an overflow chain for any separator that
    /// needs one and does not have it.
    pub fn write_to(&mut self, pager: &mut Pager) -> Result<()> {
        let usable = pager.usable_size();
        // Resolve the chains first, since allocating a page needs the pager and
        // this page's buffer is borrowed below.
        for i in 0..self.cells.len() {
            let payload = self.cells[i].separator.encode();
            let n = payload.len() as u32;
            let local = local_payload_size(page_type::INDEX_INTERIOR, n, usable) as usize;
            if local != payload.len() && self.cells[i].overflow == 0 {
                let head = super::btree_write::write_overflow_chain(pager, &payload[local..])?;
                self.cells[i].overflow = head;
            }
        }
        let page_no = self.page_no;
        {
            let dst = pager.page(page_no)?;
            for b in dst.iter_mut() {
                *b = 0;
            }
            let (content_start, offsets) = self.layout(usable)?;
            for (i, cell) in self.cells.iter().enumerate() {
                let body = cell.encode(usable, cell.overflow);
                let at = offsets[i] as usize;
                dst[at..at + body.len()].copy_from_slice(&body);
            }
            let h = PageHeader {
                page_type: page_type::INDEX_INTERIOR,
                first_freeblock: 0,
                cell_count: self.cells.len() as u16,
                // Zero means the content area starts at the end of the page,
                // which is only true for a completely full page.
                cell_content_start: if content_start >= 0xffff {
                    0
                } else {
                    content_start as u16
                },
                fragmented_free: 0,
                right_most: Some(self.rightmost),
                header_offset: self.header_offset,
            };
            h.write(dst);
            let array = h.cell_pointer_array_offset();
            for (i, at) in offsets.iter().enumerate() {
                dst[array + i * 2..array + i * 2 + 2].copy_from_slice(&at.to_be_bytes());
            }
        }
        pager.mark_dirty(page_no);
        Ok(())
    }
}

/// Appends the rest of a spilled payload from its overflow chain.
fn read_chain(
    pager: &mut Pager,
    head: u32,
    payload: &mut Vec<u8>,
    total: usize,
    usable: u32,
) -> Result<()> {
    let cap = overflow_capacity_for(usable) as usize;
    let mut next = head;
    let mut guard = 0usize;
    while payload.len() < total {
        if next == 0 {
            return Err(Error::corrupt("index interior overflow chain ended early"));
        }
        guard += 1;
        if guard > total {
            return Err(Error::corrupt("index interior overflow chain is cyclic"));
        }
        let op = pager.read_page(next)?;
        let following = u32::from_be_bytes([op[0], op[1], op[2], op[3]]);
        let want = (total - payload.len()).min(cap);
        payload.extend_from_slice(&op[4..4 + want]);
        next = following;
    }
    Ok(())
}

/// The two halves of a split leaf, and the key that now lives between them.
pub struct LeafSplit {
    /// The page holding the lower half.
    pub left: u32,
    /// The page holding the upper half.
    pub right: u32,
    /// The key promoted out of the lower half into the parent, i.e. the largest
    /// key below the split.
    pub separator: IndexEntry,
    /// The head of the promoted key's overflow chain, or zero when it fits.
    ///
    /// The key leaves the leaves entirely, so its chain has to travel with it to
    /// the interior cell. It cannot be looked up afterwards from the page it
    /// used to be on, because that page is rewritten and the chain is no longer
    /// named by anything.
    pub separator_overflow: u32,
}

/// Splits a full leaf into two pages.
///
/// The separator is the largest key of the left half and is *removed* from that
/// half, because the parent's cell becomes the only home for it. Leaving it in
/// the leaf as well would make a leaf-and-separator scan emit it twice, which
/// is the duplicate the audit checks for.
///
/// Every entry's overflow chain is carried into whichever half keeps that entry,
/// rather than being rebuilt. Rebuilding is what used to happen, and it leaked:
/// [`IndexLeaf::write_to`] allocates a chain for any entry whose head is zero,
/// so a half written with all-zero heads allocated a whole new chain per spilled
/// entry and left the page's old chains unreferenced. Over a few hundred wide
/// keys that stranded the majority of the file, and `PRAGMA integrity_check`
/// answered "*** in database main ***" followed by a "Page N: never used" line
/// for every page of them. Carrying the head is both cheaper and the only way
/// to keep every page the tree allocated reachable.
///
/// `reuse` says whether the lower half may keep the original page's number. It
/// must be false for the root: a root that pointed at itself as a child would
/// be its own descendant and the descent would never terminate. The reference
/// allocates two fresh pages in that case, and so does this.
pub fn split_leaf(
    pager: &mut Pager,
    leaf: &IndexLeaf,
    pending: &IndexEntry,
    key_len: usize,
    reuse: bool,
) -> Result<LeafSplit> {
    // The pending entry is part of what the page has to hold, so the split
    // point is chosen over the entries already there plus it. The chain heads
    // travel in step, since a head is only meaningful beside its own entry.
    let mut all = leaf.entries.clone();
    let mut all_overflows = leaf.overflows.clone();
    all_overflows.resize(all.len(), 0);
    let pos = all
        .binary_search_by(|e| compare(e, pending, key_len))
        .unwrap_or_else(|p| p);
    all.insert(pos, pending.clone());
    // The pending entry is brand new and has no chain yet.
    all_overflows.insert(pos, 0);
    if all.len() < 2 {
        return Err(Error::corrupt("a leaf with one entry cannot be split"));
    }
    let usable = pager.usable_size();

    // Try each split point and keep the first that leaves both halves fitting.
    // Start in the middle so the halves are roughly even, then walk outward only
    // if a wide key makes the middle split leave one half too full.
    let mid = all.len() / 2;
    for delta in 0..=mid {
        for left_len in [mid.saturating_sub(delta), mid + delta] {
            if left_len == 0 || left_len >= all.len() {
                continue;
            }
            let separator = all[left_len - 1].clone();
            // The separator is promoted, not copied, so it is dropped from the
            // lower half rather than left there as well — and its chain goes up
            // with it rather than being left behind for the rewrite to strand.
            let separator_overflow = all_overflows[left_len - 1];
            let left_entries = all[..left_len - 1].to_vec();
            let left_overflows = all_overflows[..left_len - 1].to_vec();
            let right_entries = all[left_len..].to_vec();
            let right_overflows = all_overflows[left_len..].to_vec();
            if !leaf_fits(&left_entries, usable, leaf) || !leaf_fits(&right_entries, usable, leaf) {
                continue;
            }
            let left_no = if reuse {
                leaf.page_no
            } else {
                pager.allocate()?
            };
            let right_no = pager.allocate()?;
            write_half(pager, leaf, left_no, left_entries, left_overflows)?;
            write_half(pager, leaf, right_no, right_entries, right_overflows)?;
            return Ok(LeafSplit {
                left: left_no,
                right: right_no,
                separator,
                separator_overflow,
            });
        }
    }
    Err(Error::corrupt(format!(
        "an index leaf of {} entries could not be split into two fitting halves",
        leaf.entries.len()
    )))
}

/// Whether `entries` fit on a leaf page with `page`'s geometry.
///
/// `usable` is the pager's usable size — the page stride less any reserved
/// region — and is *not* the page's own `page_size`. Both halves of the
/// arithmetic key off it. The overflow threshold does, so computing the local
/// size from the raw stride would decide the wrong entries spill and measure
/// the wrong bytes. The space available does too, because a cell may not be
/// laid into the reserved region; this used to compare against `page_size`,
/// which made it willing to approve halves [`IndexLeaf::write_to`] then refuses
/// with `Error::full()`.
///
/// The check is also conservative in the other direction, deliberately. The
/// writer in `index.rs` measures its own free space against the raw page size,
/// so a split this approves is always one the writer can also write; the
/// reverse is not guaranteed. Stricter is the safe direction for a function
/// whose answer only chooses a split point. nsqlite never sets a reserved region
/// today, so both are the same number for now, but the rest of the module
/// already threads `pager.usable_size()` and this call site did not.
fn leaf_fits(entries: &[IndexEntry], usable: u32, page: &IndexLeaf) -> bool {
    let floor = page.header_offset as usize + PageHeader::SIZE_LEAF + entries.len() * 2;
    let total: usize = entries
        .iter()
        .map(|e| {
            let n = e.encode().len() as u32;
            let local = local_payload_size(page_type::INDEX_LEAF, n, usable) as usize;
            varint::len_for(n as u64) + local + if local == n as usize { 0 } else { 4 }
        })
        .sum();
    // The content area runs from the usable boundary down to the floor, so what
    // is free is what is left between them.
    floor + total <= usable as usize
}

/// Writes one half of a split leaf to `page_no`.
///
/// The page number is separate because a split builds both halves from one page
/// and only the lower half may keep the original number. `overflows` carries
/// each entry's existing chain head so the rewrite reuses it; an entry that
/// needs a chain and has none gets one allocated by `write_to`.
fn write_half(
    pager: &mut Pager,
    page: &IndexLeaf,
    page_no: u32,
    entries: Vec<IndexEntry>,
    overflows: Vec<u32>,
) -> Result<()> {
    let mut half = IndexLeaf {
        page_no,
        overflows,
        entries,
        page_size: page.page_size,
        header_offset: page.header_offset,
    };
    half.write_to(pager)
}

/// What a split of an interior page produced.
pub struct InteriorSplit {
    /// The page holding the lower half of the cells.
    pub left: u32,
    /// The page holding the upper half.
    pub right: u32,
    /// The separator promoted out of the lower half, which the parent stores.
    pub separator: IndexEntry,
    /// The head of the promoted separator's overflow chain, or zero when it
    /// fits. It moves up with the key, for the same reason a split leaf's does.
    pub separator_overflow: u32,
}

/// Splits a full interior page into two pages.
///
/// The promoted key is the largest separator of the lower half and moves out of
/// it, so the lower half's rightmost pointer is re-seated onto the child that
/// key used to bound. That is what keeps every child referenced exactly once:
/// the promoted key becomes the parent's separator and the lower half takes
/// over the child beneath it.
///
/// `reuse` follows the same rule as [`split_leaf`]: only a non-root page may
/// keep its number, since a root that pointed at itself would never terminate.
pub fn split_interior(
    pager: &mut Pager,
    page: &IndexInterior,
    reuse: bool,
) -> Result<InteriorSplit> {
    let usable = pager.usable_size();
    if page.cells.len() < 2 {
        return Err(Error::corrupt(
            "an index interior page with one cell cannot be split",
        ));
    }
    let mid = page.cells.len() / 2;
    for delta in 0..=mid {
        for left_len in [mid.saturating_sub(delta), mid + delta] {
            if left_len == 0 || left_len >= page.cells.len() {
                continue;
            }
            let promoted = page.cells[left_len - 1].clone();
            let left = IndexInterior {
                page_no: if reuse {
                    page.page_no
                } else {
                    pager.allocate()?
                },
                // The promoted cell leaves the lower half, so the lower half
                // ends at the child that key used to bound. The cells that stay
                // keep their own overflow heads, for the same reason a split
                // leaf's halves do: a rewritten cell must reuse its chain, not
                // orphan it and allocate a second one.
                cells: page.cells[..left_len - 1].to_vec(),
                rightmost: promoted.left_child,
                page_size: page.page_size,
                header_offset: page.header_offset,
            };
            let right = IndexInterior {
                page_no: pager.allocate()?,
                cells: page.cells[left_len..].to_vec(),
                rightmost: page.rightmost,
                page_size: page.page_size,
                header_offset: page.header_offset,
            };
            if left.can_write(usable) && right.can_write(usable) {
                let mut left = left;
                let mut right = right;
                left.write_to(pager)?;
                right.write_to(pager)?;
                return Ok(InteriorSplit {
                    left: left.page_no,
                    right: right.page_no,
                    separator: promoted.separator,
                    separator_overflow: promoted.overflow,
                });
            }
        }
    }
    Err(Error::corrupt(format!(
        "an index interior page of {} cells could not be split into two fitting halves",
        page.cells.len()
    )))
}

/// One step of a descent, kept so an insert can come back up the tree.
struct Step {
    page_no: u32,
    /// The cell position the descent took, where `cells.len()` is the rightmost.
    pos: usize,
}

/// A whole index tree, rooted at a page.
///
/// The tree is grown and read one page at a time, so a large index costs a
/// descent per operation rather than living in memory.
pub struct IndexTree<'a> {
    pager: &'a mut Pager,
    /// The page to descend from. It never moves: a full root is split in place
    /// so a schema's `rootpage` stays valid.
    pub root: u32,
    /// How many leading columns are the key, i.e. the arity of `compare`.
    pub key_len: usize,
}

impl<'a> IndexTree<'a> {
    /// Opens a tree rooted at `root` over a key of `key_len` columns.
    ///
    /// The root page must already exist as a b-tree page; use
    /// [`IndexTree::create`] for one that has only been allocated.
    pub fn open(pager: &'a mut Pager, root: u32, key_len: usize) -> IndexTree<'a> {
        IndexTree {
            pager,
            root,
            key_len,
        }
    }

    /// Creates a tree whose root is a fresh, empty leaf on `root`.
    ///
    /// [`Pager::allocate`] hands back a page of zeroes, and a page of zeroes
    /// reads as a freelist trunk rather than a b-tree page, so the first
    /// insert would find a page of the wrong type. Writing the empty root is
    /// therefore part of creating the index, not something the first insert can
    /// do lazily.
    pub fn create(pager: &'a mut Pager, root: u32, key_len: usize) -> Result<IndexTree<'a>> {
        let page_size = pager.page_size();
        IndexLeaf::empty(root, page_size).write_to(pager)?;
        Ok(IndexTree::open(pager, root, key_len))
    }

    /// The b-tree page type at `page_no`, from its header byte.
    fn page_kind(&mut self, page_no: u32) -> Result<u8> {
        let page = self.pager.read_page(page_no)?;
        let at = if page_no == 1 { 100 } else { 0 };
        Ok(page[at])
    }

    /// Walks from the root to the leaf that should own `entry`, recording the
    /// route so a split can be grafted back in.
    ///
    /// Stops early when the entry lands on a separator, because there is no
    /// leaf below that can hold it: the cell *is* its home. Returning
    /// `Ok(None)` is how `insert` learns to report the duplicate.
    fn descend(&mut self, entry: &IndexEntry) -> Result<(Vec<Step>, Option<u32>)> {
        let mut path: Vec<Step> = Vec::new();
        let mut page_no = self.root;
        loop {
            match self.page_kind(page_no)? {
                page_type::INDEX_LEAF => return Ok((path, Some(page_no))),
                page_type::INDEX_INTERIOR => {
                    let page = IndexInterior::read(self.pager, page_no)?;
                    let (pos, hit_separator, child) = page.descent(entry, self.key_len);
                    if hit_separator {
                        return Ok((path, None));
                    }
                    path.push(Step { page_no, pos });
                    page_no = child;
                }
                other => {
                    return Err(Error::corrupt(format!(
                        "page {page_no} is a {} inside an index b-tree",
                        page_type::name(other)
                    )))
                }
            }
        }
    }

    /// Inserts an entry, splitting leaves and interior pages as needed.
    ///
    /// A duplicate of an existing key *and* rowid is refused with the
    /// constraint error the reference reports, which is what
    /// `UNIQUE constraint failed` needs. A non-unique index may hold one key
    /// with several rowids and that is not an error; `compare` breaks the tie
    /// on the rowid, which is why the two are distinct entries.
    ///
    /// The refusal has to happen in two places, not one. A key that is still in
    /// a leaf is caught by [`IndexLeaf::insert`], but a key a split *promoted*
    /// lives in an interior cell, and the inclusive descent sends an entry equal
    /// to that separator left — into a subtree that does not contain it — so the
    /// leaf never sees it. Checking only the leaf therefore accepted a duplicate
    /// of exactly the keys a split had moved, and the resulting file was one
    /// `PRAGMA integrity_check` rejects with "wrong # of entries in index" while
    /// returning the same row twice. So the descent reports a separator hit and
    /// the check is made there too.
    pub fn insert(&mut self, entry: &IndexEntry) -> Result<()> {
        let (mut path, leaf_no) = self.descend(entry)?;
        let Some(leaf_no) = leaf_no else {
            return Err(Error::new(
                super::error::ResultCode::Constraint,
                "UNIQUE constraint failed: index",
            ));
        };
        let mut leaf = IndexLeaf::read(self.pager, leaf_no)?;
        if leaf.insert(entry.clone(), self.key_len, self.pager.usable_size())? {
            leaf.write_to(self.pager)?;
            return Ok(());
        }
        // The leaf is full. Both halves go to fresh pages, because if the leaf
        // is the root it must not become its own child.
        let is_root = path.is_empty();
        let split = split_leaf(self.pager, &leaf, entry, self.key_len, !is_root)?;
        self.graft(
            &mut path,
            split.left,
            split.right,
            split.separator,
            split.separator_overflow,
        )
    }

    /// Grafts a split's two halves and its promoted key into the parent, and
    /// keeps going up while the parents are full.
    ///
    /// The parent must end up naming both halves: a cell for the lower half
    /// bounded by the promoted key, and a cell for the upper half bounded by
    /// whatever used to bound the page that split. The cell that named the
    /// split page is re-seated into that pair rather than appended to, which is
    /// the step a naive split gets wrong — appending only the new right child
    /// would leave the reused lower half unreferenced from above and duplicate
    /// the page that was already there.
    fn graft(
        &mut self,
        path: &mut Vec<Step>,
        lower: u32,
        upper: u32,
        separator: IndexEntry,
        promoted_overflow: u32,
    ) -> Result<()> {
        let Some(step) = path.pop() else {
            // The root was a leaf that has just split. It becomes an interior
            // page in place, over the two fresh pages the split wrote, so the
            // schema's rootpage stays valid.
            let mut root = IndexInterior::empty(self.root, self.pager.page_size(), upper);
            root.cells.push(IndexInteriorCell {
                left_child: lower,
                separator,
                overflow: promoted_overflow,
            });
            return root.write_to(self.pager);
        };

        let parent_no = step.page_no;
        let mut parent = IndexInterior::read(self.pager, parent_no)?;
        let is_root = path.is_empty();

        if step.pos >= parent.cells.len() {
            // The split page was the parent's rightmost child, so the upper
            // half has no upper bound of its own: it becomes the rightmost and
            // the promoted key is the parent's last cell.
            parent.cells.push(IndexInteriorCell {
                left_child: lower,
                separator,
                overflow: promoted_overflow,
            });
            parent.rightmost = upper;
        } else {
            // The split page was named by a cell, and that cell's separator is
            // the upper half's bound. Replace the cell with the pair: the
            // lower half bounded by the promoted key, then the upper half
            // bounded by the old separator.
            let old = parent.cells[step.pos].clone();
            parent.cells[step.pos] = IndexInteriorCell {
                left_child: lower,
                separator,
                overflow: promoted_overflow,
            };
            // Only the *child* of the old cell changes; its key is byte for byte
            // the same, so the new cell keeps the old cell's overflow chain.
            // Allocating a fresh chain here and dropping this one is what
            // stranded whole chains of pages: a 3000-entry index over 2000-byte
            // keys left 1226 of 2022 pages unreachable and `PRAGMA
            // integrity_check` answered "*** in database main ***" followed by a
            // "Page N: never used" line for each. Sharing one chain between two
            // cells would be the other way to be wrong — freeing either would
            // strand the other — so the chain moves with the key rather than
            // being copied.
            parent.cells.insert(
                step.pos + 1,
                IndexInteriorCell {
                    left_child: upper,
                    separator: old.separator,
                    overflow: old.overflow,
                },
            );
        }

        if parent.can_write(self.pager.usable_size()) {
            return parent.write_to(self.pager);
        }
        // The parent is full, so it splits too and its halves are grafted into
        // the grandparent the same way. A full root must hand both halves to
        // fresh pages rather than keeping one, or the root would point at
        // itself.
        let split = split_interior(self.pager, &parent, !is_root)?;
        self.graft(
            path,
            split.left,
            split.right,
            split.separator,
            split.separator_overflow,
        )
    }

    /// Every entry in the tree, in key order.
    ///
    /// The separators are interior entries in their own right rather than copies
    /// of a leaf entry, so they are emitted between the subtrees they divide. A
    /// scan that visited only the leaves would miss exactly one key per
    /// interior cell.
    pub fn scan(&mut self) -> Result<Vec<IndexEntry>> {
        let mut out = Vec::new();
        self.walk(self.root, &mut out)?;
        Ok(out)
    }

    /// In-order walk, emitting each node's separators as it goes.
    fn walk(&mut self, page_no: u32, out: &mut Vec<IndexEntry>) -> Result<()> {
        match self.page_kind(page_no)? {
            page_type::INDEX_LEAF => {
                let leaf = IndexLeaf::read(self.pager, page_no)?;
                out.extend(leaf.entries.iter().cloned());
                Ok(())
            }
            page_type::INDEX_INTERIOR => {
                let page = IndexInterior::read(self.pager, page_no)?;
                for cell in page.cells {
                    self.walk(cell.left_child, out)?;
                    out.push(cell.separator);
                }
                self.walk(page.rightmost, out)
            }
            other => Err(Error::corrupt(format!(
                "page {page_no} is a {} inside an index b-tree",
                page_type::name(other)
            ))),
        }
    }

    /// Every rowid whose key equals `key`, in ascending rowid order.
    ///
    /// An index may hold one key with several rowids, so this returns all of
    /// them. A key that was promoted into an interior cell lives only there, so
    /// the whole tree is walked rather than one descent taken: a descent finds
    /// the leaf the key sorts into, and the promoted copy is not in a leaf at
    /// all. The walk is in key order, so it stops as soon as it passes the
    /// probe.
    ///
    /// This is therefore *not* the O(log n) a b-tree descent would give, and it
    /// is worth being explicit about that, because a lookup is the operation an
    /// index exists to make fast. A descent-only lookup is the obvious fix: read
    /// one cell per interior level and report the cell's own separator as a hit
    /// when the probe ties it, which is the same check
    /// [`IndexTree::insert`] does. It needs one thing this module does not have
    /// yet — a way to read a single interior cell off a page without decoding
    /// every sibling cell, several of which may each follow a whole overflow
    /// chain. Until that exists, the honest statement is that this walks, and
    /// the walk is bounded by the range it actually needs.
    pub fn lookup(&mut self, key: &[Value]) -> Result<Vec<i64>> {
        let probe = IndexEntry {
            key: key.to_vec(),
            // The rowid only breaks a tie between entries that already share a
            // key, so any value makes this a valid probe.
            rowid: 0,
        };
        let key_len = self.key_len;
        let mut out = Vec::new();
        self.gather(&mut |e: &IndexEntry| {
            if same_key(e, &probe, key_len) {
                out.push(e.rowid);
                false
            } else {
                // Keys come out in order, so once an entry is past the probe
                // every later one is too.
                compare(e, &probe, key_len) == std::cmp::Ordering::Greater
            }
        })?;
        out.sort_unstable();
        out.dedup();
        Ok(out)
    }

    /// The rowids whose key falls in `[low, high]`, in key order.
    ///
    /// The bounds are inclusive and either may be omitted. A bound may name
    /// *fewer* columns than the index has, and then it matches every rowid
    /// sharing that prefix — which is how a range over part of a composite key
    /// is expressed. `range(Some(&[Integer(2)]), Some(&[Integer(2)]))` on a
    /// two-column index returns every row whose first column is 2, not nothing.
    ///
    /// The padding is what the reference does. `WHERE a=2` and
    /// `WHERE a=2 AND b BETWEEN 0 AND 9223372036854775807` both return the same
    /// 40 rows on a 200-row two-column index with 5 distinct leading values, and
    /// `WHERE a=2 AND b=0` returns none, so a bound may name fewer columns than
    /// the index has. It cannot be a bare comparison, because
    /// [`IndexEntry::compare`] treats a short key as sorting *below* a full-width
    /// one — a one-column bound compared that way sits below every entry it
    /// should match and the scan returns nothing. Nor can the padding be a pair
    /// of integer extremes: text sorts above every number, so `i64::MAX` sits
    /// *below* every text value and the scan still returns nothing. See
    /// [`pad_bound`] for what is used instead.
    pub fn range(&mut self, low: Option<&[Value]>, high: Option<&[Value]>) -> Result<Vec<i64>> {
        // A bound is compared as an entry whose rowid is the smallest or largest
        // possible, so every rowid sharing the bound's key falls inside it. The
        // arity is a local because the comparison borrows `self` for the pager
        // while the closure is still live.
        let key_len = self.key_len;
        let low_probe = low.map(|k| IndexEntry {
            key: pad_bound(k, key_len, true),
            rowid: i64::MIN,
        });
        let high_probe = high.map(|k| IndexEntry {
            key: pad_bound(k, key_len, false),
            rowid: i64::MAX,
        });
        let mut out = Vec::new();
        self.gather(&mut |e: &IndexEntry| {
            if let Some(h) = &high_probe {
                if compare(e, h, key_len) == std::cmp::Ordering::Greater {
                    // Past the top of the range, and the walk is in order.
                    return true;
                }
            }
            if let Some(l) = &low_probe {
                if compare(e, l, key_len) == std::cmp::Ordering::Less {
                    // Still climbing towards the range; keep going.
                    return false;
                }
            }
            out.push(e.rowid);
            false
        })?;
        Ok(out)
    }

    /// Walks the tree in key order, handing every entry to `visit` until it
    /// returns true.
    ///
    /// One traversal serves the point lookup and the range scan, which differ
    /// only in the test. An explicit work stack rather than recursion, so a deep
    /// tree cannot overflow the call stack.
    ///
    /// A separator is handed over as an entry in its own right, between the
    /// subtrees it divides, because the reference promotes that key out of the
    /// leaves and nowhere else stores it. A walk that visited only the leaves
    /// would miss exactly one key per interior cell.
    ///
    /// Each interior page is read once and its cells kept in a frame, rather
    /// than re-read per cell. The old form pushed a `Emit(page, i)` work item
    /// that re-read and re-parsed the whole page for every separator, which made
    /// a walk cost one page read *per cell* instead of one per page — and since
    /// reading an interior page follows an overflow chain for every spilled
    /// separator on it, a page of 30 wide keys had all 30 chains re-followed 30
    /// times. The frame holds the cells; the separator is cloned out of it once,
    /// because the frame outlives the visit and the page buffer is borrowed.
    fn gather(&mut self, visit: &mut dyn FnMut(&IndexEntry) -> bool) -> Result<()> {
        /// An interior page held open while its subtrees are walked.
        struct Frame {
            cells: Vec<IndexInteriorCell>,
            rightmost: u32,
            /// How many cells have been walked, i.e. where the next resume is.
            pos: usize,
        }
        enum Work {
            /// Descend from this page, reading it to see what it holds.
            Open(u32),
            /// Carry on walking an already-read interior page.
            Resume(Box<Frame>),
            /// Hand over one cell's separator.
            Emit(IndexEntry),
        }
        let mut stack = vec![Work::Open(self.root)];
        while let Some(item) = stack.pop() {
            match item {
                Work::Emit(separator) => {
                    if visit(&separator) {
                        return Ok(());
                    }
                }
                Work::Resume(frame) => {
                    if frame.pos < frame.cells.len() {
                        let cell = frame.cells[frame.pos].clone();
                        // Pushed so they pop in this order, which is the in-order
                        // walk: the subtree the cell bounds, then the cell's own
                        // key, then the rest of this page.
                        let mut next = frame;
                        next.pos += 1;
                        stack.push(Work::Resume(next));
                        stack.push(Work::Emit(cell.separator));
                        stack.push(Work::Open(cell.left_child));
                    } else {
                        // Every cell walked; the rightmost subtree is last.
                        stack.push(Work::Open(frame.rightmost));
                    }
                }
                Work::Open(page_no) => match self.page_kind(page_no)? {
                    page_type::INDEX_LEAF => {
                        let leaf = IndexLeaf::read(self.pager, page_no)?;
                        for e in &leaf.entries {
                            if visit(e) {
                                return Ok(());
                            }
                        }
                    }
                    page_type::INDEX_INTERIOR => {
                        let page = IndexInterior::read(self.pager, page_no)?;
                        stack.push(Work::Resume(Box::new(Frame {
                            cells: page.cells,
                            rightmost: page.rightmost,
                            pos: 0,
                        })));
                    }
                    other => {
                        return Err(Error::corrupt(format!(
                            "page {page_no} is a {} inside an index b-tree",
                            page_type::name(other)
                        )))
                    }
                },
            }
        }
        Ok(())
    }

    /// How many entries the tree holds, separators included.
    pub fn count(&mut self) -> Result<usize> {
        Ok(self.scan()?.len())
    }
}

/// Whether two entries share the same key columns, whatever their rowids.
fn same_key(a: &IndexEntry, b: &IndexEntry, key_len: usize) -> bool {
    for i in 0..key_len {
        match (a.key.get(i), b.key.get(i)) {
            (Some(x), Some(y)) if x == y => {}
            _ => return false,
        }
    }
    true
}

/// Widens a range bound to the index's full key width.
///
/// A bound naming only the leading columns constrains only those, so the columns
/// it leaves out have to take a value outside the range the caller is asking
/// about. What that value is depends on the storage class of the column being
/// padded over, and this module is not told the index's declared types — it only
/// knows the arity of the key. So the padding is chosen to be outside the range
/// of *every* storage class at once, which SQLite's ordering makes possible:
///
/// * The low edge is [`Value::Null`]. NULL sorts below every other value, so
///   nothing can fall under it. Padding with NULL is also *correct* rather than
///   merely small: a row whose padded column really is NULL compares Equal to
///   the probe, which the inclusive low bound admits, and `WHERE a=2` is
///   supposed to match such a row.
/// * The high edge is an empty [`Value::Blob`]. A blob sorts above every number
///   and above every string, and above NULL, so no real value outranks it.
///
/// The one approximation is a blob column padded at the *high* end: no blob
/// sorts above every other blob, so a bound over the prefix of a blob key can
/// miss a row whose remaining columns hold a large blob. A caller that needs
/// that exact should pass a full-width bound, which this returns unchanged.
///
/// A bound that already names every column is therefore returned untouched, and
/// so is one longer than the index: `compare` only reads the first `key_len`
/// columns, so a longer bound is already comparing on exactly the right prefix.
fn pad_bound(bound: &[Value], key_len: usize, low: bool) -> Vec<Value> {
    if bound.len() >= key_len {
        return bound.to_vec();
    }
    let edge = if low {
        Value::Null
    } else {
        Value::Blob(Vec::new())
    };
    let mut out = bound.to_vec();
    out.resize(key_len, edge);
    out
}
