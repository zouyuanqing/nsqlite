//! A rowid-table b-tree that grows, splits, and stays balanced.
//!
//! The low-level page formats live in [`crate::btree_write`] and
//! [`crate::btree_interior`]. This module is the tree above them: it routes a
//! rowid to the right leaf, splits leaves when they fill, grows interior nodes
//! when they split in turn, and keeps a root that a scan can walk.
//!
//! Insertion is the only operation that splits, and it splits bottom-up. That
//! is the simplest arrangement that stays correct: a leaf that overflows
//! produces two leaves and one separator, and an interior node that overflows
//! produces two interior nodes and one separator of its own, until a new root
//! is allocated. No rebalancing is needed because cells are never moved between
//! pages except at a split, which is also why the free space left behind by a
//! delete is not reused until the page splits again.
//!
//! The delete side does not merge underfull pages. SQLite does, and so must this
//! before the official suite's delete tests pass, but leaving the space unused
//! is correct, merely wasteful.

use super::btree_interior::{split_leaf, InteriorCell, InteriorPage};
use super::btree_write::{Cell, LeafPage};
use super::error::{Error, Result};
use super::pager::Pager;
use super::value::Value;

/// A rowid and its column values, as handed to the tree.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub rowid: i64,
    pub values: Vec<Value>,
}

/// A rowid table b-tree.
pub struct TableTree {
    root: u32,
    page_size: u32,
    usable: u32,
    /// How a row's columns map onto the record, in particular which column is
    /// the rowid alias and must be stored as NULL.
    rowid_alias: Option<usize>,
}

impl TableTree {
    /// Opens the tree rooted at `root`, creating an empty leaf if the page is
    /// blank.
    pub fn open(pager: &mut Pager, root: u32) -> Result<TableTree> {
        let page_size = pager.page_size();
        let usable = pager.usable_size();
        let tree = TableTree {
            root,
            page_size,
            usable,
            rowid_alias: None,
        };
        let blank = pager.read_page(root)?.iter().all(|&b| b == 0);
        if blank {
            // A page that has never been written becomes an empty leaf.
            let leaf = LeafPage::empty(root, page_size);
            write_leaf(pager, &leaf)?;
        }
        Ok(tree)
    }

    /// Sets which column is the rowid alias, so it is stored as NULL.
    pub fn with_rowid_alias(mut self, column: Option<usize>) -> TableTree {
        self.rowid_alias = column;
        self
    }

    /// The tree's current root page.
    ///
    /// This moves when the root itself splits, so a caller that reopens the
    /// database must read the fresh value rather than reusing an old one. In a
    /// real database the root page number lives in the schema table, which is
    /// how a reopened connection finds the tree again.
    pub fn root(&self) -> u32 {
        self.root
    }

    /// Adopts a root page read back from the schema, for reopening a database.
    pub fn with_root(mut self, root: u32) -> TableTree {
        self.root = root;
        self
    }

    /// Builds the record payload for a row.
    fn payload_of(&self, row: &Row) -> Vec<u8> {
        LeafPage::encode_payload(&row.values, self.rowid_alias)
    }

    /// Inserts a row, allocating and splitting pages as needed.
    pub fn insert(&mut self, pager: &mut Pager, row: &Row) -> Result<()> {
        let cell = Cell::new(row.rowid, self.payload_of(row));
        // Walk down, remembering the path so a split can propagate upward.
        let mut path: Vec<u32> = Vec::new();
        let mut page_no = self.root;
        loop {
            let kind = {
                let page = pager.read_page(page_no)?;
                let offset = if page_no == 1 { 100 } else { 0 };
                PageKind::of(&page, offset)?
            };
            if kind == PageKind::Leaf {
                return self.insert_into_leaf(pager, page_no, cell, &path);
            }
            path.push(page_no);
            let interior = InteriorPage::read(pager, page_no)?;
            page_no = interior.child_for(row.rowid);
        }
    }

    /// Inserts at a leaf, splitting and propagating upward when it is full.
    fn insert_into_leaf(
        &mut self,
        pager: &mut Pager,
        page_no: u32,
        cell: Cell,
        path: &[u32],
    ) -> Result<()> {
        let mut leaf = LeafPage::read(pager, page_no)?;
        if leaf.insert(cell.clone(), self.usable)? {
            write_leaf(pager, &leaf)?;
            return Ok(());
        }
        // The leaf is full. The split takes the pending cell into account and
        // writes both halves with it already placed, so there is nothing left to
        // insert afterwards: doing so would duplicate the row.
        //
        // The new right child stays unreachable until the graft below, so the
        // parent never points at a page that does not yet hold its rows.
        let split = split_leaf(pager, &leaf, self.usable, &cell)?;

        // The parent gains a cell naming the left half, and its rightmost child
        // becomes the new right half, which is what keeps every key reachable.
        self.graft(pager, path, split.left, split.right, split.separator)
    }

