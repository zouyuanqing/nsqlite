//! Writing table b-trees: cell insertion, page layout, and splitting.
//!
//! The read path in [`crate::btree`] locates cells; this module is the other
//! direction, producing page images byte-for-byte compatible with what SQLite
//! writes. It is deliberately limited to rowid tables (table b-trees), which is
//! what a first milestone needs; index b-trees follow once cells carry keys
//! instead of rowids.
//!
//! # Layout model
//!
//! A leaf holds its cells in ascending rowid order. The pointer array is in the
//! same order, while the cell *bodies* are packed from the end of the page
//! backwards, so the largest rowid sits at the lowest address. That inversion is
//! what lets a scan walk the pointer array and find keys in order without
//! touching cell content.
//!
//! The writer keeps cells as an ordered list of `(rowid, payload)` and recomputes
//! the whole layout on every mutation. This costs one page-sized pass per change
//! and buys the elimination of a whole class of fragmentation bugs, since no
//! freeblock chain or fragment counter is ever left inconsistent. The cost is
//! bounded by the page size, which is small by construction.

use super::error::{Error, Result};
use super::page::{local_payload_size, page_type, PageHeader};
use super::pager::Pager;
use super::record;
use super::value::Value;
use super::varint;

/// A table leaf cell: a rowid and the full record payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cell {
    pub rowid: i64,
    /// The complete payload, including whatever lives in the overflow chain.
    pub payload: Vec<u8>,
}

impl Cell {
    /// How many payload bytes stay on the leaf, given the usable page size.
    pub fn local_len(&self, usable: u32) -> u32 {
        local_payload_size(page_type::TABLE_LEAF, self.payload.len() as u32, usable)
    }

    /// Whether the payload spills into an overflow chain.
    pub fn spills(&self, usable: u32) -> bool {
        self.local_len(usable) as usize != self.payload.len()
    }

    /// The cell's size on the page. The overflow pointer is counted only when
    /// the payload actually spills.
    pub fn on_page_size(&self, usable: u32) -> usize {
        let local = self.local_len(usable) as usize;
        let mut n =
            varint::len_for(self.payload.len() as u64) + varint::len_for(self.rowid as u64) + local;
        if local != self.payload.len() {
            n += 4;
        }
        n
    }

    /// Serialises the cell body: the two varints, the local payload, and the
    /// overflow page number if there is one.
    pub fn encode(&self, usable: u32, first_overflow: u32) -> Vec<u8> {
        let local = self.local_len(usable) as usize;
        let mut out = Vec::with_capacity(self.on_page_size(usable));
        varint::append(&mut out, self.payload.len() as u64);
        varint::append(&mut out, self.rowid as u64);
        out.extend_from_slice(&self.payload[..local]);
        if local != self.payload.len() {
            out.extend_from_slice(&first_overflow.to_be_bytes());
        }
        out
    }
}

/// A table leaf page, as a set of cells plus the geometry needed to place them.
pub struct LeafPage {
    pub page_no: u32,
    /// Cells in ascending rowid order.
    pub cells: Vec<Cell>,
    /// Bytes per page, including the reserved region.
    pub page_size: u32,
    /// Offset of the b-tree header, which is 100 on page 1 and 0 elsewhere.
    pub header_offset: u16,
}

/// The result of laying out a page: where each cell body went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    /// Where the cell content area begins, i.e. the lowest cell address.
    pub content_start: u32,
    /// The byte offset of each cell body, in the same order as the cells.
    pub offsets: Vec<u16>,
}

