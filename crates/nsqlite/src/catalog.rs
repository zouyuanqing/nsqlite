//! The schema: what tables exist, what columns they have, and which of those
//! columns is the rowid alias.
//!
//! This is a first cut that keeps a table's definition in memory for the life
//! of the connection. A real implementation stores it in `sqlite_schema` and
//! reads it back on demand, which this one will do once `CREATE TABLE` writes
//! its row; the shape of the types is already the one that storage needs.

use std::collections::HashMap;

use crate::affinity::{affinity_of, Affinity};
use crate::parser::{ColumnDef, Constraint, Expr, Stmt};

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
    /// Every uniqueness constraint on this table, as the columns of each key.
    ///
    /// One entry per key, not per column: `UNIQUE(a,b)` is one entry naming
    /// both, and a row violates it only when *all* of them match a row already
    /// stored. A column-level `a UNIQUE` is a one-entry key.
    ///
    /// The rowid alias is deliberately absent. Its uniqueness is the b-tree's
    /// own key check, which already refuses a duplicate before this is asked,
    /// and including it would make `OR REPLACE` delete the row it is about to
    /// write. See `Connection::check_unique`.
    pub unique_sets: Vec<Vec<usize>>,
    /// A WITHOUT ROWID table has no rowid, which this engine does not yet
    /// implement, so such a table is recorded but not creatable.
    pub without_rowid: bool,
    /// The page the table's b-tree is rooted at.
    pub root_page: u32,
    /// The module a virtual table is hosted by, `None` for an ordinary table.
    ///
    /// A virtual table has no b-tree of its own, so `root_page` is 0 and every
    /// read of it goes through the module instead. The table still lives in
    /// `Catalog::tables` rather than beside it, because `Connection::table`
    /// resolves a name through that one map and a table in a second map would
    /// answer `no such table` to a query that should have worked.
    pub virtual_module: Option<String>,
    /// Every CHECK on the table, column-level and table-level, in the order the
    /// statement wrote them.
    ///
    /// Each carries the source text beside the tree, because the refusal quotes
    /// the text -- `CHECK constraint failed: length(c) <= 5` -- and the first one
    /// to fail is the one named, so the order is part of the behaviour and not
    /// an incidental detail of how the DDL was spelled.
    pub checks: Vec<CheckConstraint>,
}

/// One CHECK constraint: what to evaluate, and what to call it in the refusal.
#[derive(Debug, Clone, PartialEq)]
pub struct CheckConstraint {
    /// The predicate, evaluated against the row about to be written.
    pub expr: Expr,
    /// The expression's source text, exactly as the statement wrote it.
    pub text: String,
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

/// One secondary index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Index {
    pub name: String,
    pub table: String,
    /// The indexed columns, in key order.
    pub columns: Vec<String>,
    /// Whether each key column is ascending. A descending one stores its
    /// values reversed so the entries stay in ascending order on the page, which
    /// is what lets one page layout serve both directions.
    pub ascending: Vec<bool>,
    pub unique: bool,
    pub root_page: u32,
}

impl Index {
    /// How many leading columns make up the key.
    pub fn key_len(&self) -> usize {
        self.columns.len()
    }

    /// The value a column's key takes in an index entry.
    ///
    /// A descending column stores the value unchanged and reverses the order
    /// the cells are laid out in, which is what a real file shows: an ascending
    /// and a descending index over the same column hold byte-identical keys and
    /// differ only in the order the cells appear. Inverting the value instead
    /// would produce a file SQLite reads with the wrong ordering.
    pub fn key_value(&self, col: usize, v: &crate::value::Value) -> crate::value::Value {
        let _ = col;
        v.clone()
    }

    /// Whether a comparison of two keys should be reversed, which is the case
    /// for a descending column.
    pub fn reversed_at(&self, col: usize) -> bool {
        !self.ascending.get(col).copied().unwrap_or(true)
    }

    /// The order two keys are in under this index's column directions.
    pub fn compare_keys(
        &self,
        a: &[crate::value::Value],
        b: &[crate::value::Value],
    ) -> std::cmp::Ordering {
        for i in 0..self.key_len() {
            let (Some(x), Some(y)) = (a.get(i), b.get(i)) else {
                continue;
            };
            let mut ord = x.compare(y);
            if self.reversed_at(i) {
                ord = ord.reverse();
            }
            if ord != std::cmp::Ordering::Equal {
                return ord;
            }
        }
        std::cmp::Ordering::Equal
    }
}

/// Every table the connection knows about.
#[derive(Debug, Default)]
pub struct Catalog {
    tables: HashMap<String, Table>,
    indexes: HashMap<String, Index>,
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

    /// Adds or replaces an index.
    pub fn put_index(&mut self, index: Index) {
        self.indexes.insert(index.name.to_ascii_lowercase(), index);
    }

    pub fn index(&self, name: &str) -> Option<&Index> {
        self.indexes.get(&name.to_ascii_lowercase())
    }

    pub fn remove_index(&mut self, name: &str) -> Option<Index> {
        self.indexes.remove(&name.to_ascii_lowercase())
    }

    /// The indexes defined on a table, in the order their names sort, which is
    /// what a schema listing shows.
    pub fn indexes_on(&self, table: &str) -> Vec<&Index> {
        let mut v: Vec<&Index> = self
            .indexes
            .values()
            .filter(|i| i.table.eq_ignore_ascii_case(table))
            .collect();
        v.sort_by(|a, b| {
            a.name
                .to_ascii_lowercase()
                .cmp(&b.name.to_ascii_lowercase())
        });
        v
    }

