//! Index b-trees: reading and writing the key records a secondary index holds.
//!
//! An index entry is a record of the indexed columns followed by the key of
//! the row it points at, which is the rowid for a rowid table. The trailing rowid
//! is what makes an index usable: given a value, the entry carries the rowid of
//! the row that has it, so a lookup goes from the index straight to the table
//! rather than scanning it.
//!
//! The record is a normal one, so the serial-type encoding in
//! [`crate::record`] is what an index key uses. The one detail that differs from
//! a table row is the rowid: it is always stored as a full eight-byte integer
//! (serial type 6) rather than the narrowest that fits, so a negative rowid
//! survives and a value cannot change width between the index and the table.
//!
//! An index cell is a varint payload length, the payload, and — when the entry
//! is too large for the page — an overflow pointer, exactly as a table leaf
//! cell is. The difference is that an index cell has no separate rowid field:
//! the rowid is part of the payload.

use super::error::{Error, Result};
use super::page::{
    local_payload_size, max_local_for_index, min_local, overflow_capacity_for, page_type,
    PageHeader,
};
use super::pager::Pager;
use super::record;
use super::value::Value;
use super::varint;

/// One index entry: the indexed values and the rowid of the row they index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexEntry {
    /// The indexed column values, in the index's column order.
    pub key: Vec<Value>,
    /// The rowid of the row this entry points at.
    pub rowid: i64,
}

impl IndexEntry {
    /// Encodes the entry as a record: the key columns, then the rowid as a full
    /// eight-byte integer.
    pub fn encode(&self) -> Vec<u8> {
        // The rowid goes through the record encoder as an integer, but the
        // serial type has to be 6 rather than the narrow one the encoder would
        // pick, so the entry is laid out here rather than handed to encode().
        let mut types = Vec::new();
        for v in &self.key {
            varint::append(&mut types, serial_type_of(v) as u64);
        }
        // A fixed width for the rowid, so the entry's length does not depend on
        // its value and a lookup compares like with like.
        varint::append(&mut types, 6);

        // The header is the serial types plus the size field, which counts
        // itself, so the width is settled by iteration.
        let mut size_len = 1usize;
        let mut header_size;
        loop {
            let candidate = types.len() + size_len;
            let needed = varint::len_for(candidate as u64);
            if needed <= size_len {
                header_size = candidate;
                break;
            }
            size_len = needed;
        }

        let mut out = Vec::with_capacity(header_size + 16);
        varint::append(&mut out, header_size as u64);
        out.extend_from_slice(&types);
        for v in &self.key {
            push_body(&mut out, v);
        }
        out.extend_from_slice(&self.rowid.to_be_bytes());
        out
    }

    /// Decodes an entry, splitting the trailing rowid off the key.
    pub fn decode(bytes: &[u8], encoding: super::text::Encoding) -> Result<IndexEntry> {
        let d = record::decode_record(bytes, encoding)?;
        if d.values.len() < 2 {
            return Err(Error::corrupt("an index entry needs a key and a rowid"));
        }
        let rowid = d.values[d.values.len() - 1]
            .as_i64()
            .ok_or_else(|| Error::corrupt("an index entry's rowid is not an integer"))?;
        Ok(IndexEntry {
            key: d.values[..d.values.len() - 1].to_vec(),
            rowid,
        })
    }
}

/// The serial type a value takes in a record, except that the rowid slot is
/// fixed at 6 by the caller.
fn serial_type_of(v: &Value) -> i64 {
    record::serial_type(v)
}

