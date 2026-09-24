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
use crate::catalog::{Catalog, Table};
use crate::error::{Error, Result, ResultCode};
use crate::eval::{eval, truthy, EvalCtx};
use crate::pager::Pager;
use crate::parser::{
    BinOp, ColumnDef, CompoundOp, Constraint, Expr, FromItem, InsertSource, Literal, Select,
    SelectBody, Stmt, TableRef,
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
                ..
            } => self.create_table(
                name,
                *if_not_exists,
                columns,
                constraints,
                *without_rowid,
                sql,
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
        self.write_schema_row(name, root, sql_text)?;
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
            .ok_or_else(|| Error::new(ResultCode::Error, format!("no such table: {name}")))
    }

    fn table_mut(&mut self, name: &str) -> Result<&mut Table> {
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
        tree.insert(&mut self.pager, &crate::table_tree::Row { rowid, values })?;
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
        if from.len() > 1 {
            return Err(Error::new(ResultCode::Error, "a join is not supported yet"));
        }

        // A SELECT with no FROM evaluates against an empty row, so a constant
        // expression works and a column reference does not.
        let Some(item) = from.first() else {
            return self.select_constants(sel, columns);
        };
        let FromItem::Table(tref) = item else {
            return Err(Error::new(
                ResultCode::Error,
                "a subquery in FROM is not supported yet",
            ));
        };
        let table = self.table(&tref.name)?.clone();

        // The row source is the table itself. A record stores only the columns
        // up to its last non-NULL, so a read has to pad back to the declared
        // width; without that a trailing NULL column reads as absent and the
        // zip that binds names to values drops it.
        let source_rows = {
            let mut tree = TableTree::open(&mut self.pager, table.root_page)?;
            let mut rows = tree.scan(&mut self.pager)?;
            for r in &mut rows {
                r.values.resize(table.len(), Value::Null);
            }
            rows
        };

        // The output column names: the alias, or the column's own name, or the
        // expression's text.
        let mut names = Vec::with_capacity(columns.len());
        for rc in columns {
            names.push(match &rc.alias {
                Some(a) => a.clone(),
                None => match &rc.expr {
                    Expr::Column { name, .. } => name.clone(),
                    _ => render_expr(&rc.expr),
                },
            });
        }
        // A star expands to every column of the table.
        if columns.len() == 1 && is_star(&columns[0].expr) {
            names = table.columns.iter().map(|c| c.name.clone()).collect();
        }

        let mut out = Vec::new();
        for row in &source_rows {
            let mut bound: Vec<(String, Value)> = table
                .columns
                .iter()
                .zip(row.values.iter())
                .map(|(c, v)| (c.name.clone(), v.clone()))
                .collect();
            // A rowid alias is stored as NULL and recovered from the key, so
            // the bound row carries the rowid before any expression sees it.
            // Doing it once here covers the predicate and the projection.
            if let Some(i) = table.rowid_alias {
                if let Some((_, slot)) = bound.get_mut(i) {
                    *slot = Value::Integer(row.rowid);
                }
            }
            let params: Vec<Value> = Vec::new();
            let ctx = EvalCtx {
                params: &params,
                row: bound.clone(),
                columns: &bound,
                context: Some(tref.name.clone()),
            };

            if let Some(pred) = where_ {
                if !truthy(eval(pred, &ctx)?) {
                    continue;
                }
            }
            let mut values: Vec<Value> = Vec::with_capacity(names.len());
            let star = columns.len() == 1 && is_star(&columns[0].expr);
            for rc in columns {
                if star {
                    values.extend(row.values.iter().cloned());
                } else {
                    values.push(eval(&rc.expr, &ctx)?);
                }
            }
            // A rowid alias is stored as NULL and has to be recovered from the
            // key, which is what makes the alias an alias rather than a
            // column that happens to hold the same number.
            let mut values = values;
            if let (Some(i), true) = (
                table.rowid_alias,
                tref.name.eq_ignore_ascii_case(&table.name),
            ) {
                if i < values.len() {
                    values[i] = Value::Integer(row.rowid);
                }
            }
            // A rowid alias is stored as NULL and recovered from the key, which
            // is what makes it an alias rather than a column that happens to
            // hold the same number.
            if let Some(i) = table.rowid_alias {
                if i < values.len() {
                    values[i] = Value::Integer(row.rowid);
                }
            }
            out.push(Row { values });
        }

        apply_order_by(sel, &table, &mut out)?;
        apply_limit(sel, &mut out)?;
        Ok(Outcome::Query {
            columns: names,
            rows: out,
        })
    }

    /// A SELECT with no FROM, which yields exactly one row.
    fn select_constants(
        &mut self,
        sel: &Select,
        columns: &[crate::parser::ResultColumn],
    ) -> Result<Outcome> {
        let params: Vec<Value> = Vec::new();
        let ctx = EvalCtx::empty(&params);
        let mut values = Vec::with_capacity(columns.len());
        for rc in columns {
            values.push(eval(&rc.expr, &ctx)?);
        }
        let names = columns
            .iter()
            .map(|rc| match &rc.alias {
                Some(a) => a.clone(),
                None => render_expr(&rc.expr),
            })
            .collect();
        let mut out = vec![Row { values }];
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
        Expr::Function { name, args, .. } => {
            format!(
                "{name}({})",
                args.iter().map(render_expr).collect::<Vec<_>>().join(", ")
            )
        }
        other => format!("{other:?}"),
    }
}

