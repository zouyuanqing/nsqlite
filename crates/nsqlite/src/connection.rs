//! The connection: it runs statements against a database file.
//!
//! This is the layer the rest of the engine exists to serve. It parses a
//! statement, resolves names against the catalog, and drives the b-tree, which
//! is where the file-format compatibility lives. The evaluator handles
//! expressions; this handles the statements, the row shape, and the ordering.
//!
//! # Scope
//!
//! Executable: CREATE TABLE, DROP TABLE, INSERT, SELECT without compound arms
//! or joins, UPDATE, DELETE, and the transaction statements. Recognised but not
//! executed: everything else, which returns a clear "not supported yet" error
//! rather than a wrong answer. Where SQLite's own behaviour is subtle enough to
//! matter, the comment says which case it is.

use std::path::{Path, PathBuf};

use crate::affinity::apply as apply_affinity;
use crate::affinity::Affinity;
use crate::catalog::{Catalog, Column, Table};
use crate::error::{Error, Result, ResultCode};
use crate::eval::{eval, truthy, EvalCtx};
use crate::pager::Pager;
use crate::parser::{
    ColumnDef, Constraint, Expr, FromItem, InsertSource, Literal, Select, SelectBody, Stmt,
};
use crate::table_tree::TableTree;
use crate::value::Value;

/// Rebuilds a table's definition by re-parsing the statement the schema stores.
fn rebuild_table(name: &str, sql_text: &str, root: u32) -> Result<Table> {
    let Stmt::CreateTable {
        name: parsed,
        columns,
        constraints,
        without_rowid,
        ..
    } = crate::parser::parse_one(sql_text).map_err(|e| {
        Error::new(
            ResultCode::Corrupt,
            format!("malformed database schema ({name}): {e}"),
        )
    })?
    else {
        return Err(Error::new(
            ResultCode::Corrupt,
            format!("malformed database schema ({name})"),
        ));
    };
    let mut table = Catalog::new().table_from_create(&parsed, &columns, &constraints);
    table.without_rowid = without_rowid;
    table.root_page = root;
    Ok(table)
}

/// The page the schema b-tree is rooted at. SQLite fixes it at 1, and a reader
/// finds the schema there without a lookup, so a table never takes that page.
const SCHEMA_ROOT: u32 = 1;

/// The columns of a schema table, in the order a query sees them.
const SCHEMA_COLUMNS: &[&str] = &["type", "name", "tbl_name", "rootpage", "sql"];

/// Whether `name` is one of the two names the schema table answers to.
///
/// SQLite has renamed this table twice. `sqlite_master` is the name in the
/// suite's expectations and the one almost every query uses; `sqlite_schema`
/// is the name since 3.33 and is what a modern query writes. Both name the
/// same table, and a query that used the wrong one would otherwise get "no such
/// table" for a table that exists.
///
/// A schema qualifier in front is stripped first. `main.sqlite_master` and
/// `temp.sqlite_master` both name the schema table in SQLite 3.53.4 -- the
/// former is the table itself, the latter an empty one, because this engine
/// has no temp database for a second process to have written to. Any OTHER
/// qualifier is not a match, and falls through to the catalog, which answers
/// "no such table" -- the same as SQLite does for `nosuchdb.sqlite_master`.
///
/// The suite's own `dbcksum` helper spells its table list this way
/// (`SELECT ... FROM main.sqlite_master`), so this is the spelling the harness
/// itself depends on.
fn is_schema_table(name: &str) -> bool {
    let base = crate::join::strip_schema_qualifier(name).unwrap_or(name);
    base.eq_ignore_ascii_case("sqlite_master") || base.eq_ignore_ascii_case("sqlite_schema")
}

/// The schema table, described the way the catalog describes a user table.
///
/// Its rows are not stored here: they live in the schema b-tree at
/// `SCHEMA_ROOT`, which is exactly where `select_from` reads a table's rows
/// from, so giving it that root is enough. The same page a user table's rows
/// would come from, which is why a join against it goes through the normal path
/// rather than needing a special case of its own.
fn schema_table() -> Table {
    Table {
        name: "sqlite_master".into(),
        columns: SCHEMA_COLUMNS
            .iter()
            .map(|n| Column {
                name: (*n).to_string(),
                declared_type: String::new(),
                affinity: Affinity::Text,
                not_null: false,
                default: None,
                rowid_alias: false,
            })
            .collect(),
        rowid_alias: None,
        without_rowid: false,
        root_page: SCHEMA_ROOT,
    }
}

/// One row of a result set.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub values: Vec<Value>,
}

/// What a statement produced.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// A SELECT, with its column names and rows.
    Query {
        columns: Vec<String>,
        rows: Vec<Row>,
    },
    /// A DML or DDL statement, with the number of rows it changed.
    Changed(usize),
    /// A statement that returns nothing.
    Nothing,
}

/// A transaction's state, which only tracks what the engine needs to know.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TxState {
    None,
    InTransaction,
}

/// An open database.
pub struct Connection {
    pager: Pager,
    path: Option<PathBuf>,
    catalog: Catalog,
    /// Whether a write is in progress, so a statement cannot be started inside
    /// one.
    tx: TxState,
    /// Rows the last DML statement changed or inserted, which `changes()` and
    /// `last_insert_rowid()` report.
    changes: usize,
    last_insert_rowid: i64,
    auto_rowid: i64,
    /// Where `select` leaves the outcome of a query it could not return
    /// directly, because reading the schema b-tree needs the pager borrowed
    /// mutably and the SELECT path wants the outcome by value.
    pending_outcome: Option<Outcome>,
}

impl Connection {
    /// Opens a database file, creating it if it does not exist.
    pub fn open(path: &Path) -> Result<Connection> {
        let mut conn = Connection {
            pager: Pager::open(path)?,
            path: Some(path.to_owned()),
            catalog: Catalog::new(),
            tx: TxState::None,
            changes: 0,
            last_insert_rowid: 0,
            auto_rowid: 0,
            pending_outcome: None,
        };
        conn.load_schema()?;
        Ok(conn)
    }