    /// Adds `(child, separator)` to the parent of the page that just split.
    ///
    /// `path` is the chain of interior pages from the root down to the parent,
    /// so the last element is where the new child is attached. When the chain is
    /// empty the root itself was a leaf, and a new root has to be allocated
    /// above it.
    /// `split_page` is the page that split; it survives as the left half.
    fn graft(
        &mut self,
        pager: &mut Pager,
        path: &[u32],
        split_page: u32,
        new_page: u32,
        separator: i64,
    ) -> Result<()> {
        let left_child = split_page;
        let new_rightmost = new_page;
        let Some(&parent) = path.last() else {
            // The root was a leaf. It becomes the left child of a brand new
            // root, and the split-off page becomes that root's rightmost child.
            let new_root = pager.allocate()?;
            let interior = InteriorPage {
                page_no: new_root,
                cells: vec![InteriorCell {
                    left_child,
                    key: separator,
                }],
                rightmost: new_rightmost,
                page_size: self.page_size,
                header_offset: 0,
            };
            write_interior(pager, &interior)?;
            self.root = new_root;
            return Ok(());
        };

        // A split reuses the original page for its left half, so the page that
        // split is still the parent's child and now has to be represented by
        // two entries: one for each half. Where that happens depends on whether
        // the split page was the rightmost child or sat among the cells.
        let mut interior = InteriorPage::read(pager, parent)?;
        if interior.rightmost == left_child {
            // The split page was the rightmost child, so it becomes a cell and
            // the new page takes over as the rightmost.
            interior.cells.push(InteriorCell {
                left_child,
                key: separator,
            });
            interior.rightmost = new_rightmost;
        } else {
            let pos = interior
                .cells
                .iter()
                .position(|c| c.left_child == left_child)
                .ok_or_else(|| {
                    Error::corrupt(format!(
                        "page {left_child} split but the parent does not list it as a child"
                    ))
                })?;
            // The existing cell keeps its key, which is still the largest in
            // this subtree, and now points at the right half. The new cell
            // takes its place in the ordering, covering the left half.
            interior.cells[pos].left_child = new_rightmost;
            interior.cells.insert(
                pos,
                InteriorCell {
                    left_child,
                    key: separator,
                },
            );
        }
        let probe = InteriorPage {
            cells: interior.cells.clone(),
            ..interior_ref(&interior)
        };
        if probe.can_write() {
            write_interior(pager, &probe)?;
            return Ok(());
        }

        // The parent is full. Split it in two.
        //
        // The children in key order are cells[0].left_child through
        // cells[n-1].left_child, then the old rightmost. Splitting at `mid`
        // divides them as:
        //
        //   left half:  cells[0..mid],  rightmost = cells[mid].left_child
        //   right half: cells[mid+1..],  rightmost = the old rightmost
        //
        // The cell at the split point is therefore consumed rather than
        // duplicated: its child becomes the left half's rightmost, and its key
        // is the separator the grandparent needs. Leaving that cell in both
        // halves names one page twice, as the left half's rightmost and as the
        // right half's first cell, and every row under it then reads back
        // twice.
        let mid = interior.cells.len() / 2;
        let consumed = interior.cells[mid];
        let old_rightmost = interior.rightmost;
        let right_cells: Vec<InteriorCell> = interior.cells.drain(mid + 1..).collect();
        interior.cells.truncate(mid);
        interior.rightmost = consumed.left_child;
        write_interior(pager, &interior)?;

        let new_page = pager.allocate()?;
        let right = InteriorPage {
            page_no: new_page,
            cells: right_cells,
            rightmost: old_rightmost,
            page_size: self.page_size,
            header_offset: 0,
        };
        write_interior(pager, &right)?;

        // The parent's own parent now needs both halves: the page that was just
        // split keeps its identity as the left half, and the new page is right.
        self.graft(
            pager,
            &path[..path.len() - 1],
            parent,
            new_page,
            consumed.key,
        )
    }

    /// Every row in key order.
    pub fn scan(&mut self, pager: &mut Pager) -> Result<Vec<Row>> {
        let mut out = Vec::new();
        for page_no in self.leaf_pages(pager)? {
            let leaf = LeafPage::read(pager, page_no)?;
            for cell in &leaf.cells {
                let values =
                    super::record::decode_record(&cell.payload, pager.header().text_encoding)?
                        .values;
                out.push(Row {
                    rowid: cell.rowid,
                    values,
                });
            }
        }
        Ok(out)
    }

