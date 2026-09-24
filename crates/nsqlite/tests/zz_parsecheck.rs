#[test]
fn parsecheck_dump() {
    let qs = [
        "SELECT * FROM a, b WHERE a.x = b.x",
        "SELECT * FROM a JOIN b ON a.x = b.x",
        "SELECT * FROM a LEFT JOIN b ON a.x = b.x",
        "SELECT * FROM a CROSS JOIN b",
        "SELECT a.x, b.y FROM a INNER JOIN b ON a.x = b.y ORDER BY a.x",
        "SELECT * FROM a JOIN b USING (x)",
        "SELECT * FROM a AS p JOIN b q ON p.x = q.y",
        "SELECT * FROM a, b, c",
    ];
    for q in qs {
        match nsqlite::parser::parse_one(q) {
            Ok(s) => println!("OK   {q}\n     => {s:?}"),
            Err(e) => println!("ERR  {q}\n     => {e}"),
        }
    }
}