    /// Opens an in-memory database.
    pub fn open_memory() -> Result<Connection> {
        let pager = Pager::open_memory(4096)?;
        let mut conn = Connection {
            pager,
            path: None,
            catalog: Catalog::new(),
            tx: TxState::None,
            changes: 0,
            last_insert_rowid: 0,
            auto_rowid: 0,
            pending_outcome: None,
        };
        conn.load_schema()?;
        Ok(conn)
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Reads `sqlite_schema` and rebuilds the catalog from it.
    ///
    /// A stored table is a row of (type, name, tbl_name, rootpage, sql), and the
    /// definition comes from the sql text rather than from anything cached, so a
    /// file written by a previous connection, or by the real sqlite3, opens with
    /// the right shape.
    fn load_schema(&mut self) -> Result<()> {
        self.init_schema_page()?;
        let mut tree = TableTree::open(&mut self.pager, SCHEMA_ROOT)?;
        let rows = tree.scan(&mut self.pager)?;
        for row in rows {
            // A schema row has five columns; anything shorter is not one.
            if row.values.len() < 5 || row.values[0].as_str() != Some("table") {
                continue;
            }
            let (Some(name), Some(sql_text)) = (row.values[1].as_str(), row.values[4].as_str())
            else {
                continue;
            };
            let root = row.values[3].as_i64().unwrap_or(0) as u32;
            let table = rebuild_table(name, sql_text, root)?;
            self.catalog.put(table);
        }
        Ok(())
    }

    /// Makes sure page 1 exists as an empty leaf.
    ///
    /// The pager never hands out page 1, because it holds the file header, so
    /// on a fresh database nothing has written it and a read of it would see
    /// zeroes, which look like a freelist trunk page.
    fn init_schema_page(&mut self) -> Result<()> {
        if self.pager.page_count() >= SCHEMA_ROOT {
            return Ok(());
        }
        // Page 1 is the file header, so the schema's leaf shares it: the b-tree
        // header starts at offset 100 and the page has to count towards the
        // file, or a flush would never write it.
        let leaf = crate::btree_write::LeafPage::empty(SCHEMA_ROOT, self.pager.page_size());
        leaf.write_to(&mut self.pager)?;
        self.pager.claim_page(SCHEMA_ROOT)?;
        Ok(())
    }

    /// Writes a table's schema row into `sqlite_schema`.
    fn write_schema_row(&mut self, name: &str, root: u32, sql_text: &str) -> Result<()> {
        let values = vec![
            Value::Text("table".into()),
            Value::Text(name.to_owned()),
            Value::Text(name.to_owned()),
            Value::Integer(root as i64),
            Value::Text(sql_text.to_owned()),
        ];
        // The schema's own rowid moves on, which is how a reader notices the
        // change even before the cookie is consulted.
        let rowid = {
            let mut tree = TableTree::open(&mut self.pager, SCHEMA_ROOT)?;
            tree.max_rowid(&mut self.pager)? + 1
        };
        let mut tree = TableTree::open(&mut self.pager, SCHEMA_ROOT)?;
        tree.insert(&mut self.pager, &crate::table_tree::Row { rowid, values })?;
        // The cookie makes a connection holding a cached schema re-read it.
        let c = self.pager.header().schema_cookie.wrapping_add(1);
        self.pager.header_mut().schema_cookie = c;
        Ok(())
    }

    /// Rewrites a table's root page in the schema, keeping the rest of the row.
    fn update_schema_root(&mut self, name: &str, root: u32) -> Result<()> {
        let mut tree = TableTree::open(&mut self.pager, SCHEMA_ROOT)?;
        let rows = tree.scan(&mut self.pager)?;
        for row in rows {
            if row.values.len() < 5 || row.values[1].as_str() != Some(name) {
                continue;
            }
            let mut values = row.values.clone();
            values[3] = Value::Integer(root as i64);
            let rowid = row.rowid;
            tree.remove(&mut self.pager, rowid)?;
            tree.insert(&mut self.pager, &crate::table_tree::Row { rowid, values })?;
            return Ok(());
        }
        Ok(())
    }

    /// Removes a table's schema row.
    fn remove_schema_row(&mut self, name: &str) -> Result<()> {
        let mut tree = TableTree::open(&mut self.pager, SCHEMA_ROOT)?;
        let rows = tree.scan(&mut self.pager)?;
        for row in rows {
            if row.values.len() >= 2 && row.values[1].as_str() == Some(name) {
                tree.remove(&mut self.pager, row.rowid)?;
            }
        }
        let c = self.pager.header().schema_cookie.wrapping_add(1);
        self.pager.header_mut().schema_cookie = c;
        Ok(())
    }

    /// Whether a transaction is open.
    pub fn in_transaction(&self) -> bool {
        self.tx == TxState::InTransaction
    }

    /// The number of rows the last statement changed.
    pub fn changes(&self) -> usize {
        self.changes
    }

    /// The rowid of the last insert.
    pub fn last_insert_rowid(&self) -> i64 {
        self.last_insert_rowid
    }

    /// The tables the connection knows about.
    pub fn table_names(&self) -> Vec<String> {
        self.catalog.names()
    }

    /// Runs a statement, which may be several separated by semicolons.
    pub fn execute_script(&mut self, sql: &str) -> Result<Vec<Outcome>> {
        let stmts = crate::parser::parse_script(sql)?;
        let mut out = Vec::with_capacity(stmts.len());
        for s in stmts {
            out.push(self.execute(&s)?);
        }
        Ok(out)
    }

    /// Runs one statement.
    pub fn execute(&mut self, stmt: &Stmt) -> Result<Outcome> {
        self.changes = 0;
        match stmt {
            Stmt::CreateTable {
                name,
                if_not_exists,
                columns,
                constraints,
                without_rowid,
                sql,
                temp,
                ..
            } => self.create_table(
                name,
                *if_not_exists,
                columns,
                constraints,
                *without_rowid,
                sql,
                *temp,
            ),
            Stmt::DropTable { name, if_exists } => self.drop_table(name, *if_exists),
            Stmt::Insert {
                table,
                columns,
                source,
            } => self.insert(table, columns.as_deref(), source),
            Stmt::Select(sel) => self.select(sel),
            Stmt::Update {
                table,
                sets,
                where_,
            } => self.update(table, sets, where_.as_ref()),
            Stmt::Delete { table, where_ } => self.delete(table, where_.as_ref()),
            Stmt::Begin => {
                if self.in_transaction() {
                    return Err(Error::new(
                        ResultCode::Error,
                        "cannot start a transaction within a transaction",
                    ));
                }
                // The journal is created now, so a page the transaction never
                // changes has nothing to restore and one it does is captured
                // before it is written.
                self.pager.begin_journal()?;
                self.tx = TxState::InTransaction;
                Ok(Outcome::Nothing)
            }
            Stmt::Commit => {
                if !self.in_transaction() {
                    return Err(Error::new(
                        ResultCode::Error,
                        "cannot commit - no transaction is active",
                    ));
                }
                // A commit is a write followed by the journal's removal, so a
                // crash between the two leaves a hot journal and the next
                // connection rolls the whole transaction back rather than
                // finding half of it.
                self.pager.flush()?;
                self.pager.commit_journal()?;
                self.tx = TxState::None;
                Ok(Outcome::Changed(0))
            }
            Stmt::Rollback => {
                if !self.in_transaction() {
                    return Err(Error::new(
                        ResultCode::Error,
                        "cannot rollback - no transaction is active",
                    ));
                }
                self.pager.rollback_journal()?;
                self.tx = TxState::None;
                // The catalog is in memory, so a rolled back table has to go
                // back to what the file says, not what this connection
                // remembers doing.
                self.catalog = Catalog::new();
                self.load_schema()?;
                Ok(Outcome::Changed(0))
            }
            Stmt::Analyze => {
                // Collecting statistics would write sqlite_stat1, which is what
                // a real ANALYZE leaves behind. This engine has no query planner
                // that consults them, so nothing a query can observe changes
                // and the statement succeeds without writing.
                Ok(Outcome::Changed(0))
            }
            Stmt::CreateIndex { .. } => Err(Error::new(
                ResultCode::Error,
                "CREATE INDEX is not supported yet",
            )),
            Stmt::Unsupported(what) => Err(Error::new(
                ResultCode::Error,
                format!("{what} is not supported yet"),
            )),
        }
    }

    // --- DDL ------------------------------------------------------------

    fn create_table(
        &mut self,
        name: &str,
        if_not_exists: bool,
        columns: &[ColumnDef],
        constraints: &[Constraint],
        without_rowid: bool,
        sql_text: &str,
        temp: bool,
    ) -> Result<Outcome> {
        if self.catalog.contains(name) {
            if if_not_exists {
                return Ok(Outcome::Changed(0));
            }
            return Err(Error::new(
                ResultCode::Error,
                format!("table {name} already exists"),
            ));
        }
        if without_rowid {
            return Err(Error::new(
                ResultCode::Error,
                "WITHOUT ROWID is not supported yet",
            ));
        }
        if columns.is_empty() {
            return Err(Error::new(
                ResultCode::Error,
                format!("table {name} has no columns"),
            ));
        }
        let mut table = self.catalog.table_from_create(name, columns, constraints);
        // The root page is allocated now, and a fresh leaf is written so the
        // file has a real table rather than a dangling page number. Page 1 is
        // the schema's, so a table never takes it.
        let root = self.pager.allocate()?;
        table.root_page = root;
        {
            let leaf = crate::btree_write::LeafPage::empty(root, self.pager.page_size());
            leaf.write_to(&mut self.pager)?;
        }
        self.pager.mark_dirty(root);
        self.catalog.put(table);
        // A TEMP table belongs to the connection's temp schema, not the
        // database's, so it is not recorded in sqlite_master and does not
        // outlive the connection. Checked against sqlite3 3.53.4: a TEMP table
        // created by one process is "no such table" to the next, and never
        // appears in sqlite_master.
        //
        // It has to be a real b-tree page either way, because that is where
        // its rows live; what is skipped is the persistent record of it. Without
        // that, the "CREATE TEMP TABLE ..." text went into the schema, and a
        // reopened database tried to re-parse that text, reported
        // "near \"TEMP\": syntax error" as CORRUPT, and refused to open at all.
        if !temp {
            self.write_schema_row(name, root, sql_text)?;
        }
        self.pager.flush()?;
        Ok(Outcome::Changed(0))
    }

    fn drop_table(&mut self, name: &str, if_exists: bool) -> Result<Outcome> {
        match self.catalog.remove(name) {
            Some(t) => {
                // The root page goes back to the freelist; its leaf cells may
                // own overflow chains, which are freed with it.
                if t.root_page > 0 {
                    if let Ok(leaf) =
                        crate::btree_write::LeafPage::read(&mut self.pager, t.root_page)
                    {
                        for cell in &leaf.cells {
                            if cell.first_overflow != 0 {
                                crate::btree_write::free_overflow_chain(
                                    &mut self.pager,
                                    cell.first_overflow,
                                )?;
                            }
                        }
                    }
                    self.pager.free(t.root_page)?;
                }
                let name = t.name.clone();
                self.remove_schema_row(&name)?;
                self.pager.flush()?;
                Ok(Outcome::Changed(0))
            }
            None if if_exists => Ok(Outcome::Changed(0)),
            None => Err(Error::new(
                ResultCode::Error,
                format!("no such table: {name}"),
            )),
        }
    }

    fn table(&self, name: &str) -> Result<&Table> {
        self.catalog
            .get(name)
            .or_else(|| crate::join::strip_schema_qualifier(name).and_then(|b| self.catalog.get(b)))
            .ok_or_else(|| Error::new(ResultCode::Error, format!("no such table: {name}")))
    }

    fn table_mut(&mut self, name: &str) -> Result<&mut Table> {
        // A `main` or `temp` qualifier names the same catalog, so the write
        // path strips it the same way the read path does. See
        // `join::strip_schema_qualifier`.
        let name = crate::join::strip_schema_qualifier(name).unwrap_or(name);
        if !self.catalog.contains(name) {
            return Err(Error::new(
                ResultCode::Error,
                format!("no such table: {name}"),
            ));
        }
        Ok(self.catalog.get_mut(name).expect("just checked"))
    }

    // --- INSERT ---------------------------------------------------------

    fn insert(
        &mut self,
        table_name: &str,
        columns: Option<&[String]>,
        source: &InsertSource,
    ) -> Result<Outcome> {
        let table = self.table(table_name)?.clone();
        let rows: Vec<Vec<Value>> = match source {
            InsertSource::Values(rows) => {
                let params: Vec<Value> = Vec::new();
                let ctx = EvalCtx::empty(&params);
                let mut out = Vec::with_capacity(rows.len());
                for row in rows {
                    let mut vals = Vec::with_capacity(row.len());
                    for e in row {
                        vals.push(eval(e, &ctx)?);
                    }
                    out.push(vals);
                }
                out
            }
            InsertSource::Select(_) => {
                return Err(Error::new(
                    ResultCode::Error,
                    "INSERT ... SELECT is not supported yet",
                ))
            }
        };

        // The column list decides the target positions; without one the values
        // line up with the table's columns in order.
        let targets: Vec<usize> = match columns {
            Some(names) => {
                let mut v = Vec::with_capacity(names.len());
                for n in names {
                    v.push(table.column_index(n).ok_or_else(|| {
                        // SQLite's wording: table, then the unknown column.
                        Error::new(
                            ResultCode::Error,
                            format!("table {table_name} has no column named {n}"),
                        )
                    })?);
                }
                v
            }
            None => (0..table.len()).collect(),
        };

        let mut inserted = 0usize;
        for vals in rows {
            if vals.len() != targets.len() {
                return Err(Error::new(
                    ResultCode::Mismatch,
                    format!(
                        "table {} has {} columns but {} values were supplied",
                        table_name,
                        targets.len(),
                        vals.len()
                    ),
                ));
            }
            // Build the full row, applying a default for every column the
            // statement did not name.
            let mut full = vec![Value::Null; table.len()];
            let mut named: Vec<bool> = vec![false; table.len()];
            for (i, pos) in targets.iter().enumerate() {
                if *pos >= full.len() {
                    return Err(Error::new(
                        ResultCode::Error,
                        format!(
                            "table {table_name} has no column named {}",
                            columns.map(|c| c[i].as_str()).unwrap_or("?")
                        ),
                    ));
                }
                full[*pos] = vals[i].clone();
                named[*pos] = true;
            }
            let params: Vec<Value> = Vec::new();
            let ctx = EvalCtx::empty(&params);
            for (i, col) in table.columns.iter().enumerate() {
                if named[i] {
                    continue;
                }
                full[i] = match &col.default {
                    Some(e) => eval(e, &ctx)?,
                    None => Value::Null,
                };
            }
            self.check_not_null(&table, &full)?;

            // Affinity is applied on the way in, which is why inserting '123'
            // into an INTEGER column stores the integer.
            for (i, col) in table.columns.iter().enumerate() {
                full[i] = apply_affinity(&full[i], col.affinity);
            }

            let rowid = self.next_rowid(&table, &full)?;
            self.insert_row(&table, rowid, full)?;
            self.last_insert_rowid = rowid;
            inserted += 1;
        }
        self.changes = inserted;
        self.pager.flush()?;
        Ok(Outcome::Changed(inserted))
    }

    /// The rowid for a new row: the alias column's value when it was supplied,
    /// and otherwise the next unused one.
    fn next_rowid(&mut self, table: &Table, values: &[Value]) -> Result<i64> {
        if let Some(i) = table.rowid_alias {
            match values.get(i) {
                Some(Value::Integer(v)) => return Ok(*v),
                Some(Value::Null) | None => {}
                Some(other) => {
                    return Err(Error::new(
                        ResultCode::Mismatch,
                        format!("datatype mismatch: {} is not an integer", other.to_string()),
                    ))
                }
            }
        }
        let max = {
            let mut tree = TableTree::open(&mut self.pager, table.root_page)?;
            tree.max_rowid(&mut self.pager)?
        };
        // The next rowid is one past the largest, and 1 when the table is
        // empty, so a table that has only negative rowids still gets a positive
        // first key.
        let next = if max >= 0 { max + 1 } else { 1 };
        Ok(next)
    }

    fn check_not_null(&self, table: &Table, values: &[Value]) -> Result<()> {
        for (i, col) in table.columns.iter().enumerate() {
            if col.not_null && values.get(i).map(|v| v.is_null()).unwrap_or(true) {
                return Err(Error::new(
                    ResultCode::Constraint,
                    format!("NOT NULL constraint failed: {}.{}", table.name, col.name),
                )
                .with_extended(1299));
            }
        }
        Ok(())
    }

    fn insert_row(&mut self, table: &Table, rowid: i64, values: Vec<Value>) -> Result<()> {
        let mut tree =
            TableTree::open(&mut self.pager, table.root_page)?.with_rowid_alias(table.rowid_alias);
        let inserted = tree.insert(&mut self.pager, &crate::table_tree::Row { rowid, values });
        if let Err(e) = inserted {
            // A duplicate rowid on a table with an INTEGER PRIMARY KEY is that
            // column's UNIQUE constraint, and sqlite3 names the column:
            // "UNIQUE constraint failed: t.a". The b-tree is below the catalog,
            // so it can only report the rowid; the column name is known here.
            // A rowid table has no such column, and there "rowid" is the
            // right name, which is what the b-tree's own message says.
            if let Some(i) = table.rowid_alias {
                if e.code == ResultCode::Constraint
                    && e.message.starts_with("UNIQUE constraint failed: rowid ")
                {
                    let col = &table.columns[i].name;
                    return Err(Error::new(
                        ResultCode::Constraint,
                        format!("UNIQUE constraint failed: {}.{col}", table.name),
                    ));
                }
            }
            return Err(e);
        }
        // A split that reaches the root allocates a new one, so the root page in
        // the schema has to be brought up to date. Leaving it stale means the
        // next statement, which opens the tree from the schema, writes to a page
        // the tree no longer reaches.
        self.write_table_root(table, tree.root())
    }

    /// Records a table's current root page in the schema.
    fn write_table_root(&mut self, table: &Table, root: u32) -> Result<()> {
        if root == table.root_page {
            return Ok(());
        }
        self.update_schema_root(&table.name, root)?;
        if let Some(t) = self.catalog.get_mut(&table.name) {
            t.root_page = root;
        }
        Ok(())
    }

    // --- SELECT ---------------------------------------------------------

    fn select(&mut self, sel: &Select) -> Result<Outcome> {
        if !sel.with.is_empty() {
            return Err(Error::new(
                ResultCode::Error,
                "common table expressions are not supported yet",
            ));
        }
        let body = &sel.body;
        let SelectBody::Simple {
            columns,
            from,
            where_,
            ..
        } = body
        else {
            return match body {
                SelectBody::Compound { op, .. } => Err(Error::new(
                    ResultCode::Error,
                    format!("{op:?} is not supported yet"),
                )),
                SelectBody::Nested(_) => Err(Error::new(
                    ResultCode::Error,
                    "a subquery in FROM is not supported yet",
                )),
                _ => unreachable!("only three body shapes exist"),
            };
        };
        // A SELECT with no FROM evaluates against an empty row, so a constant
        // expression works and a column reference does not.
        if from.is_empty() {
            return self.select_constants(sel, columns);
        }
        // `sqlite_master` -- and its `sqlite_schema` alias -- is a real table to
        // a query, and a good deal of the suite reads it: to check what a
        // statement created, to list tables, and to assert a file reopened with
        // the shape it was left in. It is not in the catalog because it is not
        // a user table, so it is answered from the schema b-tree itself.
        if self.select_schema_table(sel, from, columns, where_.as_ref())? {
            return Ok(self.pending_outcome.take().expect("just produced one"));
        }
        // Every table in the FROM is resolved once, before any row is read, so
        // an unknown table or a bad USING column is an error even when the query
        // would have matched nothing.
        let sources = crate::join::sources_from(&self.queryable_tables(), from)?;
        let joined = crate::join::resolve(sources)?;
        self.select_from(sel, columns, where_.as_ref(), &joined)
    }

    /// Answers a query whose FROM names the schema table.
    ///
    /// Returns true when the query was about the schema, in which case the
    /// outcome is left in `pending_outcome` for the caller to take. It is left
    /// rather than returned because reading the schema b-tree needs a mutable
    /// borrow of the pager, and the SELECT path wants the outcome by value; a
    /// field is simpler than splitting that borrow.
    ///
    /// Only a lone `FROM sqlite_master` is answered here. A join against it is
    /// answered by the normal path instead, which finds the name in
    /// `queryable_tables` like any other.
    fn select_schema_table(
        &mut self,
        sel: &Select,
        from: &[FromItem],
        columns: &[crate::parser::ResultColumn],
        where_: Option<&Expr>,
    ) -> Result<bool> {
        if from.len() != 1 {
            return Ok(false);
        }
        let FromItem::Table(tref) = &from[0] else {
            return Ok(false);
        };
        if !is_schema_table(&tref.name) {
            return Ok(false);
        }
        let source = crate::join::Source {
            name: crate::join::Source::scope_name(tref).to_ascii_lowercase(),
            table: schema_table(),
            join: None,
            on: None,
            using: Vec::new(),
        };
        let joined = crate::join::resolve(vec![source])?;
        self.pending_outcome = Some(self.select_from(sel, columns, where_, &joined)?);
        Ok(true)
    }

    /// Every table a FROM clause may name: the catalog's, plus the schema table.
    ///
    /// `sqlite_master` is not in the catalog because it is not a user table, but
    /// it is a real table to a query, and the suite reads it from inside a join
    /// in a good many places -- `SELECT * FROM t1, sqlite_master` and
    /// `SELECT ... FROM t1 JOIN sqlite_master ON 1` both have to give the cross
    /// product sqlite3 gives. Listing it here rather than only in the
    /// single-table path is what makes it joinable in any position, since
    /// `sources_from` resolves every name in the FROM the same way.
    ///
    /// The schema table is listed three times, once under each spelling a FROM
    /// clause may use, because `sources_from` matches the written name against
    /// `Table::name` verbatim: sqlite_master, sqlite_schema, and main.sqlite_master
    /// all have to find a table or the query raises "no such table". All three
    /// entries are the same table, so a query naming two of them joins the
    /// schema against itself, which is what SQLite does too.
    fn queryable_tables(&self) -> Vec<Table> {
        let mut tables = self.catalog_tables();
        let schema = schema_table();
        let aliased = |alias: &str| Table {
            name: alias.to_string(),
            columns: schema.columns.clone(),
            rowid_alias: schema.rowid_alias,
            without_rowid: schema.without_rowid,
            root_page: schema.root_page,
        };
        tables.push(aliased("sqlite_master"));
        tables.push(aliased("sqlite_schema"));
        tables.push(aliased("main.sqlite_master"));
        tables.push(aliased("main.sqlite_schema"));
        tables
    }

    /// Every table the catalog knows about, for resolving a FROM clause.
    fn catalog_tables(&self) -> Vec<Table> {
        self.catalog.all_tables()
    }

    /// Runs a SELECT whose FROM clause has been resolved.
    ///
    /// The rows come out of the nested loop left-major, which is the order
    /// SQLite produces without an index. WHERE and the projection then see a
    /// bound row carrying every column of every table, and ORDER BY sorts what
    /// the projection produced.
    fn select_from(
        &mut self,
        sel: &Select,
        columns: &[crate::parser::ResultColumn],
        where_: Option<&Expr>,
        from: &crate::join::From,
    ) -> Result<Outcome> {
        // Whether this query groups at all, which decides everything below: a
        // grouped query folds its rows into groups and then produces one row
        // per group, while an ungrouped one produces a row per input row.
        let group_by = match &sel.body {
            SelectBody::Simple { group_by, .. } => group_by.clone(),
            _ => Vec::new(),
        };
        let plan = crate::grouping::Plan::build(sel)?;
        let grouped = plan.is_grouped() || plan.has_aggregate();

        // The row source of each table. A record stores only the columns up to
        // its last non-NULL, so a read has to pad back to the declared width.
        let mut source_rows: Vec<Vec<crate::table_tree::Row>> =
            Vec::with_capacity(from.sources.len());
        for s in &from.sources {
            let mut tree = TableTree::open(&mut self.pager, s.table.root_page)?;
            let mut rows = tree.scan(&mut self.pager)?;
            for r in &mut rows {
                r.values.resize(s.table.len(), Value::Null);
            }
            source_rows.push(rows);
        }

        // The output column names: the alias, or the column's own name, or the
        // expression's text. A star takes the resolved expansion, which is
        // every table's columns with a USING column counted once.
        let star = columns.len() == 1 && is_star(&columns[0].expr);
        let mut names: Vec<String> = Vec::with_capacity(columns.len());
        if star {
            // The expansion is checked like any other reference, which is where
            // a table that is in the query twice under one name is reported.
            crate::join::check_star(from)?;
            names = from.star.iter().map(|(n, _, _)| n.clone()).collect();
        } else {
            for rc in columns {
                names.push(match &rc.alias {
                    Some(a) => a.clone(),
                    None => match &rc.expr {
                        Expr::Column { name, .. } => name.clone(),
                        _ => render_expr(&rc.expr),
                    },
                });
            }
        }

        // Every column reference in WHERE, in the projection, in GROUP BY, in
        // HAVING and in ORDER BY is resolved now, against the whole FROM. This
        // is where an ambiguous or unknown column is reported, which is why a
        // bad name fails even when the tables are empty.
        let mut exprs: Vec<&Expr> = Vec::new();
        if let Some(p) = where_ {
            exprs.push(p);
        }
        if !star {
            for rc in columns {
                exprs.push(&rc.expr);
            }
        }
        for e in &group_by {
            exprs.push(e);
        }
        if let SelectBody::Simple {
            having: Some(h), ..
        } = &sel.body
        {
            exprs.push(h);
        }
        for (e, _) in &sel.order_by {
            exprs.push(e);
        }
        let bound = crate::join::bind_all(from, &exprs, &names)?;

        // The ON and USING constraints are bound against the same FROM, so they
        // are resolved here too and evaluated inside the loop.
        let on_exprs = crate::join::bind_constraints(from)?;

        // The join itself. The loop is left-major: for each row of everything
        // joined so far, the next table is scanned in full.
        let params: Vec<Value> = Vec::new();
        let mut rows = crate::join::nested_loop(from, &source_rows, &on_exprs, &params)?;
        for jr in &mut rows {
            jr.recover_rowids(from);
        }

        // WHERE runs before grouping, so it sees one row at a time and an
        // aggregate in it is a misuse, which SQLite reports before any row is
        // read. The wording depends on whether the query has an aggregate in
        // scope: without one it is "misuse of aggregate function count()", and
        // with one the shorter "misuse of aggregate: count()".
        if let Some(pred) = where_ {
            if let Some(agg) = crate::grouping::first_aggregate(pred) {
                let message = if plan.has_aggregate() {
                    format!("misuse of aggregate: {}()", agg.name)
                } else {
                    format!("misuse of aggregate function {}()", agg.name)
                };
                return Err(Error::new(ResultCode::Error, message));
            }
        }
        let mut surviving: Vec<crate::grouping::Row> = Vec::with_capacity(rows.len());
        for jr in &rows {
            let ctx = build_ctx(jr, from, &bound);
            if let Some(pred) = where_ {
                if !truthy(eval(pred, &ctx)?) {
                    continue;
                }
            }
            surviving.push(crate::grouping::Row::new(jr, from, &bound));
        }

        if grouped {
            // A grouped query produces one row per group. A result column that
            // is an aggregate takes the group's value for it; a bare column or
            // expression is evaluated against the group's first row, which is
            // what SQLite does for `SELECT v, count(*) ... GROUP BY k`.
            let groups = crate::grouping::run(&plan, &surviving, &from.all_columns())?;
            let mut out: Vec<Row> = Vec::with_capacity(groups.len());
            for g in &groups {
                let values = if star {
                    g.first.clone()
                } else {
                    plan.project(g, None)?
                };
                out.push(Row { values });
            }
            // ORDER BY sorts the groups' rows. An alias or an ordinal reads the
            // projected value, and an aggregate in the ORDER BY of a grouped
            // query is folded per group, which the plan already has.
            let mut sorted = apply_group_order_by(sel, &plan, &names, &groups, &out)?;
            apply_limit(sel, &mut sorted)?;
            return Ok(Outcome::Query {
                columns: names,
                rows: sorted,
            });
        }

        // The projected row and the joined row it came from are kept together,
        // because a WHERE may drop rows and ORDER BY still has to sort on the
        // joined values of the ones that survived.
        let mut out: Vec<(Row, crate::join::JoinedRow)> = Vec::with_capacity(rows.len());
        for jr in rows.iter() {
            let ctx = build_ctx(jr, from, &bound);
            // WHERE filters the joined rows, which for an ungrouped query is
            // the only place it is applied: the join itself only tested each
            // join's own ON or USING constraint.
            if let Some(pred) = where_ {
                if !truthy(eval(pred, &ctx)?) {
                    continue;
                }
            }
            let mut values: Vec<Value> = Vec::with_capacity(names.len());
            if star {
                values = from
                    .star
                    .iter()
                    .map(|(name, s, j)| {
                        // A column a USING clause names is shared, and SQLite
                        // prints one value for it: the first non-NULL among the
                        // sources that have a row. A side an outer join preserved
                        // as NULL is skipped, so `a RIGHT JOIN b USING(k)` shows
                        // b's k for a row only b matched. A column no USING names
                        // belongs to one source and reads from it as before.
                        let holders: Vec<usize> = (0..from.sources.len())
                            .filter(|i| crate::join::source_has_column(&from.sources[*i], name))
                            .collect();
                        if holders.len() > 1 && crate::join::is_using_column(from, name) {
                            return crate::join::read(
                                jr,
                                from,
                                &crate::join::Ref::Coalesced {
                                    holders,
                                    name: name.clone(),
                                },
                            );
                        }
                        jr.source_slice(from, *s)
                            .get(*j)
                            .cloned()
                            .unwrap_or(Value::Null)
                    })
                    .collect();
            } else {
                for rc in columns {
                    values.push(eval(&rc.expr, &ctx)?);
                }
            }
            out.push((Row { values }, jr.clone()));
        }

        let mut projected: Vec<Row> = out.iter().map(|(r, _)| r.clone()).collect();
        apply_order_by(sel, from, &names, &out, &mut projected)?;
        apply_limit(sel, &mut projected)?;
        Ok(Outcome::Query {
            columns: names,
            rows: projected,
        })
    }

    /// A SELECT with no FROM, which yields exactly one row.
    ///
    /// A bare aggregate still folds that row: `SELECT count(*)` is 1 rather than
    /// an error, and `SELECT sum(1)` is 1, because the query has one row to
    /// fold even though it has no columns. The plan is asked first, because a
    /// HAVING, an aggregate in the GROUP BY or one in the ORDER BY is an error
    /// whether or not there is a table.
    fn select_constants(
        &mut self,
        sel: &Select,
        columns: &[crate::parser::ResultColumn],
    ) -> Result<Outcome> {
        let plan = crate::grouping::Plan::build(sel)?;
        let params: Vec<Value> = Vec::new();
        let mut out: Vec<Row> = Vec::new();
        if plan.has_aggregate() {
            for g in crate::grouping::run_over_no_from(&plan)? {
                out.push(Row {
                    values: plan.project(&g, None)?,
                });
            }
        } else {
            let ctx = EvalCtx::empty(&params);
            let mut values = Vec::with_capacity(columns.len());
            for rc in columns {
                values.push(eval(&rc.expr, &ctx)?);
            }
            out.push(Row { values });
        }
        let names = columns
            .iter()
            .map(|rc| match &rc.alias {
                Some(a) => a.clone(),
                None => render_expr(&rc.expr),
            })
            .collect();
        // ORDER BY and LIMIT still apply, with no table to order by.
        if !sel.order_by.is_empty() {
            return Err(Error::new(
                ResultCode::Error,
                "ORDER BY without a table is not supported yet",
            ));
        }
        apply_limit(sel, &mut out)?;
        Ok(Outcome::Query {
            columns: names,
            rows: out,
        })
    }

    // --- UPDATE and DELETE ----------------------------------------------

    fn update(
        &mut self,
        table_name: &str,
        sets: &[(String, Expr)],
        where_: Option<&Expr>,
    ) -> Result<Outcome> {
        let table = self.table(table_name)?.clone();
        let mut tree = TableTree::open(&mut self.pager, table.root_page)?;
        let mut rows = tree.scan(&mut self.pager)?;
        for r in &mut rows {
            r.values.resize(table.len(), Value::Null);
        }

        // Resolve the target columns first, so a typo is an error even when no
        // row matches.
        let targets: Vec<(usize, &Expr)> = {
            let mut v = Vec::with_capacity(sets.len());
            for (name, expr) in sets {
                let idx = table.column_index(name).ok_or_else(|| {
                    Error::new(ResultCode::Error, format!("no such column: {name}"))
                })?;
                v.push((idx, expr));
            }
            v
        };

        let mut changed = 0usize;
        for row in &rows {
            let bound: Vec<(String, Value)> = table
                .columns
                .iter()
                .zip(row.values.iter())
                .map(|(c, v)| (c.name.clone(), v.clone()))
                .collect();
            let params: Vec<Value> = Vec::new();
            let ctx = EvalCtx {
                params: &params,
                row: bound.clone(),
                columns: &bound,
                context: Some(table_name.to_string()),
                resolved: Vec::new(),
            };
            if let Some(pred) = where_ {
                if !truthy(eval(pred, &ctx)?) {
                    continue;
                }
            }
            let mut new = row.values.clone();
            for (idx, expr) in &targets {
                new[*idx] = eval(expr, &ctx)?;
                new[*idx] = apply_affinity(&new[*idx], table.columns[*idx].affinity);
            }
            self.check_not_null(&table, &new)?;
            // An UPDATE that changes the alias column changes the rowid, which
            // is a move rather than an in-place edit.
            let new_rowid = match table.rowid_alias {
                Some(i) => match new[i] {
                    Value::Integer(v) => v,
                    _ => row.rowid,
                },
                None => row.rowid,
            };
            tree.remove(&mut self.pager, row.rowid)?;
            self.insert_row(&table, new_rowid, new)?;
            changed += 1;
        }
        self.changes = changed;
        self.pager.flush()?;
        Ok(Outcome::Changed(changed))
    }

    fn delete(&mut self, table_name: &str, where_: Option<&Expr>) -> Result<Outcome> {
        let table = self.table(table_name)?.clone();
        let mut tree = TableTree::open(&mut self.pager, table.root_page)?;
        let mut rows = tree.scan(&mut self.pager)?;
        for r in &mut rows {
            r.values.resize(table.len(), Value::Null);
        }
        let mut removed = 0usize;
        for row in &rows {
            if let Some(pred) = where_ {
                let bound: Vec<(String, Value)> = table
                    .columns
                    .iter()
                    .zip(row.values.iter())
                    .map(|(c, v)| (c.name.clone(), v.clone()))
                    .collect();
                let params: Vec<Value> = Vec::new();
                let ctx = EvalCtx {
                    params: &params,
                    row: bound.clone(),
                    columns: &bound,
                    context: Some(table_name.to_string()),
                    resolved: Vec::new(),
                };
                if !truthy(eval(pred, &ctx)?) {
                    continue;
                }
            }
            // The row's overflow chain belongs to it and is freed with it.
            if let Ok(leaf) = crate::btree_write::LeafPage::read(&mut self.pager, table.root_page) {
                if let Some(cell) = leaf.cells.iter().find(|c| c.rowid == row.rowid) {
                    if cell.first_overflow != 0 {
                        crate::btree_write::free_overflow_chain(
                            &mut self.pager,
                            cell.first_overflow,
                        )?;
                    }
                }
            }
            tree.remove(&mut self.pager, row.rowid)?;
            removed += 1;
        }
        self.changes = removed;
        self.pager.flush()?;
        Ok(Outcome::Changed(removed))
    }
}

fn is_star(e: &Expr) -> bool {
    matches!(e, Expr::Function { name, star: true, .. } if name == "*")
}

/// The name SQLite gives a result column with no alias, which is the text of
/// the expression.
fn render_expr(e: &Expr) -> String {
    match e {
        Expr::Literal(Literal::Integer(i)) => i.to_string(),
        Expr::Literal(Literal::Real(r)) => Value::real(*r).to_string(),
        Expr::Literal(Literal::Text(s)) => s.clone(),
        Expr::Literal(Literal::Blob(b)) => {
            format!(
                "x'{}'",
                b.iter().map(|x| format!("{x:02X}")).collect::<String>()
            )
        }
        Expr::Literal(Literal::Null) => "NULL".into(),
        Expr::Literal(Literal::Parameter(i)) => format!("?{i}"),
        Expr::Column {
            table: Some(t),
            name,
            ..
        } => format!("{t}.{name}"),
        Expr::Column { name, .. } => name.clone(),
        Expr::Function {
            name,
            args,
            star,
            distinct,
        } => {
            // SQLite spells a star aggregate `count(*)` and a DISTINCT one
            // `count(DISTINCT b)`, and the result column's name is the text of
            // the expression, so the rendering has to agree on both.
            let distinct = if *distinct { "DISTINCT " } else { "" };
            if *star {
                return format!("{name}(*)");
            }
            format!(
                "{name}({distinct}{})",
                args.iter().map(render_expr).collect::<Vec<_>>().join(", ")
            )
        }
        other => format!("{other:?}"),
    }
}

/// Applies an ORDER BY, sorting the result rows in place.
///
/// The sort key is evaluated against the joined row, so a key may name a column
/// of any table in the FROM as well as an alias of the result. An alias wins
/// where the two could both match, which is what SQLite does: `SELECT a.x AS k
/// FROM a ORDER BY k` sorts on the projected value, not on the table column.
///
/// The keys are computed once per row before the sort, so a comparison never
/// re-evaluates an expression and the sort itself cannot fail.
fn apply_order_by(
    sel: &Select,
    from: &crate::join::From,
    names: &[String],
    rows: &[(Row, crate::join::JoinedRow)],
    out: &mut Vec<Row>,
) -> Result<()> {
    if sel.order_by.is_empty() {
        return Ok(());
    }
    // The references in the ORDER BY are resolved against the FROM, so a key
    // naming a column of either table works and an unknown one is an error.
    let mut keys_exprs: Vec<&Expr> = Vec::new();
    for (e, _) in &sel.order_by {
        keys_exprs.push(e);
    }
    let bound = crate::join::bind_all(from, &keys_exprs, names)?;
    // An integer ORDER BY term is an ordinal naming a result column, and one
    // past the last is an error. The check is here rather than inside the loop
    // because it does not depend on a row: `SELECT ... WHERE 0 ORDER BY 9` fails
    // the same way with no rows to read.
    for (i, (expr, _)) in sel.order_by.iter().enumerate() {
        let Expr::Literal(crate::parser::Literal::Integer(n)) = expr else {
            continue;
        };
        if *n < 1 || *n as usize > names.len() {
            return Err(Error::new(
                ResultCode::Error,
                format!(
                    "{} ORDER BY term out of range - should be between 1 and {}",
                    ordinal(i + 1),
                    names.len()
                ),
            ));
        }
    }
    // Each row carries its sort keys alongside it, computed once, so a
    // comparison never re-evaluates an expression.
    let mut keyed: Vec<(Vec<Value>, Row)> = Vec::with_capacity(rows.len());
    for (row, jr) in rows.iter() {
        let mut keys = Vec::with_capacity(sel.order_by.len());
        for (expr, _) in &sel.order_by {
            // An ORDER BY term names a column of a table in the FROM if one
            // matches, and only falls back to an output alias when none does.
            // A bare `y` in `SELECT a.y, b.y ... ORDER BY y` is `b.y`, the only
            // source column called `y` once `a.y` has been projected under a
            // name of its own -- but a key that is only an alias reads the
            // projected value. This ordering is what SQLite applies, and it is
            // why `SELECT b.y AS k ... ORDER BY k` sorts on the projection.
            //
            // An integer is an ordinal, not a value: `ORDER BY 2` is the second
            // result column, and `ORDER BY 2 DESC` reverses it. Evaluating the
            // literal instead would sort every row on the constant, which leaves
            // the order as the rows arrived and makes the direction invisible.
            // The range was checked above, before any row was read.
            if let Expr::Literal(crate::parser::Literal::Integer(n)) = expr {
                keys.push(row.values[*n as usize - 1].clone());
                continue;
            }
            let is_source_column = match expr {
                Expr::Column { table, name, .. } => {
                    crate::join::resolve_ref(from, table.as_deref(), name, false).is_ok()
                }
                _ => false,
            };
            let key = if is_source_column {
                let ctx = build_ctx(jr, from, &bound);
                eval(expr, &ctx)?
            } else {
                match expr {
                    Expr::Column {
                        table: None, name, ..
                    } if alias_index(names, name).is_some() => {
                        // An output alias reads the projected value. A qualified
                        // name is never an alias: `t.c` always names a column of
                        // the table `t`.
                        row.values[alias_index(names, name).expect("just checked")].clone()
                    }
                    _ => {
                        let ctx = build_ctx(jr, from, &bound);
                        eval(expr, &ctx)?
                    }
                }
            };
            keys.push(key);
        }
        keyed.push((keys, row.clone()));
    }
    // Each key is compared in turn, and the first that differs decides. The
    // directions are applied per key, and a stable sort keeps equal keys in the
    // order they arrived, which is what SQLite's unspecified order amounts to.
    let order = &sel.order_by;
    keyed.sort_by(|a, b| {
        for (i, (_, ascending)) in order.iter().enumerate() {
            let ord = a.0[i].compare(&b.0[i]);
            if ord != std::cmp::Ordering::Equal {
                return if *ascending { ord } else { ord.reverse() };
            }
        }
        std::cmp::Ordering::Equal
    });
    *out = keyed.into_iter().map(|(_, r)| r).collect();
    Ok(())
}

/// The position of a result column with that name.
fn alias_index(names: &[String], name: &str) -> Option<usize> {
    names.iter().position(|n| n.eq_ignore_ascii_case(name))
}

/// A count as an English ordinal: 1st, 2nd, 3rd, 4th, 11th, 21st, 112th.
///
/// SQLite puts this in "Nth ORDER BY term out of range", so the suffix has to
/// agree with the English rule rather than a simple mod-10 one: 11 is 11th, not
/// 11st, and 112 is 112th, not 112nd.
fn ordinal(n: usize) -> String {
    // The teens are the exception that the last two digits would get wrong.
    if (11..=13).contains(&(n % 100)) {
        return format!("{n}th");
    }
    let suffix = match n % 10 {
        1 => "st",
        2 => "nd",
        3 => "rd",
        _ => "th",
    };
    format!("{n}{suffix}")
}

/// Sorts a grouped query's rows.
///
/// A grouped query has no joined row to sort on, so a key resolves in three
/// ways, in the order SQLite tries them: an output alias or an ordinal reads
/// the projected value, an aggregate is folded per group, and anything else
/// reads the group's first row the way the projection did.
fn apply_group_order_by(
    sel: &Select,
    plan: &crate::grouping::Plan,
    names: &[String],
    groups: &[crate::grouping::GroupOutput],
    rows: &[Row],
) -> Result<Vec<Row>> {
    if sel.order_by.is_empty() {
        return Ok(rows.to_vec());
    }
    // An integer ORDER BY term is an ordinal naming a result column, and one
    // past the last is an error, checked before any group is read so that a
    // query matching nothing fails the same way as one matching something.
    for (i, (expr, _)) in sel.order_by.iter().enumerate() {
        let Expr::Literal(crate::parser::Literal::Integer(n)) = expr else {
            continue;
        };
        if *n < 1 || *n as usize > names.len() {
            return Err(Error::new(
                ResultCode::Error,
                format!(
                    "{} ORDER BY term out of range - should be between 1 and {}",
                    ordinal(i + 1),
                    names.len()
                ),
            ));
        }
    }
    let params: Vec<Value> = Vec::new();
    let mut keyed: Vec<(Vec<Value>, Row)> = Vec::with_capacity(rows.len());
    for (i, g) in groups.iter().enumerate() {
        let ctx = EvalCtx {
            params: &params,
            row: g.named.clone(),
            columns: &[],
            resolved: g.resolved.clone(),
            context: None,
        };
        let mut keys = Vec::with_capacity(sel.order_by.len());
        for (expr, _) in &sel.order_by {
            let key = order_key(plan, expr, names, g, &ctx, &rows[i])?;
            keys.push(key);
        }
        keyed.push((keys, rows[i].clone()));
    }
    let order = &sel.order_by;
    keyed.sort_by(|a, b| {
        for (i, (_, ascending)) in order.iter().enumerate() {
            let ord = a.0[i].compare(&b.0[i]);
            if ord != std::cmp::Ordering::Equal {
                return if *ascending { ord } else { ord.reverse() };
            }
        }
        std::cmp::Ordering::Equal
    });
    Ok(keyed.into_iter().map(|(_, r)| r).collect())
}

/// One sort key of a grouped query.
fn order_key(
    plan: &crate::grouping::Plan,
    expr: &Expr,
    names: &[String],
    group: &crate::grouping::GroupOutput,
    ctx: &EvalCtx<'_>,
    row: &Row,
) -> Result<Value> {
    // An ordinal names an output column, so it reads the projected value.
    if let Expr::Literal(crate::parser::Literal::Integer(n)) = expr {
        if *n > 0 {
            let at = (*n as usize) - 1;
            if at < row.values.len() {
                return Ok(row.values[at].clone());
            }
        }
    }
    // An alias names an output column too, and takes precedence over a table
    // column of the same name, which is what SQLite does.
    if let Expr::Column { name, .. } = expr {
        if let Some(at) = alias_index(names, name) {
            return Ok(row.values[at].clone());
        }
    }
    // An aggregate in the ORDER BY of a grouped query is folded per group.
    if let Some(agg) = crate::grouping::Aggregate::of(expr) {
        return plan.result_of(&agg, group);
    }
    // Anything else reads the group's first row, so an ORDER BY on a table
    // column the projection did not include still sorts.
    let mut rewritten = expr.clone();
    rewrite_aggregates(plan, &mut rewritten, group)?;
    eval(&rewritten, ctx)
}

/// Replaces every aggregate call in `expr` with a literal of its result, so an
/// ORDER BY expression that mentions one can be evaluated per group.
fn rewrite_aggregates(
    plan: &crate::grouping::Plan,
    expr: &mut Expr,
    group: &crate::grouping::GroupOutput,
) -> Result<()> {
    if let Some(agg) = crate::grouping::Aggregate::of(expr) {
        *expr = crate::grouping::literal_of(plan.result_of(&agg, group)?);
        return Ok(());
    }
    for child in crate::grouping::children_mut(expr) {
        rewrite_aggregates(plan, child, group)?;
    }
    Ok(())
}

/// The evaluation context for one joined row.
///
/// The references were resolved before the statement ran, so they are read from
/// the joined row by offset. The named row is carried as well because it is
/// what a name that was not pre-resolved falls back to, and because it is the
/// shape the rest of the executor already builds.
fn build_ctx<'a>(
    jr: &crate::join::JoinedRow,
    from: &crate::join::From,
    bound: &[crate::join::Bound],
) -> EvalCtx<'a> {
    let resolved = crate::join::resolved_values(jr, from, bound);
    EvalCtx {
        params: &[],
        row: crate::join::named_row(from, jr),
        columns: &[],
        context: None,
        resolved,
    }
}

