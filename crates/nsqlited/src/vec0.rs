//! The `vec0` adapter: the one place that knows both crates.
//!
//! # Why this file exists
//!
//! The engine owns [`nsqlite::vtab::VtabModule`] and the vector crate owns
//! [`nsqlite_vector::vtab::VirtualTable`], and neither may depend on the other.
//! `nsqlite-vector`'s `Cargo.toml` says why:
//!
//! > this crate does not use the engine, and must not -- the engine is what
//! > plugs the vec0 module in, so a dependency the other way would be the
//! > integration contract expressed as a cycle
//!
//! So the module is adapted here, in `nsqlited`, which depends on both, and
//! nothing in either crate learns about the other. What this file contains is
//! adaptation and nothing else: no vector arithmetic, no schema grammar and no
//! shadow-table layout, because all three already exist and re-deriving them is
//! how a second implementation starts.
//!
//! # What each method has to reconcile
//!
//! | engine                      | vector crate                    | the join                      |
//! | --------------------------- | ------------------------------- | ----------------------------- |
//! | `name() -> &str`            | `Vec0Module::NAME`              | direct                        |
//! | `create(&self)`             | `create(&mut self)`             | `RefCell`                     |
//! | `connect() -> VtabInstance` | `connect() -> Box<dyn VTab>`    | [`Vec0VTab`]                  |
//! | `shadow_tables()`           | `Schema::shadow_table_sql()`    | direct                        |
//! | `declare()`                 | nothing                         | [`declaration_for`]           |
//!
//! Only `create` has a signature the engine cannot call directly, and only
//! `declare` has no counterpart at all. Both are explained where they are.
//!
//! # `declare`, and what it must leave out
//!
//! A virtual table's real definition is `CREATE VIRTUAL TABLE ... USING
//! vec0(...)`, which names no columns, so something has to say what the table
//! *has* -- that is what `SELECT *` expands and what `PRAGMA table_info`
//! parses. **Measured** against the real `sqlite3` 3.53.4, which has `rtree`:
//!
//! ```text
//! $ sqlite3 geo.db "CREATE VIRTUAL TABLE geo USING rtree(id,minx,maxx,miny,maxy);"
//! $ sqlite3 geo.db "PRAGMA table_info(geo);"
//! 0|id|INT|0||0
//! 1|minx|REAL|0||0
//! ...
//! ```
//!
//! Two facts fall out of that, and they are the two rules this file follows.
//!
//! **One listed column per declared column, and no more.** `rtree`'s `id` is
//! the rowid alias and it is *listed*, at index 0. So a `vec0` table's `rowid`
//! column is listed too rather than being dropped as an alias -- the alias is
//! the column's job, and dropping it would make `SELECT rowid FROM v` a name
//! resolved through the rowid fallback in `join::resolve_ref` rather than
//! through the schema.
//!
//! **The hidden `distance` column is not listed.** That one is measured on
//! `fts5`, which has the same shape of hidden column as `vec0`'s `distance`:
//!
//! ```text
//! $ sqlite3 :memory: "CREATE VIRTUAL TABLE ft USING fts5(body); PRAGMA table_info(ft);"
//! 0|body||0||0          -- `rank` is absent
//! $ sqlite3 :memory: "SELECT rank FROM ft;"   -- ... and is still addressable
//! ```
//!
//! So `distance` is deliberately excluded here. `VTab::column_count()` *does*
//! include it -- it has to, because it is `xColumnCount` and a query may
//! address the column -- and the two are reconciled by [`Vec0VTab::columns`],
//! which reports exactly the list [`declaration_for`] wrote. That is the whole
//! of the mismatch: one list used in two places, rather than two lists kept in
//! step by hand.
//!
//! A column's declared type is carried through verbatim, `float[3]` and all,
//! because the reference keeps it too. **Measured**: `CREATE TABLE t(a
//! float[3])` followed by `PRAGMA table_info` answers `0|a|float[3]|0||0` on
//! the real sqlite3. The engine's own parser reads `float[3]` back as one type
//! name -- `column_def` takes a run of type words and then a parenthesised
//! length -- which is why the text can be handed to it rather than
//! reconstructed around the bracket.

use std::cell::RefCell;

use nsqlite::vtab::{VtabError, VtabInstance, VtabModule, VtabRow};
use nsqlite::Value;

use nsqlite_vector::index_store::ShadowRows;
use nsqlite_vector::vtab::{MemoryVTab, Plan, Schema, Vec0Module, VTab};

/// The module name a `USING <name>` clause matches.
pub const VEC0_MODULE_NAME: &str = Vec0Module::NAME;

/// Wraps [`Vec0Module`] in the engine's [`VtabModule`].
///
/// The `RefCell` is not incidental and is not laziness about the borrow
/// checker. The engine's `create` takes `&self` because a module is shared:
/// the registry hands out an `Rc<dyn VtabModule>` and two queries can hold one
/// at once, so an `&mut self` would make the second impossible. The vector
/// crate's `create` takes `&mut self` because it mutates its own table map.
/// Interior mutability is the only way to have both, and it is the arrangement
/// `nsqlite`'s `VtabModule::create` documentation names for exactly this case.
pub struct Vec0VTabModule {
    inner: RefCell<Vec0Module>,
}

impl Vec0VTabModule {
    /// A module with no tables attached yet.
    pub fn new() -> Self {
        Vec0VTabModule {
            inner: RefCell::new(Vec0Module::new()),
        }
    }
}

impl Vec0VTabModule {
    /// The names of the tables currently attached.
    ///
    /// This is how a test checks that `create` reached the module's own
    /// registry rather than merely validating its arguments -- the distinction
    /// the `create` documentation is about, and one that cannot be seen from
    /// the return value, which is `()` either way.
    #[cfg(test)]
    pub fn table_names(&self) -> Vec<String> {
        self.inner.borrow().table_names()
    }
}

impl Default for Vec0VTabModule {
    fn default() -> Self {
        Self::new()
    }
}

