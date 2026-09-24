//! Reading table b-trees: walking cells on a leaf page, following overflow
//! chains, and reassembling payloads into rows.
//!
//! This is the read path. A writer that produces the same page layout belongs
//! here too, but for now the module is deliberately read-only so that what is
//! verified against real files is exactly what ships.

use super::error::{Error, Result};
use super::page::{local_payload_size, overflow_capacity_for, page_type, PageHeader};
use super::pager::Pager;
use super::record::{self, DecodedRecord};
use super::text::Encoding;
use super::value::Value;

/// One cell on a table leaf page, located but not yet decoded.
#[derive(Debug, Clone, Copy)]
pub struct LeafCell {
    pub rowid: i64,
    /// Offset of the cell within the page image.
    pub offset: u32,
    /// Total payload length, including any bytes that live in overflow pages.
    pub payload_len: u32,
    /// Bytes of the payload stored on the leaf itself.
    pub local_len: u32,
    /// First overflow page, or 0 when the payload fits locally.
    pub first_overflow: u32,
}

/// A rowid, i.e. the key of a table b-tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Rowid(pub i64);

/// A decoded row: its rowid and the column values of the record.
#[derive(Debug, Clone)]
pub struct Row {
    pub rowid: i64,
    pub values: Vec<Value>,
}

/// A table b-tree rooted at a page.
pub struct TableBtree<'a> {
    pager: &'a mut Pager,
    root: u32,
    encoding: Encoding,
}