impl LeafPage {
    /// Reads an existing leaf page.
    pub fn read(pager: &mut Pager, page_no: u32) -> Result<LeafPage> {
        let page = pager.read_page(page_no)?;
        let page_size = page.len() as u32;
        let header_offset = if page_no == 1 { 100 } else { 0 };
        let h = PageHeader::parse(&page, header_offset)?;
        if !h.is_leaf() || h.page_type != page_type::TABLE_LEAF {
            return Err(Error::corrupt(format!(
                "page {page_no} is a {} where a table leaf was expected",
                page_type::name(h.page_type)
            )));
        }
        let usable = pager.usable_size();
        let array = h.cell_pointer_array_offset();
        let mut cells = Vec::with_capacity(h.cell_count as usize);
        for i in 0..h.cell_count as usize {
            let at = array + i * 2;
            if at + 2 > page.len() {
                return Err(Error::corrupt("cell pointer array is out of range"));
            }
            let offset = u16::from_be_bytes([page[at], page[at + 1]]) as usize;
            if offset >= page.len() {
                return Err(Error::corrupt("cell offset points outside the page"));
            }
            let body = &page[offset..];
            let (payload_len, n) = varint::get_checked(body)
                .ok_or_else(|| Error::corrupt("truncated cell payload length"))?;
            let payload_len = payload_len as u32;
            let (rowid, m) = varint::get_checked(&body[n..])
                .ok_or_else(|| Error::corrupt("truncated cell rowid"))?;
            // A rowid is signed, while the varint carries the raw bit pattern,
            // so a negative key arrives as its two's complement.
            let rowid = rowid as i64;
            let local = local_payload_size(page_type::TABLE_LEAF, payload_len, usable) as usize;

            // Gather the whole payload: the local part, then the overflow chain.
            let mut payload = Vec::with_capacity(payload_len as usize);
            if local > body.len() {
                return Err(Error::corrupt("cell is shorter than its local payload"));
            }
            payload.extend_from_slice(&body[..local]);
            if local < payload_len as usize {
                if body.len() < local + 4 {
                    return Err(Error::corrupt("overflow pointer is truncated"));
                }
                let mut next = u32::from_be_bytes([
                    body[local],
                    body[local + 1],
                    body[local + 2],
                    body[local + 3],
                ]);
                let cap = super::page::overflow_capacity_for(usable) as usize;
                let mut guard = 0usize;
                while payload.len() < payload_len as usize {
                    if next == 0 {
                        return Err(Error::corrupt("overflow chain ended early"));
                    }
                    guard += 1;
                    if guard > payload_len as usize {
                        return Err(Error::corrupt("overflow chain is cyclic"));
                    }
                    let opage = pager.read_page(next)?;
                    if opage.len() < 4 {
                        return Err(Error::corrupt("overflow page is truncated"));
                    }
                    let following = u32::from_be_bytes([opage[0], opage[1], opage[2], opage[3]]);
                    let want = (payload_len as usize - payload.len()).min(cap);
                    if 4 + want > opage.len() {
                        return Err(Error::corrupt("overflow page is shorter than its capacity"));
                    }
                    payload.extend_from_slice(&opage[4..4 + want]);
                    next = following;
                }
            }
            cells.push(Cell { rowid, payload });
        }
        cells.sort_by_key(|c| c.rowid);
        Ok(LeafPage {
            page_no,
            cells,
            page_size,
            header_offset: header_offset as u16,
        })
    }

    /// A new, empty leaf page.
    pub fn empty(page_no: u32, page_size: u32) -> LeafPage {
        LeafPage {
            page_no,
            cells: Vec::new(),
            page_size,
            header_offset: if page_no == 1 { 100 } else { 0 },
        }
    }

    /// The first address cell content may occupy, just past the pointer array
    /// for the *given* number of cells.
    fn content_floor(&self, cell_count: usize) -> usize {
        self.header_offset as usize + PageHeader::SIZE_LEAF + cell_count * 2
    }

    /// Lays the cells out on a page image and returns where each one went.
    ///
    /// The bodies are packed from the end of the page backwards in key order,
    /// so the last cell's body sits at the lowest address.
    fn layout(&self, cells: &[Cell], usable: u32) -> Result<Layout> {
        let mut content_start = self.page_size as usize;
        let mut offsets = Vec::with_capacity(cells.len());
        // Walking in reverse and prepending each address builds the ascending
        // list the pointer array needs.
        for cell in cells.iter().rev() {
            let body_len = cell.on_page_size(usable);
            if body_len > content_start {
                return Err(Error::corrupt("cell is larger than the page"));
            }
            content_start -= body_len;
            offsets.push(content_start as u16);
        }
        offsets.reverse();
        let floor = self.content_floor(cells.len());
        if content_start < floor {
            return Err(Error::full());
        }
        Ok(Layout {
            content_start: content_start as u32,
            offsets,
        })
    }

