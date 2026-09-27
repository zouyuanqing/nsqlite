use nsqlite::connection::Connection;
use nsqlite::connection::Outcome;

fn main() {
    let queries = [
        "SELECT BB FROM Users",
        "SELECT Users.Bb FROM Users",
        "SELECT a.Bb FROM Users a",
        "SELECT Bb AS z FROM Users",
        "SELECT Bb+1 FROM Users",
        "SELECT Cc FROM Users",
        "SELECT t.b FROM t",
        "SELECT x FROM t, p",
        "SELECT p.x FROM t,p",
        "SELECT a.y FROM p AS a",
        "SELECT b AS bb FROM t ORDER BY BB",
        "SELECT Users.Cc, Users.Bb FROM Users",
        "SELECT u.Bb FROM Users u, Users v",
        "SELECT Bb FROM Users JOIN t",
        "SELECT Bb FROM Users JOIN t ON 1",
        "SELECT Bb AS Bb FROM Users",
        "SELECT (SELECT 1) AS q, Bb FROM Users",
        "SELECT Bb COLLATE NOCASE FROM Users",
    ];
    let mut c = Connection::open_memory().unwrap();
    c.execute_script("CREATE TABLE Users(Bb TEXT, Cc INT);")
        .unwrap();
    c.execute_script("CREATE TABLE t(a,b);").unwrap();
    c.execute_script("CREATE TABLE p(x,y);").unwrap();
    c.execute_script("INSERT INTO t VALUES(1,10),(2,20);")
        .unwrap();
    c.execute_script("INSERT INTO p VALUES(5,6);").unwrap();
    c.execute_script("INSERT INTO Users VALUES('u',1);")
        .unwrap();
    for q in queries {
        for (s, f) in [("on", "off"), ("off", "on"), ("off", "off"), ("on", "on")] {
            let _ = c.execute_script(&format!("PRAGMA short_column_names={s}"));
            let _ = c.execute_script(&format!("PRAGMA full_column_names={f}"));
            let out = match c.execute_script(q) {
                Ok(v) => match v.first() {
                    Some(Outcome::Query { columns, .. }) => columns.join("|"),
                    Some(Outcome::Changed(n)) => format!("changed {n}"),
                    _ => "nothing".into(),
                },
                Err(e) => format!("ERR {e}"),
            };
            println!("{q:40} | short={s:3} full={f:3} | {out}");
        }
    }
}
