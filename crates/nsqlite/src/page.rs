//! Page-level structures: the database header, the b-tree page header, and the
//! payload overflow thresholds that decide where a cell spills to a chain of
//! overflow pages.

use super::error::{Error, Result};
use super::text::Encoding;

/// The 16-byte string every database file begins with.
pub const HEADER_MAGIC: &[u8; 16] = b"SQLite format 3\0";
pub const HEADER_SIZE: usize = 100;
pub const PAGE_SIZE_MAX: u32 = 65_536;
pub const PAGE_SIZE_MIN: u32 = 512;
/// A page size of 1 in the header is the escape for the maximum.
pub const PAGE_SIZE_ENCODED_MAX: u16 = 1;

/// B-tree page flags.
pub mod page_type {
    pub const INDEX_INTERIOR: u8 = 0x02;
    pub const TABLE_INTERIOR: u8 = 0x05;
    pub const INDEX_LEAF: u8 = 0x0a;
    pub const TABLE_LEAF: u8 = 0x0d;
    pub const FREELIST_TRUNK: u8 = 0x00;
    pub const PTR_MAP_INTERIOR: u8 = 0x03;
    pub const PTR_MAP_LEAF: u8 = 0x04;

    pub const fn name(t: u8) -> &'static str {
        match t {
            INDEX_INTERIOR => "index-interior",
            TABLE_INTERIOR => "table-interior",
            INDEX_LEAF => "index-leaf",
            TABLE_LEAF => "table-leaf",
            FREELIST_TRUNK => "freelist-trunk",
            PTR_MAP_INTERIOR => "pointer-map-interior",
            PTR_MAP_LEAF => "pointer-map-leaf",
            _ => "unknown",
        }
    }

    pub const fn is_table(t: u8) -> bool {
        t == TABLE_INTERIOR || t == TABLE_LEAF
    }

    pub const fn is_index(t: u8) -> bool {
        t == INDEX_INTERIOR || t == INDEX_LEAF
    }

    /// True for the two interior page kinds, which carry a right-most pointer.
    pub const fn is_interior(t: u8) -> bool {
        t == TABLE_INTERIOR || t == INDEX_INTERIOR
    }

    pub const fn is_leaf(t: u8) -> bool {
        t == INDEX_LEAF || t == TABLE_LEAF
    }
}

/// Decoded 100-byte database header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DbHeader {
    pub page_size: u32,
    pub write_version: u8,
    pub read_version: u8,
    pub reserved_space: u8,
    pub max_payload_fraction: u8,
    pub min_payload_fraction: u8,
    pub leaf_payload_fraction: u8,
    pub change_counter: u32,
    pub db_size_pages: u32,
    pub freelist_trunk: u32,
    pub freelist_count: u32,
    pub schema_cookie: u32,
    pub schema_format: u32,
    pub default_cache_size: i32,
    pub largest_root_page: u32,
    pub text_encoding: Encoding,
    pub user_version: i32,
    pub incremental_vacuum: u32,
    pub application_id: i32,
    pub version_valid_for: u32,
    pub sqlite_version: u32,
}

impl Default for DbHeader {
    fn default() -> DbHeader {
        DbHeader {
            page_size: 4096,
            write_version: 1,
            read_version: 1,
            reserved_space: 0,
            max_payload_fraction: 64,
            min_payload_fraction: 32,
            leaf_payload_fraction: 32,
            change_counter: 0,
            db_size_pages: 0,
            freelist_trunk: 0,
            freelist_count: 0,
            schema_cookie: 0,
            schema_format: 1,
            default_cache_size: 0,
            largest_root_page: 0,
            text_encoding: Encoding::Utf8,
            user_version: 0,
            incremental_vacuum: 0,
            application_id: 0,
            version_valid_for: 0,
            sqlite_version: 0,
        }
    }
}

