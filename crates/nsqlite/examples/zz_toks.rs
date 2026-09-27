use nsqlite::tokenizer::Tokenizer;
fn main() {
    for sql in [
        "SELECT a.y FROM a JOIN b ON a.x = b.x",
        "SELECT a.y FROM a JOIN b ON a.x = b.x ORDER BY a.x",
    ] {
        println!("== {sql}");
        let mut t = Tokenizer::new(sql);
        while let Ok(Some((tok, span))) = t.next_token() {
            println!(
                "   {:?}   @{}..{}  {:?}",
                tok,
                span.start,
                span.end,
                &sql[span.start as usize..span.end as usize]
            );
        }
    }
}