impl<'a> TableBtree<'a> {
    pub fn new(pager: &'a mut Pager, root: u32) -> TableBtree<'a> {
        let encoding = pager.header().text_encoding;
        TableBtree {
            pager,
            root,
            encoding,
        }
    }

    pub fn root(&self) -> u32 {
        self.root
    }

    /// Collects every leaf page under the root, in key order.
    ///
    /// An interior table page stores (left child, key) cells plus a rightmost
    /// child, so a left-to-right walk of the tree visits rows in rowid order
    /// without any sorting.
    pub fn leaf_pages(&mut self) -> Result<Vec<u32>> {
        let mut out = Vec::new();
        let mut stack = vec![self.root];
        while let Some(n) = stack.pop() {
            let page = self.pager.read_page(n)?;
            let header_offset = if n == 1 { 100 } else { 0 };
            let h = PageHeader::parse(&page, header_offset)?;
            if h.is_leaf() {
                out.push(n);
            } else {
                // Push the rightmost child last so it is popped first and the
                // pages come out left to right.
                let children = self.children_of(&page, &h)?;
                for child in children.into_iter().rev() {
                    stack.push(child);
                }
            }
        }
        Ok(out)
    }

    /// The child page numbers of an interior page, left to right.
    fn children_of(&mut self, page: &[u8], h: &PageHeader) -> Result<Vec<u32>> {
        let mut out = Vec::with_capacity(h.cell_count as usize + 1);
        let array = h.cell_pointer_array_offset();
        for i in 0..h.cell_count as usize {
            let at = array + i * 2;
            let cell = u16::from_be_bytes([page[at], page[at + 1]]) as usize;
            if cell + 4 > page.len() {
                return Err(Error::corrupt("interior cell pointer is out of range"));
            }
            out.push(u32::from_be_bytes([
                page[cell],
                page[cell + 1],
                page[cell + 2],
                page[cell + 3],
            ]));
        }
        if let Some(right) = h.right_most {
            out.push(right);
        }
        Ok(out)
    }

    /// The cells on leaf page `n`, in key order.
    pub fn cells(&mut self, n: u32) -> Result<Vec<LeafCell>> {
        let page = self.pager.read_page(n)?;
        let usable = self.pager.usable_size();
        let header_offset = if n == 1 { 100 } else { 0 };
        let h = PageHeader::parse(&page, header_offset)?;
        if !h.is_leaf() {
            return Err(Error::corrupt(format!(
                "page {n} is a {} where a leaf was expected",
                page_type::name(h.page_type)
            )));
        }
        let pt = h.page_type;
        let mut out = Vec::with_capacity(h.cell_count as usize);
        let array = h.cell_pointer_array_offset();
        for i in 0..h.cell_count as usize {
            let at = array + i * 2;
            if at + 2 > page.len() {
                return Err(Error::corrupt("cell pointer array is out of range"));
            }
            let offset = u16::from_be_bytes([page[at], page[at + 1]]) as u32;
            if offset as usize >= page.len() {
                return Err(Error::corrupt("cell offset points outside the page"));
            }
            let cell = &page[offset as usize..];
            out.push(self.parse_leaf_cell(pt, cell, usable)?);
        }
        Ok(out)
    }

    /// Parses one leaf cell body, following the varint convention of
    /// `btreeParseCellPtr`.
    fn parse_leaf_cell(&self, pt: u8, cell: &[u8], usable: u32) -> Result<LeafCell> {
        if pt == page_type::INDEX_LEAF {
            return Err(Error::corrupt("index cells do not carry a rowid"));
        }
        let (payload_len, n) = super::varint::get_checked(cell)
            .map(|(v, n)| (v as u32, n))
            .ok_or_else(|| Error::corrupt("truncated payload length in cell"))?;
        let (rowid, m) = super::varint::get_checked(&cell[n..])
            .map(|(v, n)| (v as i64, n))
            .ok_or_else(|| Error::corrupt("truncated rowid in cell"))?;
        let body = &cell[n + m..];
        let local = local_payload_size(pt, payload_len, usable);
        if (local as usize) > body.len() {
            return Err(Error::corrupt("cell is shorter than its local payload"));
        }
        let first_overflow = if (local as usize) < body.len() {
            if body.len() < local as usize + 4 {
                return Err(Error::corrupt("overflow pointer is truncated"));
            }
            let at = local as usize;
            u32::from_be_bytes([body[at], body[at + 1], body[at + 2], body[at + 3]])
        } else {
            0
        };
        Ok(LeafCell {
            rowid,
            offset: 0,
            payload_len,
            local_len: local,
            first_overflow,
        })
    }

    /// Reassembles a cell's full payload, reading any overflow chain.
    pub fn payload(&mut self, cell: &LeafCell) -> Result<Vec<u8>> {
        if cell.first_overflow == 0 {
            let page = self.pager.read_page(1)?;
            let _ = page;
            return Err(Error::corrupt("payload length is unknown for a local cell"));
        }
        let mut out = Vec::with_capacity(cell.payload_len as usize);
        // The caller re-reads the local part through read_local, so only the
        // overflow chain is fetched here.
        let cap = overflow_capacity_for(self.pager.usable_size());
        let mut next = cell.first_overflow;
        let mut guard = 0u32;
        while (out.len() as u32) < cell.payload_len {
            if next == 0 {
                return Err(Error::corrupt("overflow chain ended early"));
            }
            guard += 1;
            if guard > cell.payload_len {
                return Err(Error::corrupt("overflow chain is cyclic"));
            }
            let page = self.pager.read_page(next)?;
            if page.len() < 4 {
                return Err(Error::corrupt("overflow page is truncated"));
            }
            let following = u32::from_be_bytes([page[0], page[1], page[2], page[3]]);
            let want = (cell.payload_len as usize - out.len()).min(cap as usize);
            if 4 + want > page.len() {
                return Err(Error::corrupt("overflow page is shorter than its capacity"));
            }
            out.extend_from_slice(&page[4..4 + want]);
            next = following;
        }
        Ok(out)
    }

    /// The full local-plus-overflow payload of a cell, starting from its page.
    fn full_payload(&mut self, page_no: u32, cell: &LeafCell) -> Result<Vec<u8>> {
        let page = self.pager.read_page(page_no)?;
        let header_offset = if page_no == 1 { 100 } else { 0 };
        let h = PageHeader::parse(&page, header_offset)?;
        let array = h.cell_pointer_array_offset();
        // Locate the cell by matching its rowid, since the caller holds only the
        // key rather than the byte offset.
        let mut body = None;
        for i in 0..h.cell_count as usize {
            let at = array + i * 2;
            let offset = u16::from_be_bytes([page[at], page[at + 1]]) as usize;
            let (_, n) = super::varint::get_checked(&page[offset..])
                .ok_or_else(|| Error::corrupt("truncated cell"))?;
            let (rowid, _) = super::varint::get_checked(&page[offset + n..])
                .ok_or_else(|| Error::corrupt("truncated rowid"))?;
            // A rowid is a signed 64-bit key, but the varint carries the raw
            // bit pattern, so a negative rowid arrives as its two's complement
            // reinterpreted as unsigned.
            let rowid = rowid as i64;
            if rowid == cell.rowid {
                let start = offset + n + super::varint::len_for(rowid as u64);
                body = Some(&page[start..start + cell.local_len as usize]);
                break;
            }
        }
        let local = body.ok_or_else(|| Error::corrupt("rowid not found on its leaf page"))?;
        let mut out = local.to_vec();
        if cell.first_overflow != 0 {
            out.extend_from_slice(&self.overflow_chain(cell)?);
        }
        Ok(out)
    }

    fn overflow_chain(&mut self, cell: &LeafCell) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        let cap = overflow_capacity_for(self.pager.usable_size());
        let mut next = cell.first_overflow;
        while (out.len() as u32) < cell.payload_len - cell.local_len {
            if next == 0 {
                return Err(Error::corrupt("overflow chain ended early"));
            }
            let page = self.pager.read_page(next)?;
            if page.len() < 4 {
                return Err(Error::corrupt("overflow page is truncated"));
            }
            let following = u32::from_be_bytes([page[0], page[1], page[2], page[3]]);
            let want = ((cell.payload_len - cell.local_len) as usize - out.len()).min(cap as usize);
            out.extend_from_slice(&page[4..4 + want]);
            next = following;
        }
        Ok(out)
    }

