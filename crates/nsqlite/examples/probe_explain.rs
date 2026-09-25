use nsqlite::connection::Connection;
fn main() {
    let c = Connection::open_in_memory().unwrap();
    for sql in [
        "CREATE TABLE t1(a,b,c)",
        "CREATE INDEX i1 ON t1(b)",
        "EXPLAIN SELECT * FROM t1 WHERE b=1",
        "EXPLAIN QUERY PLAN SELECT * FROM t1 WHERE b=1",
    ] {
        match c.execute_batch(sql) {
            Ok(v) => println!("OK   {sql}  => {v:?}"),
            Err(e) => println!("ERR  {sql}  => {e}"),
        }
    }
}