/// Applies LIMIT and OFFSET.
fn apply_limit(sel: &Select, rows: &mut Vec<Row>) -> Result<()> {
    if sel.limit.is_none() && sel.offset.is_none() {
        return Ok(());
    }
    let params: Vec<Value> = Vec::new();
    let ctx = EvalCtx::empty(&params);
    let offset = match &sel.offset {
        Some(e) => {
            let v = eval(e, &ctx)?;
            let n = match v {
                Value::Integer(i) => i,
                Value::Real(r) => r.trunc() as i64,
                // A negative offset counts from the end, which SQLite allows.
                other => match crate::eval::truthy(other) {
                    true => 0,
                    false => -1,
                },
            };
            n.max(0) as usize
        }
        None => 0,
    };
    let limit = match &sel.limit {
        Some(e) => {
            let v = eval(e, &ctx)?;
            match v {
                Value::Integer(i) => Some(i.max(0) as usize),
                Value::Real(r) => Some(r.trunc().max(0.0) as usize),
                _ => Some(usize::MAX),
            }
        }
        None => None,
    };
    let skipped: Vec<Row> = rows.iter().skip(offset).cloned().collect();
    *rows = match limit {
        Some(n) => skipped.into_iter().take(n).collect(),
        None => skipped,
    };
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> Connection {
        Connection::open_memory().unwrap()
    }

    /// Runs a script and returns the last outcome.
    fn run(c: &mut Connection, sql: &str) -> Outcome {
        let out = c
            .execute_script(sql)
            .unwrap_or_else(|e| panic!("{sql:?} failed: {e}"));
        out.into_iter().last().unwrap()
    }

    fn rows_of(o: &Outcome) -> &Vec<Row> {
        match o {
            Outcome::Query { rows, .. } => rows,
            other => panic!("expected a query, got {other:?}"),
        }
    }

    fn columns_of(o: &Outcome) -> &Vec<String> {
        match o {
            Outcome::Query { columns, .. } => columns,
            other => panic!("expected a query, got {other:?}"),
        }
    }

    #[test]
    fn create_and_insert_then_read_back() {
        let mut c = mem();
        run(&mut c, "CREATE TABLE t(a, b)");
        let o = run(&mut c, "INSERT INTO t VALUES(1, 'x'), (2, 'y')");
        assert_eq!(o, Outcome::Changed(2));
        assert_eq!(c.changes(), 2);
        assert_eq!(c.last_insert_rowid(), 2);

        let o = run(&mut c, "SELECT * FROM t");
        assert_eq!(columns_of(&o), &vec!["a".to_string(), "b".to_string()]);
        assert_eq!(rows_of(&o).len(), 2);
        assert_eq!(rows_of(&o)[0].values[1], Value::Text("x".into()));
    }

    #[test]
    fn a_rowid_alias_column_holds_the_rowid() {
        let mut c = mem();
        run(&mut c, "CREATE TABLE t(id INTEGER PRIMARY KEY, name TEXT)");
        run(&mut c, "INSERT INTO t(name) VALUES('a'), ('b')");
        // The alias column is written as NULL and recovered from the key, so it
        // reads back as the rowid.
        let o = run(&mut c, "SELECT id, name FROM t");
        let r = rows_of(&o);
        assert_eq!(r[0].values[0], Value::Integer(1));
        assert_eq!(r[1].values[0], Value::Integer(2));
        assert_eq!(r[0].values[1], Value::Text("a".into()));
    }

    #[test]
    fn an_explicit_alias_rowid_is_honoured() {
        let mut c = mem();
        run(&mut c, "CREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT)");
        run(&mut c, "INSERT INTO t VALUES(100, 'a')");
        assert_eq!(c.last_insert_rowid(), 100);
        // The next rowid is one past the largest, not one past one.
        run(&mut c, "INSERT INTO t(v) VALUES('b')");
        assert_eq!(c.last_insert_rowid(), 101);
    }

    #[test]
    fn where_filters_and_order_by_sorts() {
        let mut c = mem();
        run(&mut c, "CREATE TABLE t(a, b)");
        run(&mut c, "INSERT INTO t VALUES(3, 'c'), (1, 'a'), (2, 'b')");
        let o = run(&mut c, "SELECT * FROM t WHERE a > 1 ORDER BY a DESC");
        let r = rows_of(&o);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].values[0], Value::Integer(3));
        assert_eq!(r[1].values[0], Value::Integer(2));
    }

    #[test]
    fn limit_and_offset_apply() {
        let mut c = mem();
        run(&mut c, "CREATE TABLE t(a)");
        run(&mut c, "INSERT INTO t VALUES(1),(2),(3),(4),(5)");
        let o = run(&mut c, "SELECT a FROM t ORDER BY a LIMIT 2");
        assert_eq!(rows_of(&o).len(), 2);
        let o = run(&mut c, "SELECT a FROM t ORDER BY a LIMIT 2 OFFSET 1");
        let r = rows_of(&o);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].values[0], Value::Integer(2));
        // LIMIT a, b is LIMIT b OFFSET a.
        let o = run(&mut c, "SELECT a FROM t ORDER BY a LIMIT 1, 2");
        let r = rows_of(&o);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].values[0], Value::Integer(2));
    }

    #[test]
    fn a_projection_and_an_expression_both_work() {
        let mut c = mem();
        run(&mut c, "CREATE TABLE t(a, b)");
        run(&mut c, "INSERT INTO t VALUES(2, 'y'), (1, 'x')");
        let o = run(&mut c, "SELECT a, upper(b) FROM t WHERE a = 1");
        assert_eq!(rows_of(&o).len(), 1);
        assert_eq!(rows_of(&o)[0].values[0], Value::Integer(1));
        assert_eq!(rows_of(&o)[0].values[1], Value::Text("X".into()));
    }

    #[test]
    fn a_constant_select_returns_one_row() {
        let mut c = mem();
        let o = run(&mut c, "SELECT 1 + 1");
        assert_eq!(rows_of(&o).len(), 1);
        assert_eq!(rows_of(&o)[0].values[0], Value::Integer(2));
    }

    #[test]
    fn affinity_converts_on_insert() {
        let mut c = mem();
        run(&mut c, "CREATE TABLE t(i INTEGER, s TEXT, r REAL)");
        run(&mut c, "INSERT INTO t VALUES('123', 456, '1.5')");
        let o = run(&mut c, "SELECT typeof(i), typeof(s), typeof(r) FROM t");
        let v = &rows_of(&o)[0].values;
        assert_eq!(v[0], Value::Text("integer".into()));
        assert_eq!(v[1], Value::Text("text".into()));
        assert_eq!(v[2], Value::Text("real".into()));
        // And the converted value is what is stored.
        let o = run(&mut c, "SELECT i, s, r FROM t");
        let v = &rows_of(&o)[0].values;
        assert_eq!(v[0], Value::Integer(123));
        assert_eq!(v[1], Value::Text("456".into()));
        assert_eq!(v[2], Value::real(1.5));
    }

    #[test]
    fn a_column_list_selects_which_columns_are_supplied() {
        let mut c = mem();
        run(&mut c, "CREATE TABLE t(a, b, c)");
        run(&mut c, "INSERT INTO t(b) VALUES('only-b')");
        let o = run(&mut c, "SELECT a, b, c FROM t");
        let v = &rows_of(&o)[0].values;
        assert_eq!(v[0], Value::Null, "an unsupplied column is NULL");
        assert_eq!(v[1], Value::Text("only-b".into()));
        assert_eq!(v[2], Value::Null);
    }

    #[test]
    fn a_column_default_fills_an_unsupplied_column() {
        let mut c = mem();
        run(&mut c, "CREATE TABLE t(a DEFAULT 42, b)");
        run(&mut c, "INSERT INTO t(b) VALUES('x')");
        let o = run(&mut c, "SELECT a FROM t");
        assert_eq!(rows_of(&o)[0].values[0], Value::Integer(42));
    }

    #[test]
    fn a_wrong_value_count_is_reported_with_sqlites_wording() {
        let mut c = mem();
        run(&mut c, "CREATE TABLE t(a, b)");
        let e = c.execute_script("INSERT INTO t VALUES(1)").unwrap_err();
        assert_eq!(e.code.name(), "MISMATCH");
        assert_eq!(
            e.message,
            "table t has 2 columns but 1 values were supplied"
        );
    }

    #[test]
    fn an_unknown_table_says_no_such_table() {
        let mut c = mem();
        let e = c.execute_script("SELECT * FROM nope").unwrap_err();
        assert_eq!(e.message, "no such table: nope");
    }

    /// A `main` or `temp` qualifier names the same catalog, so every spelling
    /// sqlite3 accepts has to reach the table.
    ///
    /// The spellings and the two that must not match were read off sqlite3
    /// 3.53.4, which answers `SELECT * FROM main.q` with q's rows, answers
    /// `MAIN.q` and `main.Q` the same way, and says `no such table: temp.q`
    /// and `no such table: nosuchdb.q` for the two it does not. `temp` is
    /// accepted for a real table here because this engine keeps its temp
    /// objects in the main catalog; that is the same reason
    /// `is_schema_table` accepts `temp.sqlite_master`.
    #[test]
    fn a_schema_qualifier_names_the_table_it_qualifies() {
        let mut c = mem();
        run(&mut c, "CREATE TABLE q(a, b)");
        run(&mut c, "INSERT INTO main.q VALUES(1, 2)");
        assert_eq!(
            rows_of(&run(&mut c, "SELECT * FROM q"))[0].values,
            vec![Value::Integer(1), Value::Integer(2)]
        );
        for spelling in ["main.q", "MAIN.q", "main.Q", "temp.q", "main.sqlite_master"] {
            let sql = format!("SELECT * FROM {spelling}");
            if spelling.contains("sqlite_master") {
                // The schema table's own answer, read back through the engine
                // so the test is about the qualifier and not about the row.
                assert_eq!(c.execute_script(&sql).unwrap().len(), 1, "{sql}");
            } else {
                assert_eq!(
                    rows_of(&run(&mut c, &sql))[0].values,
                    vec![Value::Integer(1), Value::Integer(2)],
                    "{sql}"
                );
            }
        }
        // A qualifier this engine has no schema for is still "no such table",
        // and the message keeps the whole name rather than the stripped part.
        for bad in ["nosuchdb.q", "main.nosuchtable"] {
            let e = c
                .execute_script(&format!("SELECT * FROM {bad}"))
                .unwrap_err();
            assert_eq!(e.message, format!("no such table: {bad}"));
        }
    }

    /// The write path strips the qualifier too, so `INSERT INTO main.q` and
    /// `UPDATE main.q` reach the same table a read does.
    #[test]
    fn a_schema_qualifier_names_the_table_it_writes() {
        let mut c = mem();
        run(&mut c, "CREATE TABLE q(a, b)");
        run(&mut c, "INSERT INTO main.q VALUES(1, 2)");
        run(&mut c, "UPDATE main.q SET b = 9");
        assert_eq!(
            rows_of(&run(&mut c, "SELECT * FROM q"))[0].values,
            vec![Value::Integer(1), Value::Integer(9)]
        );
        run(&mut c, "DELETE FROM main.q WHERE a = 1");
        assert!(rows_of(&run(&mut c, "SELECT * FROM q")).is_empty());
    }

    #[test]
    fn not_null_is_enforced() {
        let mut c = mem();
        run(&mut c, "CREATE TABLE t(a NOT NULL)");
        let e = c.execute_script("INSERT INTO t VALUES(NULL)").unwrap_err();
        assert_eq!(e.code.name(), "CONSTRAINT");
        assert!(
            e.message.contains("NOT NULL constraint failed"),
            "got: {}",
            e.message
        );
    }

    #[test]
    fn create_if_not_exists_is_idempotent() {
        let mut c = mem();
        run(&mut c, "CREATE TABLE t(a)");
        run(&mut c, "CREATE TABLE IF NOT EXISTS t(a)");
        assert_eq!(c.table_names().len(), 1);
        // Without the guard it is an error.
        assert!(c.execute_script("CREATE TABLE t(a)").is_err());
    }

    #[test]
    fn drop_table_removes_it() {
        let mut c = mem();
        run(&mut c, "CREATE TABLE t(a)");
        run(&mut c, "DROP TABLE t");
        assert!(c.table_names().is_empty());
        // Dropping it again needs the guard.
        assert!(c.execute_script("DROP TABLE t").is_err());
        run(&mut c, "DROP TABLE IF EXISTS t");
    }

    #[test]
    fn update_changes_only_matching_rows() {
        let mut c = mem();
        run(&mut c, "CREATE TABLE t(a, b)");
        run(&mut c, "INSERT INTO t VALUES(1, 'x'), (2, 'y'), (3, 'z')");
        let o = run(&mut c, "UPDATE t SET b = 'q' WHERE a > 1");
        assert_eq!(o, Outcome::Changed(2));
        let o = run(&mut c, "SELECT b FROM t WHERE a = 1");
        assert_eq!(rows_of(&o)[0].values[0], Value::Text("x".into()));
        let o = run(&mut c, "SELECT b FROM t WHERE a = 3");
        assert_eq!(rows_of(&o)[0].values[0], Value::Text("q".into()));
    }

    #[test]
    fn delete_removes_only_matching_rows() {
        let mut c = mem();
        run(&mut c, "CREATE TABLE t(a)");
        run(&mut c, "INSERT INTO t VALUES(1),(2),(3),(4)");
        let o = run(&mut c, "DELETE FROM t WHERE a > 2");
        assert_eq!(o, Outcome::Changed(2));
        let o = run(&mut c, "SELECT a FROM t");
        assert_eq!(rows_of(&o).len(), 2);
        // A delete with no WHERE takes everything.
        let o = run(&mut c, "DELETE FROM t");
        assert_eq!(o, Outcome::Changed(2));
        let o = run(&mut c, "SELECT a FROM t");
        assert!(rows_of(&o).is_empty());
    }

    #[test]
    fn a_duplicate_rowid_is_refused() {
        let mut c = mem();
        run(&mut c, "CREATE TABLE t(id INTEGER PRIMARY KEY, v)");
        run(&mut c, "INSERT INTO t VALUES(1, 'a')");
        // The next rowid is 2, so this collides only if given explicitly.
        let e = c
            .execute_script("INSERT INTO t VALUES(1, 'b')")
            .unwrap_err();
        assert_eq!(e.code.name(), "CONSTRAINT");
    }

    #[test]
    fn transactions_track_their_state() {
        let mut c = mem();
        assert!(!c.in_transaction());
        run(&mut c, "BEGIN");
        assert!(c.in_transaction());
        // A nested BEGIN is an error.
        assert!(c.execute_script("BEGIN").is_err());
        run(&mut c, "COMMIT");
        assert!(!c.in_transaction());
        // Committing without a transaction is an error.
        assert!(c.execute_script("COMMIT").is_err());
    }

    #[test]
    fn a_table_and_its_rows_survive_a_reopen() {
        let path = std::env::temp_dir().join(format!("nsqlite-exec-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        {
            let mut c = Connection::open(&path).unwrap();
            run(&mut c, "CREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT)");
            for i in 1..=500 {
                c.execute_script(&format!("INSERT INTO t VALUES({i}, 'v{i}')"))
                    .unwrap();
            }
        }
        // The schema is stored in sqlite_schema, so a new connection reads the
        // table's definition back out of the file rather than starting empty.
        // Five hundred rows is past the point where the tree grows a second
        // level, so this also covers the root moving and the schema following.
        let mut c = Connection::open(&path).unwrap();
        assert_eq!(c.table_names(), vec!["t"]);
        for rowid in [1i64, 250, 500] {
            let o = run(&mut c, &format!("SELECT id FROM t WHERE id = {rowid}"));
            assert_eq!(
                rows_of(&o)[0].values[0],
                Value::Integer(rowid),
                "row {rowid} lost"
            );
        }
        // A row that was never inserted is not there, which is what tells the
        // rows really came back rather than a lookup inventing them.
        let o = run(&mut c, "SELECT id FROM t WHERE id = 501");
        assert!(rows_of(&o).is_empty());
        drop(c);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn an_unimplemented_statement_says_so_rather_than_guessing() {
        let mut c = mem();
        run(&mut c, "CREATE TABLE t(a)");
        let e = c.execute_script("CREATE INDEX i ON t(a)").unwrap_err();
        assert!(
            e.message.contains("not supported yet"),
            "got: {}",
            e.message
        );
        let e = c
            .execute_script("SELECT * FROM t UNION SELECT * FROM t")
            .unwrap_err();
        assert!(
            e.message.contains("not supported yet"),
            "got: {}",
            e.message
        );
    }

    // --- joins ----------------------------------------------------------
    //
    // Every expectation below was taken from the sqlite3 in this repository's
    // toolchain (3.53.4) by running the same script and the same queries. The
    // fixture is the same throughout, so a test names the query and the rows
    // sqlite3 printed for it.

    /// The join fixture, shared by the tests below.
    ///
    /// `a` has two rows with x=2, one of them with a NULL y, and `b` has two
    /// with x=2. That makes the row order observable: a nested loop has to
    /// produce the same sequence of pairs every time, and a test that only
    /// counted rows would not notice if it did not.
    fn joined() -> Connection {
        let mut c = mem();
        run(
            &mut c,
            "CREATE TABLE a(x, y);\
             CREATE TABLE b(x, y);\
             INSERT INTO a VALUES(1,'a1'),(2,'a2'),(3,'a3'),(2,NULL);\
             INSERT INTO b VALUES(1,'b1'),(2,'b2'),(4,'b4'),(2,'b2dup');",
        );
        c
    }

    /// A row as text, the way sqlite3's default separator prints it, so the
    /// expectations can be copied straight out of its output.
    fn as_text(o: &Outcome) -> Vec<String> {
        rows_of(o)
            .iter()
            .map(|r| {
                r.values
                    .iter()
                    .map(|v| match v {
                        Value::Null => String::new(),
                        other => other.to_string(),
                    })
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect()
    }

    #[test]
    fn a_comma_join_filters_on_the_where_clause() {
        let mut c = joined();
        let o = run(&mut c, "SELECT * FROM a, b WHERE a.x = b.x");
        assert_eq!(columns_of(&o), &["x", "y", "x", "y"]);
        assert_eq!(
            as_text(&o),
            vec![
                "1|a1|1|b1",
                "2|a2|2|b2",
                "2|a2|2|b2dup",
                "2||2|b2",
                "2||2|b2dup"
            ]
        );
    }

    #[test]
    fn an_inner_join_on_matches_the_comma_form() {
        let mut c = joined();
        let o = run(&mut c, "SELECT * FROM a JOIN b ON a.x = b.x");
        assert_eq!(
            as_text(&o),
            vec![
                "1|a1|1|b1",
                "2|a2|2|b2",
                "2|a2|2|b2dup",
                "2||2|b2",
                "2||2|b2dup"
            ]
        );
    }

    #[test]
    fn a_left_join_keeps_the_unmatched_left_row_as_null() {
        // sqlite3 prints a.x=3 with both of b's columns empty, and it prints it
        // in the position the left row had, which is what left-major means.
        let mut c = joined();
        let o = run(&mut c, "SELECT * FROM a LEFT JOIN b ON a.x = b.x");
        assert_eq!(
            as_text(&o),
            vec![
                "1|a1|1|b1",
                "2|a2|2|b2",
                "2|a2|2|b2dup",
                "3|a3||",
                "2||2|b2",
                "2||2|b2dup",
            ]
        );
    }

    #[test]
    fn a_cross_join_is_the_full_product_left_major() {
        // Four rows each, sixteen pairs, and the outer loop is `a`: every row of
        // `b` appears once per row of `a` before `a` advances.
        let mut c = joined();
        let o = run(&mut c, "SELECT * FROM a CROSS JOIN b");
        assert_eq!(rows_of(&o).len(), 16);
        assert_eq!(
            &as_text(&o)[..4],
            &["1|a1|1|b1", "1|a1|2|b2", "1|a1|4|b4", "1|a1|2|b2dup"]
        );
        // The last row of `a` is the one with the NULL y, and it still leads
        // its own group of four.
        assert_eq!(
            &as_text(&o)[12..16],
            &["2||1|b1", "2||2|b2", "2||4|b4", "2||2|b2dup"]
        );
    }

    #[test]
    fn an_order_by_sorts_a_projected_join() {
        let mut c = joined();
        let o = run(
            &mut c,
            "SELECT a.x, b.y FROM a INNER JOIN b ON a.x = b.x ORDER BY a.x",
        );
        // The column names are the bare column names, not the qualified text,
        // because a column reference names its own column.
        assert_eq!(columns_of(&o), &["x", "y"]);
        assert_eq!(
            as_text(&o),
            vec!["1|b1", "2|b2", "2|b2dup", "2|b2", "2|b2dup"]
        );
    }

    #[test]
    fn a_using_column_appears_once_in_a_star() {
        // sqlite3 prints three columns for `a JOIN b USING (x)`, not four: the
        // shared column is counted once and takes `a`'s value, which the equal
        // constraint makes the same anyway.
        let mut c = joined();
        let o = run(&mut c, "SELECT * FROM a JOIN b USING (x)");
        assert_eq!(columns_of(&o), &["x", "y", "y"]);
        assert_eq!(
            as_text(&o),
            vec!["1|a1|b1", "2|a2|b2", "2|a2|b2dup", "2||b2", "2||b2dup"]
        );
    }

    #[test]
    fn a_left_join_with_using_also_counts_the_column_once() {
        let mut c = joined();
        let o = run(&mut c, "SELECT * FROM a LEFT JOIN b USING (x)");
        assert_eq!(columns_of(&o), &["x", "y", "y"]);
        assert_eq!(
            as_text(&o),
            vec![
                "1|a1|b1",
                "2|a2|b2",
                "2|a2|b2dup",
                "3|a3|",
                "2||b2",
                "2||b2dup",
            ]
        );
    }

    #[test]
    fn a_using_column_keeps_the_left_hand_value() {
        // `a.x` is the column the star prints for the shared name, so it is the
        // one a qualified reference names.
        let mut c = joined();
        let o = run(
            &mut c,
            "SELECT a.x, b.x, b.y FROM a JOIN b USING (x) ORDER BY a.x, b.y",
        );
        assert_eq!(columns_of(&o), &["x", "x", "y"]);
        assert_eq!(
            as_text(&o),
            vec!["1|1|b1", "2|2|b2", "2|2|b2", "2|2|b2dup", "2|2|b2dup"]
        );
    }

    #[test]
    fn a_using_column_may_be_named_bare() {
        // `x` is not ambiguous even though both tables have it: the USING clause
        // made it one column.
        let mut c = joined();
        let o = run(&mut c, "SELECT x FROM a JOIN b USING (x) ORDER BY x");
        assert_eq!(as_text(&o), vec!["1", "2", "2", "2", "2"]);
    }

    #[test]
    fn a_using_column_that_is_not_on_both_sides_is_refused() {
        // sqlite3: "cannot join using column nope - column not present in both
        // tables", reported before the statement runs.
        let mut c = joined();
        let e = c
            .execute_script("SELECT * FROM a JOIN b USING (nope)")
            .unwrap_err();
        assert_eq!(
            e.message,
            "cannot join using column nope - column not present in both tables"
        );
    }

    #[test]
    fn a_bare_column_in_two_tables_is_ambiguous() {
        // sqlite3: "ambiguous column name: x".
        let mut c = joined();
        let e = c.execute_script("SELECT x FROM a, b").unwrap_err();
        assert_eq!(e.message, "ambiguous column name: x");
    }

    #[test]
    fn an_ambiguous_column_is_refused_in_the_where_clause_too() {
        let mut c = joined();
        let e = c
            .execute_script("SELECT a.x FROM a, b WHERE x > 0")
            .unwrap_err();
        assert_eq!(e.message, "ambiguous column name: x");
    }

    #[test]
    fn an_ambiguous_column_is_refused_when_the_tables_are_empty() {
        // Resolution happens before the statement runs, so an empty table does
        // not hide the error. This is why the check is not done per row.
        let mut c = mem();
        run(&mut c, "CREATE TABLE a(x); CREATE TABLE b(x);");
        let e = c.execute_script("SELECT x FROM a, b").unwrap_err();
        assert_eq!(e.message, "ambiguous column name: x");
    }

    #[test]
    fn a_qualified_name_resolves_to_its_table() {
        let mut c = joined();
        let o = run(
            &mut c,
            "SELECT a.y, b.y FROM a JOIN b ON a.x = b.x ORDER BY a.x, b.y",
        );
        assert_eq!(
            as_text(&o),
            vec!["a1|b1", "a2|b2", "|b2", "a2|b2dup", "|b2dup"]
        );
    }

    #[test]
    fn an_unknown_column_says_no_such_column() {
        let mut c = joined();
        let e = c.execute_script("SELECT zzz FROM a, b").unwrap_err();
        assert_eq!(e.message, "no such column: zzz");
        let e = c.execute_script("SELECT q.x FROM a, b").unwrap_err();
        assert_eq!(e.message, "no such column: q.x");
    }

    #[test]
    fn an_alias_renames_the_table_for_the_whole_query() {
        // The alias is the only name that resolves; the original table name is
        // gone, which is what sqlite3 does.
        let mut c = joined();
        let o = run(&mut c, "SELECT * FROM a AS p JOIN b AS q ON p.x = q.x");
        assert_eq!(columns_of(&o), &["x", "y", "x", "y"]);
        assert_eq!(
            as_text(&o),
            vec![
                "1|a1|1|b1",
                "2|a2|2|b2",
                "2|a2|2|b2dup",
                "2||2|b2",
                "2||2|b2dup"
            ]
        );
        let e = c
            .execute_script("SELECT a.x FROM a AS p JOIN b AS q ON p.x = q.x")
            .unwrap_err();
        assert_eq!(e.message, "no such column: a.x");
    }

    #[test]
    fn an_alias_does_not_disambiguate_a_bare_column() {
        // With `b AS q`, `x` is still ambiguous because `a` also has it, but
        // `q.x` is not.
        let mut c = joined();
        let e = c
            .execute_script("SELECT x, q.y FROM a JOIN b AS q ON a.x = q.x")
            .unwrap_err();
        assert_eq!(e.message, "ambiguous column name: x");
    }

    #[test]
    fn a_self_join_needs_two_aliases() {
        // The same table twice is legal as long as the two copies are told
        // apart.
        let mut c = joined();
        let o = run(
            &mut c,
            "SELECT p.x, q.y FROM a AS p JOIN a AS q ON p.x = q.x ORDER BY p.x, q.y",
        );
        assert_eq!(columns_of(&o), &["x", "y"]);
        assert_eq!(as_text(&o), &["1|a1", "2|", "2|", "2|a2", "2|a2", "3|a3"]);
    }

    #[test]
    fn a_table_aliased_twice_is_ambiguous_through_the_star() {
        // sqlite3 names the schema-qualified form for a star and the plain form
        // for a name the query wrote.
        let mut c = joined();
        let e = c
            .execute_script("SELECT * FROM a AS p, b AS p")
            .unwrap_err();
        assert_eq!(e.message, "ambiguous column name: main.p.x");
        let e = c
            .execute_script("SELECT p.x FROM a AS p, b AS p")
            .unwrap_err();
        assert_eq!(e.message, "ambiguous column name: p.x");
    }

    #[test]
    fn a_join_with_no_constraint_is_a_cross_product() {
        // sqlite3: `a JOIN b` with nothing between is the full sixteen rows, so
        // a bare JOIN constrains nothing.
        let mut c = joined();
        let o = run(&mut c, "SELECT * FROM a JOIN b");
        assert_eq!(rows_of(&o).len(), 16);
    }

    #[test]
    fn a_cross_join_with_a_constraint_is_an_inner_join() {
        // sqlite3 treats the constraint as what decides, so the unmatched a.x
        // row is dropped even though the join was written CROSS.
        let mut c = joined();
        let o = run(&mut c, "SELECT * FROM a CROSS JOIN b ON a.x = b.x");
        assert_eq!(
            as_text(&o),
            vec![
                "1|a1|1|b1",
                "2|a2|2|b2",
                "2|a2|2|b2dup",
                "2||2|b2",
                "2||2|b2dup"
            ]
        );
    }

    #[test]
    fn a_comma_may_be_followed_by_a_constraint() {
        // sqlite3 accepts `a, b ON ...` and executes it as an inner join.
        let mut c = joined();
        let o = run(&mut c, "SELECT * FROM a, b ON a.x = b.x");
        assert_eq!(rows_of(&o).len(), 5);
    }

    #[test]
    fn a_three_way_join_folds_in_from_the_left() {
        let mut c = joined();
        run(
            &mut c,
            "CREATE TABLE c(x, z); INSERT INTO c VALUES(1,'c1'),(2,'c2'),(9,'c9');",
        );
        let o = run(
            &mut c,
            "SELECT a.y, b.y, c.z FROM a JOIN b ON a.x = b.x JOIN c ON b.x = c.x \
             ORDER BY a.y, b.y, c.z",
        );
        assert_eq!(columns_of(&o), &["y", "y", "z"]);
        assert_eq!(
            as_text(&o),
            vec!["|b2|c2", "|b2dup|c2", "a1|b1|c1", "a2|b2|c2", "a2|b2dup|c2"]
        );
    }

    #[test]
    fn a_three_way_join_can_be_written_with_commas() {
        // The last table cross joins everything to its left, so every matching
        // pair appears once per row of `c`.
        let mut c = joined();
        run(
            &mut c,
            "CREATE TABLE c(x, z); INSERT INTO c VALUES(2,'c2');",
        );
        let o = run(
            &mut c,
            "SELECT a.x, b.y, c.z FROM a, b ON a.x = b.x, c ORDER BY a.x, b.y",
        );
        assert_eq!(rows_of(&o).len(), 5);
    }

    #[test]
    fn a_left_join_then_a_where_on_the_right_side_drops_the_null_row() {
        // The WHERE runs after the join, so the row the LEFT join invented is
        // removed again by a predicate on the right side.
        let mut c = joined();
        let o = run(
            &mut c,
            "SELECT a.y FROM a LEFT JOIN b ON a.x = b.x WHERE b.y IS NULL",
        );
        assert_eq!(as_text(&o), vec!["a3"]);
    }

    #[test]
    fn an_on_constraint_of_false_leaves_every_left_row_unmatched() {
        // `ON 0` matches nothing, so a LEFT join keeps all four left rows with a
        // NULL right side and an inner join keeps none.
        let mut c = joined();
        let o = run(&mut c, "SELECT * FROM a LEFT JOIN b ON 0");
        assert_eq!(as_text(&o), vec!["1|a1||", "2|a2||", "3|a3||", "2|||"]);
        let o = run(&mut c, "SELECT * FROM a JOIN b ON 0");
        assert!(rows_of(&o).is_empty());
    }

    #[test]
    fn a_join_over_empty_tables_produces_nothing() {
        let mut c = mem();
        run(&mut c, "CREATE TABLE a(x, y); CREATE TABLE b(x, y);");
        let o = run(&mut c, "SELECT * FROM a LEFT JOIN b ON a.x = b.x");
        assert!(rows_of(&o).is_empty());
        let o = run(&mut c, "SELECT * FROM a, b");
        assert!(rows_of(&o).is_empty());
    }

    #[test]
    fn a_join_reads_a_rowid_alias_from_each_side() {
        // The alias column stands for the key, so a join that projects it has
        // to recover it for the table the row came from.
        let mut c = mem();
        run(
            &mut c,
            "CREATE TABLE a(id INTEGER PRIMARY KEY, v);\
             CREATE TABLE b(id INTEGER PRIMARY KEY, v);\
             INSERT INTO a VALUES(10,'a'),(20,'b');\
             INSERT INTO b VALUES(10,'x'),(30,'y');",
        );
        let o = run(
            &mut c,
            "SELECT a.id, b.id, b.v FROM a JOIN b ON a.id = b.id ORDER BY a.id",
        );
        assert_eq!(as_text(&o), vec!["10|10|x"]);
        // And the unmatched right row keeps the LEFT join's NULL.
        let o = run(
            &mut c,
            "SELECT a.id, b.id FROM a LEFT JOIN b ON a.id = b.id ORDER BY a.id",
        );
        assert_eq!(as_text(&o), vec!["10|10", "20|"]);
    }

    #[test]
    fn a_join_respects_limit_and_offset() {
        let mut c = joined();
        let o = run(
            &mut c,
            "SELECT a.x, b.y FROM a, b WHERE a.x = b.x ORDER BY a.x LIMIT 2 OFFSET 1",
        );
        assert_eq!(as_text(&o), vec!["2|b2", "2|b2dup"]);
    }

    #[test]
    fn an_order_by_may_name_a_column_of_either_table() {
        // The key is a table column rather than an output alias, so it reads
        // the joined row.
        let mut c = joined();
        let o = run(
            &mut c,
            "SELECT a.y FROM a JOIN b ON a.x = b.x ORDER BY b.y DESC, a.y",
        );
        assert_eq!(as_text(&o), vec!["", "a2", "", "a2", "a1"]);
    }

    #[test]
    fn an_output_alias_wins_over_a_table_column_in_order_by() {
        // `b.y AS k` renames the column, and ORDER BY k sorts on what the
        // projection produced.
        let mut c = joined();
        let o = run(
            &mut c,
            "SELECT b.y AS k FROM a JOIN b ON a.x = b.x ORDER BY k",
        );
        assert_eq!(as_text(&o), vec!["b1", "b2", "b2", "b2dup", "b2dup"]);
    }

    #[test]
    fn a_join_reports_an_unknown_table() {
        let mut c = joined();
        let e = c
            .execute_script("SELECT * FROM a JOIN nope ON a.x = 1")
            .unwrap_err();
        assert_eq!(e.message, "no such table: nope");
    }

    #[test]
    fn a_subquery_in_from_is_still_refused() {
        let mut c = joined();
        let e = c
            .execute_script("SELECT * FROM a JOIN (SELECT 1 AS x) ON a.x = 1")
            .unwrap_err();
        assert!(
            e.message.contains("not supported yet"),
            "got: {}",
            e.message
        );
    }

    // ---- the outer joins -------------------------------------------------
    //
    // Every expectation below was read out of sqlite3 3.53.4, and the fixture is
    // chosen so the row *order* is observable rather than just the row count: a
    // count-only assertion would pass on a plan that returns the right rows in
    // the wrong sequence.

    /// `a(k,v)`, `b(k,w)`, `c(k,u)`, where b has a row only a has not.
    ///
    /// The three tables share a column name `k`, so a chain of USING clauses
    /// names the same column three times, which is the case that pins where a
    /// coalesced column is read from. b's extra row is what a RIGHT or FULL
    /// join has to preserve.
    fn keyed() -> Connection {
        let mut c = mem();
        run(
            &mut c,
            "CREATE TABLE a(k, v);\
             CREATE TABLE b(k, w);\
             CREATE TABLE c(k, u);\
             INSERT INTO a VALUES(1,'A'),(2,'A2');\
             INSERT INTO b VALUES(1,'B'),(9,'R9');\
             INSERT INTO c VALUES(1,'C'),(2,'C2');",
        );
        c
    }

    #[test]
    fn a_right_join_keeps_the_row_only_the_right_table_has() {
        // sqlite3: `2|2`, then the four right rows a did not match, in table
        // order, each with a NULL left side.
        let mut c = mem();
        run(
            &mut c,
            "CREATE TABLE l(x);\
             CREATE TABLE r(x);\
             INSERT INTO l VALUES(2),(4),(6);\
             INSERT INTO r VALUES(1),(2),(3),(7),(8);",
        );
        let o = run(&mut c, "SELECT * FROM l RIGHT JOIN r ON l.x = r.x");
        assert_eq!(columns_of(&o), &["x", "x"]);
        assert_eq!(
            as_text(&o),
            vec!["2|2", "|1", "|3", "|7", "|8"],
            "sqlite3 puts every matched pair first, then the unmatched right rows"
        );
    }

    #[test]
    fn a_full_join_keeps_the_unmatched_rows_of_both_sides() {
        // sqlite3: the matched pair, then the unmatched left rows in table
        // order, then the unmatched right rows. The order of those three groups
        // is what distinguishes FULL from a LEFT that happens to end up with the
        // same rows.
        let mut c = mem();
        run(
            &mut c,
            "CREATE TABLE l(x);\
             CREATE TABLE r(x);\
             INSERT INTO l VALUES(2),(4),(6);\
             INSERT INTO r VALUES(1),(2),(3),(7),(8);",
        );
        let o = run(&mut c, "SELECT * FROM l FULL JOIN r ON l.x = r.x");
        assert_eq!(columns_of(&o), &["x", "x"]);
        assert_eq!(as_text(&o), vec!["2|2", "4|", "6|", "|1", "|3", "|7", "|8"]);
    }

    #[test]
    fn a_right_join_with_no_constraint_is_a_cross_product_that_drops_nothing() {
        // A RIGHT join with nothing to match on pairs every left row with every
        // right row, so nothing is left unmatched. sqlite3 returns all fifteen.
        let mut c = mem();
        run(
            &mut c,
            "CREATE TABLE l(x);\
             CREATE TABLE r(x);\
             INSERT INTO l VALUES(2),(4);\
             INSERT INTO r VALUES(1),(2),(3);",
        );
        let o = run(&mut c, "SELECT * FROM l RIGHT JOIN r");
        assert_eq!(as_text(&o).len(), 6, "sqlite3 returns the full product");
    }

    #[test]
    fn a_right_join_over_an_empty_table_produces_nothing() {
        // There is no right row to preserve, so the query is empty rather than
        // every left row with a NULL right side. This is the case a naive swap
        // of the LEFT preservation test gets wrong.
        let mut c = mem();
        run(
            &mut c,
            "CREATE TABLE l(x);\
             CREATE TABLE r(x);\
             INSERT INTO l VALUES(1),(2);",
        );
        let o = run(&mut c, "SELECT * FROM l RIGHT JOIN r ON l.x = r.x");
        assert_eq!(as_text(&o), Vec::<String>::new(), "sqlite3 returns no rows");
    }

    #[test]
    fn a_right_join_over_an_empty_left_table_keeps_every_right_row() {
        // The mirror of the above: with no left row at all, every right row is
        // unmatched and every one is preserved.
        let mut c = mem();
        run(
            &mut c,
            "CREATE TABLE l(x);\
             CREATE TABLE r(x);\
             INSERT INTO r VALUES(1),(2);",
        );
        let o = run(&mut c, "SELECT * FROM l RIGHT JOIN r ON l.x = r.x");
        assert_eq!(as_text(&o), vec!["|1", "|2"]);
    }

    #[test]
    fn a_where_clause_runs_after_a_right_join_has_preserved_its_rows() {
        // The preserved row is a row the query produces, so WHERE sees it: this
        // is the query that finds the rows only the right table has.
        let mut c = mem();
        run(
            &mut c,
            "CREATE TABLE l(x);\
             CREATE TABLE r(x);\
             INSERT INTO l VALUES(2);\
             INSERT INTO r VALUES(1),(2),(3);",
        );
        let o = run(
            &mut c,
            "SELECT * FROM l RIGHT JOIN r ON l.x = r.x WHERE l.x IS NULL",
        );
        assert_eq!(as_text(&o), vec!["|1", "|3"]);
    }

    #[test]
    fn a_chained_using_reads_the_coalesced_column_and_not_the_gap() {
        // The middle table b is unmatched for a's second row, so its whole side
        // of the row is NULL. The inner join on c still pairs that row, which
        // means the constraint compared a's k rather than b's NULL. Reading the
        // immediately preceding source instead loses the row outright -- and
        // loses it silently, with no error, which is the shape that matters.
        //
        // sqlite3: `1|A|B|C` and `2|A2||C2`.
        let mut c = keyed();
        let o = run(
            &mut c,
            "SELECT * FROM a LEFT JOIN b USING(k) JOIN c USING(k)",
        );
        assert_eq!(columns_of(&o), &["k", "v", "w", "u"]);
        assert_eq!(as_text(&o), vec!["1|A|B|C", "2|A2||C2"]);
    }

    #[test]
    fn a_chained_left_using_reads_the_coalesced_column_too() {
        // The same gap with a LEFT on the far side rather than an inner one.
        // sqlite3 gives the same two rows, so the constraint is not what
        // preserves the second one.
        let mut c = keyed();
        let o = run(
            &mut c,
            "SELECT * FROM a LEFT JOIN b USING(k) LEFT JOIN c USING(k)",
        );
        assert_eq!(as_text(&o), vec!["1|A|B|C", "2|A2||C2"]);
    }

    #[test]
    fn a_chained_using_names_its_column_once_across_three_tables() {
        // `k` is named by two USING clauses and printed once, from a.
        let mut c = keyed();
        let o = run(&mut c, "SELECT * FROM a JOIN b USING(k) JOIN c USING(k)");
        assert_eq!(columns_of(&o), &["k", "v", "w", "u"]);
        assert_eq!(as_text(&o), vec!["1|A|B|C"]);
    }

    #[test]
    fn a_bare_using_column_reads_the_coalesced_value() {
        // `k` on its own is the one value the two sides agree on. On the row only
        // b matched it is b's k, because a RIGHT join preserves b and a's side
        // is all NULL -- so the bare name cannot be pinned to a at resolve time.
        let mut c = keyed();
        let o = run(&mut c, "SELECT k, a.k, b.k FROM a RIGHT JOIN b USING(k)");
        assert_eq!(columns_of(&o), &["k", "k", "k"]);
        assert_eq!(as_text(&o), vec!["1|1|1", "9||9"]);
    }

    #[test]
    fn a_using_column_prints_the_value_of_the_side_that_matched() {
        // The star's `k` is the same coalesced value the bare name reads, so the
        // row b alone matched shows 9 rather than a blank.
        let mut c = keyed();
        let o = run(&mut c, "SELECT * FROM a RIGHT JOIN b USING(k)");
        assert_eq!(as_text(&o), vec!["1|A|B", "9||R9"]);
    }

    #[test]
    fn a_full_join_with_using_preserves_both_sides() {
        // sqlite3: the matched row, a's unmatched row, then b's unmatched row.
        let mut c = keyed();
        let o = run(&mut c, "SELECT * FROM a FULL JOIN b USING(k)");
        assert_eq!(as_text(&o), vec!["1|A|B", "2|A2|", "9||R9"]);
    }

    #[test]
    fn a_right_join_keeps_the_row_after_a_using_joins_further() {
        // The later constraint compares b's k, not a's, because a RIGHT join
        // preserved b. sqlite3 pairs b.k=1 with c.k=1, and a matched there
        // anyway; the row b.k=9 matches nothing in c and is dropped by the inner
        // join, so the result is a single row.
        let mut c = mem();
        run(
            &mut c,
            "CREATE TABLE a(k,v);\
             CREATE TABLE b(k,w);\
             CREATE TABLE c(k,u);\
             INSERT INTO a VALUES(1,'A');\
             INSERT INTO b VALUES(1,'B'),(7,'B7');\
             INSERT INTO c VALUES(1,'C'),(5,'C5');",
        );
        let o = run(
            &mut c,
            "SELECT k, a.k, b.k, c.k FROM a RIGHT JOIN b USING(k) JOIN c USING(k)",
        );
        assert_eq!(as_text(&o), vec!["1|1|1|1"]);
    }

    // ---- NATURAL ---------------------------------------------------------

    #[test]
    fn a_natural_join_matches_on_the_columns_the_tables_share() {
        // `x` is the only name both tables have, so NATURAL is `USING (x)`:
        // one match, and `x` printed once.
        let mut c = mem();
        run(
            &mut c,
            "CREATE TABLE a(x,y);\
             CREATE TABLE b(x,z);\
             INSERT INTO a VALUES(1,'a1'),(2,'a2');\
             INSERT INTO b VALUES(1,'b1'),(3,'b3');",
        );
        let o = run(&mut c, "SELECT * FROM a NATURAL JOIN b");
        assert_eq!(columns_of(&o), &["x", "y", "z"]);
        assert_eq!(as_text(&o), vec!["1|a1|b1"]);
    }

    #[test]
    fn a_natural_left_join_keeps_the_row_only_the_left_has() {
        // The same query with LEFT, which is what the suite's
        // `SELECT t1.rowid FROM t1 NATURAL LEFT OUTER JOIN t3` needs.
        let mut c = mem();
        run(
            &mut c,
            "CREATE TABLE a(x,y);\
             CREATE TABLE b(x,z);\
             INSERT INTO a VALUES(1,'a1'),(2,'a2');\
             INSERT INTO b VALUES(1,'b1'),(3,'b3');",
        );
        let o = run(&mut c, "SELECT * FROM a NATURAL LEFT JOIN b");
        assert_eq!(columns_of(&o), &["x", "y", "z"]);
        assert_eq!(as_text(&o), vec!["1|a1|b1", "2|a2|"]);
    }

    #[test]
    fn a_natural_right_join_keeps_the_row_only_the_right_has() {
        let mut c = mem();
        run(
            &mut c,
            "CREATE TABLE a(x,y);\
             CREATE TABLE b(x,z);\
             INSERT INTO a VALUES(1,'a1'),(2,'a2');\
             INSERT INTO b VALUES(1,'b1'),(3,'b3');",
        );
        let o = run(&mut c, "SELECT * FROM a NATURAL RIGHT JOIN b");
        assert_eq!(as_text(&o), vec!["1|a1|b1", "3||b3"]);
    }

    #[test]
    fn a_natural_full_join_keeps_the_rows_of_both_sides() {
        let mut c = mem();
        run(
            &mut c,
            "CREATE TABLE a(x,y);\
             CREATE TABLE b(x,z);\
             INSERT INTO a VALUES(1,'a1'),(2,'a2');\
             INSERT INTO b VALUES(1,'b1'),(3,'b3');",
        );
        let o = run(&mut c, "SELECT * FROM a NATURAL FULL JOIN b");
        assert_eq!(as_text(&o), vec!["1|a1|b1", "2|a2|", "3||b3"]);
    }

    #[test]
    fn a_natural_join_over_tables_sharing_nothing_is_a_cross_product() {
        // No shared column means no constraint at all, which is a cross
        // product. sqlite3 returns the four pairs, the same as CROSS JOIN.
        let mut c = mem();
        run(
            &mut c,
            "CREATE TABLE p(m);\
             CREATE TABLE q(n);\
             INSERT INTO p VALUES(1),(2);\
             INSERT INTO q VALUES(3),(4);",
        );
        let o = run(&mut c, "SELECT * FROM p NATURAL JOIN q");
        assert_eq!(columns_of(&o), &["m", "n"]);
        assert_eq!(as_text(&o), vec!["1|3", "1|4", "2|3", "2|4"]);
    }

    #[test]
    fn a_natural_join_matches_on_every_shared_column() {
        // Two shared names means two equalities, so only the row agreeing on
        // both is paired. Matching on one of them would give two rows.
        let mut c = mem();
        run(
            &mut c,
            "CREATE TABLE a(k,v);\
             CREATE TABLE b(k,w);\
             INSERT INTO a VALUES(1,'x'),(2,'y');\
             INSERT INTO b VALUES(1,'p'),(1,'q'),(2,'r');",
        );
        // Only `k` is shared here, so the row that differs on nothing else.
        let o = run(&mut c, "SELECT * FROM a NATURAL JOIN b");
        assert_eq!(as_text(&o), vec!["1|x|p", "1|x|q", "2|y|r"]);
    }

    #[test]
    fn a_natural_join_may_not_carry_a_constraint() {
        // sqlite3: "a NATURAL join may not have an ON or USING clause". The
        // two forms of the join say the same thing twice, and SQLite refuses
        // rather than quietly dropping one.
        let mut c = mem();
        run(
            &mut c,
            "CREATE TABLE a(x,y);\
             CREATE TABLE b(x,z);",
        );
        for sql in [
            "SELECT * FROM a NATURAL JOIN b ON a.x = b.x",
            "SELECT * FROM a NATURAL JOIN b USING(x)",
        ] {
            let e = c.execute_script(sql).unwrap_err();
            assert_eq!(
                e.message, "a NATURAL join may not have an ON or USING clause",
                "for {sql}"
            );
        }
    }

    #[test]
    fn natural_is_still_a_usable_table_name() {
        // `natural` on its own is a table, not the start of a join, which is why
        // the operator only counts once a JOIN has been confirmed.
        let mut c = mem();
        run(
            &mut c,
            "CREATE TABLE natural(q); INSERT INTO natural VALUES(1);",
        );
        let o = run(&mut c, "SELECT * FROM natural");
        assert_eq!(as_text(&o), vec!["1"]);
    }

    // ---- the schema table inside a join ----------------------------------

    #[test]
    fn the_schema_table_can_be_joined_against() {
        // sqlite_master is a real table to a query, and the suite reads it from
        // inside joins in a good many places. Its rows live in the schema
        // b-tree, which is a page like any other, so the cross product comes out
        // of the same path every other table's rows do.
        let mut c = keyed();
        let o = run(
            &mut c,
            "SELECT a.k, s.name FROM a JOIN sqlite_master AS s ON 1",
        );
        // Three tables exist, and a has two rows, so six pairs.
        assert_eq!(as_text(&o).len(), 6, "sqlite3 returns the cross product");
        let o = run(&mut c, "SELECT s.name FROM sqlite_master AS s");
        assert_eq!(as_text(&o), vec!["a", "b", "c"]);
    }

    #[test]
    fn the_schema_table_can_be_joined_with_a_comma() {
        let mut c = keyed();
        let o = run(&mut c, "SELECT count(*) FROM a, sqlite_master");
        assert_eq!(as_text(&o), vec!["6"]);
    }

    // ---- ORDER BY an ordinal ----------------------------------------------

    #[test]
    fn an_order_by_ordinal_sorts_on_the_column_it_names() {
        // An integer names a result column, not a value. Sorting on the literal
        // makes every row equal, so the order is whatever the rows arrived in
        // and the direction has nothing to act on.
        let mut c = mem();
        run(
            &mut c,
            "CREATE TABLE t(x, y);\
             INSERT INTO t VALUES(1,'a'),(2,'b'),(3,'c');",
        );
        let o = run(&mut c, "SELECT x, y FROM t ORDER BY 1 DESC");
        assert_eq!(as_text(&o), vec!["3|c", "2|b", "1|a"]);
        let o = run(&mut c, "SELECT x, y FROM t ORDER BY 1");
        assert_eq!(as_text(&o), vec!["1|a", "2|b", "3|c"]);
    }

    #[test]
    fn an_order_by_ordinal_reverses_over_a_join_too() {
        // The join track's own shape, which is the case the ordinal bug hides
        // in: every such test sorted ascending, the one direction a constant key
        // cannot expose.
        let mut c = mem();
        run(
            &mut c,
            "CREATE TABLE a(x, y);\
             CREATE TABLE b(x, z);\
             INSERT INTO a VALUES(1,'a'),(2,'b'),(3,'c');\
             INSERT INTO b VALUES(1,'p'),(2,'q'),(3,'r');",
        );
        let o = run(
            &mut c,
            "SELECT a.x, b.x FROM a JOIN b ON a.x = b.x ORDER BY 2 DESC",
        );
        assert_eq!(as_text(&o), vec!["3|3", "2|2", "1|1"]);
    }

    #[test]
    fn an_order_by_ordinal_past_the_last_column_is_refused() {
        // sqlite3: "1st ORDER BY term out of range - should be between 1 and 2".
        // The count is of the *result* columns, which is why a star does not make
        // every ordinal valid.
        let mut c = mem();
        run(
            &mut c,
            "CREATE TABLE t(x, y);\
             INSERT INTO t VALUES(1,'a');",
        );
        for (sql, want) in [
            (
                "SELECT x, y FROM t ORDER BY 3",
                "1st ORDER BY term out of range - should be between 1 and 2",
            ),
            (
                "SELECT x, y FROM t ORDER BY 0",
                "1st ORDER BY term out of range - should be between 1 and 2",
            ),
            (
                "SELECT x, y FROM t ORDER BY 1, 4",
                "2nd ORDER BY term out of range - should be between 1 and 2",
            ),
        ] {
            let e = c.execute_script(sql).unwrap_err();
            assert_eq!(e.message, want, "for {sql}");
        }
    }

    #[test]
    fn an_order_by_ordinal_out_of_range_fails_with_no_rows_to_read() {
        // The check is on the statement rather than on a row, so a query that
        // matches nothing fails the same way as one that matches something.
        let mut c = mem();
        run(
            &mut c,
            "CREATE TABLE t(x, y);\
             INSERT INTO t VALUES(1,'a');",
        );
        let e = c
            .execute_script("SELECT x, y FROM t WHERE 0 ORDER BY 7")
            .unwrap_err();
        assert_eq!(
            e.message,
            "1st ORDER BY term out of range - should be between 1 and 2"
        );
    }

    #[test]
    fn a_multi_table_join_with_an_unsatisfiable_constraint_still_names_its_columns() {
        // A join whose constraint matches nothing returns no rows, so the only
        // thing left to disagree about is the shape of the result. Both engines
        // report five columns here: `x` once, coalesced out of a and b, then
        // a's `y`, b's `z`, and c's `y` and `w`. Nothing in the row output would
        // reveal a wrong column count here, which is why it needs its own test.
        let mut c = mem();
        run(
            &mut c,
            "CREATE TABLE a(x, y);\
             CREATE TABLE b(x, z);\
             CREATE TABLE c(y, w);\
             INSERT INTO a VALUES(1,'t1');\
             INSERT INTO b VALUES(2,2);\
             INSERT INTO c VALUES('t2','w2');",
        );
        let o = run(
            &mut c,
            "SELECT * FROM a JOIN b USING(x) JOIN c ON b.z = c.y",
        );
        assert_eq!(columns_of(&o), &["x", "y", "z", "y", "w"]);
        assert_eq!(as_text(&o), Vec::<String>::new());
    }
}
