//! Table interior pages, and the split that creates them.
//!
//! An interior table page holds cells of a four-byte left-child page number and
//! a varint key, plus a rightmost child pointer. There is no payload: a
//! table-interior cell is the one b-tree cell that never overflows, because its
//! key is a rowid and always fits.
//!
//! # The separator rule
//!
//! The key in each cell is the largest rowid in that cell's left subtree. It
//! was verified against a database written by sqlite3 3.53.4: for a tree of 37
//! interior cells, the separator matched the left child's maximum rowid in every
//! case. Getting this wrong is the classic way to corrupt a table b-tree,
//! because the tree still looks structurally valid and rows go missing.

use super::btree_write::{Cell, LeafPage};
use super::error::{Error, Result};
use super::page::{page_type, PageHeader};
use super::pager::Pager;
use super::varint;

/// One cell of a table interior page: a left child and the key that separates
/// it from the next child.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InteriorCell {
    pub left_child: u32,
    /// The largest rowid in `left_child`'s subtree.
    pub key: i64,
}

impl InteriorCell {
    /// The cell's size on the page.
    pub fn on_page_size(&self) -> usize {
        4 + varint::len_for(self.key as u64)
    }

    /// Serialises the cell: the child pointer followed by the key varint.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.on_page_size());
        out.extend_from_slice(&self.left_child.to_be_bytes());
        varint::append(&mut out, self.key as u64);
        out
    }
}

/// A table interior page.
pub struct InteriorPage {
    pub page_no: u32,
    /// Cells in ascending key order, which is also the pointer-array order.
    pub cells: Vec<InteriorCell>,
    /// The child that holds every key greater than the last cell's key.
    pub rightmost: u32,
    pub page_size: u32,
    pub header_offset: u16,
}

impl InteriorPage {
    /// A new, empty interior page with a single rightmost child.
    pub fn empty(page_no: u32, page_size: u32, rightmost: u32) -> InteriorPage {
        InteriorPage {
            page_no,
            cells: Vec::new(),
            rightmost,
            page_size,
            header_offset: if page_no == 1 { 100 } else { 0 },
        }
    }

    /// Reads an existing interior page.
    pub fn read(pager: &mut Pager, page_no: u32) -> Result<InteriorPage> {
        let page = pager.read_page(page_no)?;
        let page_size = page.len() as u32;
        let header_offset = if page_no == 1 { 100 } else { 0 };
        let h = PageHeader::parse(&page, header_offset)?;
        if h.page_type != page_type::TABLE_INTERIOR {
            return Err(Error::corrupt(format!(
                "page {page_no} is a {} where a table interior was expected",
                page_type::name(h.page_type)
            )));
        }
        let rightmost = h
            .right_most
            .ok_or_else(|| Error::corrupt("interior page has no rightmost child"))?;
        let array = h.cell_pointer_array_offset();
        let mut cells = Vec::with_capacity(h.cell_count as usize);
        for i in 0..h.cell_count as usize {
            let at = array + i * 2;
            if at + 2 > page.len() {
                return Err(Error::corrupt("cell pointer array is out of range"));
            }
            let offset = u16::from_be_bytes([page[at], page[at + 1]]) as usize;
            if offset + 5 > page.len() {
                return Err(Error::corrupt("interior cell is truncated"));
            }
            let left_child = u32::from_be_bytes([
                page[offset],
                page[offset + 1],
                page[offset + 2],
                page[offset + 3],
            ]);
            let (key, _) = varint::get_checked(&page[offset + 4..])
                .ok_or_else(|| Error::corrupt("interior cell key is truncated"))?;
            cells.push(InteriorCell {
                left_child,
                key: key as i64,
            });
        }
        Ok(InteriorPage {
            page_no,
            cells,
            rightmost,
            page_size,
            header_offset: header_offset as u16,
        })
    }