    /// The index whose leading column is exactly this one, which is the one a
    /// lookup on that column can use.
    pub fn index_on_column(&self, table: &str, column: &str) -> Option<&Index> {
        self.indexes_on(table).into_iter().find(|i| {
            i.columns
                .first()
                .map(|c| c.eq_ignore_ascii_case(column))
                .unwrap_or(false)
        })
    }

    pub fn index_count(&self) -> usize {
        self.indexes.len()
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

    /// Every table, sorted by name, for resolving a FROM clause that names more
    /// than one.
    pub fn all_tables(&self) -> Vec<Table> {
        let mut v: Vec<Table> = self.tables.values().cloned().collect();
        v.sort_by_key(|t| t.name.to_ascii_lowercase());
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
        // A column-level INTEGER PRIMARY KEY, in a table without WITHOUT ROWID,
        // is the alias. `PRIMARY KEY DESC` is deliberately excluded: SQLite
        // does not make such a column an alias, because the alias is looked up
        // in ascending order.
        let column_alias: Option<usize> = columns.iter().position(|c| {
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
                    Constraint::Default { expr: e, .. } => Some(e.clone()),
                    _ => None,
                }),
                rowid_alias: false,
            })
            .collect();

        // Every CHECK, in the order the statement wrote them: the column-level
        // ones as each column is read, then the table-level ones. A failing
        // statement names the first one to fail, so the order is observable.
        let mut checks: Vec<CheckConstraint> = Vec::new();
        for c in columns {
            for k in &c.constraints {
                if let Constraint::Check { expr, text } = k {
                    checks.push(CheckConstraint {
                        expr: expr.clone(),
                        text: text.clone(),
                    });
                }
            }
        }
        for k in table_constraints {
            if let Constraint::Check { expr, text } = k {
                checks.push(CheckConstraint {
                    expr: expr.clone(),
                    text: text.clone(),
                });
            }
        }

        // Every uniqueness constraint, from the three places one can be written,
        // resolved to column positions. The rowid alias is dropped at the end,
        // not here: it is a uniqueness constraint like any other until the
        // b-tree's own key check has already refused its duplicate, and the
        // exclusion is easier to see in one place.
        let mut unique_sets: Vec<Vec<usize>> = Vec::new();

        // `a UNIQUE` on a column of its own.
        for (i, c) in columns.iter().enumerate() {
            if c.constraints
                .iter()
                .any(|k| matches!(k, Constraint::Unique { .. }))
            {
                unique_sets.push(vec![i]);
            }
        }

        // A column-level `a PRIMARY KEY` is a uniqueness constraint too, unless
        // it is the alias -- which is the whole difference between `a PRIMARY
        // KEY` and `a INTEGER PRIMARY KEY` on the same table. This is what
        // sqlite3 reports `UNIQUE constraint failed: t.a` for, and the b-tree
        // never sees it, so nothing else would refuse it.
        for (i, c) in columns.iter().enumerate() {
            if column_alias != Some(i)
                && c.constraints.iter().any(|k| {
                    matches!(k, Constraint::PrimaryKey { ascending: true, .. })
                })
            {
                unique_sets.push(vec![i]);
            }
        }

        // The table-level forms, which name their columns and so carry a key of
        // any width. A name the table does not have contributes nothing rather
        // than making the key unrestrictable: the reference refuses such a
        // table at CREATE time, and this engine does not, so the key is simply
        // the columns that resolved.
        for c in table_constraints {
            let names: &[String] = match c {
                Constraint::PrimaryKey { columns, .. } if columns.len() > 1 => columns,
                Constraint::Unique { columns } => columns,
                _ => continue,
            };
            let mut key = Vec::with_capacity(names.len());
            for n in names {
                if let Some(i) = cols.iter().position(|c| c.name.eq_ignore_ascii_case(n)) {
                    key.push(i);
                }
            }
            if !key.is_empty() {
                unique_sets.push(key);
            }
        }

        // A table-level `PRIMARY KEY(a)` is the alias when `a` is the single
        // INTEGER column it names, and a uniqueness constraint otherwise. Only
        // a column-level INTEGER primary key was consulted above, so this is
        // the one case a table-level key can still be an alias.
        let mut table_alias: Option<usize> = None;
        for c in table_constraints {
            if let Constraint::PrimaryKey { columns, .. } = c {
                if columns.len() != 1 {
                    continue;
                }
                if let Some(i) = cols
                    .iter()
                    .position(|c| c.name.eq_ignore_ascii_case(&columns[0]))
                {
                    if cols[i].affinity == Affinity::Integer {
                        table_alias = Some(i);
                    }
                }
            }
        }
        // A column-level INTEGER primary key wins: it is unambiguous, whereas
        // the table-level form is a second opinion about the same table.
        let alias = column_alias.or(table_alias);

        let mut table = Table {
            name: name.to_owned(),
            columns: cols,
            rowid_alias: alias,
            unique_sets,
            without_rowid: false,
            root_page: 0,
            // A `CREATE TABLE` never names a module; only
            // `CREATE VIRTUAL TABLE` does, and that path builds the entry
            // itself rather than coming through here.
            virtual_module: None,
            checks,
        };
        // The alias is enforced by the b-tree, which refuses a duplicate key
        // before this table's `unique_sets` is ever consulted, so a key that is
        // only the alias would double every check -- and under OR REPLACE would
        // make the conflict handling delete the row it is about to write.
        table.unique_sets.retain(|key| {
            !(key.len() == 1 && table.rowid_alias == Some(key[0]))
        });
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
        // The spelling as declared, not the key. Every lookup folds, so the key
        // may be; but a message that has to name the table echoes what
        // `CREATE TABLE` wrote, and `no such column: main.MiXeD.a` is right
        // where `main.mixed.a` is not. See `parser::query_name`.
        assert_eq!(c.names(), vec!["Users"]);
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