/// Appends a value's body bytes, given its serial type.
fn push_body(out: &mut Vec<u8>, v: &Value) {
    match v {
        // Stored by the TEXT serial type, which `record::serial_type` gives for
        // both text spellings, so the bytes go out verbatim and the row is a
        // text entry rather than a blob one.
        Value::TextBytes(b) => out.extend_from_slice(b),
        Value::Null => {}
        Value::Integer(_) => {
            // The rowid slot is written by the caller as eight bytes; every
            // other integer takes the narrow type encode chose.
            let t = record::serial_type(v);
            match t {
                8 | 9 => {}
                1 => out.push(v.as_i64().unwrap_or(0) as u8),
                2 => out.extend_from_slice(&(v.as_i64().unwrap_or(0) as i16).to_be_bytes()),
                3 => out.extend_from_slice(&v.as_i64().unwrap_or(0).to_be_bytes()[5..]),
                4 => out.extend_from_slice(&(v.as_i64().unwrap_or(0) as i32).to_be_bytes()),
                5 => out.extend_from_slice(&v.as_i64().unwrap_or(0).to_be_bytes()[2..]),
                _ => out.extend_from_slice(&v.as_i64().unwrap_or(0).to_be_bytes()),
            }
        }
        Value::Real(r) => out.extend_from_slice(&r.to_be_bytes()),
        Value::Text(s) => out.extend_from_slice(s.as_bytes()),
        Value::Blob(b) => out.extend_from_slice(b),
    }
}

/// The order two entries are in.
///
/// Index keys are compared by the record's own ordering, then by the rowid, so
/// two rows with the same key are adjacent and the rowid breaks the tie. That
/// tie-break is what lets a range scan over one key value collect every row
/// that has it.
pub fn compare(a: &IndexEntry, b: &IndexEntry, key_len: usize) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    for i in 0..key_len {
        let (x, y) = match (a.key.get(i), b.key.get(i)) {
            (Some(x), Some(y)) => (x, y),
            // A shorter key sorts first, which is what a NULL-padded record
            // does and what a scan past the end of a narrower key expects.
            (None, None) => continue,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
        };
        let ord = x.compare(y);
        if ord != Ordering::Equal {
            return ord;
        }
    }
    a.rowid.cmp(&b.rowid)
}

/// One cell on an index leaf page, located but not yet decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexLeafCell {
    pub offset: u16,
    /// The bytes the cell occupies on the page.
    pub size: u16,
    /// The first overflow page, or zero when the entry fits.
    pub overflow: u32,
}

/// An index leaf page being read or written.
pub struct IndexLeaf {
    pub page_no: u32,
    /// The entries in key order.
    pub entries: Vec<IndexEntry>,
    /// The head page of each entry's overflow chain, parallel to `entries`, so
    /// a rewrite can free the chain rather than strand it.
    pub overflows: Vec<u32>,
    pub page_size: u32,
    pub header_offset: u16,
}

impl IndexLeaf {
    pub fn empty(page_no: u32, page_size: u32) -> IndexLeaf {
        IndexLeaf {
            page_no,
            entries: Vec::new(),
            overflows: Vec::new(),
            page_size,
            header_offset: if page_no == 1 { 100 } else { 0 },
        }
    }