impl DbHeader {
    /// The number of payload bytes that fit in a page before the reserved
    /// region is taken off. B-tree cell accounting is done in these units.
    pub fn usable_size(&self) -> u32 {
        self.page_size - self.reserved_space as u32
    }

    /// The largest payload kept entirely on a table leaf page.
    pub fn max_leaf_payload(&self) -> u32 {
        self.usable_size() - 35
    }

    /// The smallest payload that is allowed to be wholly local. Anything at or
    /// above this spills, so that a long record does not push a page to
    /// near-capacity and make future inserts expensive.
    pub fn min_local_payload(&self) -> u32 {
        ((self.usable_size() - 12) * 32 / 255) - 23
    }

    /// Parses the 100 bytes at the start of a file.
    pub fn parse(buf: &[u8]) -> Result<DbHeader> {
        if buf.len() < HEADER_SIZE {
            return Err(Error::not_a_db());
        }
        if &buf[0..16] != HEADER_MAGIC {
            return Err(Error::not_a_db());
        }
        let raw_page_size = u16::from_be_bytes([buf[16], buf[17]]) as u32;
        let page_size = if raw_page_size == PAGE_SIZE_ENCODED_MAX as u32 {
            PAGE_SIZE_MAX
        } else {
            raw_page_size
        };
        if !(PAGE_SIZE_MIN..=PAGE_SIZE_MAX).contains(&page_size) || !page_size.is_power_of_two() {
            // A non-power-of-two or out-of-range size is what the reference
            // reader uses to decide the file is not a database.
            return Err(Error::not_a_db());
        }
        let reserved = buf[20];
        if reserved as u32 + 480 > page_size {
            return Err(Error::not_a_db());
        }
        let encoding = Encoding::from_i32(i32::from_be_bytes([buf[56], buf[57], buf[58], buf[59]]));
        Ok(DbHeader {
            page_size,
            write_version: buf[18],
            read_version: buf[19],
            reserved_space: reserved,
            max_payload_fraction: buf[21],
            min_payload_fraction: buf[22],
            leaf_payload_fraction: buf[23],
            change_counter: be32(buf, 24),
            db_size_pages: be32(buf, 28),
            freelist_trunk: be32(buf, 32),
            freelist_count: be32(buf, 36),
            schema_cookie: be32(buf, 40),
            schema_format: be32(buf, 44),
            default_cache_size: be32(buf, 48) as i32,
            largest_root_page: be32(buf, 52),
            text_encoding: encoding,
            user_version: be32(buf, 60) as i32,
            incremental_vacuum: be32(buf, 64),
            application_id: be32(buf, 68) as i32,
            version_valid_for: be32(buf, 92),
            sqlite_version: be32(buf, 96),
        })
    }

    /// Serialises the header, taking the 100-byte page-size escape into account.
    pub fn to_bytes(&self) -> [u8; HEADER_SIZE] {
        let mut b = [0u8; HEADER_SIZE];
        b[0..16].copy_from_slice(HEADER_MAGIC);
        let encoded: u16 = if self.page_size == PAGE_SIZE_MAX {
            PAGE_SIZE_ENCODED_MAX
        } else {
            self.page_size as u16
        };
        b[16..18].copy_from_slice(&encoded.to_be_bytes());
        b[18] = self.write_version;
        b[19] = self.read_version;
        b[20] = self.reserved_space;
        b[21] = self.max_payload_fraction;
        b[22] = self.min_payload_fraction;
        b[23] = self.leaf_payload_fraction;
        put32(&mut b, 24, self.change_counter);
        put32(&mut b, 28, self.db_size_pages);
        put32(&mut b, 32, self.freelist_trunk);
        put32(&mut b, 36, self.freelist_count);
        put32(&mut b, 40, self.schema_cookie);
        put32(&mut b, 44, self.schema_format);
        put32(&mut b, 48, self.default_cache_size as u32);
        put32(&mut b, 52, self.largest_root_page);
        put32(&mut b, 56, self.text_encoding as i32 as u32);
        put32(&mut b, 60, self.user_version as u32);
        put32(&mut b, 64, self.incremental_vacuum);
        put32(&mut b, 68, self.application_id as u32);
        // 72..92 is reserved and must stay zero.
        put32(&mut b, 92, self.version_valid_for);
        put32(&mut b, 96, self.sqlite_version);
        b
    }
}

