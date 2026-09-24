//! The schema: what tables exist, what columns they have, and which of those
//! columns is the rowid alias.
//!
//! This is a first cut that keeps a table's definition in memory for the life
//! of the connection. A real implementation stores it in `sqlite_schema` and
//! reads it back on demand, which this one will do once `CREATE TABLE` writes
//! its row; the shape of the types is already the one that storage needs.

use std::collections::HashMap;

use crate::affinity::{affinity_of, Affinity};
use crate::parser::{ColumnDef, Constraint, Stmt};

/// One column of a table.
#[derive(Debug, Clone, PartialEq)]
pub struct Column {
    pub name: String,
    /// The type as declared, with its length, since that is what a schema
    /// round-trip has to reproduce.
    pub declared_type: String,
    pub affinity: Affinity,
    pub not_null: bool,
    /// The DEFAULT expression, if the column has one.
    pub default: Option<crate::parser::Expr>,
    /// Whether this column is the table's rowid alias, which is an INTEGER
    /// PRIMARY KEY on a rowid table and nothing else.
    pub rowid_alias: bool,
}

impl Column {
    /// The name SQLite uses in error messages: an unquoted name lowercased, and
    /// a quoted one as written.
    pub fn display_name(&self) -> &str {
        &self.name
    }
}

/// One table.
#[derive(Debug, Clone, PartialEq)]
pub struct Table {
    pub name: String,
    pub columns: Vec<Column>,
    /// The rowid alias, which is at most one column.
    pub rowid_alias: Option<usize>,
    /// A WITHOUT ROWID table has no rowid, which this engine does not yet
    /// implement, so such a table is recorded but not creatable.
    pub without_rowid: bool,
    /// The page the table's b-tree is rooted at.
    pub root_page: u32,
}

impl Table {
    /// The index of a column by name, matching case-insensitively as SQLite
    /// does for ASCII names.
    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.columns
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(name))
    }

    /// The number of columns, which is what an INSERT checks its value count
    /// against.
    pub fn len(&self) -> usize {
        self.columns.len()
    }

    pub fn is_empty(&self) -> bool {
        self.columns.is_empty()
    }
}

/// Every table the connection knows about.
#[derive(Debug, Default)]
pub struct Catalog {
    tables: HashMap<String, Table>,
}

impl Catalog {
    pub fn new() -> Catalog {
        Catalog::default()
    }

    /// Inserts or replaces a table. Names are matched case-insensitively, since
    /// SQLite does the same for table names.
    pub fn put(&mut self, table: Table) {
        self.tables.insert(table.name.to_ascii_lowercase(), table);
    }

    pub fn get(&self, name: &str) -> Option<&Table> {
        self.tables.get(&name.to_ascii_lowercase())
    }

    pub fn get_mut(&mut self, name: &str) -> Option<&mut Table> {
        self.tables.get_mut(&name.to_ascii_lowercase())
    }

    pub fn remove(&mut self, name: &str) -> Option<Table> {
        self.tables.remove(&name.to_ascii_lowercase())
    }

    pub fn contains(&self, name: &str) -> bool {
        self.tables.contains_key(&name.to_ascii_lowercase())
    }

    pub fn len(&self) -> usize {
        self.tables.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tables.is_empty()
    }

    /// Every table name, sorted, which is what a schema listing needs.
    pub fn names(&self) -> Vec<String> {
        let mut v: Vec<String> = self.tables.values().map(|t| t.name.clone()).collect();
        v.sort_by_key(|n| n.to_ascii_lowercase());
        v
    }