    /// Every row in the table, in rowid order.
    pub fn scan(&mut self) -> Result<Vec<Row>> {
        let mut rows = Vec::new();
        for page_no in self.leaf_pages()? {
            for cell in self.cells(page_no)? {
                let bytes = self.full_payload(page_no, &cell)?;
                let decoded = record::decode_record(&bytes, self.encoding)?;
                rows.push(Row {
                    rowid: cell.rowid,
                    values: decoded.values,
                });
            }
        }
        Ok(rows)
    }

    /// The rowid of the largest key in the tree, or 0 when it is empty.
    pub fn max_rowid(&mut self) -> Result<i64> {
        let mut max = 0;
        for page_no in self.leaf_pages()? {
            if let Some(last) = self.cells(page_no)?.last() {
                max = max.max(last.rowid);
            }
        }
        Ok(max)
    }

    /// The rowid of the smallest key in the tree, or 0 when it is empty.
    pub fn min_rowid(&mut self) -> Result<i64> {
        for page_no in self.leaf_pages()? {
            if let Some(first) = self.cells(page_no)?.first() {
                return Ok(first.rowid);
            }
        }
        Ok(0)
    }

    /// The row with the given rowid, or `None`.
    pub fn find(&mut self, rowid: i64) -> Result<Option<Row>> {
        for page_no in self.leaf_pages()? {
            for cell in self.cells(page_no)? {
                if cell.rowid != rowid {
                    continue;
                }
                let bytes = self.full_payload(page_no, &cell)?;
                let decoded = record::decode_record(&bytes, self.encoding)?;
                return Ok(Some(Row {
                    rowid,
                    values: decoded.values,
                }));
            }
        }
        Ok(None)
    }
}

/// Decodes a record, padding it out to `columns` values with NULLs.
///
/// SQLite stores trailing NULL columns implicitly, so a three-column row whose
/// last two values are NULL reads back as a one-value record; the caller knows
/// the declared width and restores it here.
pub fn pad_to(values: Vec<Value>, columns: usize) -> Vec<Value> {
    let mut v = values;
    v.resize(columns, Value::Null);
    v
}

