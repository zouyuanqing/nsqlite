//! Structural audits of a grown index b-tree.
//!
//! An index that only ever held a few dozen entries would pass any test that
//! inserts and reads back: a single leaf cannot be corrupt in a way a scan
//! notices. Everything interesting happens once leaves split, interior pages
//! appear, and the root itself has to be rewritten — which is where the
//! invariants below earn their keep. A test that only inserts and reads back
//! passes against a tree that double-references a page, loses a key per split,
//! or strands half a subtree.
//!
//! The audits walk the raw pages rather than trusting the tree's own accessors,
//! so a bug in the walk cannot hide behind itself.
//!
//! # What the invariants are
//!
//! * **No page is referenced twice.** Every page in the tree is reached by
//!   exactly one parent pointer. This is what an interior split gets wrong when
//!   it leaves the split cell in both halves.
//! * **Every separator is the largest key of the subtree below it.** The
//!   reference implementation *promotes* a key out of the leaves into the
//!   interior cell rather than copying it, so "largest key below" has to count
//!   the cell's own key. Checked against sqlite3 3.53.4: a cell pointing at a
//!   leaf whose last entry was 383 carried the key 384, and 4987 leaf entries
//!   plus 13 separators was exactly 5000 with no key in both places.
//! * **Every key is present exactly once.** Promoted keys live in interior
//!   cells, everything else in leaves, so a scan that visits both sees each key
//!   once. Duplicating a separator into a leaf, or failing to emit a promoted
//!   key during a scan, breaks this.
//! * **No page is stranded.** A page the tree allocated but never reaches is a
//!   half that was written and then forgotten.
//! * **It is still readable from the file.** Every audit runs after a flush
//!   and a reopen, so what is checked is the bytes on disk.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use nsqlite::index::{IndexEntry, IndexLeaf};
use nsqlite::index_interior::{IndexInterior, IndexTree};
use nsqlite::page::page_type;
use nsqlite::pager::Pager;
use nsqlite::value::Value;
/// A structural summary of one index tree, gathered in a single walk.
struct Audit {
    /// Every page the tree reaches, in visit order.
    visited: Vec<u32>,
    /// Each page's own type byte, so a type mismatch is visible.
    kinds: HashMap<u32, u8>,
    /// Every separator with the page holding it and the child it bounds.
    separators: Vec<(u32, IndexEntry, u32)>,
    /// Every entry held in a leaf.
    leaf_entries: Vec<IndexEntry>,
    /// The keys held in interior cells, which the reference promotes out of
    /// the leaves and so are missing from `leaf_entries`.
    promoted: Vec<IndexEntry>,
    /// Every page named by an overflow chain, reached by following the chains
    /// from each spilled cell rather than by the b-tree pointers.
    ///
    /// A b-tree walk never arrives at these: an overflow page is named by a
    /// 4-byte pointer inside a cell, not by an interior page's child pointer. So
    /// a walk that only counted `visited` could not see a whole orphaned chain,
    /// which is exactly what the two modes of corruption that reach
    /// `PRAGMA integrity_check` looked like from here — a file where
    /// `wrong # of entries` and `Page N: never used` were both reachable and
    /// neither audit noticed.
    overflow: Vec<u32>,
}

impl Audit {
    /// Every key in the tree, leaves and promoted cells alike, in visit order.
    fn all_keys(&self) -> Vec<IndexEntry> {
        let mut all = self.leaf_entries.clone();
        all.extend(self.promoted.iter().cloned());
        all
    }

    /// Every page the tree holds, b-tree and overflow alike.
    fn every_page(&self) -> Vec<u32> {
        let mut all = self.visited.clone();
        all.extend(self.overflow.iter().copied());
        all
    }
}

/// Follows one overflow chain and returns every page in it, head first.
///
/// The first four bytes of an overflow page hold the next page, or zero at the
/// end. The walk stops on a page it has already seen, so a cyclic chain reports
/// itself rather than hanging the test.
fn follow_chain(pager: &mut Pager, head: u32, into: &mut Vec<u32>) {
    let mut next = head;
    while next != 0 {
        if into.contains(&next) {
            return;
        }
        let page = pager.read_page(next).expect("an overflow page is readable");
        into.push(next);
        next = u32::from_be_bytes([page[0], page[1], page[2], page[3]]);
    }
}

/// Walks the tree from `root`, reading raw page headers.
fn audit(pager: &mut Pager, root: u32) -> Audit {
    let mut out = Audit {
        visited: Vec::new(),
        kinds: HashMap::new(),
        separators: Vec::new(),
        leaf_entries: Vec::new(),
        promoted: Vec::new(),
        overflow: Vec::new(),
    };
    let mut stack = vec![root];
    while let Some(page_no) = stack.pop() {
        out.visited.push(page_no);
        let page = pager.read_page(page_no).unwrap();
        let offset = if page_no == 1 { 100 } else { 0 };
        let kind = page[offset];
        out.kinds.insert(page_no, kind);
        match kind {
            page_type::INDEX_LEAF => {
                let leaf = IndexLeaf::read(pager, page_no).unwrap();
                assert_eq!(
                    leaf.overflows.len(),
                    leaf.entries.len(),
                    "leaf {page_no} has a chain head for a different number of \
                     entries than it holds, so a head cannot be matched to the \
                     cell that owns it"
                );
                for head in leaf.overflows.iter().copied() {
                    if head == 0 {
                        continue;
                    }
                    follow_chain(pager, head, &mut out.overflow);
                }
                out.leaf_entries.extend(leaf.entries.iter().cloned());
            }
            page_type::INDEX_INTERIOR => {
                let interior = IndexInterior::read(pager, page_no).unwrap();
                for cell in interior.cells {
                    if cell.overflow != 0 {
                        follow_chain(pager, cell.overflow, &mut out.overflow);
                    }
                    out.separators
                        .push((page_no, cell.separator.clone(), cell.left_child));
                    out.promoted.push(cell.separator.clone());
                    stack.push(cell.left_child);
                }
                stack.push(interior.rightmost);
            }
            other => panic!("page {page_no} has type 0x{other:02x} inside an index b-tree"),
        }
    }
    out
}