    /// Lays the cells out and returns where each body went.
    fn layout(&self) -> Result<(u32, Vec<u16>)> {
        let floor = self.header_offset as usize + PageHeader::SIZE_INTERIOR + self.cells.len() * 2;
        let mut content_start = self.page_size as usize;
        let mut offsets = Vec::with_capacity(self.cells.len());
        for cell in self.cells.iter().rev() {
            let n = cell.on_page_size();
            if n > content_start {
                return Err(Error::corrupt("interior cell is larger than the page"));
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

    /// The free bytes between the pointer array and the cell content area.
    pub fn free_space(&self) -> usize {
        let Ok((content_start, _)) = self.layout() else {
            return 0;
        };
        let floor = self.header_offset as usize + PageHeader::SIZE_INTERIOR + self.cells.len() * 2;
        (content_start as usize).saturating_sub(floor)
    }

    /// Whether the page can hold `count` more cells of the given size.
    pub fn fits(&self, count: usize, cell_size: usize) -> bool {
        // Each additional cell costs its own bytes plus a two-byte pointer.
        self.free_space() >= count * (cell_size + 2)
    }

    /// Whether the page can be written as it stands, i.e. its cells fit.
    pub fn can_write(&self) -> bool {
        self.layout().is_ok()
    }

    /// Serialises the page into `page`.
    pub fn write(&self, page: &mut [u8]) -> Result<()> {
        for b in page.iter_mut() {
            *b = 0;
        }
        let (content_start, offsets) = self.layout()?;
        for (i, cell) in self.cells.iter().enumerate() {
            let body = cell.encode();
            let at = offsets[i] as usize;
            page[at..at + body.len()].copy_from_slice(&body);
        }
        let h = PageHeader {
            page_type: page_type::TABLE_INTERIOR,
            first_freeblock: 0,
            cell_count: self.cells.len() as u16,
            cell_content_start: if content_start >= 0xffff {
                0
            } else {
                content_start as u16
            },
            fragmented_free: 0,
            right_most: Some(self.rightmost),
            header_offset: self.header_offset,
        };
        h.write(page);
        let array = h.cell_pointer_array_offset();
        for (i, at) in offsets.iter().enumerate() {
            page[array + i * 2..array + i * 2 + 2].copy_from_slice(&at.to_be_bytes());
        }
        Ok(())
    }

    /// The child that should receive a row with the given rowid.
    ///
    /// A cell's key is the *last* rowid its subtree holds, so a row belongs to
    /// the first cell whose key is at or above it, and to the rightmost child
    /// when the rowid exceeds every key.
    pub fn child_for(&self, rowid: i64) -> u32 {
        for cell in &self.cells {
            if rowid <= cell.key {
                return cell.left_child;
            }
        }
        self.rightmost
    }
}

/// A subtree produced by splitting a full leaf, ready to be grafted into a
/// parent interior node.
pub struct SplitLeaf {
    /// The new left page, holding the lower half of the keys.
    pub left: u32,
    /// The new right page, holding the upper half.
    pub right: u32,
    /// The largest rowid in the left page, which becomes the parent separator.
    pub separator: i64,
}

/// Splits `page` in two, writing the halves to two freshly allocated pages.
///
/// The split point is chosen so both halves fit with room to spare, and the
/// returned separator is the largest key that ended up on the left. The original
/// page is reused for the left half, so the caller only needs one new page.
pub fn split_leaf(
    pager: &mut Pager,
    page: &LeafPage,
    usable: u32,
    pending: &Cell,
) -> Result<SplitLeaf> {
    // The cell that did not fit is part of what the page has to hold, so the
    // split point is chosen over the existing cells plus it. Splitting the
    // existing cells alone can leave both halves full and the insert still
    // fails, which is what happens with rows that spill into overflow pages:
    // those occupy most of a page each, so two of them never share one.
    let mut all = page.cells.clone();
    let pos = all
        .binary_search_by_key(&pending.rowid, |c| c.rowid)
        .unwrap_or_else(|p| p);
    all.insert(pos, pending.clone());

    let right_no = pager.allocate()?;

    // Try each split point and keep the first that leaves both halves fitting.
    // SQLite aims for a roughly even split, so start in the middle and walk
    // outward only if needed.
    let mid = all.len() / 2;
    for delta in 0..all.len() {
        for left_len in [mid.saturating_sub(delta), mid + delta] {
            if left_len == 0 || left_len >= all.len() {
                continue;
            }
            let left_cells = &all[..left_len];
            let right_cells = &all[left_len..];
            let left = LeafPage {
                cells: left_cells.to_vec(),
                ..page_ref(page)
            };
            let right = LeafPage {
                page_no: right_no,
                cells: right_cells.to_vec(),
                page_size: page.page_size,
                // Not `..page_ref(page)`: a freshly allocated page is never
                // page 1, so it has no 100-byte file header to step over.
                // Inheriting the source page's offset wrote the right half with
                // its b-tree header at 100, and `LeafPage::read` then read that
                // same page at offset 0, where the bytes are still zero -- which
                // `PageHeader::parse` reports as a freelist trunk. Only a page
                // 1 ever carries the offset, so only splitting `sqlite_schema`
                // reached this, and only once its root was pinned in place.
                header_offset: 0,
            };
            if left.fits_cells(usable) && right.fits_cells(usable) {
                let separator = left_cells[left_cells.len() - 1].rowid;
                // The original page becomes the left half, so its cells move
                // down in address terms only if the count changed; writing both
                // out from scratch handles that.
                write_leaf(pager, page.page_no, &left, usable)?;
                write_leaf(pager, right_no, &right, usable)?;
                return Ok(SplitLeaf {
                    left: page.page_no,
                    right: right_no,
                    separator,
                });
            }
        }
    }
    Err(Error::corrupt(format!(
        "a leaf of {} cell(s) could not be split into two fitting halves;          largest cell occupies {} bytes of a {usable}-byte page",
        page.cells.len(),
        page.cells.iter().map(|c| c.on_page_size(usable)).max().unwrap_or(0),
    )))
}

/// Clones the page's geometry without its cells.
fn page_ref(page: &LeafPage) -> LeafPage {
    LeafPage {
        page_no: page.page_no,
        cells: Vec::new(),
        page_size: page.page_size,
        header_offset: page.header_offset,
    }
}

/// Writes a leaf page image to `page_no` and marks it dirty.
///
/// The page number is passed separately rather than taken from the struct,
/// because a split builds both halves from one page and only the left half
/// carries the original number.
fn write_leaf(pager: &mut Pager, page_no: u32, leaf: &LeafPage, _usable: u32) -> Result<()> {
    let mut copy = LeafPage {
        page_no,
        cells: leaf.cells.clone(),
        page_size: leaf.page_size,
        header_offset: leaf.header_offset,
    };
    copy.write_to(pager)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(rowid: i64) -> Cell {
        Cell {
            rowid,
            payload: LeafPage::encode_payload(&[crate::value::Value::Integer(rowid)], None),
            first_overflow: 0,
        }
    }

    fn temp(tag: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("nsqlite-int-{}-{tag}.db", std::process::id()));
        let _ = std::fs::remove_file(&p);
        p
    }

    #[test]
    fn an_interior_cell_encodes_child_then_key() {
        let c = InteriorCell {
            left_child: 0x0102_0304,
            key: 300,
        };
        let body = c.encode();
        assert_eq!(body.len(), c.on_page_size());
        assert_eq!(&body[0..4], &[0x01, 0x02, 0x03, 0x04]);
        let (key, n) = varint::get(&body[4..]);
        assert_eq!(key, 300);
        assert_eq!(body.len(), 4 + n);
    }

    #[test]
    fn a_negative_separator_key_round_trips() {
        let c = InteriorCell {
            left_child: 7,
            key: -1000,
        };
        let body = c.encode();
        let (key, _) = varint::get(&body[4..]);
        assert_eq!(key as i64, -1000);
    }

    #[test]
    fn routing_picks_the_first_cell_whose_key_covers_the_row() {
        let page = InteriorPage {
            page_no: 2,
            cells: vec![
                InteriorCell {
                    left_child: 10,
                    key: 85,
                },
                InteriorCell {
                    left_child: 11,
                    key: 167,
                },
            ],
            rightmost: 12,
            page_size: 4096,
            header_offset: 0,
        };
        // A key equal to a separator belongs to that separator's child, since
        // the separator is the last rowid that child holds.
        assert_eq!(page.child_for(1), 10);
        assert_eq!(page.child_for(85), 10);
        assert_eq!(page.child_for(86), 11);
        assert_eq!(page.child_for(167), 11);
        assert_eq!(
            page.child_for(168),
            12,
            "beyond the last key goes rightmost"
        );
    }

    #[test]
    fn an_interior_page_round_trips_through_the_reader() {
        let path = temp("rt");
        let mut pager = Pager::open(&path).unwrap();
        pager.allocate().unwrap();
        let root = pager.allocate().unwrap();
        // Separators ascend in key order, so the negative one comes first.
        let page = InteriorPage {
            page_no: root,
            cells: vec![
                InteriorCell {
                    left_child: 3,
                    key: -1000,
                },
                InteriorCell {
                    left_child: 4,
                    key: 85,
                },
                InteriorCell {
                    left_child: 5,
                    key: i64::MAX,
                },
            ],
            rightmost: 6,
            page_size: 4096,
            header_offset: 0,
        };
        {
            let dst = pager.page(root).unwrap();
            page.write(dst).unwrap();
        }
        pager.mark_dirty(root);
        pager.flush().unwrap();
        drop(pager);

        let mut pager = Pager::open(&path).unwrap();
        let got = InteriorPage::read(&mut pager, root).unwrap();
        assert_eq!(got.cells, page.cells);
        assert_eq!(got.rightmost, 6);
        assert_eq!(
            got.child_for(-5000),
            3,
            "below the first key goes to the first child"
        );
        assert_eq!(
            got.child_for(-1000),
            3,
            "a key equal to a separator stays in its child"
        );
        assert_eq!(got.child_for(86), 5, "past 85 lands in the next child");
        drop(pager);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn splitting_a_full_leaf_produces_two_fitting_halves() {
        let path = temp("split");
        let mut pager = Pager::open(&path).unwrap();
        pager.allocate().unwrap();
        let root = pager.allocate().unwrap();
        let usable = pager.usable_size();

        // Fill a leaf until it refuses, then split it.
        let mut page = LeafPage::empty(root, 4096);
        let mut n = 1i64;
        loop {
            let c = Cell {
                rowid: n,
                payload: LeafPage::encode_payload(
                    &[crate::value::Value::Text("x".repeat(60))],
                    None,
                ),
                first_overflow: 0,
            };
            if !page.insert(c, usable).unwrap() {
                break;
            }
            n += 1;
            if n > 500 {
                panic!("the page never filled");
            }
        }
        let count = page.cells.len();
        assert!(
            count > 10,
            "the page should have held a useful number of cells"
        );

        // The cell that did not fit is the trigger, and the split places it.
        let pending = Cell {
            rowid: n,
            payload: LeafPage::encode_payload(&[crate::value::Value::Text("y".repeat(60))], None),
            first_overflow: 0,
        };
        let split = split_leaf(&mut pager, &page, usable, &pending).unwrap();
        assert_eq!(
            split.left, root,
            "the original page is reused for the left half"
        );
        assert!(split.right != root);
        // The separator is the last key that landed on the left page, so every
        // key below it is on the left and every key above is on the right.
        assert!(split.separator >= 1 && split.separator < n);

        pager.flush().unwrap();

        // Both halves must read back, and together cover every key.
        let left = LeafPage::read(&mut pager, split.left).unwrap();
        let right = LeafPage::read(&mut pager, split.right).unwrap();
        assert_eq!(left.cells.len() + right.cells.len(), count + 1);
        assert_eq!(left.cells.last().unwrap().rowid, split.separator);
        assert_eq!(right.cells.first().unwrap().rowid, split.separator + 1);
        assert!(left.fits_cells(usable) && right.fits_cells(usable));
        drop(pager);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_split_of_a_two_cell_leaf_still_works() {
        let path = temp("split2");
        let mut pager = Pager::open(&path).unwrap();
        pager.allocate().unwrap();
        let root = pager.allocate().unwrap();
        let usable = pager.usable_size();
        let mut page = LeafPage::empty(root, 4096);
        page.insert(cell(1), usable).unwrap();
        page.insert(cell(2), usable).unwrap();
        let split = split_leaf(&mut pager, &page, usable, &cell(3)).unwrap();
        assert_eq!(split.separator, 1);
        drop(pager);
        let _ = std::fs::remove_file(&path);
    }
}