/// Turns the module's own error into the engine's, keeping its text.
///
/// The vector crate's `VTabError` already says the right thing -- `no such
/// vtable: v`, `table v already exists`, a schema complaint quoting the
/// offending text -- so the `Display` form is carried over verbatim rather than
/// re-worded. A module that could not use its arguments is
/// [`VtabError::BadArguments`], which is the variant a caller has to be able
/// to tell apart from an arbitrary failure.
fn map_error(e: nsqlite_vector::vtab::VTabError) -> VtabError {
    use nsqlite_vector::vtab::VTabError as V;
    match e {
        V::Schema(p) => VtabError::BadArguments {
            module: VEC0_MODULE_NAME.to_string(),
            detail: p.to_string(),
        },
        other => VtabError::Other(other.to_string()),
    }
}

/// Parses a `USING vec0(...)` argument list, reporting a bad one as
/// [`VtabError::BadArguments`].
///
/// Three of the four module methods begin here. It is one function rather than
/// three copies because the parse is the same answer every time, and a schema
/// that parses for `shadow_tables` but not for `declare` is a schema bug that
/// would only show up after the engine had already written four tables.
fn parse_schema(name: &str, args: &str) -> Result<Schema, VtabError> {
    Schema::parse(name, args).map_err(|e| VtabError::BadArguments {
        module: VEC0_MODULE_NAME.to_string(),
        detail: e.to_string(),
    })
}

impl VtabModule for Vec0VTabModule {
    fn name(&self) -> &str {
        Vec0Module::NAME
    }

    /// `xCreate`. Delegates so the module's own table registry is populated.
    ///
    /// The delegation is the point, not a convenience. `Vec0Module::create`
    /// inserts an empty `MemoryVTab` into the module's map, and that map is
    /// what a later `connect` finds the table in. An adapter that parsed the
    /// arguments, validated them, and stopped would satisfy the engine's
    /// `create` and then fail every `connect` afterwards with `no such vtable`,
    /// which is the shape of bug that looks like a working feature until the
    /// first read.
    fn create(&self, name: &str, args: &str) -> Result<(), VtabError> {
        self.inner
            .borrow_mut()
            .create(name, args)
            .map_err(map_error)
    }

    /// `xConnect`.
    fn connect(&self, name: &str, args: &str) -> Result<Box<dyn VtabInstance>, VtabError> {
        let vtab = self
            .inner
            .borrow()
            .connect(name, args)
            .map_err(map_error)?;
        Ok(Box::new(Vec0VTab { inner: vtab }))
    }

    /// `xConnect` for a table whose rows are handed in rather than looked up.
    ///
    /// **This is what makes a reopened database work, and it is the whole of
    /// the reopen path.** The module's own `connect` finds the table in its
    /// in-memory map, and a fresh process's map is empty -- so without this,
    /// reopening a database that holds a `vec0` table would answer `no such
    /// vtable: v` for a table whose vectors are sitting in the file.
    ///
    /// The reference implementation gave this up: its HNSW graph is in memory
    /// only and its documentation says the index "must be repopulated by
    /// re-inserting rows after each open". Here the *rows* are the durable
    /// thing -- they are in `<name>_vectors` and `<name>_chunks`, which are
    /// ordinary tables -- and `from_shadow_rows` rebuilds the index from them
    /// per query. So nothing is left dangling between one open and the next.
    ///
    /// It does not consult the module's map at all. Rebuilding from the rows
    /// that were actually read is the stronger answer: a session that
    /// *created* the table and a process that *reopened* it produce identical
    /// rows, rather than the first reading its in-memory copy and the second
    /// reading the file and the two being allowed to drift.
    fn connect_from_shadow(
        &self,
        name: &str,
        args: &str,
        vectors: &[(i64, Vec<u8>)],
        chunks: &[(i64, Option<Vec<u8>>)],
    ) -> Result<Box<dyn VtabInstance>, VtabError> {
        let schema = parse_schema(name, args)?;
        let rows = rows_from_shadow_tables(&schema, vectors, chunks)?;
        Ok(Box::new(ShadowVTab { schema, rows }))
    }

    fn shadow_tables(&self, name: &str, args: &str) -> Result<Vec<(String, String)>, VtabError> {
        // The schema is parsed here rather than delegated to a `Vec0Module`
        // method that does not exist: `shadow_table_sql` is a method on
        // `Schema`, and the schema is a value rather than module state. Parsing
        // it here is also what makes a bad argument list fail before the engine
        // has written anything -- `create_virtual_table` calls `create` and
        // `shadow_tables` before it creates a single table.
        Ok(parse_schema(name, args)?.shadow_table_sql())
    }

    /// The `CREATE TABLE` text a client should be told the virtual table has.
    ///
    /// See the module documentation for the two measurements this follows: one
    /// listed column per declared column, and the hidden `distance` excluded.
    fn declare(&self, name: &str, args: &str) -> Result<String, VtabError> {
        Ok(declaration_for(&parse_schema(name, args)?))
    }
}

/// The `CREATE TABLE` text standing in for a `vec0` table's declaration.
///
/// The name is quoted because a table name is an identifier and a bare one
/// would be read as whatever the next token made it. The column types are the
/// module's own `decl_type` strings: `float[3]` for a vector, and **empty** for
/// a rowid alias, partition or auxiliary column, because `Schema::parse`
/// refuses a type on those three and a type invented here would be a
/// declaration the module itself would reject.
pub fn declaration_for(schema: &Schema) -> String {
    let mut sql = format!("CREATE TABLE \"{}\"(", schema.name);
    for (i, column) in schema.columns.iter().enumerate() {
        if i > 0 {
            sql.push_str(", ");
        }
        sql.push_str(&quote_identifier(&column.name));
        if !column.decl_type.is_empty() {
            sql.push(' ');
            sql.push_str(&column.decl_type);
        }
    }
    sql.push(')');
    sql
}

