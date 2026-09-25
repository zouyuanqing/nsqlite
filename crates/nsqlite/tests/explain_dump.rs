//! Prints the plan this engine produces for every case in the test file, so the
//! expectations can be diffed against the real sqlite3 in one pass.
//!
//! Run with:  cargo test -p nsqlite --test explain -- --ignored --nocapture
use nsqlite::affinity::Affinity;
use nsqlite::catalog::{Catalog, Column, Index, Table};
use nsqlite::connection::Outcome;
use nsqlite::explain;

fn table(name: &str, columns: &[&str]) -> Table {
    Table {
        name: name.to_string(),
        columns: columns
            .iter()
            .map(|c| Column {
                name: (*c).to_string(),
                declared_type: String::new(),
                affinity: Affinity::Text,
                not_null: false,
                default: None,
                rowid_alias: false,
            })
            .collect(),
        rowid_alias: None,
        without_rowid: false,
        root_page: 0,
    }
}

fn index(name: &str, table: &str, columns: &[&str], unique: bool) -> Index {
    Index {
        name: name.to_string(),
        table: table.to_string(),
        columns: columns.iter().map(|c| (*c).to_string()).collect(),
        ascending: columns.iter().map(|_| true).collect(),
        unique,
        root_page: 0,
    }
}

fn catalog(tables: Vec<Table>, indexes: Vec<Index>) -> Catalog {
    let mut c = Catalog::new();
    for t in tables {
        c.put(t);
    }
    for i in indexes {
        c.put_index(i);
    }
    c
}

fn base() -> (Vec<&'static str>, Catalog) {
    let ddl = vec![
        "CREATE TABLE t1(a, b, c)",
        "CREATE TABLE t2(x, y, z)",
        "CREATE TABLE t3(p, q)",
        "CREATE INDEX i1 ON t1(b)",
        "CREATE INDEX i2 ON t1(b, c)",
        "CREATE INDEX i3 ON t3(p, q)",
    ];
    let cat = catalog(
        vec![
            table("t1", &["a", "b", "c"]),
            table("t2", &["x", "y", "z"]),
            table("t3", &["p", "q"]),
        ],
        vec![
            index("i1", "t1", &["b"], false),
            index("i2", "t1", &["b", "c"], false),
            index("i3", "t3", &["p", "q"], false),
        ],
    );
    (ddl, cat)
}

fn plan(body: &str, cat: &Catalog) -> String {
    // `explain::parse` takes the text *after* the EXPLAIN keyword.
    let sql = format!(" QUERY PLAN {body}");
    match explain::parse(&sql) {
        Ok(e) => match explain::execute(&e, cat) {
            Ok(Outcome::Query { rows, .. }) => rows
                .iter()
                .map(|r| match &r.values[3] {
                    nsqlite::Value::Text(s) => s.clone(),
                    o => format!("{o:?}"),
                })
                .collect::<Vec<_>>()
                .join(" ~ "),
            Ok(_) => "<not a query>".into(),
            Err(e) => format!("E {}", e.message),
        },
        Err(e) => format!("E {}", e.message),
    }
}