/// A decoded record plus the width it should be read at.
pub type PaddedRecord = DecodedRecord;

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a single-page table b-tree by hand, so the reader is tested
    /// against bytes rather than against itself.
    fn build_leaf(pager: &mut Pager, page_no: u32, rows: &[(i64, Vec<Value>)]) {
        let usable = pager.usable_size();
        let pt = page_type::TABLE_LEAF;
        let header_offset = if page_no == 1 { 100 } else { 0 };
        let array = header_offset + PageHeader::SIZE_LEAF;

        // Encode the cells back-to-back from the end of the page.
        let mut cells: Vec<Vec<u8>> = Vec::new();
        let mut content_start = pager.page_size() as usize;
        for (rowid, values) in rows {
            let enc = record::encode(values);
            let mut cell = Vec::new();
            let mut tmp = [0u8; 9];
            let n = super::super::varint::put(&mut tmp, enc.bytes.len() as u64);
            cell.extend_from_slice(&tmp[..n]);
            let n = super::super::varint::put(&mut tmp, *rowid as u64);
            cell.extend_from_slice(&tmp[..n]);
            cell.extend_from_slice(&enc.bytes);
            assert!(cell.len() < usable as usize);
            content_start -= cell.len();
            cells.push(cell);
        }

        let pointers: Vec<u16> = cells
            .iter()
            .scan(content_start, |pos, cell| {
                let at = *pos;
                *pos += cell.len();
                Some(at as u16)
            })
            .collect();

        {
            let page = pager.page(page_no).unwrap();
            for b in page.iter_mut() {
                *b = 0;
            }
            let h = PageHeader {
                page_type: pt,
                first_freeblock: 0,
                cell_count: rows.len() as u16,
                cell_content_start: content_start as u16,
                fragmented_free: 0,
                right_most: None,
                header_offset: header_offset as u16,
            };
            h.write(page);
            for (i, at) in pointers.iter().enumerate() {
                let a = array + i * 2;
                page[a..a + 2].copy_from_slice(&at.to_be_bytes());
            }
            for (i, cell) in cells.iter().enumerate() {
                let at = pointers[i] as usize;
                page[at..at + cell.len()].copy_from_slice(cell);
            }
        }
        pager.mark_dirty(page_no);
    }

    fn temp(tag: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("nsqlite-btree-{}-{tag}.db", std::process::id()));
        let _ = std::fs::remove_file(&p);
        p
    }

    #[test]
    fn a_hand_built_leaf_reads_back() {
        let path = temp("leaf");
        let mut pager = Pager::open(&path).unwrap();
        pager.allocate().unwrap(); // page 1
        let root = pager.allocate().unwrap();
        build_leaf(
            &mut pager,
            root,
            &[
                (1, vec![Value::Integer(10), Value::Text("a".into())]),
                (2, vec![Value::Integer(20), Value::Text("b".into())]),
                (3, vec![Value::Integer(30), Value::Text("c".into())]),
            ],
        );
        pager.flush().unwrap();
        drop(pager);

        let mut pager = Pager::open(&path).unwrap();
        let mut bt = TableBtree::new(&mut pager, root);
        let rows = bt.scan().unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].rowid, 1);
        assert_eq!(rows[2].values[1], Value::Text("c".into()));
        assert_eq!(bt.min_rowid().unwrap(), 1);
        assert_eq!(bt.max_rowid().unwrap(), 3);
        assert!(bt.find(2).unwrap().is_some());
        assert!(bt.find(99).unwrap().is_none());
        drop(bt);
        drop(pager);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn an_empty_tree_yields_nothing() {
        let path = temp("empty");
        let mut pager = Pager::open(&path).unwrap();
        pager.allocate().unwrap();
        let root = pager.allocate().unwrap();
        build_leaf(&mut pager, root, &[]);
        pager.flush().unwrap();
        drop(pager);

        let mut pager = Pager::open(&path).unwrap();
        let mut bt = TableBtree::new(&mut pager, root);
        assert!(bt.scan().unwrap().is_empty());
        assert_eq!(bt.max_rowid().unwrap(), 0);
        assert_eq!(bt.min_rowid().unwrap(), 0);
        drop(bt);
        drop(pager);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_large_row_spills_into_overflow_pages() {
        let path = temp("overflow");
        let mut pager = Pager::open(&path).unwrap();
        pager.allocate().unwrap();
        let root = pager.allocate().unwrap();
        let usable = pager.usable_size();
        // Force a payload past the local limit, then lay out the chain.
        let big = "x".repeat(usable as usize * 2);
        let enc = record::encode(&[Value::Text(big.clone())]);
        let payload_len = enc.bytes.len() as u32;
        let local = crate::page::local_payload_size(page_type::TABLE_LEAF, payload_len, usable);
        assert!(payload_len > local, "the test payload must overflow");

        let chain_pages = (payload_len - local).div_ceil(overflow_capacity_for(usable));
        let mut page_nos = Vec::new();
        for _ in 0..chain_pages {
            page_nos.push(pager.allocate().unwrap());
        }

        // Fill the overflow chain, each page linking to the next.
        let cap = overflow_capacity_for(usable) as usize;
        let mut written = 0usize;
        for (i, &pn) in page_nos.iter().enumerate() {
            let next = page_nos.get(i + 1).copied().unwrap_or(0);
            let remaining = (payload_len - local) as usize - written;
            let take = remaining.min(cap);
            {
                let page = pager.page(pn).unwrap();
                for b in page.iter_mut() {
                    *b = 0;
                }
                page[0..4].copy_from_slice(&next.to_be_bytes());
                for (j, b) in enc.bytes[local as usize + written..local as usize + written + take]
                    .iter()
                    .enumerate()
                {
                    page[4 + j] = *b;
                }
            }
            pager.mark_dirty(pn);
            written += take;
        }
        assert_eq!(written, (payload_len - local) as usize);

        // Now the leaf cell itself.
        let header_offset = 0u16;
        let array = header_offset as usize + PageHeader::SIZE_LEAF;
        let mut cell = Vec::new();
        let mut tmp = [0u8; 9];
        let n = crate::varint::put(&mut tmp, payload_len as u64);
        cell.extend_from_slice(&tmp[..n]);
        let n = crate::varint::put(&mut tmp, 1);
        cell.extend_from_slice(&tmp[..n]);
        cell.extend_from_slice(&enc.bytes[..local as usize]);
        cell.extend_from_slice(&page_nos[0].to_be_bytes());

        let content_start = pager.page_size() as usize - cell.len();
        {
            let page = pager.page(root).unwrap();
            for b in page.iter_mut() {
                *b = 0;
            }
            let h = PageHeader {
                page_type: page_type::TABLE_LEAF,
                first_freeblock: 0,
                cell_count: 1,
                cell_content_start: content_start as u16,
                fragmented_free: 0,
                right_most: None,
                header_offset,
            };
            h.write(page);
            page[array..array + 2].copy_from_slice(&(content_start as u16).to_be_bytes());
            let at = content_start;
            page[at..at + cell.len()].copy_from_slice(&cell);
        }
        pager.mark_dirty(root);
        pager.flush().unwrap();
        drop(pager);

        let mut pager = Pager::open(&path).unwrap();
        let mut bt = TableBtree::new(&mut pager, root);
        let rows = bt.scan().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].values[0], Value::Text(big));
        drop(bt);
        drop(pager);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_page_one_root_is_read_through_the_file_header_offset() {
        let path = temp("page1root");
        let mut pager = Pager::open(&path).unwrap();
        let root = pager.allocate().unwrap();
        assert_eq!(root, 1);
        build_leaf(&mut pager, root, &[(7, vec![Value::Integer(7)])]);
        pager.flush().unwrap();
        drop(pager);

        let mut pager = Pager::open(&path).unwrap();
        let mut bt = TableBtree::new(&mut pager, 1);
        let rows = bt.scan().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].rowid, 7);
        drop(bt);
        drop(pager);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn padding_restores_implicit_trailing_nulls() {
        let vals = vec![Value::Integer(1)];
        assert_eq!(
            pad_to(vals, 3),
            vec![Value::Integer(1), Value::Null, Value::Null]
        );
    }
}
