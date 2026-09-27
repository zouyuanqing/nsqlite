#[test]
fn probe_where() {
    let mut c = nsqlite::connection::Connection::open_memory().unwrap();
    c.execute_script("CREATE TABLE t(a,b); INSERT INTO t VALUES(1,10),(2,20),(3,30);")
        .unwrap();
    for sql in [
        "SELECT b AS bb FROM t ORDER BY BB",
        "SELECT b AS bb FROM t GROUP BY BB",
        "SELECT b AS bb FROM t WHERE BB=1",
        "SELECT b AS bb FROM t HAVING BB=1",
    ] {
        match c.execute_script(sql) {
            Ok(_) => eprintln!("OK   {sql}"),
            Err(e) => eprintln!("ERR  {sql}  ->  {e}"),
        }
    }
}