/// Applies an ORDER BY, sorting the result rows in place.
///
/// The sort key is evaluated per row, which is what lets a key name an output
/// column alias as well as a table column.
fn apply_order_by(sel: &Select, table: &Table, rows: &mut Vec<Row>) -> Result<()> {
    if sel.order_by.is_empty() {
        return Ok(());
    }
    let params: Vec<Value> = Vec::new();
    // Each row carries its sort keys alongside it, computed once, so a
    // comparison never re-evaluates an expression.
    let mut keyed: Vec<(Vec<Value>, Row)> = Vec::with_capacity(rows.len());
    for row in rows.iter() {
        let bound: Vec<(String, Value)> = table
            .columns
            .iter()
            .zip(row.values.iter())
            .map(|(c, v)| (c.name.clone(), v.clone()))
            .collect();
        let mut ctx = EvalCtx {
            params: &params,
            row: bound,
            columns: &[],
            context: Some(table.name.clone()),
        };
        // An ORDER BY may name a column of the table or an alias of the result,
        // and a qualified alias is written table.alias.
        let mut keys = Vec::with_capacity(sel.order_by.len());
        for (expr, _) in &sel.order_by {
            let key = match expr {
                Expr::Column {
                    table: Some(t),
                    name,
                    ..
                } if t == &table.name => {
                    // A qualified reference that matches a result column is an
                    // alias of this query.
                    let idx = (0..table.columns.len())
                        .find(|&i| table.columns[i].name.eq_ignore_ascii_case(name));
                    match idx {
                        Some(i) => row.values.get(i).cloned().unwrap_or(Value::Null),
                        None => eval(expr, &ctx)?,
                    }
                }
                _ => match expr {
                    Expr::Column { name, .. } => {
                        let out = (0..row.values.len()).find(|&i| {
                            // The result column names are the table's, for a
                            // star, and the aliases otherwise.
                            table
                                .columns
                                .get(i)
                                .map(|c| c.name.eq_ignore_ascii_case(name))
                                .unwrap_or(false)
                        });
                        match out {
                            Some(i) => row.values[i].clone(),
                            None => eval(expr, &ctx)?,
                        }
                    }
                    _ => eval(expr, &ctx)?,
                },
            };
            keys.push(key);
        }
        ctx.params = &params;
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
    *rows = keyed.into_iter().map(|(_, r)| r).collect();
    Ok(())
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

    #[test]
    fn a_join_is_refused_rather_than_silently_cross_joined() {
        let mut c = mem();
        run(&mut c, "CREATE TABLE a(x)");
        run(&mut c, "CREATE TABLE b(y)");
        let e = c.execute_script("SELECT * FROM a, b").unwrap_err();
        assert!(
            e.message.contains("not supported yet"),
            "got: {}",
            e.message
        );
    }
}
