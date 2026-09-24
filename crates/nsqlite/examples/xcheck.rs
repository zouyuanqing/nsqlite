use nsqlite::tokenizer::Tokenizer;
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    for src in args {
        match Tokenizer::tokenize_all(&src) {
            Ok(ts) => println!(
                "OK   {src:?} -> {:?}",
                ts.iter().map(|(t, _)| t).collect::<Vec<_>>()
            ),
            Err(e) => println!("ERR  {src:?} -> {}", e.message),
        }
    }
}