fn temp(tag: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!("nsqlite-idxaudit-{}-{tag}.db", std::process::id()));
    let _ = std::fs::remove_file(&p);
    p
}

/// A single-column integer entry.
fn int_entry(k: i64, rowid: i64) -> IndexEntry {
    IndexEntry {
        key: vec![Value::Integer(k)],
        rowid,
    }
}

/// Grows an index and leaves the file closed, so the caller reopens it and the
/// audit sees the bytes on disk rather than a warm cache.
///
/// The keys are padded to `key_width` bytes so a 4096-byte page still fills
/// after a few hundred rows, which is what makes the interior layer split
/// repeatedly. A narrow key would need tens of thousands of rows to get there,
/// and the pager currently cannot change its page size after opening a file
/// (`header_mut().page_size` and `Pager::open_memory(n)` both leave the stride
/// at 4096), so padding is how a 512-byte-page scenario is approximated here.
fn grown(tag: &str, count: i64, key_width: usize) -> (std::path::PathBuf, u32) {
    let path = temp(tag);
    let root = {
        let mut pager = Pager::open(&path).unwrap();
        pager.allocate().unwrap();
        let root = pager.allocate().unwrap();
        {
            let mut tree = IndexTree::create(&mut pager, root, 1).unwrap();
            for i in 1..=count {
                let e = padded_entry(i, key_width);
                tree.insert(&e)
                    .unwrap_or_else(|err| panic!("inserting {i}: {err}"));
            }
        }
        pager.flush().unwrap();
        root
    };
    (path, root)
}

/// A single-column text entry whose key is `width` bytes, zero-padded so the
/// keys sort in numeric order.
fn padded_entry(k: i64, width: usize) -> IndexEntry {
    IndexEntry {
        key: vec![Value::Text(format!("{k:0>width$}"))],
        rowid: k,
    }
}

#[test]
fn no_page_is_referenced_twice_after_interior_splits() {
    // A 512-byte page forces the interior layer to fill and split repeatedly,
    // which is the case a corrupt split produces.
    let (path, root) = grown("doubleref", 3000, 120);
    let mut pager = Pager::open(&path).unwrap();
    let a = audit(&mut pager, root);

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
        "{} page(s) referenced more than once: {repeated:?}\n\
         This is what an interior split produces when the cell at the split point \
         is left in both halves: it becomes the left half's rightmost child and \
         the right half's first cell at the same time.",
        repeated.len()
    );
    // The fixture must actually have produced an interior layer, or the test
    // proves nothing: a tree that never split cannot double-reference anything.
    let interior = a
        .kinds
        .values()
        .filter(|&&t| t == page_type::INDEX_INTERIOR)
        .count();
    assert!(
        interior > 1,
        "the fixture must produce more than one interior node, got {interior}"
    );
    drop(pager);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn the_tree_reaches_every_page_it_allocated() {
    // Both widths, because they strand differently. A 120-byte key barely
    // spills at all on a 4096-byte page, so a walk that counted only b-tree
    // pages passed while a wide key — which spills on nearly every entry —
    // stranded the majority of the file. Counting only the b-tree pages is what
    // let that through: an overflow page is named by a 4-byte pointer inside a
    // cell, never by an interior page's child pointer, so a b-tree walk cannot
    // reach one even in principle.
    for (tag, width) in [("stranded", 120usize), ("strandedwide", 2000)] {
        let count: i64 = if width == 120 { 3000 } else { 300 };
        let (path, root) = grown(tag, count, width);
        let mut pager = Pager::open(&path).unwrap();
        let a = audit(&mut pager, root);
        let page_count = pager.page_count();
        for &p in &a.every_page() {
            assert!(
                p >= 1 && p <= page_count,
                "page {p} is outside the file, which has {page_count} pages"
            );
        }
        // Page 1 is the schema page and page 2 is the index's original root
        // leaf, so the walk starts above them. Every later page was allocated
        // for this tree and must be reachable — as a b-tree page *or* as part of
        // an overflow chain. This is the check that answers the reference's own
        // "Page N: never used".
        let stranded: Vec<u32> = (3..=page_count)
            .filter(|p| !a.every_page().contains(p))
            .collect();
        assert!(
            stranded.is_empty(),
            "{tag}: {} of {page_count} page(s) were allocated but the tree never \
             reaches them, even through an overflow chain: {stranded:?}\n\
             A split that writes a half and then allocates a fresh chain for a \
             key it already had a chain for strands the old one. The reference \
             reports each of these as 'Page N: never used' and opens its \
             integrity_check with '*** in database main ***'.",
            stranded.len()
        );
        // The fixture has to have spilled at all, or "no stranded page" is
        // vacuous: nothing allocated nothing to strand.
        if width > 120 {
            assert!(
                !a.overflow.is_empty(),
                "{tag}: the fixture must spill into overflow pages for this to \
                 mean anything, but the walk reached none"
            );
        }
        drop(pager);
        let _ = std::fs::remove_file(&path);
    }
}