    /// Builds a table from a CREATE TABLE statement.
    ///
    /// The rowid alias is the single INTEGER PRIMARY KEY column of a rowid
    /// table, which is the only case where a column stands for the rowid. A
    /// composite or non-integer primary key is an ordinary uniqueness
    /// constraint, and this engine does not yet enforce uniqueness at all.
    pub fn table_from_create(
        &self,
        name: &str,
        columns: &[ColumnDef],
        table_constraints: &[Constraint],
    ) -> Table {
        // A table-level PRIMARY KEY names its columns, and a single INTEGER one
        // of those is the alias.
        let mut pk_from_constraint: Option<Vec<String>> = None;
        for c in table_constraints {
            if let Constraint::PrimaryKey { .. } = c {
                // The parser does not retain the column list, so a table-level
                // primary key is treated as no alias, which is the safe reading:
                // claiming an alias that does not exist would make writes put
                // the value in the wrong slot.
                pk_from_constraint = Some(Vec::new());
            }
        }
        let _ = pk_from_constraint;

        // A column-level INTEGER PRIMARY KEY, in a table without WITHOUT ROWID,
        // is the alias. `PRIMARY KEY DESC` is deliberately excluded: SQLite
        // does not make such a column an alias, because the alias is looked up
        // in ascending order.
        let alias: Option<usize> = columns.iter().position(|c| {
            c.constraints.iter().any(|k| {
                matches!(
                    k,
                    Constraint::PrimaryKey {
                        ascending: true,
                        ..
                    }
                )
            }) && affinity_of(&c.ty) == Affinity::Integer
        });

        let cols: Vec<Column> = columns
            .iter()
            .map(|c| Column {
                name: c.name.clone(),
                declared_type: c.ty.clone(),
                affinity: affinity_of(&c.ty),
                not_null: c
                    .constraints
                    .iter()
                    .any(|k| matches!(k, Constraint::NotNull)),
                default: c.constraints.iter().find_map(|k| match k {
                    Constraint::Default(e) => Some(e.clone()),
                    _ => None,
                }),
                rowid_alias: false,
            })
            .collect();

        let mut table = Table {
            name: name.to_owned(),
            columns: cols,
            rowid_alias: alias,
            without_rowid: false,
            root_page: 0,
        };
        if let Some(i) = table.rowid_alias {
            table.columns[i].rowid_alias = true;
        }
        table
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse_one;
    use crate::value::Value;

    fn table_of(sql: &str) -> Table {
        let Stmt::CreateTable {
            name,
            columns,
            constraints,
            ..
        } = parse_one(sql).unwrap()
        else {
            panic!("expected CREATE TABLE")
        };
        Catalog::new().table_from_create(&name, &columns, &constraints)
    }

    #[test]
    fn columns_carry_their_declared_type_and_affinity() {
        let t = table_of("CREATE TABLE t (a INTEGER, b TEXT, c REAL, d BLOB, e)");
        assert_eq!(t.columns.len(), 5);
        assert_eq!(t.columns[0].affinity, Affinity::Integer);
        assert_eq!(t.columns[1].affinity, Affinity::Text);
        assert_eq!(t.columns[2].affinity, Affinity::Real);
        assert_eq!(t.columns[3].affinity, Affinity::Blob);
        // An undeclared type is BLOB, which converts nothing.
        assert_eq!(t.columns[4].affinity, Affinity::Blob);
        // An unquoted identifier is folded to lower case, as SQLite does.
        assert_eq!(t.columns[0].declared_type, "integer");
    }

    #[test]
    fn a_single_integer_primary_key_is_the_rowid_alias() {
        let t = table_of("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)");
        assert_eq!(t.rowid_alias, Some(0));
        assert!(t.columns[0].rowid_alias);
        assert!(!t.columns[1].rowid_alias);
    }

    #[test]
    fn a_text_primary_key_is_not_the_alias() {
        let t = table_of("CREATE TABLE t (id TEXT PRIMARY KEY, name TEXT)");
        assert_eq!(t.rowid_alias, None);
    }

    #[test]
    fn a_descending_integer_primary_key_is_not_the_alias() {
        // The alias is looked up in ascending order, so a descending key is an
        // ordinary column and a NULL there is stored as NULL.
        let t = table_of("CREATE TABLE t (id INTEGER PRIMARY KEY DESC, v TEXT)");
        assert_eq!(t.rowid_alias, None);
    }

    #[test]
    fn not_null_is_recorded() {
        let t = table_of("CREATE TABLE t (a TEXT NOT NULL, b TEXT)");
        assert!(t.columns[0].not_null);
        assert!(!t.columns[1].not_null);
    }

    #[test]
    fn names_match_case_insensitively() {
        let t = table_of("CREATE TABLE t (Alpha TEXT, BETA TEXT)");
        assert_eq!(t.column_index("alpha"), Some(0));
        assert_eq!(t.column_index("ALPHA"), Some(0));
        assert_eq!(t.column_index("beta"), Some(1));
        assert_eq!(t.column_index("gamma"), None);
    }

    #[test]
    fn the_catalog_finds_a_table_by_any_spelling() {
        let mut c = Catalog::new();
        c.put(table_of("CREATE TABLE Users (a)"));
        assert!(c.contains("users"));
        assert!(c.contains("USERS"));
        assert!(c.contains("Users"));
        assert!(!c.contains("others"));
        assert_eq!(c.names(), vec!["users"]);
        c.remove("USERS");
        assert!(c.is_empty());
    }

    #[test]
    fn a_default_is_kept_for_the_writer_to_evaluate() {
        let t = table_of("CREATE TABLE t (a INTEGER DEFAULT 42, b TEXT)");
        assert!(t.columns[0].default.is_some());
        assert!(t.columns[1].default.is_none());
    }
}
