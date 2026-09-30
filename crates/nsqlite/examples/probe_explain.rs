// Smoke probe for the explain module: builds a small catalog in memory, then
// prints the plan for the statement named on the command line. Run it with no
// argument to sweep the cases the module's own tests pin.
use nsqlite::affinity::Affinity;
use nsqlite::catalog::{Catalog, Column, Index, Table};
use nsqlite::connection::Outcome;
use nsqlite::explain::{self, Mode};
use nsqlite::value::Value;

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
        // These explain tests only ever build a plain table: no constraint
        // and no virtual module, which is what the catalog records for one.
        unique_sets: Vec::new(),
        without_rowid: false,
        root_page: 0,
        virtual_module: None,
    }
}

fn index(name: &str, table: &str, columns: &[&str]) -> Index {
    Index {
        name: name.to_string(),
        table: table.to_string(),
        columns: columns.iter().map(|c| (*c).to_string()).collect(),
        ascending: columns.iter().map(|_| true).collect(),
        unique: false,
        root_page: 0,
    }
}

fn main() {
    let mut c = Catalog::new();
    c.put(table("t1", &["a", "b", "c"]));
    c.put(table("t2", &["x", "y"]));
    c.put_index(index("i1", "t1", &["b"]));
    c.put_index(index("i2", "t1", &["b", "c"]));
    c.put_index(index("i3", "t1", &["c"]));
    c.put_index(index("i4", "t2", &["x", "y"]));

    for sql in std::env::args().skip(1) {
        match explain::parse(&format!(" QUERY PLAN {sql}")) {
            Err(e) => println!("ERR  {sql}  => {e}"),
            Ok(e) => {
                assert_eq!(e.mode, Mode::QueryPlan);
                match explain::execute(&e, &c) {
                    Err(e) => println!("ERR  {sql}  => {e}"),
                    Ok(Outcome::Query { rows, .. }) => {
                        let plan = rows
                            .iter()
                            .map(|r| match &r.values[3] {
                                Value::Text(s) => s.clone(),
                                other => format!("{other:?}"),
                            })
                            .collect::<Vec<_>>()
                            .join(" ~ ");
                        println!("{plan}");
                    }
                    Ok(_) => println!("(not a query)"),
                }
            }
        }
    }
}