    /// The leaf pages in key order, which is a left-to-right walk of the tree.
    fn leaf_pages(&mut self, pager: &mut Pager) -> Result<Vec<u32>> {
        let mut out = Vec::new();
        let mut stack = vec![self.root];
        while let Some(page_no) = stack.pop() {
            let page = pager.read_page(page_no)?;
            let offset = if page_no == 1 { 100 } else { 0 };
            match PageKind::of(&page, offset)? {
                PageKind::Leaf => out.push(page_no),
                PageKind::Interior => {
                    // Push the rightmost child first so it is popped last and
                    // the leaves come out left to right.
                    let interior = InteriorPage::read(pager, page_no)?;
                    stack.push(interior.rightmost);
                    for cell in interior.cells.iter().rev() {
                        stack.push(cell.left_child);
                    }
                }
            }
        }
        Ok(out)
    }

    /// The row with the given rowid, or `None`.
    pub fn get(&mut self, pager: &mut Pager, rowid: i64) -> Result<Option<Row>> {
        let mut page_no = self.root;
        loop {
            let page = pager.read_page(page_no)?;
            let offset = if page_no == 1 { 100 } else { 0 };
            match PageKind::of(&page, offset)? {
                PageKind::Leaf => {
                    let leaf = LeafPage::read(pager, page_no)?;
                    let Some(cell) = leaf.cells.iter().find(|c| c.rowid == rowid) else {
                        return Ok(None);
                    };
                    let values =
                        super::record::decode_record(&cell.payload, pager.header().text_encoding)?
                            .values;
                    return Ok(Some(Row { rowid, values }));
                }
                PageKind::Interior => {
                    let interior = InteriorPage::read(pager, page_no)?;
                    page_no = interior.child_for(rowid);
                }
            }
        }
    }

    /// The largest rowid in the tree, or 0 when it is empty.
    pub fn max_rowid(&mut self, pager: &mut Pager) -> Result<i64> {
        let pages = self.leaf_pages(pager)?;
        let Some(&last) = pages.last() else {
            return Ok(0);
        };
        let leaf = LeafPage::read(pager, last)?;
        Ok(leaf.cells.last().map_or(0, |c| c.rowid))
    }

    /// The smallest rowid in the tree, or 0 when it is empty.
    pub fn min_rowid(&mut self, pager: &mut Pager) -> Result<i64> {
        let pages = self.leaf_pages(pager)?;
        let Some(&first) = pages.first() else {
            return Ok(0);
        };
        let leaf = LeafPage::read(pager, first)?;
        Ok(leaf.cells.first().map_or(0, |c| c.rowid))
    }

    /// Removes a row, returning whether it was present.
    ///
    /// The freed space is not returned to a free list or merged with a sibling;
    /// the page keeps the hole until it splits again. That is correct, only
    /// wasteful, and it is what the delete tests will later force a fix for.
    pub fn remove(&mut self, pager: &mut Pager, rowid: i64) -> Result<bool> {
        let mut page_no = self.root;
        loop {
            let page = pager.read_page(page_no)?;
            let offset = if page_no == 1 { 100 } else { 0 };
            match PageKind::of(&page, offset)? {
                PageKind::Leaf => {
                    let mut leaf = LeafPage::read(pager, page_no)?;
                    if leaf.remove(rowid).is_none() {
                        return Ok(false);
                    }
                    write_leaf(pager, &leaf)?;
                    return Ok(true);
                }
                PageKind::Interior => {
                    let interior = InteriorPage::read(pager, page_no)?;
                    page_no = interior.child_for(rowid);
                }
            }
        }
    }

    /// The number of rows in the tree.
    pub fn count(&mut self, pager: &mut Pager) -> Result<usize> {
        Ok(self.leaf_pages(pager)?.iter().try_fold(0usize, |acc, &p| {
            let leaf = LeafPage::read(pager, p)?;
            Ok::<usize, Error>(acc + leaf.cells.len())
        })?)
    }
}

/// Whether a page is a leaf or an interior node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PageKind {
    Leaf,
    Interior,
}