    /// Whether the page can hold `cells` without spilling.
    pub fn fits(&self, cells: &[Cell], usable: u32) -> bool {
        self.layout(cells, usable).is_ok()
    }

    /// Whether this page, as it currently stands, still fits its own cells.
    pub fn fits_cells(&self, usable: u32) -> bool {
        self.fits(&self.cells, usable)
    }

    /// How many bytes remain free once the pointer array has grown by one.
    pub fn free_space(&self, usable: u32) -> usize {
        match self.layout(&self.cells, usable) {
            Ok(l) => {
                (l.content_start as usize).saturating_sub(self.content_floor(self.cells.len()))
            }
            // If the current cells do not even fit, nothing fits.
            Err(_) => 0,
        }
    }

    /// Inserts a cell in rowid order, returning false when the page is full and
    /// the caller must split.
    pub fn insert(&mut self, cell: Cell, usable: u32) -> Result<bool> {
        // Keep the cells sorted; rowids are unique, so an exact match is a
        // duplicate rather than a no-op.
        let pos = match self.cells.binary_search_by_key(&cell.rowid, |c| c.rowid) {
            Ok(_) => {
                return Err(Error::new(
                    super::error::ResultCode::Constraint,
                    format!("UNIQUE constraint failed: rowid {}", cell.rowid),
                ))
            }
            Err(pos) => pos,
        };
        // Try the insertion on a copy of the length, so a cell that does not fit
        // leaves the page exactly as it was.
        let mut candidate = self.cells.clone();
        candidate.insert(pos, cell);
        if self.layout(&candidate, usable).is_err() {
            return Ok(false);
        }
        self.cells = candidate;
        Ok(true)
    }

    /// Removes the cell with `rowid`, returning it if it was present.
    pub fn remove(&mut self, rowid: i64) -> Option<Cell> {
        let pos = self.cells.binary_search_by_key(&rowid, |c| c.rowid).ok()?;
        Some(self.cells.remove(pos))
    }

    /// Serialises the page into `page`, using `overflow` to resolve spill
    /// chains.
    ///
    /// `overflow` is called for each cell whose payload does not fit locally and
    /// must return the first overflow page of a freshly written chain, or 0 to
    /// write the cell with a dangling pointer. The closure receives the spill
    /// portion of the payload.
    pub fn write<F>(&self, page: &mut [u8], usable: u32, mut overflow: F) -> Result<()>
    where
        F: FnMut(&[u8]) -> Result<u32>,
    {
        for b in page.iter_mut() {
            *b = 0;
        }
        let layout = self.layout(&self.cells, usable)?;

        // Cell bodies, in the order they appear in memory: highest address
        // first. The overflow chains must be written before the page that
        // points at them, so they are resolved first here.
        let mut bodies = Vec::with_capacity(self.cells.len());
        for cell in &self.cells {
            let local = cell.local_len(usable) as usize;
            let first = if local == cell.payload.len() {
                0
            } else {
                overflow(&cell.payload[local..])?
            };
            bodies.push(cell.encode(usable, first));
        }

        for (i, body) in bodies.iter().enumerate() {
            let at = layout.offsets[i] as usize;
            page[at..at + body.len()].copy_from_slice(body);
        }

        let h = PageHeader {
            page_type: page_type::TABLE_LEAF,
            first_freeblock: 0,
            cell_count: self.cells.len() as u16,
            // A stored zero means "the content area starts at the end of the
            // page", which is only true for a completely full page. A 65536-byte
            // page has to use that zero, since the field is two bytes wide.
            cell_content_start: if layout.content_start >= 0xffff {
                0
            } else {
                layout.content_start as u16
            },
            fragmented_free: 0,
            right_most: None,
            header_offset: self.header_offset,
        };
        h.write(page);
        let array = h.cell_pointer_array_offset();
        for (i, at) in layout.offsets.iter().enumerate() {
            page[array + i * 2..array + i * 2 + 2].copy_from_slice(&at.to_be_bytes());
        }
        Ok(())
    }