fn oracle(ddl: &[&str], body: &str) -> String {
    let path = std::env::temp_dir().join(format!("nsqlite_oracle_{}.db", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let setup = ddl.join(";\n") + ";";
    let script = format!("{setup}\nEXPLAIN QUERY PLAN {body};\n");
    let mut child = std::process::Command::new("sqlite3")
        .arg(&path)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("sqlite3");
    {
        use std::io::Write;
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(script.as_bytes())
            .unwrap();
    }
    let out = child.wait_with_output().unwrap();
    // The shell renders EQP as a glyph tree; strip the glyphs to get the
    // `detail` column of each row, in output order.
    let details: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim_end)
        .filter(|l| !l.trim().is_empty() && l.trim() != "QUERY PLAN")
        .map(|l| {
            l.trim_start_matches(|c: char| c == '|' || c == '`' || c == '-' || c.is_whitespace())
                .to_string()
        })
        .collect();
    let _ = std::fs::remove_file(&path);
    details.join(" ~ ")
}

#[test]
#[ignore]
fn dump() {
    for (schema, sql) in CASES {
        let mine = plan(sql, &build(schema));
        let theirs = oracle(&ddl_for(schema), sql);
        let flag = if mine == theirs { "  ok" } else { "DIFF" };
        println!("{flag}\t{schema}\t{sql}");
        if mine != theirs {
            println!("      mine: {mine}");
            println!("    sqlite: {theirs}");
        }
    }
}

/// The DDL a named schema stands for, so the oracle builds the same database.
fn ddl_for(name: &str) -> Vec<&'static str> {
    match name {
        "base" => vec![
            "CREATE TABLE t1(a,b,c)",
            "CREATE TABLE t2(x,y,z)",
            "CREATE TABLE t3(p,q)",
            "CREATE INDEX i1 ON t1(b)",
            "CREATE INDEX i2 ON t1(b,c)",
            "CREATE INDEX i3 ON t3(p,q)",
        ],
        "noindex" => vec!["CREATE TABLE t1(a,b,c)"],
        "aonly" => vec!["CREATE TABLE t1(a,b)", "CREATE INDEX i1 ON t1(a)"],
        "ab" => vec!["CREATE TABLE t1(a,b)", "CREATE INDEX iab ON t1(a,b)"],
        "abc_sub" => vec![
            "CREATE TABLE t1(a,b,c)",
            "CREATE INDEX iab ON t1(a,b)",
            "CREATE INDEX iac ON t1(a,c)",
            "CREATE INDEX ibc ON t1(b,c)",
        ],
        "ab2" => vec![
            "CREATE TABLE t1(a,b,c)",
            "CREATE INDEX ia ON t1(a)",
            "CREATE INDEX iab ON t1(a,b)",
        ],
        "eqord" => vec![
            "CREATE TABLE t1(a,b,c)",
            "CREATE INDEX ia ON t1(a)",
            "CREATE INDEX ibc ON t1(b,c)",
        ],
        "abgr" => vec![
            "CREATE TABLE t1(a,b,c)",
            "CREATE INDEX ia ON t1(a)",
            "CREATE INDEX ib ON t1(b)",
        ],
        "minmax" => vec!["CREATE TABLE t1(a,b,c)", "CREATE INDEX i1 ON t1(b)"],
        "idxby" => vec![
            "CREATE TABLE t1(a,b,c)",
            "CREATE INDEX iab ON t1(a,b)",
            "CREATE INDEX iac ON t1(a,c)",
        ],
        "orderb" => vec!["CREATE TABLE t1(a,b,c,d)", "CREATE INDEX i1 ON t1(a)"],
        "t1t3" => vec!["CREATE TABLE t1(a,b,c)", "CREATE TABLE t3(p,q)"],
        // `bcdw`: i1(b), i2(b,c), i3(b,c,d) -- the width tie-break.
        "bcdw" => vec![
            "CREATE TABLE t1(a,b,c,d)",
            "CREATE INDEX i1 ON t1(b)",
            "CREATE INDEX i2 ON t1(b,c)",
            "CREATE INDEX i3 ON t1(b,c,d)",
        ],
        _ => vec![],
    }
}

fn build(name: &str) -> Catalog {
    match name {
        "base" => catalog(
            vec![
                table("t1", &["a", "b", "c"]),
                table("t2", &["x", "y", "z"]),
                table("t3", &["p", "q"]),
            ],
            vec![
                index("i1", "t1", &["b"], false),
                index("i2", "t1", &["b", "c"], false),
                index("i3", "t3", &["p", "q"], false),
            ],
        ),
        "noindex" => catalog(vec![table("t1", &["a", "b", "c"])], Vec::new()),
        "aonly" => catalog(
            vec![table("t1", &["a", "b"])],
            vec![index("i1", "t1", &["a"], false)],
        ),
        "ab" => catalog(
            vec![table("t1", &["a", "b"])],
            vec![index("iab", "t1", &["a", "b"], false)],
        ),
        "abc_sub" => catalog(
            vec![table("t1", &["a", "b", "c"])],
            vec![
                index("iab", "t1", &["a", "b"], false),
                index("iac", "t1", &["a", "c"], false),
                index("ibc", "t1", &["b", "c"], false),
            ],
        ),
        "ab2" => catalog(
            vec![table("t1", &["a", "b", "c"])],
            vec![
                index("ia", "t1", &["a"], false),
                index("iab", "t1", &["a", "b"], false),
            ],
        ),
        "eqord" => catalog(
            vec![table("t1", &["a", "b", "c"])],
            vec![
                index("ia", "t1", &["a"], false),
                index("ibc", "t1", &["b", "c"], false),
            ],
        ),
        "abgr" => catalog(
            vec![table("t1", &["a", "b", "c"])],
            vec![
                index("ia", "t1", &["a"], false),
                index("ib", "t1", &["b"], false),
            ],
        ),
        "minmax" => catalog(
            vec![table("t1", &["a", "b", "c"])],
            vec![index("i1", "t1", &["b"], false)],
        ),
        "idxby" => catalog(
            vec![table("t1", &["a", "b", "c"])],
            vec![
                index("iab", "t1", &["a", "b"], false),
                index("iac", "t1", &["a", "c"], false),
            ],
        ),
        "orderb" => catalog(
            vec![table("t1", &["a", "b", "c", "d"])],
            vec![index("i1", "t1", &["a"], false)],
        ),
        "t1t3" => catalog(
            vec![table("t1", &["a", "b", "c"]), table("t3", &["p", "q"])],
            Vec::new(),
        ),
        "bcdw" => catalog(
            vec![table("t1", &["a", "b", "c", "d"])],
            vec![
                index("i1", "t1", &["b"], false),
                index("i2", "t1", &["b", "c"], false),
                index("i3", "t1", &["b", "c", "d"], false),
            ],
        ),
        _ => base().1,
    }
}

