use nsqlite::tokenizer::Tokenizer;
fn main() {
    for src in ["1e+ 2", "1e+1_ 2", "1e- ", "1e+", "0x1e+ "] {
        let mut t = Tokenizer::new(src);
        println!("--- src = {src:?}");
        loop {
            match t.next_token() {
                Ok(Some((tok, sp))) => {
                    println!(
                        "  {:?} col={} start={} end={}",
                        tok, sp.col, sp.start, sp.end
                    )
                }
                Ok(None) => {
                    println!("  <eof>");
                    break;
                }
                Err(e) => println!("  ERR {}", e),
            }
        }
    }
}