fn be32(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn put32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_be_bytes());
}

/// The b-tree page header, 8 bytes on a leaf or 12 on an interior page.
///
/// `header_offset` is 100 on page 1, whose first 100 bytes are the file header,
/// and 0 on every other page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageHeader {
    pub page_type: u8,
    pub first_freeblock: u16,
    pub cell_count: u16,
    pub cell_content_start: u16,
    pub fragmented_free: u8,
    pub right_most: Option<u32>,
    pub header_offset: u16,
}

impl PageHeader {
    pub const SIZE_LEAF: usize = 8;
    pub const SIZE_INTERIOR: usize = 12;

    /// The offset from the start of the page to the first byte of the cell
    /// pointer array.
    pub fn cell_pointer_array_offset(&self) -> usize {
        let size = if self.is_interior() {
            Self::SIZE_INTERIOR
        } else {
            Self::SIZE_LEAF
        };
        self.header_offset as usize + size
    }

    /// Offset of the first cell's content, interpreted from the end of the page
    /// when the stored value is zero (a completely full page).
    pub fn content_start(&self, page_size: u32) -> u32 {
        if self.cell_content_start == 0 {
            page_size
        } else {
            self.cell_content_start as u32
        }
    }

    pub fn is_interior(&self) -> bool {
        self.right_most.is_some()
    }

    pub fn is_leaf(&self) -> bool {
        self.right_most.is_none()
    }

    /// Parses the page header at `offset` within a page image.
    pub fn parse(page: &[u8], header_offset: u16) -> Result<PageHeader> {
        let at = header_offset as usize;
        if page.len() < at + Self::SIZE_LEAF {
            return Err(Error::corrupt("page is too short for a b-tree header"));
        }
        let page_type = page[at];
        let right_most =
            if page_type == page_type::TABLE_INTERIOR || page_type == page_type::INDEX_INTERIOR {
                Some(be32(page, at + 8))
            } else {
                None
            };
        let size = if right_most.is_some() {
            Self::SIZE_INTERIOR
        } else {
            Self::SIZE_LEAF
        };
        if page.len() < at + size {
            return Err(Error::corrupt("page is too short for an interior header"));
        }
        Ok(PageHeader {
            page_type,
            first_freeblock: u16::from_be_bytes([page[at + 1], page[at + 2]]),
            cell_count: u16::from_be_bytes([page[at + 3], page[at + 4]]),
            cell_content_start: u16::from_be_bytes([page[at + 5], page[at + 6]]),
            fragmented_free: page[at + 7],
            right_most,
            header_offset,
        })
    }

    /// Writes the header back into a page image.
    pub fn write(&self, page: &mut [u8]) {
        let at = self.header_offset as usize;
        page[at] = self.page_type;
        page[at + 1..at + 3].copy_from_slice(&self.first_freeblock.to_be_bytes());
        page[at + 3..at + 5].copy_from_slice(&self.cell_count.to_be_bytes());
        page[at + 5..at + 7].copy_from_slice(&self.cell_content_start.to_be_bytes());
        page[at + 7] = self.fragmented_free;
        if let Some(right) = self.right_most {
            put32(page, at + 8, right);
        }
    }
}

/// A decoded cell pointer, with the byte span the cell occupies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellPointer {
    pub offset: u16,
    /// Total on-page size of the cell, excluding any overflow chain.
    pub size: u16,
}