/// The cases: a schema name and a statement.
const CASES: &[(&str, &str)] = &[
    ("base", "SELECT 5"),
    ("base", "SELECT 5 ORDER BY 1"),
    ("base", "SELECT 5 UNION ALL SELECT 3 ORDER BY 1"),
    ("base", "SELECT * FROM t1"),
    ("base", "SELECT * FROM t2"),
    ("base", "SELECT * FROM t1 ORDER BY b"),
    ("base", "SELECT * FROM t1 ORDER BY b, c"),
    ("base", "SELECT * FROM t1 WHERE b=1"),
    ("base", "SELECT b FROM t1 WHERE b=1"),
    ("base", "SELECT * FROM t1 WHERE b>5"),
    ("base", "SELECT * FROM t1 WHERE b BETWEEN 1 AND 5"),
    ("base", "SELECT * FROM t1 WHERE b=1 AND c=2"),
    ("base", "SELECT * FROM t1 WHERE a=5"),
    ("base", "SELECT * FROM t1 WHERE 0"),
    ("base", "SELECT DISTINCT c FROM t1 GROUP BY a ORDER BY b"),
    ("base", "SELECT b, count(*) FROM t1 GROUP BY a ORDER BY c"),
    ("base", "DELETE FROM t1 WHERE b=2"),
    ("base", "UPDATE t1 SET a=1 WHERE b=2"),
    ("base", "DELETE FROM t1"),
    ("base", "INSERT INTO t1 VALUES(1,2,3)"),
    ("base", "SELECT * FROM t1, t2"),
    ("base", "SELECT * FROM t1 CROSS JOIN t2"),
    ("base", "SELECT * FROM t1 JOIN t2 ON t1.a=t2.x"),
    ("base", "SELECT * FROM t1 LEFT JOIN t2 ON t1.a=t2.x"),
    ("base", "SELECT * FROM t1 AS q"),
    ("base", "SELECT a FROM t1 UNION ALL SELECT x FROM t2"),
    (
        "base",
        "SELECT a FROM t1 UNION ALL SELECT x FROM t2 UNION ALL SELECT p FROM t3",
    ),
    ("base", "SELECT a FROM t1 UNION SELECT x FROM t2"),
    ("base", "SELECT a FROM t1 INTERSECT SELECT x FROM t2"),
    ("base", "SELECT a FROM t1 EXCEPT SELECT x FROM t2"),
    (
        "base",
        "SELECT a FROM t1 UNION ALL SELECT x FROM t2 ORDER BY 1",
    ),
    ("noindex", "SELECT * FROM t1"),
    ("noindex", "SELECT * FROM t1 WHERE a=1"),
    ("noindex", "SELECT * FROM t1 ORDER BY a"),
    ("noindex", "SELECT a, count(*) FROM t1 GROUP BY a"),
    ("aonly", "SELECT * FROM t1 ORDER BY a, b"),
    ("aonly", "SELECT * FROM t1 ORDER BY +a"),
    ("aonly", "SELECT * FROM t1 ORDER BY 1"),
    ("aonly", "SELECT * FROM t1 ORDER BY 2"),
    ("ab", "SELECT a FROM t1"),
    ("ab", "SELECT a, b FROM t1"),
    ("ab", "SELECT * FROM t1"),
    ("ab", "SELECT * FROM t1 WHERE a=1"),
    ("ab", "SELECT a, b FROM t1 WHERE a=1"),
    ("abc_sub", "SELECT * FROM t1 ORDER BY a, b"),
    ("abc_sub", "SELECT * FROM t1 ORDER BY a, c"),
    ("abc_sub", "SELECT * FROM t1 ORDER BY b, c"),
    ("abc_sub", "SELECT * FROM t1 WHERE a=1 AND b=1"),
    ("abc_sub", "SELECT * FROM t1 WHERE a=1 AND c=1"),
    ("ab2", "SELECT * FROM t1 WHERE a=1 ORDER BY b"),
    ("ab2", "SELECT * FROM t1 WHERE a=1 ORDER BY b, c"),
    ("eqord", "SELECT * FROM t1 WHERE a=1 ORDER BY b"),
    ("abgr", "SELECT a, count(*) FROM t1 GROUP BY b ORDER BY a"),
    ("abgr", "SELECT b, count(*) FROM t1 GROUP BY a ORDER BY b"),
    ("abgr", "SELECT DISTINCT a FROM t1"),
    ("abgr", "SELECT DISTINCT b FROM t1"),
    ("abgr", "SELECT a, count(*) FROM t1 GROUP BY a"),
    ("minmax", "SELECT min(b) FROM t1"),
    ("minmax", "SELECT min(a) FROM t1"),
    ("minmax", "SELECT max(a) FROM t1"),
    ("idxby", "SELECT * FROM t1 INDEXED BY iab ORDER BY a, c"),
    ("idxby", "SELECT * FROM t1 INDEXED BY iac ORDER BY a, c"),
    ("orderb", "SELECT * FROM t1 ORDER BY a, b, c, d"),
    ("orderb", "SELECT * FROM t1 ORDER BY a, b, c"),
    ("bcdw", "SELECT * FROM t1 WHERE b=1"),
    ("bcdw", "SELECT * FROM t1 WHERE b>1"),
    ("bcdw", "SELECT * FROM t1 WHERE b=1 AND c=1"),
    ("bcdw", "SELECT * FROM t1 ORDER BY b, c"),
    // The covering-index tie-break, swept over the columns the query needs.
    // The narrowest index that covers wins, and the one that covers nothing
    // does not appear at all.
    ("bcdw", "SELECT b FROM t1 WHERE b=1"),
    ("bcdw", "SELECT c FROM t1 WHERE b=1"),
    ("bcdw", "SELECT d FROM t1 WHERE b=1"),
    ("bcdw", "SELECT b, c FROM t1 WHERE b=1"),
    ("bcdw", "SELECT c, d FROM t1 WHERE b=1"),
    ("bcdw", "SELECT b, c, d FROM t1 WHERE b=1"),
    ("bcdw", "SELECT a FROM t1 WHERE b=1"),
    ("bcdw", "SELECT count(*) FROM t1 WHERE b=1"),
    ("bcdw", "SELECT b FROM t1 WHERE b>1"),
    ("bcdw", "SELECT c FROM t1 WHERE b>1"),
    ("bcdw", "SELECT b, c FROM t1 WHERE b>1"),
    ("bcdw", "SELECT b FROM t1"),
    ("bcdw", "SELECT c FROM t1"),
    ("bcdw", "SELECT b, c FROM t1"),
    ("bcdw", "SELECT count(*) FROM t1"),
    // An equality holds its key columns constant wherever the ORDER BY names
    // them, which is not the same as counting them off the front.
    ("ab2", "SELECT * FROM t1 WHERE a=1 ORDER BY a"),
    ("ab2", "SELECT * FROM t1 WHERE a=1 ORDER BY a, b"),
    ("ab2", "SELECT * FROM t1 WHERE a=1 ORDER BY a, c"),
    ("ab2", "SELECT * FROM t1 WHERE a=1 ORDER BY a, b, c"),
    ("ab2", "SELECT * FROM t1 WHERE a=1 ORDER BY c, a"),
    ("ab2", "SELECT * FROM t1 WHERE a=1 ORDER BY b, a"),
    ("ab", "SELECT * FROM t1 WHERE a=1 AND b=2 ORDER BY b"),
    ("ab", "SELECT * FROM t1 WHERE a=1 AND b=2 ORDER BY a, b"),
    // A GROUP BY sorter settles the row order before an ORDER BY sees it.
    ("base", "SELECT DISTINCT c FROM t1 GROUP BY a ORDER BY b"),
    ("base", "SELECT DISTINCT b FROM t1 GROUP BY a ORDER BY a"),
    ("base", "SELECT DISTINCT b FROM t1 GROUP BY b ORDER BY a"),
    ("base", "SELECT DISTINCT c FROM t1 GROUP BY a ORDER BY b, c"),
    (
        "base",
        "SELECT DISTINCT c FROM t1 GROUP BY a ORDER BY a, b, c",
    ),
    ("base", "SELECT c, count(*) FROM t1 GROUP BY a ORDER BY b"),
    ("noindex", "SELECT DISTINCT b FROM t1 GROUP BY a ORDER BY b"),
    ("noindex", "SELECT DISTINCT b FROM t1 GROUP BY b ORDER BY b"),
    (
        "noindex",
        "SELECT DISTINCT b FROM t1 GROUP BY b, a ORDER BY b, a",
    ),
    ("noindex", "SELECT DISTINCT b FROM t1 ORDER BY b"),
    ("noindex", "SELECT DISTINCT b FROM t1 ORDER BY a"),
    (
        "noindex",
        "SELECT a, count(*) FROM t1 GROUP BY a ORDER BY a",
    ),
    (
        "noindex",
        "SELECT a, count(*) FROM t1 GROUP BY a, b ORDER BY a, b",
    ),
    (
        "noindex",
        "SELECT a, count(*) FROM t1 GROUP BY a, b ORDER BY a",
    ),
    // A sub-select in FROM: flattened, or a co-routine of its own.
    ("base", "SELECT * FROM (SELECT a FROM t1) s"),
    ("base", "SELECT * FROM (SELECT a FROM t1 WHERE b=1) s"),
    ("base", "SELECT * FROM (SELECT a FROM t1 ORDER BY a) s"),
    ("base", "SELECT * FROM (SELECT a FROM t1 LIMIT 1) s"),
    (
        "base",
        "SELECT * FROM (SELECT a FROM t1 LIMIT 1 OFFSET 1) s",
    ),
    (
        "base",
        "SELECT * FROM (SELECT a FROM (SELECT a FROM t1) z) s",
    ),
    ("base", "SELECT * FROM (SELECT 1) v1"),
    ("base", "SELECT * FROM (SELECT a FROM t1 GROUP BY a) s"),
    ("base", "SELECT * FROM (SELECT DISTINCT a FROM t1) s"),
    ("base", "SELECT * FROM (SELECT count(*) FROM t1) s"),
    (
        "base",
        "SELECT * FROM (SELECT a, b FROM t1 UNION ALL SELECT a, b FROM t1) s",
    ),
    // A CTE is planned as though its body had been written in place.
    ("base", "WITH q AS (SELECT a FROM t1) SELECT * FROM q"),
    (
        "base",
        "WITH q AS (SELECT a, b FROM t1) SELECT * FROM q WHERE b=2",
    ),
    (
        "base",
        "WITH q AS (SELECT a, b FROM t1 WHERE b=1) SELECT * FROM q",
    ),
    (
        "base",
        "WITH q AS (SELECT a FROM t1) SELECT * FROM q ORDER BY a",
    ),
    (
        "base",
        "WITH q AS (SELECT a FROM t1) SELECT * FROM q GROUP BY a",
    ),
    (
        "base",
        "WITH q AS (SELECT a FROM t1) SELECT * FROM q LIMIT 5",
    ),
    ("base", "WITH q AS (SELECT a FROM t1) SELECT * FROM q, t2"),
    // ORDER BY ordinals resolved through a star.
    ("aonly", "SELECT * FROM t1 ORDER BY 1"),
    ("aonly", "SELECT * FROM t1 ORDER BY 2"),
    // A whole-table rewrite has no access line.
    ("base", "DELETE FROM t1"),
    ("base", "UPDATE t1 SET a=1"),
    ("base", "DELETE FROM t1 WHERE 1"),
    ("base", "DELETE FROM t1 WHERE a=5"),
];