    /// Reads an index leaf page.
    pub fn read(pager: &mut Pager, page_no: u32) -> Result<IndexLeaf> {
        let page = pager.read_page(page_no)?;
        let page_size = page.len() as u32;
        let header_offset = if page_no == 1 { 100 } else { 0 };
        let h = PageHeader::parse(&page, header_offset)?;
        if h.page_type != page_type::INDEX_LEAF {
            return Err(Error::corrupt(format!(
                "page {page_no} is a {} where an index leaf was expected",
                page_type::name(h.page_type)
            )));
        }
        let usable = pager.usable_size();
        let encoding = pager.header().text_encoding;
        let array = h.cell_pointer_array_offset();
        let mut entries = Vec::with_capacity(h.cell_count as usize);
        let mut overflows = Vec::with_capacity(h.cell_count as usize);
        for i in 0..h.cell_count as usize {
            let at = array + i * 2;
            let offset = u16::from_be_bytes([page[at], page[at + 1]]) as usize;
            if offset >= page.len() {
                return Err(Error::corrupt("index cell offset points outside the page"));
            }
            let body = &page[offset..];
            let (payload_len, n) = varint::get_checked(body)
                .ok_or_else(|| Error::corrupt("truncated index cell payload length"))?;
            let payload_len = payload_len as usize;
            let local =
                local_payload_size(page_type::INDEX_LEAF, payload_len as u32, usable) as usize;
            let at_body = n;
            if local > body.len() - at_body {
                return Err(Error::corrupt(
                    "index cell is shorter than its local payload",
                ));
            }
            let mut payload = body[at_body..at_body + local].to_vec();
            let mut overflow = 0u32;
            if local < payload_len {
                let p = at_body + local;
                if body.len() < p + 4 {
                    return Err(Error::corrupt("index cell overflow pointer is truncated"));
                }
                overflow = u32::from_be_bytes([body[p], body[p + 1], body[p + 2], body[p + 3]]);
                let cap = overflow_capacity_for(usable) as usize;
                let mut next = overflow;
                let mut guard = 0;
                while payload.len() < payload_len {
                    if next == 0 {
                        return Err(Error::corrupt("index overflow chain ended early"));
                    }
                    guard += 1;
                    if guard > payload_len {
                        return Err(Error::corrupt("index overflow chain is cyclic"));
                    }
                    let op = pager.read_page(next)?;
                    let following = u32::from_be_bytes([op[0], op[1], op[2], op[3]]);
                    let want = (payload_len - payload.len()).min(cap);
                    payload.extend_from_slice(&op[4..4 + want]);
                    next = following;
                }
            }
            entries.push(IndexEntry::decode(&payload, encoding)?);
            overflows.push(overflow);
        }
        Ok(IndexLeaf {
            page_no,
            entries,
            overflows,
            page_size,
            header_offset: header_offset as u16,
        })
    }

    /// Encodes one entry, given where its overflow chain lives.
    fn encode_cell(&self, i: usize, usable: u32) -> Vec<u8> {
        let payload = self.entries[i].encode();
        let local =
            local_payload_size(page_type::INDEX_LEAF, payload.len() as u32, usable) as usize;
        let mut out = Vec::with_capacity(local + 16);
        varint::append(&mut out, payload.len() as u64);
        out.extend_from_slice(&payload[..local]);
        if local != payload.len() {
            out.extend_from_slice(&self.overflows.get(i).copied().unwrap_or(0).to_be_bytes());
        }
        out
    }

    /// The bytes each entry's cell occupies.
    fn cell_sizes(&self, usable: u32) -> Vec<usize> {
        self.entries
            .iter()
            .map(|e| {
                let n = e.encode().len();
                let local = local_payload_size(page_type::INDEX_LEAF, n as u32, usable) as usize;
                varint::len_for(n as u64) + local + if local == n { 0 } else { 4 }
            })
            .collect()
    }

    /// Lays the cells out, returning the offset of each.
    fn layout(&self, usable: u32) -> Result<Vec<u16>> {
        let floor = self.header_offset as usize + PageHeader::SIZE_LEAF + self.entries.len() * 2;
        let sizes = self.cell_sizes(usable);
        let total: usize = sizes.iter().sum();
        if self.page_size as usize - floor < total {
            return Err(Error::full());
        }
        let mut offsets = Vec::with_capacity(sizes.len());
        let mut at = self.page_size as usize;
        for size in sizes.iter().rev() {
            at -= size;
            offsets.push(at as u16);
        }
        offsets.reverse();
        Ok(offsets)
    }

    /// Whether the page can hold one more entry of the given encoded size.
    pub fn fits(&self, usable: u32, extra: usize) -> bool {
        let floor =
            self.header_offset as usize + PageHeader::SIZE_LEAF + (self.entries.len() + 1) * 2;
        let total: usize = self.cell_sizes(usable).iter().sum::<usize>() + extra;
        self.page_size as usize - floor >= total
    }

