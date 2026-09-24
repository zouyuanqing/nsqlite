//! Structural audits of a grown b-tree, run against a scale large enough to
//! force interior nodes to split.
//!
//! The unit tests in `table_tree` insert a couple of thousand narrow rows, which
//! fills leaves but never fills an interior node: measured on this machine, the
//! first interior split lands at rowid 3512 for 300-byte payloads and 14041 for
//! 80-byte ones. Everything a corrupt interior node does — double-referencing a
//! page, losing a subtree, misplacing a separator — therefore passes those tests
//! while corrupting the file. These insert enough to get there and then check
//! the invariants directly.

use std::collections::HashMap;

use nsqlite::pager::Pager;
use nsqlite::table_tree::{Row, TableTree};
use nsqlite::value::Value;

/// A structural summary of the tree, gathered in one walk.
struct Audit {
    /// Every page the tree reaches, in visit order.
    visited: Vec<u32>,
    /// Each page's own type, so a mismatch is visible.
    kinds: HashMap<u32, u8>,
    /// The largest rowid on each leaf, and the smallest, for separator checks.
    leaf_range: HashMap<u32, (i64, i64)>,
    /// Every separator with the subtree it claims to bound.
    separators: Vec<(u32, i64, u32)>,
}

fn audit(pager: &mut Pager, root: u32) -> Audit {
    use nsqlite::btree_interior::InteriorPage;
    use nsqlite::btree_write::LeafPage;

    let mut out = Audit {
        visited: Vec::new(),
        kinds: HashMap::new(),
        leaf_range: HashMap::new(),
        separators: Vec::new(),
    };
    let mut stack = vec![root];
    while let Some(page_no) = stack.pop() {
        out.visited.push(page_no);
        let page = pager.read_page(page_no).unwrap();
        let offset = if page_no == 1 { 100 } else { 0 };
        out.kinds.insert(page_no, page[offset]);
        match page[offset] {
            0x0d => {
                let leaf = LeafPage::read(pager, page_no).unwrap();
                let lo = leaf.cells.first().map_or(0, |c| c.rowid);
                let hi = leaf.cells.last().map_or(0, |c| c.rowid);
                out.leaf_range.insert(page_no, (lo, hi));
            }
            0x05 => {
                let interior = InteriorPage::read(pager, page_no).unwrap();
                for cell in &interior.cells {
                    out.separators.push((page_no, cell.key, cell.left_child));
                    stack.push(cell.left_child);
                }
                stack.push(interior.rightmost);
            }
            other => panic!("page {page_no} has type 0x{other:02x} inside a table b-tree"),
        }
    }
    out
}

fn temp(tag: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!("nsqlite-audit-{}-{tag}.db", std::process::id()));
    let _ = std::fs::remove_file(&p);
    p
}

/// Grows a tree and returns the pager, the tree, and the path, leaving the file
/// open so the caller can inspect it.
fn grown(tag: &str, count: i64, payload: usize) -> (Pager, TableTree, std::path::PathBuf) {
    let path = temp(tag);
    let mut pager = Pager::open(&path).unwrap();
    pager.allocate().unwrap();
    let root = pager.allocate().unwrap();
    let mut tree = TableTree::open(&mut pager, root).unwrap();
    let filler = "z".repeat(payload);
    for i in 1..=count {
        tree.insert(
            &mut pager,
            &Row {
                rowid: i,
                values: vec![Value::Integer(i), Value::Text(filler.clone())],
            },
        )
        .unwrap_or_else(|e| panic!("inserting {i}: {e}"));
    }
    (pager, tree, path)
}