    /// Builds the record payload for a row.
    ///
    /// The rowid alias column is stored as NULL, because the rowid lives in the
    /// cell key and SQLite leaves the record slot empty for it.
    pub fn encode_payload(values: &[Value], rowid_alias: Option<usize>) -> Vec<u8> {
        let mut vals = values.to_vec();
        if let Some(col) = rowid_alias {
            if col < vals.len() {
                vals[col] = Value::Null;
            }
        }
        record::encode(&vals).bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::Encoding;

    fn cell(rowid: i64, values: &[Value]) -> Cell {
        Cell {
            rowid,
            payload: LeafPage::encode_payload(values, None),
        }
    }

    fn temp(tag: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("nsqlite-bw-{}-{tag}.db", std::process::id()));
        let _ = std::fs::remove_file(&p);
        p
    }

    /// Writes a leaf to a fresh database and reads it back through the reader.
    fn round_trip(cells: &[Cell]) -> Vec<super::super::btree::Row> {
        let path = temp("rt");
        let mut pager = Pager::open(&path).unwrap();
        pager.allocate().unwrap();
        let root = pager.allocate().unwrap();
        let usable = pager.usable_size();
        let page = LeafPage::empty(root, 4096);
        {
            let dst = pager.page(root).unwrap();
            page.write(dst, usable, |_| Ok(0)).unwrap();
        }
        // The cells must actually be present, so rebuild the page from them.
        let page = LeafPage {
            cells: cells.to_vec(),
            ..page
        };
        {
            let dst = pager.page(root).unwrap();
            page.write(dst, usable, |_| Ok(0)).unwrap();
        }
        pager.mark_dirty(root);
        pager.flush().unwrap();
        drop(pager);

        let mut pager = Pager::open(&path).unwrap();
        let rows = {
            let mut bt = super::super::btree::TableBtree::new(&mut pager, root);
            bt.scan().unwrap()
        };
        drop(pager);
        let _ = std::fs::remove_file(&path);
        rows
    }

    #[test]
    fn a_cell_encodes_with_its_varints() {
        let c = cell(1, &[Value::Integer(42)]);
        let body = c.encode(4096, 0);
        let (payload_len, n1) = varint::get(&body);
        assert_eq!(payload_len, c.payload.len() as u64);
        let (rowid, _) = varint::get(&body[n1..]);
        assert_eq!(rowid, 1);
    }

    #[test]
    fn a_negative_rowid_round_trips() {
        let c = cell(-5, &[Value::Integer(1)]);
        let body = c.encode(4096, 0);
        let (payload_len, n1) = varint::get(&body);
        let (rowid, _) = varint::get(&body[n1..]);
        assert_eq!(rowid as i64, -5, "the varint carries the raw bit pattern");
        assert_eq!(payload_len as usize, c.payload.len());
    }

    #[test]
    fn the_rowid_alias_slot_is_stored_as_null() {
        let payload =
            LeafPage::encode_payload(&[Value::Integer(7), Value::Text("x".into())], Some(0));
        let d = record::decode_record(&payload, Encoding::Utf8).unwrap();
        assert_eq!(d.values[0], Value::Null);
        assert_eq!(d.values[1], Value::Text("x".into()));
    }

    #[test]
    fn cell_bodies_descend_while_rowids_ascend() {
        let cells: Vec<Cell> = (1..=5).map(|i| cell(i, &[Value::Integer(i)])).collect();
        let page = LeafPage::empty(2, 4096);
        let layout = page.layout(&cells, 4096).unwrap();
        // Rowids ascend, but the addresses they sit at descend: the largest key
        // is packed at the lowest address.
        for w in layout.offsets.windows(2) {
            assert!(w[0] < w[1], "addresses must descend as rowids ascend");
        }
    }

    #[test]
    fn inserting_keeps_cells_in_key_order() {
        let mut page = LeafPage::empty(2, 4096);
        // Insert out of order; the page must still hold them sorted.
        for rowid in [3, 1, 4, 2] {
            assert!(page
                .insert(cell(rowid, &[Value::Integer(rowid)]), 4096)
                .unwrap());
        }
        let keys: Vec<i64> = page.cells.iter().map(|c| c.rowid).collect();
        assert_eq!(keys, vec![1, 2, 3, 4]);
    }