/// The largest payload an index cell keeps on its own page.
///
/// The value is roughly a quarter of the page, chosen so that a page always
/// holds at least four index keys. It is not `usable - 35`: the reference
/// computes it as `((usable - 12) * 64 / 255) - 23` and, in the source's own
/// words, the arithmetic "cannot be changed without resulting in an
/// incompatible file format".
pub const fn max_local_for_index(usable: u32) -> u32 {
    ((usable - 12) * 64 / 255) - 23
}

/// The largest payload a table leaf cell keeps on its own page. Table rows are
/// the one case where the whole payload may stay local.
pub const fn max_local_for_table_leaf(usable: u32) -> u32 {
    usable - 35
}

/// The smallest payload that may be wholly local, below which a cell always
/// spills so that a long record cannot leave a page nearly full.
pub const fn min_local(usable: u32) -> u32 {
    ((usable - 12) * 32 / 255) - 23
}

/// How many payload bytes a cell keeps on its own page.
///
/// Mirrors the reference `btreeParseCellPtr`. Below the threshold the whole
/// payload stays local. Above it — which only a table leaf can reach, since its
/// threshold is the larger of the two — the same surplus calculation applies:
/// how much is kept is a non-monotonic function of the payload size, chosen so
/// the remainder fills overflow pages exactly.
pub fn local_payload_size(page_type: u8, payload_len: u32, usable: u32) -> u32 {
    use page_type::*;
    let max_local = match page_type {
        TABLE_LEAF => max_local_for_table_leaf(usable),
        _ => max_local_for_index(usable),
    };
    if payload_len <= max_local {
        return payload_len;
    }
    let min_local = min_local(usable);
    let surplus = min_local + (payload_len - min_local) % (usable - 4);
    if surplus <= max_local {
        surplus
    } else {
        min_local
    }
}

/// Overflow payload capacity per page for a given usable page size: the first
/// four bytes of an overflow page hold the next page in the chain.
pub const fn overflow_capacity_for(usable: u32) -> u32 {
    usable - 4
}

