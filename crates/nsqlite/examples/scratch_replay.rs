use nsqlite::journal;
use nsqlite::pager::Pager;
use nsqlite::table_tree::{Row, TableTree};
use nsqlite::value::Value;

fn page2(path: &std::path::Path) -> Vec<u8> {
    let d = std::fs::read(path).unwrap();
    d[4096..8192].to_vec()
}

fn main() {
    let d = std::env::temp_dir().join("nsqlite-scratch-replay");
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let path = d.join("a.db");

    {
        let mut p = Pager::open(&path).unwrap();
        let leaf = nsqlite::btree_write::LeafPage::empty(2, 4096);
        leaf.write_to(&mut p).unwrap();
        p.claim_page(2).unwrap();
        p.write_header().unwrap();
        p.flush().unwrap();
        let mut tree = TableTree::open(&mut p, 2).unwrap();
        tree.insert(&mut p, &Row { rowid: 1, values: vec![Value::Text("one".into()), Value::Integer(1)] }).unwrap();
        p.flush().unwrap();
    }
    let committed = page2(&path);
    println!("committed page2[100..130] = {:02x?}", &committed[0..24]);

    let mut p = Pager::open(&path).unwrap();
    p.begin_journal().unwrap();
    let mut tree = TableTree::open(&mut p, 2).unwrap();
    tree.insert(&mut p, &Row { rowid: 2, values: vec![Value::Text("ghost".into()), Value::Integer(2)] }).unwrap();
    p.flush().unwrap();
    let crashed = page2(&path);
    println!("crashed  page2[100..130] = {:02x?}", &crashed[0..24]);
    drop(p);
    println!("on-disk  page2[100..130] = {:02x?}", &page2(&path)[0..24]);

    // Reopen with the pager only.
    let mut p2 = Pager::open(&path).unwrap();
    let restored = p2.read_page(2).unwrap();
    println!("restored page2[100..130] = {:02x?}", &restored[0..24]);
    println!("restored == committed ? {}", restored == committed);
    let diffs: Vec<usize> = (0..4096).filter(|&i| restored[i] != committed[i]).collect();
    println!("differing bytes: {}", diffs.len());
    for &i in diffs.iter().take(8) {
        println!("  at {i}: committed={:02x} restored={:02x}", committed[i], restored[i]);
    }
    println!("journal gone? {}", !journal::journal_path(&path).exists());
    println!("file pages after recovery: {}", std::fs::metadata(&path).unwrap().len() / 4096);
    let _ = std::fs::remove_dir_all(&d);
}