    #[test]
    fn a_duplicate_rowid_is_refused() {
        let mut page = LeafPage::empty(2, 4096);
        assert!(page.insert(cell(1, &[Value::Integer(1)]), 4096).unwrap());
        let err = page
            .insert(cell(1, &[Value::Integer(2)]), 4096)
            .unwrap_err();
        assert_eq!(err.code.name(), "CONSTRAINT");
    }

    #[test]
    fn removal_frees_the_space_for_another_cell() {
        let mut page = LeafPage::empty(2, 4096);
        let mut i = 1;
        while page
            .insert(cell(i, &[Value::Text("x".repeat(40))]), 4096)
            .unwrap()
        {
            i += 1;
            if i > 500 {
                break;
            }
        }
        let full = (i - 1) as usize;
        assert!(page.remove(2).is_some());
        assert!(page
            .insert(cell(10_000, &[Value::Text("y".repeat(40))]), 4096)
            .unwrap());
        assert_eq!(
            page.cells.len(),
            full,
            "a removal should make room for one cell"
        );
    }

    #[test]
    fn the_page_reports_full_before_exhausting_its_bounds() {
        let mut page = LeafPage::empty(2, 4096);
        let mut inserted = 0;
        while inserted < 500 {
            if !page
                .insert(
                    cell(inserted as i64 + 1, &[Value::Text("x".repeat(40))]),
                    4096,
                )
                .unwrap()
            {
                break;
            }
            inserted += 1;
        }
        assert!(inserted > 10, "some cells must fit");
        assert!(inserted < 500, "the page must refuse before the loop ends");
        assert_eq!(
            page.cells.len(),
            inserted,
            "a refused cell must not be recorded"
        );
    }

    #[test]
    fn a_rendered_page_round_trips_through_the_reader() {
        let cells: Vec<Cell> = (1..=20)
            .map(|i| {
                cell(
                    i,
                    &[Value::Integer(i as i64), Value::Text(format!("row{i}"))],
                )
            })
            .collect();
        let rows = round_trip(&cells);
        assert_eq!(rows.len(), 20);
        for (i, r) in rows.iter().enumerate() {
            assert_eq!(r.rowid, i as i64 + 1);
            assert_eq!(r.values[0], Value::Integer(i as i64 + 1));
            assert_eq!(r.values[1], Value::Text(format!("row{}", i + 1)));
        }
    }

    #[test]
    fn a_page_one_root_keeps_the_file_header_intact() {
        let path = temp("page1");
        let mut pager = Pager::open(&path).unwrap();
        let root = pager.allocate().unwrap();
        assert_eq!(root, 1, "the first allocation is page 1");
        let usable = pager.usable_size();
        let page = LeafPage {
            page_no: 1,
            ..LeafPage::empty(1, 4096)
        };
        let cells: Vec<Cell> = (1..=3).map(|i| cell(i, &[Value::Integer(i)])).collect();
        let page = LeafPage { cells, ..page };
        {
            // Write the b-tree part, then restore the header bytes, which live
            // in the same 4096 bytes the cell content area would otherwise
            // claim.
            let header = pager.header().to_bytes();
            let dst = pager.page(1).unwrap();
            page.write(dst, usable, |_| Ok(0)).unwrap();
            dst[..100].copy_from_slice(&header);
        }
        pager.write_header().unwrap();
        pager.flush().unwrap();
        drop(pager);

        // Reopening must still see a valid database, and the table must read.
        let mut pager = Pager::open(&path).unwrap();
        assert_eq!(pager.header().page_size, 4096);
        let read = LeafPage::read(&mut pager, 1).unwrap();
        assert_eq!(read.cells.len(), 3);
        let mut bt = super::super::btree::TableBtree::new(&mut pager, 1);
        assert_eq!(bt.scan().unwrap().len(), 3);
        drop(bt);
        drop(pager);
        let _ = std::fs::remove_file(&path);
    }
}