impl PageKind {
    fn of(page: &[u8], offset: usize) -> Result<PageKind> {
        use super::page::{page_type, PageHeader};
        if page.len() <= offset {
            return Err(Error::corrupt("page is too short for a b-tree header"));
        }
        let h = PageHeader::parse(page, offset as u16)?;
        match h.page_type {
            page_type::TABLE_LEAF => Ok(PageKind::Leaf),
            page_type::TABLE_INTERIOR => Ok(PageKind::Interior),
            other => Err(Error::corrupt(format!(
                "page is a {}, which is not part of a table b-tree",
                page_type::name(other)
            ))),
        }
    }
}

/// Clones an interior page's geometry without its cells, for probing capacity.
fn interior_ref(page: &InteriorPage) -> InteriorPage {
    InteriorPage {
        page_no: page.page_no,
        cells: Vec::new(),
        rightmost: page.rightmost,
        page_size: page.page_size,
        header_offset: page.header_offset,
    }
}

fn write_leaf(pager: &mut Pager, leaf: &LeafPage) -> Result<()> {
    // write_to allocates any overflow chain the page needs, so a spilling row
    // never ends up with a head of zero.
    leaf.write_to(pager)
}

fn write_interior(pager: &mut Pager, interior: &InteriorPage) -> Result<()> {
    let page_no = interior.page_no;
    {
        let dst = pager.page(page_no)?;
        interior.write(dst)?;
    }
    pager.mark_dirty(page_no);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::Encoding;

    fn temp(tag: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("nsqlite-tree-{}-{tag}.db", std::process::id()));
        let _ = std::fs::remove_file(&p);
        p
    }

    /// A tree rooted on its own page, so the tests do not collide with the
    /// schema page.
    fn new_db(tag: &str) -> (Pager, TableTree) {
        let path = temp(tag);
        let mut pager = Pager::open(&path).unwrap();
        pager.allocate().unwrap(); // page 1, the schema
        let root = pager.allocate().unwrap();
        let tree = TableTree::open(&mut pager, root).unwrap();
        (pager, tree)
    }

    fn row(rowid: i64, n: usize) -> Row {
        Row {
            rowid,
            values: vec![Value::Integer(rowid), Value::Text("x".repeat(n))],
        }
    }

    #[test]
    fn an_empty_tree_has_no_rows() {
        let (mut pager, mut tree) = new_db("empty");
        assert!(tree.scan(&mut pager).unwrap().is_empty());
        assert_eq!(tree.max_rowid(&mut pager).unwrap(), 0);
        assert_eq!(tree.min_rowid(&mut pager).unwrap(), 0);
    }

    #[test]
    fn a_few_rows_fit_in_one_leaf() {
        let (mut pager, mut tree) = new_db("few");
        for i in 1..=10 {
            tree.insert(&mut pager, &row(i, 20)).unwrap();
        }
        let rows = tree.scan(&mut pager).unwrap();
        assert_eq!(rows.len(), 10);
        for (i, r) in rows.iter().enumerate() {
            assert_eq!(r.rowid, i as i64 + 1, "rows must come back in key order");
        }
        assert_eq!(tree.count(&mut pager).unwrap(), 10);
    }

    #[test]
    fn rows_come_back_in_key_order_however_they_went_in() {
        let (mut pager, mut tree) = new_db("order");
        // Descending insertion is the case a naive split gets wrong, because the
        // new cell always belongs on the rightmost leaf.
        for i in (1..=50).rev() {
            tree.insert(&mut pager, &row(i, 20)).unwrap();
        }
        let rows = tree.scan(&mut pager).unwrap();
        assert_eq!(rows.len(), 50);
        for (i, r) in rows.iter().enumerate() {
            assert_eq!(r.rowid, i as i64 + 1);
        }
    }

    #[test]
    fn enough_rows_force_the_tree_to_grow() {
        let (mut pager, mut tree) = new_db("grow");
        // Each row is wide enough that a page holds only a few dozen.
        for i in 1..=2000 {
            tree.insert(&mut pager, &row(i, 80)).unwrap();
        }
        assert_eq!(tree.count(&mut pager).unwrap(), 2000);
        assert_eq!(tree.min_rowid(&mut pager).unwrap(), 1);
        assert_eq!(tree.max_rowid(&mut pager).unwrap(), 2000);

        let rows = tree.scan(&mut pager).unwrap();
        assert_eq!(rows.len(), 2000);
        for (i, r) in rows.iter().enumerate() {
            assert_eq!(r.rowid, i as i64 + 1, "the walk must stay in key order");
            assert_eq!(r.values[0], Value::Integer(i as i64 + 1));
            assert_eq!(r.values[1], Value::Text("x".repeat(80)));
        }
    }

    #[test]
    fn lookups_find_every_row_in_a_grown_tree() {
        let (mut pager, mut tree) = new_db("lookup");
        for i in 1..=800 {
            tree.insert(&mut pager, &row(i, 80)).unwrap();
        }
        for i in 1..=800 {
            let got = tree.get(&mut pager, i).unwrap().expect("row must be found");
            assert_eq!(got.rowid, i);
            assert_eq!(got.values[0], Value::Integer(i));
        }
        assert!(tree.get(&mut pager, 801).unwrap().is_none());
    }

    #[test]
    fn negative_rowids_survive_the_growth() {
        let (mut pager, mut tree) = new_db("neg");
        let mut keys: Vec<i64> = (1..=200).map(|i| -i).collect();
        keys.extend(1..=200);
        for k in &keys {
            tree.insert(&mut pager, &row(*k, 80)).unwrap();
        }
        assert_eq!(tree.count(&mut pager).unwrap(), 400);
        assert_eq!(tree.min_rowid(&mut pager).unwrap(), -200);
        assert_eq!(tree.max_rowid(&mut pager).unwrap(), 200);
        let rows = tree.scan(&mut pager).unwrap();
        let got: Vec<i64> = rows.iter().map(|r| r.rowid).collect();
        let mut want = keys.clone();
        want.sort_unstable();
        assert_eq!(got, want, "negative keys must sort ahead of positive ones");
    }

    #[test]
    fn a_duplicate_rowid_is_refused() {
        let (mut pager, mut tree) = new_db("dup");
        tree.insert(&mut pager, &row(1, 20)).unwrap();
        let err = tree.insert(&mut pager, &row(1, 20)).unwrap_err();
        assert_eq!(err.code.name(), "CONSTRAINT");
    }

    #[test]
    fn removing_a_row_takes_it_out_of_the_scan() {
        let (mut pager, mut tree) = new_db("remove");
        for i in 1..=30 {
            tree.insert(&mut pager, &row(i, 80)).unwrap();
        }
        assert!(tree.remove(&mut pager, 15).unwrap());
        assert!(
            !tree.remove(&mut pager, 15).unwrap(),
            "removing twice is a no-op"
        );
        assert!(!tree.remove(&mut pager, 9999).unwrap());
        let rows = tree.scan(&mut pager).unwrap();
        assert_eq!(rows.len(), 29);
        assert!(rows.iter().all(|r| r.rowid != 15));
    }

    #[test]
    fn a_rowid_alias_column_is_read_back_from_the_key() {
        let path = temp("alias");
        let mut pager = Pager::open(&path).unwrap();
        pager.allocate().unwrap();
        let root = pager.allocate().unwrap();
        let mut tree = TableTree::open(&mut pager, root)
            .unwrap()
            .with_rowid_alias(Some(0));
        for i in 1..=5 {
            tree.insert(
                &mut pager,
                &Row {
                    rowid: i,
                    values: vec![Value::Integer(i), Value::Text("t".into())],
                },
            )
            .unwrap();
        }
        // On disk the alias slot is NULL, so reading the raw record shows NULL
        // while the rowid key carries the value.
        let leaf = LeafPage::read(&mut pager, root).unwrap();
        let decoded =
            super::super::record::decode_record(&leaf.cells[0].payload, Encoding::Utf8).unwrap();
        assert_eq!(decoded.values[0], Value::Null);
        assert_eq!(leaf.cells[0].rowid, 1);
        drop(tree);
        drop(pager);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_reopened_tree_still_reads_every_row() {
        let path = temp("reopen");
        let root;
        {
            let mut pager = Pager::open(&path).unwrap();
            pager.allocate().unwrap();
            let r = pager.allocate().unwrap();
            let mut tree = TableTree::open(&mut pager, r).unwrap();
            for i in 1..=500 {
                tree.insert(&mut pager, &row(i, 80)).unwrap();
            }
            // Splitting the root moves it, so the value a reopening connection
            // would read out of the schema is taken after the inserts.
            root = tree.root();
            pager.flush().unwrap();
        }
        let mut pager = Pager::open(&path).unwrap();
        let mut tree = TableTree::open(&mut pager, 1).unwrap().with_root(root);
        assert_eq!(tree.count(&mut pager).unwrap(), 500);
        assert_eq!(tree.max_rowid(&mut pager).unwrap(), 500);
        let rows = tree.scan(&mut pager).unwrap();
        assert_eq!(rows.len(), 500);
        for (i, r) in rows.iter().enumerate() {
            assert_eq!(
                r.rowid,
                i as i64 + 1,
                "a reopened tree must stay in key order"
            );
        }
        drop(tree);
        drop(pager);
        let _ = std::fs::remove_file(&path);
    }
}