/// Quotes an identifier for `CREATE TABLE`, doubling an embedded quote.
///
/// SQLite's rule, measured: `CREATE TABLE t("a""b")` declares one column named
/// `a"b`. Without the doubling a name carrying a quote would produce text the
/// engine's own parser rejects, and this declaration is a piece of SQL the
/// engine has to read back.
pub fn quote_identifier(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// A connected `vec0` table, as the engine reads it.
///
/// `VTab` is the vector crate's `xConnect`/`xFilter`/`xColumn` shape and
/// `VtabInstance` is the engine's, and the two disagree in exactly one place:
/// `VtabInstance::scan` is EAGER and takes no arguments, where `VTab::filter`
/// takes a [`Plan`]. This is the read path the engine has, so the plan is the
/// one that means "every row" -- [`full_scan_plan`] -- and what that costs is
/// recorded there.
pub struct Vec0VTab {
    inner: Box<dyn VTab>,
}


impl VtabInstance for Vec0VTab {
    /// The declared columns, in declaration order, **without** the hidden
    /// `distance`.
    ///
    /// `VTab::column_count()` is `schema.columns.len() + 1` because it is
    /// `xColumnCount` and a query may address the hidden column. A row's
    /// values, however, are produced by `VTab::filter` into a `Cursor::row`,
    /// which builds one slot per declared column and then pushes the distance
    /// last. So the hidden column is the last value of every row and this list
    /// is every value but that one, which is what keeps a row's `values` the
    /// same width as the column list a query binds against.
    ///
    /// This is the reconciliation the module documentation describes: the
    /// hidden column exists in the module's numbering and not in the engine's.
    fn columns(&self) -> Vec<String> {
        declared_columns(&self.inner.schema())
    }

    /// Every row, as the module produces it.
    fn scan(&self) -> Result<Vec<VtabRow>, VtabError> {
        let schema = self.inner.schema();
        let cursor = self.inner.filter(&full_scan_plan(schema)).map_err(map_error)?;
        Ok(rows_of(schema, &cursor))
    }
}

/// The declared columns of a `vec0` schema, in declaration order.
///
/// The single definition of that list, shared by [`Vec0VTab::columns`] and by
/// the reopen path, so that a row's width and the column list a query binds
/// against cannot come from two places.
pub fn declared_columns(schema: &Schema) -> Vec<String> {
    schema.columns.iter().map(|c| c.name.clone()).collect()
}

/// A `vec0` table rebuilt from its shadow tables, with its rows already read.
///
/// The rows are materialised once, when the handle is made, and `scan` hands
/// them back. That is the eager shape the engine's `VtabInstance` is, and it
/// is not a shortcut around [`rows_from_shadow_tables`]: that function is what
/// builds them, through the module's own `filter`, so the column order, the
/// NULL-for-a-non-numeric-slot and the dropped hidden distance are the ones the
/// module produces rather than ones re-decided here.
pub struct ShadowVTab {
    schema: Schema,
    rows: Vec<VtabRow>,
}


impl VtabInstance for ShadowVTab {
    fn columns(&self) -> Vec<String> {
        declared_columns(&self.schema)
    }

    fn scan(&self) -> Result<Vec<VtabRow>, VtabError> {
        Ok(self.rows.clone())
    }
}

/// Turns the module's rows into the engine's, dropping the hidden `distance`.
///
/// `row.values` has one slot per declared column and then the distance, so the
/// declared width is `schema.columns.len()` and the last value is not a column
/// a query named. Keeping it would make every row one value wider than the
/// column list, which misaligns a join: the second source's first column would
/// be read out of the first source's distance.
///
/// # The rows are sorted by rowid
///
/// The cursor's order is the order `filter` searched in, which is ascending
/// distance from the plan's query vector -- and for a full scan that query is
/// the zero vector, so the order is by distance from the origin rather than by
/// anything the query said. MEASURED: rows 3, 1, 2 come back in that order,
/// which is their distances from the origin (4.25, 14, 77) and not their rowids.
///
/// The sort is stable, so two rows with the same rowid -- which cannot happen,
/// since a rowid is a key -- would keep the module's relative order. The result
/// is the order a plain `SELECT` over an ordinary table gives, which is what a
/// query that named no order has any reason to expect.
fn rows_of(schema: &Schema, cursor: &nsqlite_vector::vtab::Cursor) -> Vec<VtabRow> {
    let width = schema.columns.len();
    let mut rows: Vec<VtabRow> = cursor
        .rows()
        .iter()
        .map(|row| VtabRow {
            rowid: row.rowid,
            values: row.values.iter().take(width).map(value_of).collect(),
        })
        .collect();
    rows.sort_by_key(|r| r.rowid);
    rows
}

/// One module value slot as an engine value.
///
/// A `None` slot is a SQL `NULL` and is mapped to [`Value::Null`], **not** to
/// `0.0`. The distinction is the module's own and it is load-bearing: a
/// partition or auxiliary column is stored as bytes, and `Cursor::row` puts
/// `None` in that slot precisely so it is not fabricated into a number. A
/// `Some` slot is a one-element vector -- the module returns a vector column's
/// first component, and the hidden `distance` as a single value -- so the first
/// element is the value and an empty one is a NULL.
fn value_of(slot: &Option<Vec<f64>>) -> Value {
    match slot {
        None => Value::Null,
        Some(values) => values.first().map_or(Value::Null, |v| Value::Real(*v)),
    }
}

/// The plan that means "every row of this table".
///
/// `VTab::filter` takes a `Plan`, and a `Plan` is a nearest-neighbour query:
/// `query` is the vector to search around and `k` is how many rows to return.
/// So a plan over the zero vector of the table's own width is used, with a `k`
/// no row count can exceed. It is not a NULL distance -- the module would
/// reject a query whose width does not match the table's -- and it is not
/// `f64::INFINITY`, which is what a "match everything" plan would ask for and
/// is not a value a distance can be here.
///
/// **This is a compromise and it is documented rather than hidden.** A real
/// `SELECT * FROM v` on a `vec0` table should enumerate rows by rowid with no
/// distance computed at all; going through `filter` ranks every row against the
/// zero vector first. That is what the engine's EAGER `VtabInstance::scan` can
/// express today, because it takes no constraints and a `vec0` table's row
/// enumeration is defined by the module rather than by a b-tree. The rows and
/// their order are right; the work done to produce them is more than it needs
/// to be. When the engine grows `xBestIndex` (item 5 of the module's own
/// `ENGINE_CONTRACT`) a plan with no query becomes expressible and this
/// collapses to a straight enumeration.
///
/// # The order the rows come back in, which is NOT rowid order
///
/// **MEASURED, and this corrects an earlier claim in this file that the rows
/// came back in rowid order. They do not.** `filter` runs a *search*, and a
/// search returns its hits ranked by distance -- every row is a different
/// distance from the zero vector, so there is no tie to break and the ranking
/// is the whole order. On a table holding
///
/// ```text
/// rowid 1 -> [1.0, 2.0, 3.0]   L2^2 from zero = 14.0
/// rowid 2 -> [4.0, 5.0, 6.0]   L2^2 from zero = 77.0
/// rowid 3 -> [-1.0, 1.0, 1.5]  L2^2 from zero = 4.25
/// ```
///
/// `SELECT rowid, a FROM v` returns `3, 1, 2` -- ascending distance, exactly.
///
/// That is why `order_by_distance` is cleared here. `Plan::literal` sets it to
/// `true`, and leaving it would be a claim that the rows are in ascending
/// distance order -- which, as measured, they *are*, but for an arbitrary reason
/// (distance from the origin rather than from a query the user wrote) and not
/// as the meaning of `orderByConsumed`. Reporting `true` would let an engine
/// skip a sort on the strength of a coincidence. Reporting `false` is
/// conservative and cannot be wrong: it only ever costs a sort the engine was
/// going to do anyway.
///
/// The row ORDER the engine sees is then a property of this adapter, not of the
/// module, so [`rows_of`] imposes rowid order on the result. That is what a
/// `SELECT` over an ordinary table gives, and a query that asked for no
/// particular order has no reason to receive one that depends on where the
/// vectors happen to sit relative to the origin.
fn full_scan_plan(schema: &Schema) -> Plan {
    let mut plan = Plan::literal(vec![0.0; schema.dim()], usize::MAX);
    plan.order_by_distance = false;
    plan
}

/// Reads a `vec0` table's rows back out of its shadow tables.
///
/// This is the half of the reopen path the module cannot do for itself.
/// `Vec0Module::connect` finds the table in the module's own in-memory map,
/// which a **reopened process does not have**: a fresh `Vec0Module` is empty,
/// so `connect` answers `no such vtable: v`. The rows are on disk in the two
/// payload shadow tables, and this function is what puts them back together
/// with the schema the stored `sql` text describes.
///
/// It is also what makes a reopened database behave like a session that never
/// closed. The reference implementation deliberately gave this up -- its HNSW
/// graph is in-memory only and its documentation says the index "must be
/// repopulated by re-inserting rows after each open" -- so the shape being
/// matched here is the one where the *data* survives on its own and the index
/// is rebuilt from it. `Vec0Table::from_shadow_rows` is that rebuild, and it
/// builds its index per query rather than caching one, so there is nothing left
/// dangling between one open and the next.
///
/// # The two tables
///
/// `<table>_vectors` is `(rowid, embedding BLOB)` and `<table>_chunks` is
/// `(rowid, aux BLOB)`. Only those two are read. `<table>_rowids` makes a scan
/// deterministic and `<table>_info` holds the schema parameters; neither
/// carries a row this function needs, and `ShadowRows` has no field for them.
///
/// # Ordering
///
/// `shadow_rows()` returns its vectors "in rowid order", and `from_shadow_rows`
/// does not depend on that for correctness -- it inserts by explicit rowid
/// either way. The rows are sorted by rowid here anyway, which makes the order
/// a property of the file rather than of a page layout, and matches what a
/// `SELECT` over an ordinary table gives.
///
/// # Row width
///
/// The declared row is `schema.columns.len()` values wide: `Cursor::row` fills
/// one slot per declared column and then pushes the hidden `distance`, and that
/// last value is dropped for the reason [`rows_of`] gives.
pub fn rows_from_shadow_tables(
    schema: &Schema,
    vectors: &[(i64, Vec<u8>)],
    chunks: &[(i64, Option<Vec<u8>>)],
) -> Result<Vec<VtabRow>, VtabError> {
    let mut shadow = ShadowRows {
        vectors: vectors.to_vec(),
        metadata: chunks.to_vec(),
        // The rowid a fresh INSERT would hand out. It is read from the rows
        // rather than recomputed, so a table whose highest rowid is high but
        // whose rows are sparse still allocates where it left off.
        max_rowid: vectors.iter().map(|(rowid, _)| *rowid).max().unwrap_or(0),
    };
    shadow.vectors.sort_by_key(|(rowid, _)| *rowid);
    shadow.metadata.sort_by_key(|(rowid, _)| *rowid);

    let table = nsqlite_vector::index_store::Vec0Table::from_shadow_rows(
        schema.clone(),
        &shadow,
    )
    .map_err(|e| VtabError::Other(e.to_string()))?;

    // The rows come back through the module's own filter rather than through
    // the store directly, so the values a query sees are the ones the module
    // would have produced for the same rows -- the same column order, the same
    // NULL for a non-numeric slot, the same hidden distance last. Building
    // them here instead would be a second answer to "what does a vec0 row look
    // like", and the two would drift.
    let vtab = MemoryVTab::new(table);
    let cursor = vtab.filter(&full_scan_plan(schema)).map_err(map_error)?;
    Ok(rows_of(schema, &cursor))
}

#[cfg(test)]
mod tests {
    use super::*;
    use nsqlite::connection::{Connection, Outcome, Row};

    const ARGS: &str = "(a float[3], distance_metric=L2)";

    /// A temporary database that removes itself, so a failed assertion does not
    /// leave a file behind for the next test to trip over.
    struct Db(std::path::PathBuf);

    impl Db {
        fn new(tag: &str) -> Db {
            let path = std::env::temp_dir().join(format!(
                "vec0-{}-{tag}.db",
                std::process::id()
            ));
            let _ = std::fs::remove_file(&path);
            Db(path)
        }

        /// A connection with `vec0` registered, which is what `nsqlited` does
        /// at startup.
        fn open(&self) -> Connection {
            let mut c = Connection::open(&self.0).expect("open");
            c.register_vtab_module(std::rc::Rc::new(Vec0VTabModule::new()));
            c
        }

        /// A blob literal the engine's parser reads as bytes.
        fn blob(b: &[u8]) -> String {
            format!(
                "x'{}'",
                b.iter().map(|x| format!("{x:02X}")).collect::<String>()
            )
        }

        /// The module's own encoding of a vector: big-endian `f64`s, no header.
        fn vector(v: &[f64]) -> String {
            let mut raw = Vec::new();
            for x in v {
                raw.extend_from_slice(&x.to_be_bytes());
            }
            Db::blob(&raw)
        }
    }

    impl Drop for Db {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn run(c: &mut Connection, sql: &str) -> Outcome {
        c.execute_script(sql)
            .unwrap_or_else(|e| panic!("{sql:?} failed: {e}"))
            .into_iter()
            .last()
            .expect("one outcome")
    }

    fn rows(o: &Outcome) -> &[Row] {
        match o {
            Outcome::Query { rows, .. } => rows,
            other => panic!("expected a query, got {other:?}"),
        }
    }

    /// The values of the one row a query returned.
    fn one(o: &Outcome) -> &[Value] {
        let r = rows(o);
        assert_eq!(r.len(), 1, "expected exactly one row");
        &r[0].values
    }

    // -- method 1: name --------------------------------------------------

    #[test]
    fn the_module_is_named_vec0() {
        assert_eq!(Vec0VTabModule::new().name(), "vec0");
        assert_eq!(VEC0_MODULE_NAME, "vec0");
        // The registry matches a `USING` clause case-insensitively, so the name
        // has to survive being registered and looked up in either spelling.
        let mut reg = nsqlite::vtab::VtabRegistry::new();
        reg.register(std::rc::Rc::new(Vec0VTabModule::new()));
        assert!(reg.lookup("VEC0").is_some());
        assert!(reg.lookup("vec0").is_some());
    }

    // -- method 2: create ------------------------------------------------

    #[test]
    fn create_populates_the_modules_own_table_registry() {
        // The delegation is the point. A `create` that validated the
        // arguments and stopped would pass a test that only called `create`,
        // and then fail every `connect`, because `connect` finds the table in
        // that registry.
        let m = Vec0VTabModule::new();
        m.create("v", ARGS).expect("create");
        assert_eq!(m.table_names(), vec!["v".to_string()]);
        assert!(
            m.connect("v", ARGS).is_ok(),
            "connect must find what create registered"
        );
    }

    #[test]
    fn create_refuses_a_bad_argument_list_and_names_the_module() {
        let m = Vec0VTabModule::new();
        // No distance_metric: the module requires one rather than defaulting,
        // because a DDL string has no business leaving a detail to a default.
        let e = m.create("v", "(a float[3])").expect_err("must be refused");
        assert!(
            matches!(&e, VtabError::BadArguments { module, .. } if module == "vec0"),
            "expected a BadArguments naming vec0, got {e:?}"
        );
        assert!(
            m.table_names().is_empty(),
            "a refused create leaves nothing behind"
        );
    }

    #[test]
    fn create_twice_is_refused() {
        let m = Vec0VTabModule::new();
        m.create("v", ARGS).unwrap();
        assert!(m.create("v", ARGS).is_err());
    }

    // -- method 3: connect -----------------------------------------------

    #[test]
    fn connect_reports_its_columns_without_the_hidden_distance() {
        let m = Vec0VTabModule::new();
        m.create("v", ARGS).unwrap();
        let vtab = m.connect("v", ARGS).expect("connect");
        assert_eq!(vtab.columns(), vec!["a".to_string()]);
        assert_eq!(
            vtab.scan().expect("scan").len(),
            0,
            "a table with no rows scans to no rows"
        );
    }

    #[test]
    fn connect_names_a_table_that_was_never_created() {
        let m = Vec0VTabModule::new();
        let e = m
            .connect("nope", ARGS)
            .err()
            .expect("connecting to a table that was never created must fail");
        assert!(
            e.to_string().contains("no such vtable"),
            "expected 'no such vtable', got {e}"
        );
    }

    // -- method 4: shadow_tables -----------------------------------------

    #[test]
    fn shadow_tables_are_the_four_named_by_the_module() {
        let m = Vec0VTabModule::new();
        let tables = m.shadow_tables("v", ARGS).expect("shadow_tables");
        let names: Vec<&str> = tables.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["v_vectors", "v_chunks", "v_rowids", "v_info"]);
        for (name, sql) in &tables {
            // The engine runs each of these through its own parser and stores
            // the text, so text it cannot read is a failure here rather than at
            // the first reopen.
            assert!(
                matches!(
                    nsqlite::parser::parse_one(sql),
                    Ok(nsqlite::parser::Stmt::CreateTable { .. })
                ),
                "{name}: {sql:?} is not a CREATE TABLE"
            );
        }
    }

    #[test]
    fn shadow_tables_refuse_a_bad_argument_list_before_anything_is_written() {
        let m = Vec0VTabModule::new();
        assert!(m.shadow_tables("v", "(a float[3])").is_err());
    }

    // -- method 5: declare -----------------------------------------------

    #[test]
    fn declare_lists_the_declared_columns_and_not_the_hidden_distance() {
        let m = Vec0VTabModule::new();
        let sql = m.declare("v", ARGS).expect("declare");
        // MEASURED on the reference's rtree: the rowid alias IS listed --
        // rtree's `id` is at index 0 in PRAGMA table_info. MEASURED on fts5: a
        // hidden column (`rank`) is NOT, though it is still addressable. So one
        // listed column per declared one, and `distance` absent.
        assert_eq!(sql, "CREATE TABLE \"v\"(\"a\" float[3])");
        assert!(!sql.contains("distance"), "the hidden column is not declared");
        // And the engine's own parser must read it, since the declaration is
        // stored and re-read on every open.
        match nsqlite::parser::parse_one(&sql) {
            Ok(nsqlite::parser::Stmt::CreateTable { name, columns, .. }) => {
                assert_eq!(name, "v");
                assert_eq!(columns.len(), 1);
                assert_eq!(columns[0].name, "a");
            }
            other => panic!("the declaration must be a CREATE TABLE, got {other:?}"),
        }
    }

    #[test]
    fn declare_carries_every_column_kind_and_leaves_typeless_ones_untyped() {
        let m = Vec0VTabModule::new();
        let sql = m
            .declare("d", "(rowid, *user, a float[3], +note, distance_metric=L2)")
            .expect("declare");
        // A rowid alias, a partition and an auxiliary column take no type --
        // `Schema::parse` refuses one, so inventing one here would produce a
        // declaration the module itself would reject.
        assert_eq!(
            sql,
            "CREATE TABLE \"d\"(\"rowid\", \"user\", \"a\" float[3], \"note\")"
        );
        // The rowid alias IS listed, though: MEASURED on the reference's
        // rtree, whose `id` is the alias and is at index 0 in table_info.
    }

    #[test]
    fn declare_quotes_an_embedded_quote_in_a_column_name() {
        // A module argument list cannot easily produce a name carrying a quote,
        // so the quoting rule is exercised directly -- the declaration is SQL
        // the engine has to read back, and unquoted it would not be.
        assert_eq!(quote_identifier("plain"), "\"plain\"");
        assert_eq!(quote_identifier("a\"b"), "\"a\"\"b\"");
    }

    // -- the read path, end to end --------------------------------------

    #[test]
    fn a_select_on_a_created_table_returns_its_columns() {
        let db = Db::new("created");
        let mut c = db.open();
        run(
            &mut c,
            "CREATE VIRTUAL TABLE v USING vec0(a float[3], distance_metric=L2);",
        );
        let o = run(&mut c, "SELECT rowid, a FROM v;");
        match &o {
            Outcome::Query { columns, .. } => assert_eq!(columns, &["rowid", "a"]),
            other => panic!("expected a query, got {other:?}"),
        }
        assert!(rows(&o).is_empty());
    }

    #[test]
    fn a_select_reads_the_rows_the_shadow_tables_hold() {
        let db = Db::new("read");
        let mut c = db.open();
        run(
            &mut c,
            "CREATE VIRTUAL TABLE v USING vec0(a float[3], distance_metric=L2);",
        );
        // `INSERT INTO v` is not wired -- the engine has no xUpdate yet, which
        // is item 11 of the module's own contract -- so rows are written
        // through the shadow table, which is the path a module's own write
        // takes.
        for (rowid, vec) in [(1i64, [1.0, 2.0, 3.0]), (2, [4.0, 5.0, 6.0])] {
            run(
                &mut c,
                &format!(
                    "INSERT INTO v_vectors(rowid, embedding) VALUES({rowid}, {});",
                    Db::vector(&vec)
                ),
            );
        }
        let o = run(&mut c, "SELECT rowid, a FROM v;");
        let got = rows(&o);
        assert_eq!(got.len(), 2, "both rows come back: {got:?}");
        // A VALUE comparison, not a rendering: `a` is the first component of
        // the stored vector.
        assert_eq!(got[0].values[0], Value::Integer(1));
        assert_eq!(got[0].values[1], Value::Real(1.0));
        assert_eq!(got[1].values[0], Value::Integer(2));
        assert_eq!(got[1].values[1], Value::Real(4.0));
        // And the engine's own comparison agrees, which is the check that would
        // catch a value that merely renders the same.
        let o = run(&mut c, "SELECT count(*) FROM v WHERE a = 4.0;");
        assert_eq!(rows(&o)[0].values[0], Value::Integer(1));
    }

    #[test]
    fn a_star_expands_to_the_declared_columns_and_not_to_distance() {
        let db = Db::new("star");
        let mut c = db.open();
        run(
            &mut c,
            "CREATE VIRTUAL TABLE v USING vec0(a float[3], distance_metric=L2);",
        );
        run(
            &mut c,
            &format!(
                "INSERT INTO v_vectors(rowid, embedding) VALUES(1, {});",
                Db::vector(&[1.0, 2.0, 3.0])
            ),
        );
        let o = run(&mut c, "SELECT * FROM v;");
        match &o {
            Outcome::Query { columns, rows } => {
                assert_eq!(columns, &["a"], "the hidden distance is not in the star");
                assert_eq!(rows.len(), 1);
                // One value per declared column. A row carrying the distance as
                // a second column would misalign every source behind it in a
                // join, which is why `rows_of` drops it.
                assert_eq!(rows[0].values, vec![Value::Real(1.0)]);
            }
            other => panic!("expected a query, got {other:?}"),
        }
    }

    #[test]
    fn a_rowid_is_the_rows_own_field_and_not_a_column() {
        let db = Db::new("rowid");
        let mut c = db.open();
        run(
            &mut c,
            "CREATE VIRTUAL TABLE v USING vec0(a float[3], distance_metric=L2);",
        );
        for (rowid, first) in [(7i64, 9.0), (11, 8.0)] {
            run(
                &mut c,
                &format!(
                    "INSERT INTO v_vectors(rowid, embedding) VALUES({rowid}, {});",
                    Db::vector(&[first, 0.0, 0.0])
                ),
            );
        }
        // `recover_rowids` writes the rowid into the alias column and
        // `Ref::Rowid` reads the row's own field; both must give the rowid the
        // shadow table stored, not a position in the result.
        let o = run(&mut c, "SELECT rowid, a FROM v ORDER BY rowid;");
        let got = rows(&o);
        assert_eq!(got[0].values[0], Value::Integer(7));
        assert_eq!(got[1].values[0], Value::Integer(11));
        assert_eq!(got[0].values[1], Value::Real(9.0));
        assert_eq!(got[1].values[1], Value::Real(8.0));
    }

    #[test]
    fn a_virtual_table_joins_against_an_ordinary_one() {
        // The rowid lives in the row's own field, so a join has to recover it
        // through the same path. This is the check a misaligned row fails
        // rather than passes quietly.
        let db = Db::new("join");
        let mut c = db.open();
        run(
            &mut c,
            "CREATE VIRTUAL TABLE v USING vec0(a float[3], distance_metric=L2);",
        );
        run(&mut c, "CREATE TABLE t(k INTEGER, want REAL);");
        run(&mut c, "INSERT INTO t VALUES(2, 4.0), (1, 1.0);");
        run(
            &mut c,
            &format!(
                "INSERT INTO v_vectors(rowid, embedding) VALUES(1, {});",
                Db::vector(&[1.0, 2.0, 3.0])
            ),
        );
        run(
            &mut c,
            &format!(
                "INSERT INTO v_vectors(rowid, embedding) VALUES(2, {});",
                Db::vector(&[4.0, 5.0, 6.0])
            ),
        );
        let o = run(
            &mut c,
            "SELECT t.k, v.a FROM t JOIN v ON t.k = v.rowid ORDER BY t.k;",
        );
        let got = rows(&o);
        assert_eq!(got.len(), 2, "{got:?}");
        assert_eq!(got[0].values, vec![Value::Integer(1), Value::Real(1.0)]);
        assert_eq!(got[1].values, vec![Value::Integer(2), Value::Real(4.0)]);
    }

    #[test]
    fn an_auxiliary_column_reads_back_as_null_not_as_zero() {
        // The distinction the module's own docs insist on: absent is NULL, 0.0
        // is a number. `Cursor::row` puts `None` in that slot deliberately, and
        // mapping it to 0.0 would invent a value the table does not hold.
        let db = Db::new("aux");
        let mut c = db.open();
        run(
            &mut c,
            "CREATE VIRTUAL TABLE v USING vec0(a float[3], +note, distance_metric=L2);",
        );
        run(
            &mut c,
            &format!(
                "INSERT INTO v_vectors(rowid, embedding) VALUES(1, {});",
                Db::vector(&[1.0, 2.0, 3.0])
            ),
        );
        let o = run(&mut c, "SELECT a, note FROM v;");
        let got = rows(&o);
        assert_eq!(got[0].values[0], Value::Real(1.0));
        assert_eq!(
            got[0].values[1],
            Value::Null,
            "an absent auxiliary column is NULL, never 0.0"
        );
    }

    #[test]
    fn an_unknown_column_is_an_error_even_with_no_rows() {
        // Names are resolved before any row is read, which is the ordering that
        // makes a bad name fail on an empty table too.
        let db = Db::new("unknown");
        let mut c = db.open();
        run(
            &mut c,
            "CREATE VIRTUAL TABLE v USING vec0(a float[3], distance_metric=L2);",
        );
        let e = c
            .execute_script("SELECT nosuchcolumn FROM v;")
            .expect_err("an unknown column must be refused");
        assert!(
            e.message.contains("no such column"),
            "expected 'no such column', got {}",
            e.message
        );
    }

    #[test]
    fn an_unregistered_module_is_refused_and_leaves_nothing_behind() {
        // MEASURED on the reference: `CREATE VIRTUAL TABLE t USING nosuch(x)`
        // answers `no such module: nosuch` and writes no sqlite_schema row.
        let path = std::env::temp_dir().join(format!("vec0-nosuch-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut c = Connection::open(&path).expect("open");
        let e = c
            .execute_script("CREATE VIRTUAL TABLE v USING nosuch(x);")
            .expect_err("must be refused");
        assert_eq!(e.message, "no such module: nosuch");
        let o = c
            .execute_script("SELECT count(*) FROM sqlite_schema;")
            .expect("query")
            .into_iter()
            .last()
            .expect("one outcome");
        assert_eq!(one(&o)[0], Value::Integer(0), "nothing was written");
        let _ = std::fs::remove_file(&path);
    }

    // -- the reopen test -------------------------------------------------

    #[test]
    fn a_reopened_database_reads_its_rows_back() {
        // THE REOPEN TEST. A fresh `Vec0Module` has an empty table map, so
        // `connect` alone would answer `no such vtable` for a table whose
        // vectors are sitting in the file. The reference implementation gave
        // this up -- its HNSW graph is in memory only and its documentation
        // says the index "must be repopulated by re-inserting rows after each
        // open". Here the rows are the durable thing and the index is rebuilt
        // from them.
        let db = Db::new("reopen");
        {
            let mut c = db.open();
            run(
                &mut c,
                "CREATE VIRTUAL TABLE v USING vec0(a float[3], distance_metric=L2);",
            );
            for (rowid, first) in [(1i64, 1.0), (2, 4.0), (3, -1.0)] {
                run(
                    &mut c,
                    &format!(
                        "INSERT INTO v_vectors(rowid, embedding) VALUES({rowid}, {});",
                        Db::vector(&[first, 2.0, 3.0])
                    ),
                );
            }
        }
        // A BRAND NEW connection: a new module, an empty table registry, and
        // nothing but the file to go on.
        let mut c = db.open();
        let o = run(&mut c, "SELECT rowid, a FROM v;");
        let got = rows(&o);
        assert_eq!(got.len(), 3, "every row survives the reopen: {got:?}");
        assert_eq!(
            got.iter().map(|r| r.values[0].clone()).collect::<Vec<_>>(),
            vec![Value::Integer(1), Value::Integer(2), Value::Integer(3)],
            "rowids come back in rowid order"
        );
        // A VALUE comparison on the floats, because a row that merely renders
        // the same is not a row that reads back the same.
        let o = run(&mut c, "SELECT count(*) FROM v WHERE a = 1.0;");
        assert_eq!(rows(&o)[0].values[0], Value::Integer(1));
        let o = run(&mut c, "SELECT count(*) FROM v WHERE a = -1.0;");
        assert_eq!(rows(&o)[0].values[0], Value::Integer(1));
    }

    #[test]
    fn a_reopened_virtual_table_is_a_catalog_table_not_a_missing_one() {
        let db = Db::new("reopen-catalog");
        {
            let mut c = db.open();
            run(
                &mut c,
                "CREATE VIRTUAL TABLE v USING vec0(a float[3], distance_metric=L2);",
            );
        }
        let mut c = db.open();
        // It is listed, and listed as the shadow tables are -- as `type =
        // 'table'` rows, which is how SQLite stores a virtual table.
        let o = run(&mut c, "SELECT name FROM sqlite_schema ORDER BY name;");
        let names: Vec<String> = rows(&o)
            .iter()
            .map(|r| r.values[0].as_str().unwrap_or_default().to_string())
            .collect();
        for want in ["v", "v_vectors", "v_chunks", "v_rowids", "v_info"] {
            assert!(
                names.contains(&want.to_string()),
                "{want} missing from {names:?}"
            );
        }
        // And it has columns, which is what makes it a table a query resolves
        // against rather than one that answers `no such table`.
        let o = run(&mut c, "SELECT a FROM v;");
        assert!(rows(&o).is_empty());
    }

    #[test]
    fn a_reopen_matches_what_the_creating_session_read() {
        // The strongest form of the reopen claim: the same query, in the
        // session that created the table and in one that reopened it, must give
        // the same rows. A reopen that read the file differently from the
        // in-memory copy would fail here even though both were non-empty.
        let db = Db::new("reopen-same");
        let mut c = db.open();
        run(
            &mut c,
            "CREATE VIRTUAL TABLE v USING vec0(a float[3], distance_metric=L2);",
        );
        for (rowid, first) in [(1i64, 1.5), (2, -2.25)] {
            run(
                &mut c,
                &format!(
                    "INSERT INTO v_vectors(rowid, embedding) VALUES({rowid}, {});",
                    Db::vector(&[first, 0.0, 0.0])
                ),
            );
        }
        let in_session = rows(&run(&mut c, "SELECT rowid, a FROM v;")).iter().map(|r| r.values.clone()).collect::<Vec<_>>();
        drop(c);
        let mut c = db.open();
        let reopened = rows(&run(&mut c, "SELECT rowid, a FROM v;")).iter().map(|r| r.values.clone()).collect::<Vec<_>>();
        assert_eq!(in_session, reopened);
        assert_eq!(reopened.len(), 2);
    }

    #[test]
    fn an_empty_table_reopens_as_an_empty_table_not_an_error() {
        let db = Db::new("reopen-empty");
        {
            let mut c = db.open();
            run(
                &mut c,
                "CREATE VIRTUAL TABLE v USING vec0(a float[3], distance_metric=L2);",
            );
        }
        let mut c = db.open();
        let o = run(&mut c, "SELECT rowid, a FROM v;");
        assert!(rows(&o).is_empty());
        let o = run(&mut c, "SELECT count(*) FROM v;");
        assert_eq!(rows(&o)[0].values[0], Value::Integer(0));
    }

    // -- rows_from_shadow_tables, on its own -----------------------------

    #[test]
    fn rows_from_shadow_tables_orders_by_rowid_whatever_the_input_order() {
        // The function sorts, so a file whose pages happened to come back in
        // another order still reads back in rowid order.
        let schema = Schema::parse("v", ARGS).expect("schema");
        let raw = |v: &[f64]| {
            let mut b = Vec::new();
            for x in v {
                b.extend_from_slice(&x.to_be_bytes());
            }
            b
        };
        let vectors = vec![
            (3i64, raw(&[-1.0, 0.0, 0.0])),
            (1, raw(&[1.0, 0.0, 0.0])),
            (2, raw(&[2.0, 0.0, 0.0])),
        ];
        let rows = rows_from_shadow_tables(&schema, &vectors, &[]).expect("rows");
        assert_eq!(
            rows.iter().map(|r| r.rowid).collect::<Vec<_>>(),
            vec![1, 2, 3],
            "sorted by rowid regardless of the order they arrived in"
        );
        assert_eq!(rows[0].values, vec![Value::Real(1.0)]);
        assert_eq!(rows[2].values, vec![Value::Real(-1.0)]);
        // The hidden distance is dropped: one value per DECLARED column.
        assert!(rows.iter().all(|r| r.values.len() == schema.columns.len()));
    }

    #[test]
    fn a_blob_of_the_wrong_width_is_refused_rather_than_truncated() {
        // A short embedding is a corrupt row, and the module's answer to that
        // is to refuse it. Truncating would produce a vector of the wrong width
        // that then failed a later check with a much less useful message.
        let schema = Schema::parse("v", ARGS).expect("schema");
        let vectors = vec![(1i64, vec![0u8; 8])];
        let e = rows_from_shadow_tables(&schema, &vectors, &[]).expect_err("must be refused");
        assert!(
            e.to_string().contains("float[3]"),
            "the message names the declared width, got {e}"
        );
    }

    #[test]
    fn auxiliary_payloads_come_back_through_the_chunks_table() {
        // A present payload and an absent one are different values, and the
        // distinction survives the round trip: `None` stays NULL rather than
        // becoming a zero-length blob.
        let schema =
            Schema::parse("v", "(a float[3], +note, distance_metric=L2)").expect("schema");
        let mut raw = Vec::new();
        for x in [1.0f64, 2.0, 3.0] {
            raw.extend_from_slice(&x.to_be_bytes());
        }
        let vectors = vec![(1i64, raw.clone()), (2, raw)];
        let chunks = vec![(1i64, Some(b"payload".to_vec())), (2, None)];
        let rows = rows_from_shadow_tables(&schema, &vectors, &chunks).expect("rows");
        assert_eq!(rows.len(), 2);
        // The vector column is the first; the auxiliary is the second.
        assert_eq!(rows[0].values[0], Value::Real(1.0));
        assert_eq!(rows[1].values[1], Value::Null, "an absent payload is NULL");
    }
}