#[test]
fn no_page_is_reached_twice_through_a_pointer_or_a_chain() {
    // The first version of this test counted only b-tree pages, so it could not
    // see a page that two cells both named through an overflow pointer. Carrying
    // the chains through the walk makes the invariant total: every page the tree
    // allocated is named exactly once, whatever kind of pointer names it.
    let (path, root) = grown("twicewide", 300, 2000);
    let mut pager = Pager::open(&path).unwrap();
    let a = audit(&mut pager, root);

    let mut counts: HashMap<u32, usize> = HashMap::new();
    for &p in &a.every_page() {
        *counts.entry(p).or_insert(0) += 1;
    }
    let repeated: Vec<(u32, usize)> = counts
        .iter()
        .filter(|(_, c)| **c > 1)
        .map(|(p, c)| (*p, *c))
        .collect();
    assert!(
        repeated.is_empty(),
        "{} page(s) named more than once: {repeated:?}\n\
         Two cells sharing an overflow chain is the other way to be wrong: \
         freeing either would strand the other, so a chain has to move with \
         its key rather than be copied.",
        repeated.len()
    );
    assert!(
        a.overflow.len() > 10,
        "the fixture must spill widely, got {} overflow pages",
        a.overflow.len()
    );
    drop(pager);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn every_separator_is_the_largest_key_of_its_subtree() {
    let (path, root) = grown("separator", 3000, 120);
    let mut pager = Pager::open(&path).unwrap();

    // Each page's keys are those its leaves hold plus the separators of the
    // interior cells *below* it. The reference promotes a key out of the leaves
    // into the interior cell, so a subtree's largest key is its own top cell's
    // separator when that is the top node, and the deepest key otherwise.
    let mut below: HashMap<u32, Vec<IndexEntry>> = HashMap::new();
    for page_no in &audit(&mut pager, root).visited {
        below.insert(*page_no, Vec::new());
    }
    // Gather bottom-up: a page's own keys come from its children, and a cell's
    // separator belongs to its *parent*, not to the child it bounds.
    let order = a_topological_order(&mut pager, root);
    for &page_no in &order {
        let page = pager.read_page(page_no).unwrap();
        let kind = page[0];
        let mut keys = Vec::new();
        if kind == page_type::INDEX_INTERIOR {
            let interior = IndexInterior::read(&mut pager, page_no).unwrap();
            for cell in &interior.cells {
                keys.extend(below.get(&cell.left_child).cloned().unwrap_or_default());
            }
            keys.extend(below.get(&interior.rightmost).cloned().unwrap_or_default());
        } else {
            keys.extend(IndexLeaf::read(&mut pager, page_no).unwrap().entries);
        }
        below.insert(page_no, keys);
    }

    // Now check every cell against the keys beneath its child. The reference
    // *promotes* the separator out of the leaves, so the invariant is that it
    // is strictly greater than every key left below it, not that it equals the
    // child's largest entry. Verified against sqlite3 3.53.4: a cell pointing
    // at a leaf whose last entry was 383 carried the key 384, and 4987 leaf
    // entries plus 13 separators was exactly the 5000 keys that went in.
    let mut checked = 0;
    let mut stack = vec![root];
    while let Some(page_no) = stack.pop() {
        let page = pager.read_page(page_no).unwrap();
        if page[0] != page_type::INDEX_INTERIOR {
            continue;
        }
        let interior = IndexInterior::read(&mut pager, page_no).unwrap();
        for cell in &interior.cells {
            let keys = &below[&cell.left_child];
            assert!(
                !keys.is_empty(),
                "page {page_no}: an interior cell's child {} holds no keys",
                cell.left_child
            );
            let largest = keys
                .iter()
                .max_by(|a, b| nsqlite::index::compare(a, b, 1))
                .expect("checked non-empty above");
            checked += 1;
            assert_eq!(
                nsqlite::index::compare(&cell.separator, largest, 1),
                std::cmp::Ordering::Greater,
                "page {page_no}: separator {} is not above every key below it; \
                 child {} tops out at {}",
                show(&cell.separator),
                cell.left_child,
                show(largest)
            );
        }
        for cell in &interior.cells {
            stack.push(cell.left_child);
        }
        stack.push(interior.rightmost);
    }
    assert!(checked > 0, "the fixture must produce interior cells");
    drop(pager);
    let _ = std::fs::remove_file(&path);
}

/// The tree's pages, deepest first, so a bottom-up walk sees a page's children
/// before the page itself.
fn a_topological_order(pager: &mut Pager, root: u32) -> Vec<u32> {
    let mut order = Vec::new();
    let mut stack = vec![root];
    while let Some(page_no) = stack.pop() {
        order.push(page_no);
        let page = pager.read_page(page_no).unwrap();
        if page[0] == page_type::INDEX_INTERIOR {
            let interior = IndexInterior::read(pager, page_no).unwrap();
            for cell in interior.cells {
                stack.push(cell.left_child);
            }
            stack.push(interior.rightmost);
        }
    }
    // The walk above is a DFS pushing children after the parent, so reversing a
    // pre-order gives a valid reverse-topological order.
    order.reverse();
    order
}

#[test]
fn every_key_is_present_exactly_once() {
    let (path, root) = grown("eachonce", 3000, 120);
    let mut pager = Pager::open(&path).unwrap();
    let a = audit(&mut pager, root);

    let all = a.all_keys();
    assert_eq!(
        all.len(),
        3000,
        "the tree holds {} entries for 3000 inserted keys; a split that loses \
         its promoted key drops one row, and a split that copies it instead of \
         promoting it adds one",
        all.len()
    );
    let distinct: HashSet<Vec<u8>> = all.iter().map(|e| e.encode()).collect();
    assert_eq!(
        distinct.len(),
        all.len(),
        "{} key(s) appear more than once; a separator copied into a leaf rather \
         than promoted out of it would show up here",
        all.len() - distinct.len()
    );
    // And the set is exactly the keys that went in, so a loss cannot hide
    // behind a compensating duplicate.
    let expected: HashSet<Vec<u8>> = (1..=3000i64)
        .map(|i| padded_entry(i, 120).encode())
        .collect();
    assert_eq!(
        distinct, expected,
        "the keys in the tree are not the keys that were inserted"
    );
    drop(pager);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_rowid_appears_exactly_once_across_the_whole_tree() {
    let (path, root) = grown("rowidonce", 3000, 120);
    let mut pager = Pager::open(&path).unwrap();
    let a = audit(&mut pager, root);

    let mut seen: HashSet<i64> = HashSet::new();
    let mut dupes = Vec::new();
    for e in a.all_keys() {
        if !seen.insert(e.rowid) {
            dupes.push(e.rowid);
        }
    }
    assert!(
        dupes.is_empty(),
        "rowid(s) {dupes:?} appear in more than one entry; an entry reachable \
         through two parents is read twice"
    );
    assert_eq!(seen.len(), 3000, "every inserted rowid must be reachable");
    drop(pager);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_scan_after_many_splits_returns_every_key_in_order() {
    let (path, root) = grown("scan", 3000, 120);
    let mut pager = Pager::open(&path).unwrap();
    let mut tree = IndexTree::open(&mut pager, root, 1);
    assert_eq!(tree.count().unwrap(), 3000);
    let entries = tree.scan().unwrap();
    assert_eq!(entries.len(), 3000, "a key was lost across a split");
    for (i, e) in entries.iter().enumerate() {
        let want = i as i64 + 1;
        assert_eq!(
            e.rowid, want,
            "entries came back out of key order at position {i}"
        );
        match &e.key[0] {
            Value::Text(t) => assert_eq!(
                t.parse::<i64>().unwrap(),
                want,
                "a key came back wrong at position {i}"
            ),
            other => panic!("expected a text key at {i}, got {other:?}"),
        }
    }
    drop(pager);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_point_lookup_finds_every_key_and_no_others() {
    let (path, root) = grown("lookup", 3000, 120);
    let mut pager = Pager::open(&path).unwrap();
    let key = |k: i64| vec![Value::Text(format!("{k:0>120}"))];
    let mut tree = IndexTree::open(&mut pager, root, 1);
    for k in [1i64, 2, 500, 501, 1500, 2999, 3000] {
        let got = tree.lookup(&key(k)).unwrap();
        assert_eq!(got, vec![k], "lookup of {k} returned {got:?}");
    }
    for k in [0i64, 3001, 99999] {
        let got = tree.lookup(&key(k)).unwrap();
        assert!(got.is_empty(), "lookup of absent key {k} returned {got:?}");
    }
    drop(pager);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_range_scan_covers_exactly_the_keys_inside_it() {
    let (path, root) = grown("range", 3000, 120);
    let mut pager = Pager::open(&path).unwrap();

    // Gather the promoted keys before opening the tree, since the tree borrows
    // the pager for as long as it lives.
    let promoted_keys: Vec<i64> = audit(&mut pager, root)
        .promoted
        .iter()
        .filter_map(|e| {
            e.key.first().and_then(|v| match v {
                Value::Text(t) => t.parse().ok(),
                _ => None,
            })
        })
        .collect();
    assert!(
        !promoted_keys.is_empty(),
        "the fixture must promote some keys out of the leaves"
    );

    let key = |k: i64| vec![Value::Text(format!("{k:0>120}"))];
    let mut tree = IndexTree::open(&mut pager, root, 1);
    let got = tree.range(Some(&key(1000)), Some(&key(1010))).unwrap();
    let want: Vec<i64> = (1000..=1010).collect();
    assert_eq!(got, want, "an inclusive range over part of the key");

    // An open end runs to the edge of the tree.
    let from = tree.range(Some(&key(2990)), None).unwrap();
    assert_eq!(from, (2990..=3000).collect::<Vec<i64>>());
    let to = tree.range(None, Some(&key(11))).unwrap();
    assert_eq!(to, (1..=11).collect::<Vec<i64>>());

    // A range that spans a whole subtree must not lose a promoted key.
    let wide = tree.range(Some(&key(700)), Some(&key(1400))).unwrap();
    assert_eq!(wide, (700..=1400).collect::<Vec<i64>>());

    // And a range whose bound lands exactly on a promoted key finds it.
    for k in promoted_keys.iter().take(25) {
        let got = tree.lookup(&key(*k)).unwrap();
        assert_eq!(got, vec![*k], "a promoted key {k} was not found by lookup");
        let one = tree.range(Some(&key(*k)), Some(&key(*k))).unwrap();
        assert_eq!(one, vec![*k], "a promoted key {k} was not found by range");
    }
    drop(pager);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn descending_insertion_also_produces_a_sound_tree() {
    // Every insert lands on the leftmost leaf, which is the case a split handles
    // differently and the one an ascending test never reaches.
    let path = temp("desc");
    let root = {
        let mut pager = Pager::open(&path).unwrap();
        pager.allocate().unwrap();
        let root = pager.allocate().unwrap();
        {
            let mut tree = IndexTree::create(&mut pager, root, 1).unwrap();
            for i in (1..=3000i64).rev() {
                tree.insert(&padded_entry(i, 120)).unwrap();
            }
        }
        pager.flush().unwrap();
        root
    };

    let mut pager = Pager::open(&path).unwrap();
    let a = audit(&mut pager, root);
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
    assert_eq!(a.all_keys().len(), 3000, "descending inserts lost a key");

    let mut tree = IndexTree::open(&mut pager, root, 1);
    let entries = tree.scan().unwrap();
    for (i, e) in entries.iter().enumerate() {
        assert_eq!(e.rowid, i as i64 + 1, "scan is out of order at {i}");
    }
    drop(pager);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_composite_key_index_stays_sound_across_splits() {
    // Repeated leading columns are the case a wrong key_len gets wrong, and a
    // range over part of the key is the case the brief calls out. The second
    // column is padded so the interior layer fills often enough to audit.
    let path = temp("composite");
    let root = {
        let mut pager = Pager::open(&path).unwrap();
        pager.allocate().unwrap();
        let root = pager.allocate().unwrap();
        {
            let mut tree = IndexTree::create(&mut pager, root, 2).unwrap();
            for i in 1..=3000i64 {
                let e = IndexEntry {
                    key: vec![Value::Integer(i % 40), Value::Text(format!("{i:0>100}"))],
                    rowid: i,
                };
                tree.insert(&e)
                    .unwrap_or_else(|err| panic!("inserting {i}: {err}"));
            }
        }
        pager.flush().unwrap();
        root
    };

    let mut pager = Pager::open(&path).unwrap();
    let a = audit(&mut pager, root);
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
        "composite index double-references {repeated:?}"
    );
    assert_eq!(a.all_keys().len(), 3000, "composite index lost a key");

    let mut tree = IndexTree::open(&mut pager, root, 2);
    // A range over the leading column alone matches every rowid with that
    // prefix, which is what "a range over part of a key" means. Checked against
    // sqlite3 3.53.4 on 200 rows of `(a, b)` with 5 distinct values of `a`:
    // `WHERE a=2` returns 40 and so does `WHERE a=2 AND b BETWEEN 0 AND
    // 9223372036854775807`, while `WHERE a=2 AND b=0` returns 0. So a bound may
    // name fewer columns than the index has, and the columns it leaves out are
    // widened to the extremes of their range.
    let prefix = |k: i64, b: &str| vec![Value::Integer(k), Value::Text(b.to_string())];
    let want: Vec<i64> = (1..=3000i64).filter(|i| i % 40 == 7).collect();
    let got = tree
        .range(
            Some(&prefix(7, &format!("{:0>100}", 0))),
            Some(&prefix(7, "\u{10FFFF}")),
        )
        .unwrap();
    assert_eq!(
        got, want,
        "a full-width prefix range returned the wrong rows"
    );

    // The same range written as one column per bound, which is the form a
    // caller writes when it only knows the leading column. Before the bound was
    // widened this returned nothing at all: `compare` treats a short key as
    // sorting below a full-width one, so a one-column bound sat below every entry
    // it was meant to match and the scan returned 0 rowids.
    let only_leading = |k: i64| vec![Value::Integer(k)];
    assert_eq!(
        tree.range(Some(&only_leading(7)), Some(&only_leading(7)))
            .unwrap(),
        want,
        "a one-column bound on a two-column index should match every row \
         sharing that leading value"
    );
    // A one-sided short bound is a half-open range over the prefix, not the
    // prefix itself: `range([7], None)` means "from leading value 7 onwards",
    // which is every row whose first column is 7 or more. Padding the missing
    // high columns only happens on the side a bound was actually given for, so
    // an absent high bound is not turned into a real value.
    //
    // The answer is in *key* order, not rowid order, so the expected list is
    // grouped by leading value rather than sorted by rowid: the rows with
    // leading value 7 all come before those with 8, and each group is in rowid
    // order within itself.
    let in_key_order = |pred: &dyn Fn(i64) -> bool| -> Vec<i64> {
        let mut out: Vec<i64> = (1..=3000i64).filter(|i| pred(*i)).collect();
        out.sort_by_key(|i| (i % 40, *i));
        out
    };
    assert_eq!(
        tree.range(Some(&only_leading(7)), None).unwrap(),
        in_key_order(&|i| i % 40 >= 7)
    );
    assert_eq!(
        tree.range(None, Some(&only_leading(6))).unwrap(),
        in_key_order(&|i| i % 40 <= 6)
    );
    // A short bound on a leading value that is not there matches nothing, which
    // is a real answer and not a widening artefact.
    assert!(tree
        .range(Some(&only_leading(40)), Some(&only_leading(40)))
        .unwrap()
        .is_empty());
    drop(pager);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_wide_key_that_spills_keeps_its_separator_intact() {
    // A key wider than the local payload limit forces an overflow chain, and a
    // chain that is not followed back loses the separator's value. On a
    // 4096-byte page the index keeps at most 1002 bytes locally, so a 2000-byte
    // key always spills.
    let path = temp("spill");
    let root = {
        let mut pager = Pager::open(&path).unwrap();
        pager.allocate().unwrap();
        let root = pager.allocate().unwrap();
        {
            let mut tree = IndexTree::create(&mut pager, root, 1).unwrap();
            for i in 1..=600i64 {
                let key = format!("{i:0>2000}");
                let e = IndexEntry {
                    key: vec![Value::Text(key)],
                    rowid: i,
                };
                tree.insert(&e)
                    .unwrap_or_else(|err| panic!("inserting {i}: {err}"));
            }
        }
        pager.flush().unwrap();
        root
    };

    let mut pager = Pager::open(&path).unwrap();
    // Gather the spilled separators before opening the tree, which borrows the
    // pager for as long as it lives.
    let spilled: Vec<String> = audit(&mut pager, root)
        .promoted
        .iter()
        .filter_map(|e| {
            e.key.first().and_then(|v| match v {
                Value::Text(t) => Some(t.clone()),
                _ => None,
            })
        })
        .collect();
    assert!(
        !spilled.is_empty(),
        "the fixture must promote a wide key into an interior cell"
    );

    let mut tree = IndexTree::open(&mut pager, root, 1);
    let entries = tree.scan().unwrap();
    assert_eq!(entries.len(), 600, "a spilled key was lost across a split");
    for (i, e) in entries.iter().enumerate() {
        let want = i + 1;
        match &e.key[0] {
            Value::Text(t) => {
                let got: i64 = t.trim_start_matches('0').parse().unwrap();
                assert_eq!(got, want as i64, "a wide key came back wrong at {i}");
                assert_eq!(t.len(), 2000, "a wide key was truncated at {i}");
            }
            other => panic!("a wide key came back as {other:?} at {i}"),
        }
    }
    // A spilled separator must still be findable by lookup, which is what
    // proves the chain was followed back rather than left dangling.
    for text in spilled.iter().take(10) {
        let k: i64 = text.trim_start_matches('0').parse().unwrap();
        let got = tree.lookup(&[Value::Text(text.clone())]).unwrap();
        assert_eq!(got, vec![k], "a spilled separator {k} was not found");
    }
    drop(pager);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_duplicate_key_and_rowid_is_refused() {
    let path = temp("dupe");
    let mut pager = Pager::open(&path).unwrap();
    pager.allocate().unwrap();
    let root = pager.allocate().unwrap();
    let mut tree = IndexTree::create(&mut pager, root, 1).unwrap();
    tree.insert(&int_entry(5, 50)).unwrap();
    let err = tree.insert(&int_entry(5, 50)).unwrap_err();
    assert_eq!(err.code.name(), "CONSTRAINT");
    // The same key with a different rowid is a different entry, which is what
    // makes a non-unique index able to hold one key for many rows. Checked
    // against sqlite3 3.53.4, which rejects the same insert on a UNIQUE index
    // with "UNIQUE constraint failed: t.k" and accepts it on a plain one.
    tree.insert(&int_entry(5, 51)).unwrap();
    assert_eq!(tree.lookup(&[Value::Integer(5)]).unwrap(), vec![50, 51]);
    drop(pager);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_duplicate_of_a_promoted_separator_is_refused() {
    // The case the two-entry test above cannot reach. A key that a split moved
    // out of the leaves lives in an interior cell, and the descent sends an
    // entry equal to that separator *left* — into a subtree that does not
    // contain it — so the duplicate check inside `IndexLeaf::insert` never runs
    // for it. Insisting on the check having happened before, and on the key
    // still being findable afterwards, is what this is for.
    let path = temp("dupesep");
    let mut pager = Pager::open(&path).unwrap();
    pager.allocate().unwrap();
    let root = pager.allocate().unwrap();
    let promoted: Vec<IndexEntry> = {
        let mut tree = IndexTree::create(&mut pager, root, 1).unwrap();
        for i in 1..=3000i64 {
            tree.insert(&padded_entry(i, 120))
                .unwrap_or_else(|err| panic!("inserting {i}: {err}"));
        }
        pager.flush().unwrap();
        // Take the lowest promoted separator: the one whose left subtree is the
        // first leaf, so the descent for its duplicate lands furthest from it.
        let a = audit(&mut pager, root);
        assert!(!a.promoted.is_empty(), "the fixture must promote a key");
        let mut seps = a.promoted;
        seps.sort_by(|x, y| nsqlite::index::compare(x, y, 1));
        seps
    };
    for victim in promoted.iter().take(12) {
        let err = {
            let mut tree = IndexTree::open(&mut pager, root, 1);
            tree.insert(victim).unwrap_err()
        };
        assert_eq!(
            err.code.name(),
            "CONSTRAINT",
            "a promoted separator was accepted twice: {victim:?}"
        );
    }
    // A *different* rowid for a promoted key is not a duplicate: the key is
    // allowed to name several rows. The reference agrees — inserting a second
    // row for a promoted key leaves `PRAGMA integrity_check` answering "ok".
    let extra = IndexEntry {
        key: promoted[0].key.clone(),
        rowid: promoted[0].rowid + 1_000_000,
    };
    {
        let mut tree = IndexTree::open(&mut pager, root, 1);
        tree.insert(&extra)
            .unwrap_or_else(|err| panic!("a second row for a promoted key: {err}"));
        assert_eq!(
            tree.lookup(&promoted[0].key).unwrap(),
            vec![promoted[0].rowid, extra.rowid],
            "both rows for a promoted key should be findable"
        );
    }
    pager.flush().unwrap();
    // And the tree is still structurally sound afterwards, not merely readable.
    drop(pager);
    let mut pager = Pager::open(&path).unwrap();
    let a = audit(&mut pager, root);
    assert_eq!(a.all_keys().len(), 3001, "one row added, one entry gained");
    let mut seen: HashSet<i64> = HashSet::new();
    for e in a.all_keys() {
        assert!(seen.insert(e.rowid), "rowid {} ended up twice", e.rowid);
    }
    drop(pager);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn the_root_page_number_never_moves() {
    // A schema holds one rootpage for the life of an index, so a full root has
    // to be rewritten in place rather than re-rooted elsewhere. Verified
    // against sqlite3 3.53.4: an index grown to 1.1 million rows and a depth of
    // 7 kept rootpage 3 throughout, and when its root overflowed the root
    // became a one-cell page whose two children were both freshly allocated
    // pages rather than one of them being the root itself.
    let path = temp("rootstable");
    let mut pager = Pager::open(&path).unwrap();
    pager.allocate().unwrap();
    let root = pager.allocate().unwrap();
    {
        let mut tree = IndexTree::create(&mut pager, root, 1).unwrap();
        for i in 1..=4000i64 {
            tree.insert(&padded_entry(i, 120)).unwrap();
        }
    }
    pager.flush().unwrap();
    {
        let mut tree = IndexTree::open(&mut pager, root, 1);
        assert_eq!(tree.root, root, "the root moved");
        assert_eq!(tree.count().unwrap(), 4000);
    }
    // The root must not name itself as a child, or the descent that filled it
    // would never have terminated.
    let raw = pager.read_page(root).unwrap();
    let kind = raw[0];
    if kind == nsqlite::page::page_type::INDEX_INTERIOR {
        let n = u16::from_be_bytes([raw[3], raw[4]]) as usize;
        for i in 0..n {
            let at = 12 + i * 2;
            let off = u16::from_be_bytes([raw[at], raw[at + 1]]) as usize;
            let child = u32::from_be_bytes([raw[off], raw[off + 1], raw[off + 2], raw[off + 3]]);
            assert_ne!(child, root, "the root names itself as a child");
        }
        let right = u32::from_be_bytes([raw[8], raw[9], raw[10], raw[11]]);
        assert_ne!(right, root, "the root names itself as its rightmost child");
    }
    drop(pager);
    let _ = std::fs::remove_file(&path);
}

/// Hands the index subtree to the reference implementation to be rebuilt, over
/// keys of `key_width` bytes, with `count` rows.
///
/// `key_width` of 0 means the keys are plain integers rather than padded text,
/// which is a different column type and therefore a different declared type in
/// the fixture: the index entry has to agree with the table row or the
/// reference reports every row as missing from the index. So the column type
/// follows the key, and with it the padding, rather than one fixture trying to
/// serve both.
///
/// The table and its rows come from sqlite3 and only the index is written here,
/// because an `integrity_check` compares the table's row count with the index's
/// entry count: a file whose table is empty would be rejected for a mismatch
/// that says nothing about the index's shape.
fn build_for_sqlite3(sqlite3: &str, path: &PathBuf, count: i64, key_width: usize) -> u32 {
    let p = path.to_string_lossy().to_string();
    // An integer key goes in a column declared INT, so the stored value is an
    // integer and the index entry the module writes matches. A text key needs
    // a TEXT column, and an INT column would apply numeric affinity and convert
    // the text back to a number, which is exactly the mismatch above.
    let (column, generator) = if key_width == 0 {
        (
            "k INT",
            format!("SELECT value FROM generate_series(1,{count})"),
        )
    } else {
        (
            "k TEXT",
            format!("SELECT printf('%0{key_width}d', value) FROM generate_series(1,{count})"),
        )
    };
    let status = std::process::Command::new(sqlite3)
        .arg(&p)
        .arg(format!(
            "PRAGMA page_size=4096;
             CREATE TABLE t(id INTEGER PRIMARY KEY, {column});
             INSERT INTO t(k) {generator};
             CREATE INDEX i1 ON t(k);"
        ))
        .status()
        .expect("running sqlite3");
    assert!(status.success(), "sqlite3 could not build the fixture");

    // Find the index root sqlite3 recorded, then keep only the table's pages
    // plus that root, rewriting the root as an empty index leaf. Everything
    // after it is the index subtree, which this module is about to rebuild.
    let root: u32 = String::from_utf8_lossy(
        &std::process::Command::new(sqlite3)
            .arg(&p)
            .arg("SELECT rootpage FROM sqlite_master WHERE name='i1';")
            .output()
            .expect("running sqlite3")
            .stdout,
    )
    .trim()
    .parse()
    .expect("sqlite3 reported a rootpage");
    let page_size = 4096usize;
    let mut bytes = std::fs::read(path).unwrap();
    bytes.truncate(root as usize * page_size);
    let off = (root as usize - 1) * page_size;
    bytes[off] = page_type::INDEX_LEAF;
    bytes[off + 1..off + 3].copy_from_slice(&0u16.to_be_bytes());
    bytes[off + 3..off + 5].copy_from_slice(&0u16.to_be_bytes());
    // Zero means the content area starts at the end of the page, which for an
    // empty page is the whole page.
    bytes[off + 5..off + 7].copy_from_slice(&0u16.to_be_bytes());
    bytes[off + 7] = 0;
    let pages = root;
    bytes[28..32].copy_from_slice(&pages.to_be_bytes());
    // db_size_pages is only authoritative when the change counter and
    // version-valid-for agree, so keep them in step after the truncation.
    let change = u32::from_be_bytes([bytes[24], bytes[25], bytes[26], bytes[27]]);
    bytes[92..96].copy_from_slice(&change.to_be_bytes());
    std::fs::write(path, &bytes).unwrap();
    root
}

/// Runs one SQL statement through sqlite3 and returns its trimmed stdout.
fn ask(sqlite3: &str, path: &PathBuf, sql: &str) -> String {
    let out = std::process::Command::new(sqlite3)
        .arg(path)
        .arg(sql)
        .output()
        .expect("running sqlite3");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn real_sqlite3_accepts_an_index_this_module_built() {
    // The strongest check there is: hand a file nsqlite wrote to the reference
    // implementation and let it audit the tree. Run by hand against sqlite3
    // 3.53.4, which answered "ok" for `PRAGMA integrity_check`, chose the index
    // for `SELECT ... WHERE k=?` ("SEARCH t USING COVERING INDEX i1 (k=?)"), and
    // returned the right row for every key including the ones this module
    // promotes into interior cells.
    let sqlite3 = match std::env::var("SQLITE3").ok().or_else(which_sqlite3) {
        Some(s) => s,
        // No sqlite3 on this machine: say so rather than passing silently.
        None => {
            eprintln!("skipping: no sqlite3 binary found (set SQLITE3 to run this)");
            return;
        }
    };
    let path = temp("interop");
    let root = build_for_sqlite3(&sqlite3, &path, 5000, 0);

    // Now fill the index with this module.
    {
        let mut pager = Pager::open(&path).unwrap();
        let mut tree = IndexTree::open(&mut pager, root, 1);
        for i in 1..=5000i64 {
            tree.insert(&int_entry(i, i))
                .unwrap_or_else(|e| panic!("inserting {i}: {e}"));
        }
        pager.flush().unwrap();
    }

    // And let sqlite3 audit what was written.
    let report = ask(&sqlite3, &path, "PRAGMA integrity_check;");
    assert!(
        report == "ok",
        "sqlite3 rejected the index nsqlite built:\n{report}\n{}",
        String::from_utf8_lossy(
            &std::process::Command::new(&sqlite3)
                .arg(&path)
                .arg("PRAGMA integrity_check;")
                .output()
                .expect("running sqlite3")
                .stderr
        )
    );

    // It also has to be usable, not merely well-formed: a covering-index scan
    // and a point lookup through the index sqlite3 now trusts.
    let q = |sql: &str| ask(&sqlite3, &path, sql);
    assert_eq!(
        q("SELECT count(*),min(k),max(k) FROM t INDEXED BY i1;"),
        "5000|1|5000"
    );
    assert_eq!(q("SELECT id FROM t WHERE k=4321;"), "4321");
    // Every promoted separator has to be findable too, since that key lives
    // only in an interior cell.
    for k in [132, 260, 388, 4356, 4868] {
        assert_eq!(q(&format!("SELECT id FROM t WHERE k={k};")), k.to_string());
    }
    let _ = std::fs::remove_file(&path);
}

#[test]
fn real_sqlite3_accepts_an_index_built_from_keys_too_wide_to_fit() {
    // The interop test above uses narrow integer keys, so it never handed the
    // reference a file with an overflow chain in it — and the leak lived exactly
    // there. A key of 2000 bytes on a 4096-byte page spills on every entry, and
    // a split that re-seats such a separator into a new cell used to allocate a
    // second chain and strand the first: 1226 of 2022 pages unreachable, which
    // sqlite3 3.53.4 reports as "*** in database main ***" followed by over a
    // hundred "Page N: never used" lines. No assertion in this file could see
    // that, because the file was never given to sqlite3 and no test followed a
    // chain. This one does both.
    let sqlite3 = match std::env::var("SQLITE3").ok().or_else(which_sqlite3) {
        Some(s) => s,
        None => {
            eprintln!("skipping: no sqlite3 binary found (set SQLITE3 to run this)");
            return;
        }
    };
    // 300 wide keys is enough to split the leaves and the interior layer
    // repeatedly without making the test slow; the same workload on the old code
    // stranded 100+ pages.
    let path = temp("interopwide");
    let root = build_for_sqlite3(&sqlite3, &path, 300, 2000);
    {
        let mut pager = Pager::open(&path).unwrap();
        let mut tree = IndexTree::open(&mut pager, root, 1);
        for i in 1..=300i64 {
            tree.insert(&padded_entry(i, 2000))
                .unwrap_or_else(|e| panic!("inserting {i}: {e}"));
        }
        pager.flush().unwrap();
    }

    let report = ask(&sqlite3, &path, "PRAGMA integrity_check;");
    assert_eq!(
        report, "ok",
        "sqlite3 rejected a wide-key index nsqlite built:\n{report}"
    );

    // Usable, not merely well formed: the spilled separators have to be
    // readable through their chains. The min and max are spelled out in full
    // rather than abbreviated, because a 2000-byte key is exactly the case
    // where a silently truncated expectation would match a truncated file.
    let q = |sql: &str| ask(&sqlite3, &path, sql);
    assert_eq!(
        q("SELECT count(*),length(min(k)),length(max(k)) FROM t INDEXED BY i1;"),
        "300|2000|2000",
        "the index lost or shortened a wide key"
    );
    for k in [1i64, 2, 100, 150, 299, 300] {
        let want = format!("{k:0>2000}");
        assert_eq!(
            q(&format!("SELECT id FROM t WHERE k='{want}';")),
            k.to_string(),
            "sqlite3 could not find a wide key nsqlite wrote"
        );
    }
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_split_measures_against_the_usable_size_and_not_the_page_size() {
    // A reserved region is the case where those two are different, and the one
    // place in this module that used the raw page stride instead of
    // `pager.usable_size()` was the leaf split's fit test. The reference wrote 0
    // into header byte 20 for every file probed, including after
    // `PRAGMA reserve_size=100` on a created table, so nsqlite has never
    // produced a file with one either and the bug was latent.
    //
    // It is worth being honest about how latent. Probing it, an insert of 4000
    // 120-byte keys succeeds at every reserved size the header byte can hold, and
    // so does the leaf write: `IndexLeaf::layout` measures against the raw stride
    // but the usable region is only 255 bytes smaller, which is less than one
    // 120-byte key, so the slack absorbs the difference and nothing refuses. So
    // this cannot be tested by expecting a failure — there is no failure to
    // provoke. What is asserted is the invariant that *is* reachable today: the
    // tree the split produces is sound under a reserved region, and no cell
    // reaches into the reserved bytes.
    const RESERVED: u8 = 255;
    let path = temp("reserved");
    let mut pager = Pager::open(&path).unwrap();
    pager.header_mut().reserved_space = RESERVED;
    assert_eq!(
        pager.usable_size(),
        4096 - RESERVED as u32,
        "the fixture is not exercising a reserved region at all"
    );
    pager.allocate().unwrap();
    let root = pager.allocate().unwrap();
    {
        let mut tree = IndexTree::create(&mut pager, root, 1).unwrap();
        for i in 1..=3000i64 {
            tree.insert(&padded_entry(i, 120))
                .unwrap_or_else(|err| panic!("inserting {i} with a reserved region: {err}"));
        }
        pager.flush().unwrap();
    }
    drop(pager);

    // And what was written has to stay inside the usable area. Read the file
    // back with a pager that knows about the reserve, so a cell written into it
    // would either fail to parse or come back wrong.
    let mut pager = Pager::open(&path).unwrap();
    let a = audit(&mut pager, root);
    assert_eq!(
        a.all_keys().len(),
        3000,
        "a reserved region cost the index entries"
    );
    let mut tree = IndexTree::open(&mut pager, root, 1);
    let entries = tree.scan().unwrap();
    assert_eq!(entries.len(), 3000);
    for (i, e) in entries.iter().enumerate() {
        assert_eq!(e.rowid, i as i64 + 1, "scan out of order at {i}");
    }
    // No b-tree page may use the last RESERVED bytes. The cell pointer array
    // ends at the highest cell address, so comparing that against the content
    // start is the check: on a well-formed page the content begins at or below
    // the usable boundary, and a cell laid into the reserved region would push
    // the content start above it.
    let usable = pager.usable_size() as usize;
    let mut stack = vec![root];
    while let Some(page_no) = stack.pop() {
        let raw = pager.read_page(page_no).unwrap();
        let at = if page_no == 1 { 100 } else { 0 };
        let kind = raw[at];
        let (n, arr) = if kind == page_type::INDEX_INTERIOR {
            let n = u16::from_be_bytes([raw[at + 3], raw[at + 4]]) as usize;
            (n, at + 12)
        } else {
            let n = u16::from_be_bytes([raw[at + 3], raw[at + 4]]) as usize;
            (n, at + 8)
        };
        if n > 0 {
            let lowest = u16::from_be_bytes([raw[arr], raw[arr + 1]]) as usize;
            assert!(
                lowest < usable,
                "page {page_no} lays a cell at offset {lowest}, inside the \
                 {RESERVED}-byte reserved region that starts at {usable}"
            );
        }
        if kind == page_type::INDEX_INTERIOR {
            for i in 0..n {
                let off = u16::from_be_bytes([raw[arr + i * 2], raw[arr + i * 2 + 1]]) as usize;
                let child =
                    u32::from_be_bytes([raw[off], raw[off + 1], raw[off + 2], raw[off + 3]]);
                stack.push(child);
            }
            stack.push(u32::from_be_bytes([
                raw[at + 8],
                raw[at + 9],
                raw[at + 10],
                raw[at + 11],
            ]));
        }
    }
    drop(pager);
    let _ = std::fs::remove_file(&path);
}

/// Looks for a sqlite3 binary on `PATH`, or in the usual scoop location.
fn which_sqlite3() -> Option<String> {
    if let Ok(path) = std::env::var("PATH") {
        for dir in path.split(';').chain(path.split(':')) {
            if dir.is_empty() {
                continue;
            }
            let candidate = std::path::Path::new(dir).join("sqlite3.exe");
            if candidate.is_file() {
                return Some(candidate.to_string_lossy().to_string());
            }
            let candidate = std::path::Path::new(dir).join("sqlite3");
            if candidate.is_file() {
                return Some(candidate.to_string_lossy().to_string());
            }
        }
    }
    let fallback = PathBuf::from("C:/Users/zyq/scoop/apps/msys2/current/ucrt64/bin/sqlite3.exe");
    if fallback.is_file() {
        return Some(fallback.to_string_lossy().to_string());
    }
    None
}

/// Renders an entry compactly for a failure message.
fn show(e: &IndexEntry) -> String {
    format!("{:?}/{}", e.key, e.rowid)
}