/// Bytes of a payload that spill into the overflow chain.
pub fn overflow_bytes(payload_len: u32, local: u32) -> u32 {
    payload_len.saturating_sub(local)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_size_escape_is_understood() {
        let mut h = DbHeader::default();
        h.page_size = 65_536;
        let b = h.to_bytes();
        assert_eq!(
            u16::from_be_bytes([b[16], b[17]]),
            1,
            "65536 is stored as 1"
        );
        assert_eq!(DbHeader::parse(&b).unwrap().page_size, 65_536);
        h.page_size = 4096;
        let b = h.to_bytes();
        assert_eq!(u16::from_be_bytes([b[16], b[17]]), 4096);
    }

    #[test]
    fn header_round_trips() {
        let h = DbHeader {
            change_counter: 0xdead_beef,
            db_size_pages: 1234,
            text_encoding: Encoding::Utf16Le,
            user_version: -1,
            application_id: 0x1122_3344,
            reserved_space: 4,
            ..DbHeader::default()
        };
        let b = h.to_bytes();
        let got = DbHeader::parse(&b).unwrap();
        assert_eq!(got.change_counter, 0xdead_beef);
        assert_eq!(got.db_size_pages, 1234);
        assert_eq!(got.text_encoding, Encoding::Utf16Le);
        assert_eq!(got.user_version, -1);
        assert_eq!(got.application_id, 0x1122_3344);
        assert_eq!(got.reserved_space, 4);
    }

    #[test]
    fn reserved_bytes_stay_zero() {
        let b = DbHeader::default().to_bytes();
        assert!(b[72..92].iter().all(|&x| x == 0));
    }

    #[test]
    fn non_database_input_is_rejected() {
        assert_eq!(
            DbHeader::parse(&[0u8; 200]).unwrap_err().code.name(),
            "NOTADB"
        );
        let mut h = DbHeader::default();
        h.page_size = 1000; // not a power of two
        assert_eq!(
            DbHeader::parse(&h.to_bytes()).unwrap_err().code.name(),
            "NOTADB"
        );
    }

    #[test]
    fn usable_size_accounts_for_reserved_space() {
        let h = DbHeader {
            page_size: 4096,
            reserved_space: 32,
            ..DbHeader::default()
        };
        assert_eq!(h.usable_size(), 4064);
        assert_eq!(h.max_leaf_payload(), 4029);
    }

    #[test]
    fn index_max_local_is_a_quarter_of_the_page() {
        // SQLite's own constant: a 4096-byte page holds at most 1002 index
        // payload bytes locally, which is what guarantees a minimum fanout of
        // four. Using usable - 35 here would be off by 3000 bytes and would
        // misplace the boundary between local and spilled data.
        assert_eq!(max_local_for_index(4096), 1002);
        assert_eq!(max_local_for_index(512), ((500) * 64 / 255) - 23);
        assert_eq!(max_local_for_table_leaf(4096), 4061);
        assert_eq!(min_local(4096), 489);
    }

    #[test]
    fn local_payload_matches_the_reference_geometry() {
        let u = 4096u32;
        let x = max_local_for_index(u);
        let m = min_local(u);
        // Below the threshold everything stays local, for both page kinds.
        for p in [0, 1, 500, x] {
            assert_eq!(local_payload_size(page_type::INDEX_LEAF, p, u), p);
            assert_eq!(local_payload_size(page_type::TABLE_LEAF, p, u), p);
        }
        // Above it, the index spills, and the kept size is never above x.
        for p in (x + 1)..(x + 300) {
            let local = local_payload_size(page_type::INDEX_LEAF, p, u);
            assert!(local <= x, "p={p} local={local} exceeds x={x}");
            assert!(local >= m, "p={p} local={local} below m={m}");
            // A table leaf still holds the whole payload, since it has a much
            // larger local threshold.
            assert_eq!(
                local_payload_size(page_type::TABLE_LEAF, p, u),
                p.min(max_local_for_table_leaf(u))
            );
        }

        // Past its own threshold a table leaf spills the same way, and the kept
        // size jumps around non-monotonically as the payload grows.
        let tx = max_local_for_table_leaf(u);
        assert!(local_payload_size(page_type::TABLE_LEAF, tx + 1, u) < tx);
        let mut seen = Vec::new();
        // The kept size resets every time the payload grows by the overflow page
        // capacity, so a range spanning more than one such step must show both
        // a rise and a drop.
        for p in (tx + 1)..(tx + 2 * (u - 4) + 8) {
            let local = local_payload_size(page_type::TABLE_LEAF, p, u);
            assert!(local >= m && local <= tx, "table p={p} local={local}");
            seen.push(local);
        }
        assert!(
            seen.windows(2).any(|w| w[1] > w[0]) && seen.windows(2).any(|w| w[1] < w[0]),
            "the surplus formula must rise and fall as the payload grows; a \
             monotonic result means the overflow geometry is wrong"
        );
    }

    #[test]
    fn a_table_leaf_keeps_more_than_an_index_does() {
        let u = 4096u32;
        let p = 2000;
        assert_eq!(local_payload_size(page_type::TABLE_LEAF, p, u), p);
        assert!(local_payload_size(page_type::INDEX_LEAF, p, u) < p);
    }

    #[test]
    fn reserved_space_shrinks_the_thresholds_proportionally() {
        let h = DbHeader {
            page_size: 4096,
            reserved_space: 32,
            ..DbHeader::default()
        };
        let u = h.usable_size();
        assert_eq!(u, 4064);
        assert_eq!(h.max_leaf_payload(), max_local_for_table_leaf(u));
        assert_eq!(h.min_local_payload(), min_local(u));
        assert_eq!(max_local_for_index(u), ((u - 12) * 64 / 255) - 23);
    }

    #[test]
    fn local_payload_thresholds_match_the_reference() {
        let usable = 4096u32;
        let table_max = max_local_for_table_leaf(usable);
        let index_max = max_local_for_index(usable);
        let m = min_local(usable);
        // A table leaf keeps the whole payload while it fits.
        assert_eq!(local_payload_size(page_type::TABLE_LEAF, 100, usable), 100);
        assert_eq!(
            local_payload_size(page_type::TABLE_LEAF, table_max, usable),
            table_max
        );
        // One byte past its threshold, a table leaf drops to the surplus, which
        // for a 4065-byte payload on a 4096-byte page is the 489-byte minimum.
        // Verified against a file written by sqlite3 3.53.4, whose cell holds
        // 489 local bytes plus a 2-byte payload varint, a 1-byte rowid varint
        // and a 4-byte overflow pointer.
        assert_eq!(local_payload_size(page_type::TABLE_LEAF, 4065, usable), m);
        assert_eq!(m, 489);
        assert_eq!(table_max, 4061);
        // An index leaf keeps far less before spilling.
        assert_eq!(
            local_payload_size(page_type::INDEX_LEAF, index_max, usable),
            index_max
        );
        let spilled = local_payload_size(page_type::INDEX_LEAF, index_max + 1, usable);
        assert!(spilled < index_max);
        // A table leaf is still holding whole payloads at that size, because
        // its own threshold is four times larger.
        assert_eq!(
            local_payload_size(page_type::TABLE_LEAF, index_max + 1, usable),
            index_max + 1
        );
    }

    #[test]
    fn index_local_payload_lands_inside_the_surplus_bound() {
        let usable = 4096u32;
        let max_local = usable - 35;
        let min_local = ((usable - 12) * 32 / 255) - 23;
        for payload in (max_local + 1)..(max_local + 500) {
            for pt in [
                page_type::INDEX_LEAF,
                page_type::INDEX_INTERIOR,
                page_type::TABLE_INTERIOR,
            ] {
                let local = local_payload_size(pt, payload, usable);
                assert!(
                    local >= min_local && local <= payload,
                    "{}: local={local} payload={payload} outside [{min_local}, {payload}]",
                    page_type::name(pt)
                );
                assert!(
                    local <= max_local,
                    "{}: local={local} exceeds max",
                    page_type::name(pt)
                );
            }
        }
    }

    #[test]
    fn page_header_round_trips_for_both_shapes() {
        for &pt in &[
            page_type::TABLE_LEAF,
            page_type::INDEX_LEAF,
            page_type::TABLE_INTERIOR,
            page_type::INDEX_INTERIOR,
        ] {
            let h = PageHeader {
                page_type: pt,
                first_freeblock: 0,
                cell_count: 3,
                cell_content_start: 4000,
                fragmented_free: 1,
                right_most: if page_type::is_interior(pt) {
                    Some(7)
                } else {
                    None
                },
                header_offset: 0,
            };
            let mut page = vec![0u8; 4096];
            h.write(&mut page);
            assert_eq!(PageHeader::parse(&page, 0).unwrap(), h);
        }
    }

    #[test]
    fn page_one_header_starts_after_the_file_header() {
        let mut page = vec![0u8; 4096];
        let h = PageHeader {
            page_type: page_type::TABLE_INTERIOR,
            first_freeblock: 0,
            cell_count: 0,
            cell_content_start: 4096,
            fragmented_free: 0,
            right_most: Some(2),
            header_offset: 100,
        };
        h.write(&mut page);
        let got = PageHeader::parse(&page, 100).unwrap();
        assert_eq!(got.right_most, Some(2));
        assert_eq!(got.cell_pointer_array_offset(), 112);
    }

    #[test]
    fn a_full_page_reports_the_page_size_as_content_start() {
        let h = PageHeader {
            page_type: page_type::TABLE_LEAF,
            first_freeblock: 0,
            cell_count: 1,
            cell_content_start: 0,
            fragmented_free: 0,
            right_most: None,
            header_offset: 0,
        };
        assert_eq!(h.content_start(4096), 4096);
    }
}