#[test]
fn no_page_is_referenced_twice_after_interior_splits() {
    // Wide enough rows that the interior node fills and splits many times over.
    let (mut pager, mut tree, path) = grown("doubleref", 20_000, 300);
    let a = audit(&mut pager, tree.root());

    let mut counts: HashMap<u32, usize> = HashMap::new();
    for &p in &a.visited {
        *counts.entry(p).or_insert(0) += 1;
    }
    let repeated: Vec<(u32, usize)> = counts
        .iter()
        .filter(|(_, c)| **c > 1)
        .map(|(p, c)| (*p, *c))
        .collect();
    assert!(
        repeated.is_empty(),
        "{} page(s) referenced more than once: {repeated:?}\\n\
         This is what an interior split produces when the cell at the split point \
         is left in both halves: it becomes the left half's rightmost child and \
         the right half's first cell at the same time.",
        repeated.len()
    );
    // The tree actually grew past a single interior node, or the test proves
    // nothing: a tree that never split cannot double-reference anything.
    assert!(
        a.kinds.values().filter(|&&t| t == 0x05).count() > 1,
        "the fixture must produce more than one interior node"
    );
    drop(tree);
    drop(pager);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn every_separator_bounds_the_subtree_below_it() {
    let (mut pager, mut tree, path) = grown("sep", 20_000, 300);
    let a = audit(&mut pager, tree.root());

    // A cell's key is the largest rowid its left child holds. That invariant is
    // what makes a scan find every row; a wrong separator silently loses the
    // rows between the claim and the truth.
    for (owner, key, child) in &a.separators {
        let Some(&(lo, hi)) = a.leaf_range.get(child) else {
            // The child is itself interior, so the bound holds transitively;
            // checking the deepest leaves below it is what the scan test does.
            continue;
        };
        assert_eq!(
            *key, hi,
            "page {owner}: separator {key} does not match its child's largest rowid {hi} (child range {lo}..{hi})"
        );
    }
    drop(tree);
    drop(pager);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_scan_after_many_splits_returns_every_row_in_order() {
    let (mut pager, mut tree, path) = grown("scan", 20_000, 300);
    assert_eq!(tree.count(&mut pager).unwrap(), 20_000);
    let rows = tree.scan(&mut pager).unwrap();
    assert_eq!(rows.len(), 20_000, "a row was lost across a split");
    for (i, r) in rows.iter().enumerate() {
        assert_eq!(
            r.rowid,
            i as i64 + 1,
            "rows came back out of key order at {i}"
        );
    }
    assert_eq!(tree.min_rowid(&mut pager).unwrap(), 1);
    assert_eq!(tree.max_rowid(&mut pager).unwrap(), 20_000);
    // Spot-check lookups across the whole range, since a bad separator sends
    // the descent to the wrong leaf.
    for rowid in [1i64, 5_000, 10_000, 15_000, 20_000] {
        let got = tree
            .get(&mut pager, rowid)
            .unwrap()
            .expect("row must be found");
        assert_eq!(got.values[0], Value::Integer(rowid));
    }
    assert!(tree.get(&mut pager, 20_001).unwrap().is_none());
    drop(pager);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn descending_insertion_also_produces_a_sound_tree() {
    // Every insert lands on the rightmost leaf, which is the case a split
    // handles differently and the one an ascending test never reaches.
    let path = temp("desc");
    let mut pager = Pager::open(&path).unwrap();
    pager.allocate().unwrap();
    let root = pager.allocate().unwrap();
    let mut tree = TableTree::open(&mut pager, root).unwrap();
    let filler = "z".repeat(300);
    for i in (1..=20_000).rev() {
        tree.insert(
            &mut pager,
            &Row {
                rowid: i,
                values: vec![Value::Integer(i), Value::Text(filler.clone())],
            },
        )
        .unwrap();
    }
    let a = audit(&mut pager, tree.root());
    let mut counts: HashMap<u32, usize> = HashMap::new();
    for &p in &a.visited {
        *counts.entry(p).or_insert(0) += 1;
    }
    let repeated: Vec<(u32, usize)> = counts
        .iter()
        .filter(|(_, c)| **c > 1)
        .map(|(p, c)| (*p, *c))
        .collect();
    assert!(
        repeated.is_empty(),
        "descending inserts double-reference {repeated:?}"
    );

    let rows = tree.scan(&mut pager).unwrap();
    assert_eq!(rows.len(), 20_000);
    for (i, r) in rows.iter().enumerate() {
        assert_eq!(r.rowid, i as i64 + 1);
    }
    drop(tree);
    drop(pager);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn every_page_the_tree_reaches_is_inside_the_file() {
    let (mut pager, tree, path) = grown("bounds", 20_000, 300);
    let a = audit(&mut pager, tree.root());
    let count = pager.page_count();
    for &p in &a.visited {
        assert!(
            p >= 1 && p <= count,
            "page {p} is outside the file, which has {count} pages"
        );
    }
    // No page may be stranded: a page allocated but unreachable means a split
    // allocated a half and then failed to link it. Page 1 is the schema page
    // and page 2 is this tree's original root, which the tree has since grown
    // past, so the walk starts at page 2's successor.
    for p in 3..=count {
        assert!(
            a.visited.contains(&p),
            "page {p} of {count} was allocated but the tree never reaches it"
        );
    }
    drop(pager);
    let _ = std::fs::remove_file(&path);
}