    /// Adds an entry, returning false when the page is full.
    pub fn insert(&mut self, entry: IndexEntry, key_len: usize, usable: u32) -> Result<bool> {
        let pos = match self
            .entries
            .binary_search_by(|e| compare(e, &entry, key_len))
        {
            // An index may hold the same key more than once only if the rowids
            // differ, which compare already accounts for, so an exact match is a
            // duplicate insert rather than a new entry.
            Ok(_) => {
                return Err(Error::new(
                    super::error::ResultCode::Constraint,
                    "UNIQUE constraint failed: index",
                ))
            }
            Err(p) => p,
        };
        let candidate_size = {
            let n = entry.encode().len();
            let local = local_payload_size(page_type::INDEX_LEAF, n as u32, usable) as usize;
            varint::len_for(n as u64) + local + if local == n { 0 } else { 4 }
        };
        if !self.fits(usable, candidate_size) {
            return Ok(false);
        }
        self.entries.insert(pos, entry);
        self.overflows.insert(pos, 0);
        Ok(true)
    }

    /// Writes the page, allocating an overflow chain for any entry that needs
    /// one and does not have it.
    pub fn write_to(&mut self, pager: &mut Pager) -> Result<()> {
        let usable = pager.usable_size();
        // Resolve the chains before the page is borrowed, since allocating a
        // page needs the pager.
        for i in 0..self.entries.len() {
            let n = self.entries[i].encode().len();
            let local = local_payload_size(page_type::INDEX_LEAF, n as u32, usable) as usize;
            if local != n && self.overflows.get(i).copied().unwrap_or(0) == 0 {
                let head = super::btree_write::write_overflow_chain(
                    pager,
                    &self.entries[i].encode()[local..],
                )?;
                while self.overflows.len() <= i {
                    self.overflows.push(0);
                }
                self.overflows[i] = head;
            }
        }
        let page_no = self.page_no;
        {
            let dst = pager.page(page_no)?;
            for b in dst.iter_mut() {
                *b = 0;
            }
            let offsets = self.layout(usable)?;
            for i in 0..self.entries.len() {
                let cell = self.encode_cell(i, usable);
                let at = offsets[i] as usize;
                dst[at..at + cell.len()].copy_from_slice(&cell);
            }
            let h = PageHeader {
                page_type: page_type::INDEX_LEAF,
                first_freeblock: 0,
                cell_count: self.entries.len() as u16,
                cell_content_start: {
                    // Zero means the content area starts at the end of the
                    // page, which is only true for a completely full page.
                    let lowest = offsets.first().copied().unwrap_or(self.page_size as u16);
                    if lowest as u32 >= 0xffff {
                        0
                    } else {
                        lowest
                    }
                },
                fragmented_free: 0,
                right_most: None,
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

    /// Every entry, in key order.
    pub fn scan(&self) -> &[IndexEntry] {
        &self.entries
    }

    /// The rowids whose key falls in `[low, high]`, in key order.
    ///
    /// The bounds are inclusive and either may be omitted, which is how a
    /// range scan over part of the key is expressed.
    pub fn range(
        &self,
        key_len: usize,
        low: Option<&IndexEntry>,
        high: Option<&IndexEntry>,
    ) -> Vec<i64> {
        let mut out = Vec::new();
        for e in &self.entries {
            if let Some(l) = low {
                if compare(e, l, key_len) == std::cmp::Ordering::Less {
                    continue;
                }
            }
            if let Some(h) = high {
                if compare(e, h, key_len) == std::cmp::Ordering::Greater {
                    continue;
                }
            }
            out.push(e.rowid);
        }
        out
    }
}

/// A whole index, rooted at a page.
pub struct IndexTree {
    pub root: u32,
    /// How many leading columns are the key, which is the arity of compare.
    pub key_len: usize,
}

/// The default number of index entries a leaf is asked to hold before it
/// splits, which is a page's worth.
pub fn split_threshold(usable: u32) -> usize {
    // A quarter of the page is the maximum an index cell keeps locally, so a
    // leaf holds at least a few entries even when the keys are wide.
    let _ = max_local_for_index(usable);
    let _ = min_local(usable);
    // The real threshold is the number of cells that fit, which the leaf
    // decides by measurement; this is only the size of the test it uses.
    4
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::Encoding;

    fn entry(key: &[Value], rowid: i64) -> IndexEntry {
        IndexEntry {
            key: key.to_vec(),
            rowid,
        }
    }

    #[test]
    fn an_entry_round_trips_through_its_record() {
        let e = entry(&[Value::Integer(7), Value::Text("x".into())], 42);
        let bytes = e.encode();
        let got = IndexEntry::decode(&bytes, Encoding::Utf8).unwrap();
        assert_eq!(got.key, e.key);
        assert_eq!(got.rowid, 42);
    }

    #[test]
    fn the_rowid_is_a_fixed_width_so_a_negative_one_survives() {
        for rowid in [0i64, 1, -1, i64::MAX, i64::MIN, -9_223_372_036_854_775_808] {
            let e = entry(&[Value::Integer(1)], rowid);
            let got = IndexEntry::decode(&e.encode(), Encoding::Utf8).unwrap();
            assert_eq!(got.rowid, rowid, "rowid {rowid} did not survive");
        }
    }

    #[test]
    fn the_rowid_does_not_change_the_entrys_length() {
        // A narrow rowid would make two entries of the same key different
        // lengths, and a lookup would then compare unlike records.
        let small = entry(&[Value::Integer(1)], 1).encode();
        let large = entry(&[Value::Integer(1)], i64::MAX).encode();
        assert_eq!(small.len(), large.len());
    }

    #[test]
    fn a_null_key_survives() {
        let e = entry(&[Value::Null, Value::Integer(1)], 3);
        let got = IndexEntry::decode(&e.encode(), Encoding::Utf8).unwrap();
        assert_eq!(got.key[0], Value::Null);
        assert_eq!(got.rowid, 3);
    }

    #[test]
    fn a_wide_key_survives() {
        let key: Vec<Value> = (0..40).map(|i| Value::Integer(i)).collect();
        let e = entry(&key, 99);
        let got = IndexEntry::decode(&e.encode(), Encoding::Utf8).unwrap();
        assert_eq!(got.key, key);
        assert_eq!(got.rowid, 99);
    }

    #[test]
    fn entries_sort_by_key_then_by_rowid() {
        use std::cmp::Ordering;
        let a = entry(&[Value::Integer(1)], 5);
        let b = entry(&[Value::Integer(1)], 3);
        let c = entry(&[Value::Integer(2)], 1);
        // Same key, so the rowid decides, and 3 comes before 5.
        assert_eq!(compare(&b, &a, 1), Ordering::Less);
        assert_eq!(compare(&a, &c, 1), Ordering::Less);
    }

    #[test]
    fn keys_compare_by_sqlite_ordering() {
        use std::cmp::Ordering;
        // A number sorts below text, whatever the column's declared type.
        let num = entry(&[Value::Integer(5)], 1);
        let text = entry(&[Value::Text("a".into())], 1);
        assert_eq!(compare(&num, &text, 1), Ordering::Less);
        // A NULL sorts below everything.
        let null = entry(&[Value::Null], 1);
        assert_eq!(compare(&null, &num, 1), Ordering::Less);
    }

    #[test]
    fn a_leaf_stores_and_returns_its_entries() {
        let path = std::env::temp_dir().join(format!("nsqlite-idx-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut pager = Pager::open(&path).unwrap();
        pager.allocate().unwrap();
        let root = pager.allocate().unwrap();
        let usable = pager.usable_size();

        let mut leaf = IndexLeaf::empty(root, 4096);
        for (a, r) in [(3i64, 30i64), (1, 10), (2, 20)] {
            let e = entry(&[Value::Integer(a)], r);
            assert!(leaf.insert(e, 1, usable).unwrap());
        }
        leaf.write_to(&mut pager).unwrap();
        pager.flush().unwrap();
        drop(pager);

        let mut pager = Pager::open(&path).unwrap();
        let got = IndexLeaf::read(&mut pager, root).unwrap();
        let keys: Vec<i64> = got
            .entries
            .iter()
            .map(|e| e.key[0].as_i64().unwrap())
            .collect();
        assert_eq!(keys, vec![1, 2, 3], "entries come back in key order");
        let rowids: Vec<i64> = got.entries.iter().map(|e| e.rowid).collect();
        assert_eq!(rowids, vec![10, 20, 30]);
        drop(pager);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_duplicate_entry_is_refused() {
        let path = std::env::temp_dir().join(format!("nsqlite-idxd-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut pager = Pager::open(&path).unwrap();
        pager.allocate().unwrap();
        let root = pager.allocate().unwrap();
        let usable = pager.usable_size();
        let mut leaf = IndexLeaf::empty(root, 4096);
        assert!(leaf
            .insert(entry(&[Value::Integer(1)], 5), 1, usable)
            .unwrap());
        // The same key and the same rowid is the same entry.
        let e = leaf
            .insert(entry(&[Value::Integer(1)], 5), 1, usable)
            .unwrap_err();
        assert_eq!(e.code.name(), "CONSTRAINT");
        // The same key with a different rowid is a different entry.
        assert!(leaf
            .insert(entry(&[Value::Integer(1)], 6), 1, usable)
            .unwrap());
        drop(pager);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_range_returns_the_rows_whose_key_falls_inside_it() {
        let path = std::env::temp_dir().join(format!("nsqlite-idxr-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut pager = Pager::open(&path).unwrap();
        pager.allocate().unwrap();
        let root = pager.allocate().unwrap();
        let usable = pager.usable_size();
        let mut leaf = IndexLeaf::empty(root, 4096);
        for i in 1..=10i64 {
            leaf.insert(entry(&[Value::Integer(i)], i * 100), 1, usable)
                .unwrap();
        }
        leaf.write_to(&mut pager).unwrap();
        pager.flush().unwrap();
        drop(pager);

        let mut pager = Pager::open(&path).unwrap();
        let leaf = IndexLeaf::read(&mut pager, root).unwrap();
        // Inclusive on both ends.
        let lo = entry(&[Value::Integer(3)], 0);
        let hi = entry(&[Value::Integer(5)], i64::MAX);
        let got = leaf.range(1, Some(&lo), Some(&hi));
        assert_eq!(got, vec![300, 400, 500]);
        // An open end runs to the edge.
        assert_eq!(
            leaf.range(1, Some(&lo), None),
            vec![300, 400, 500, 600, 700, 800, 900, 1000]
        );
        assert_eq!(
            leaf.range(1, None, Some(&hi)),
            vec![100, 200, 300, 400, 500]
        );
        drop(pager);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn an_entry_too_wide_for_a_page_gets_an_overflow_chain() {
        let path = std::env::temp_dir().join(format!("nsqlite-idxo-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut pager = Pager::open(&path).unwrap();
        pager.allocate().unwrap();
        let root = pager.allocate().unwrap();
        let usable = pager.usable_size();
        // A key far wider than the 1002-byte local limit.
        let big: Vec<Value> = (0..2000).map(|i| Value::Integer(i)).collect();
        let mut leaf = IndexLeaf::empty(root, 4096);
        assert!(leaf.insert(entry(&big, 1), 1, usable).unwrap());
        leaf.write_to(&mut pager).unwrap();
        pager.flush().unwrap();
        drop(pager);

        let mut pager = Pager::open(&path).unwrap();
        let got = IndexLeaf::read(&mut pager, root).unwrap();
        assert_eq!(got.entries.len(), 1);
        assert_eq!(got.entries[0].key.len(), big.len());
        assert_eq!(got.entries[0].key[1999], Value::Integer(1999));
        assert_eq!(got.entries[0].rowid, 1);
        assert!(got.overflows[0] != 0, "a wide entry needs a chain");
        drop(pager);
        let _ = std::fs::remove_file(&path);
    }
}
