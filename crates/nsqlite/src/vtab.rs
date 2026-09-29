//! The engine side of a virtual table: what a module must provide, and what the
//! engine owes it in return.
//!
//! # Why the trait is here and not in `nsqlite-vector`
//!
//! `nsqlite-vector` holds the `vec0` module and the whole vector store behind
//! it, and it deliberately does **not** depend on this crate. Its
//! `Cargo.toml` says why:
//!
//! > this crate does not use the engine, and must not -- the engine is what
//! > plugs the vec0 module in, so a dependency the other way would be the
//! > integration contract expressed as a cycle
//!
//! So the engine owns the trait and the module adapts to it. The adapter lives
//! outside both crates — in `nsqlited`, which depends on both — so neither has
//! to know about the other. `nsqlite-capi` needs nothing here: it exposes no
//! vtab bindings, and `nsqlite-vector`'s own vtab module puts the C
//! `sqlite3_module` layout explicitly out of scope, since that only matters to
//! a loadable `.so`.
//!
//! # What the first version does and does not do
//!
//! The surface is deliberately small, and it is small for a measured reason.
//! A reference implementation of a pure-Rust engine was read before this was
//! written, and its first virtual-table layer is read-only with no
//! `xBestIndex`: every scan is a full enumeration and the executor wraps the
//! result in a filter. That is enough to host a module, and it is what a
//! filter pushdown buys — fewer predicate evaluations and the module's own
//! ordering, not fewer I/O — so there is nothing to gain from building the
//! pushdown before a module needs it.
//!
//! Naming follows the same split. SQLite's C API has one `sqlite3_module`
//! struct with `xConnect`, `xCreate`, `xBestIndex` and the rest. This is two
//! traits, because the two halves are called at different times and from
//! different places: [`VtabModule`] is registered once at connection open and
//! asked to build a table; [`VtabInstance`] is what a query then reads through.

use crate::value::Value;

/// A registry entry: one module, asked to host virtual tables by name.
pub trait VtabModule {
    /// The name a `USING <name>(...)` clause matches, compared
    /// case-insensitively the way SQLite compares a module name.
    fn name(&self) -> &str;

    /// `xCreate`: stand a new virtual table up.
    ///
    /// `args` is the parenthesised argument list **verbatim**, parentheses
    /// included, because the module owns that grammar. The parser captures it
    /// between span offsets for exactly this reason.
    ///
    /// This returns no handle. That is a deliberate departure from the C shape,
    /// where `xCreate` hands back a `sqlite3_vtab*` the caller drops when the
    /// statement ends: a Rust handle would live on past the statement, and
    /// since it is a shared reference it would block every later write to the
    /// new table for as long as it was held. Creation and acquisition are split
    /// so an engine can `create`, then `connect` when it wants to read.
    ///
    /// `&self`, not `&mut self`, and that is not a stylistic choice. A module
    /// is shared: the registry hands out an `Rc`, so two queries can hold one
    /// at once, and an `&mut self` would make the second one impossible. A
    /// module that needs to change its own state while creating a table uses
    /// interior mutability — `RefCell` — which is the arrangement a reference
    /// implementation of a pure-Rust engine settled on for the same reason.
    /// `nsqlite-vector`'s own trait takes `&mut self`, so the adapter in
    /// `nsqlited` holds its module in a `RefCell` and hands it out.
    fn create(&self, name: &str, args: &str) -> Result<(), VtabError>;

    /// `xConnect`: attach to a table whose shadow tables already exist.
    ///
    /// This must not create anything, and it is the path a **reopened** database
    /// takes: the engine reads the original `CREATE VIRTUAL TABLE` text back out
    /// of `sqlite_schema` and hands it here, so the module rebuilds exactly
    /// what the first session built. A module that answered this from
    /// in-session state alone would get one behaviour in a session and another
    /// after a reopen, which is the failure `vec0`'s own `connect` is written
    /// to prevent.
    fn connect(&self, name: &str, args: &str) -> Result<Box<dyn VtabInstance>, VtabError>;

    /// `xConnect` for a table whose rows are being handed in rather than looked
    /// up.
    ///
    /// **This is the reopen path, and it exists because `connect` cannot cover
    /// it.** A module keeps its tables in whatever state it likes -- and
    /// `vec0`'s is an in-memory map, so a fresh module in a fresh process has
    /// none. `connect` would answer `no such vtable` for a table whose data is
    /// sitting in the file, which is the exact failure the reference
    /// implementation documented when it gave up on reopening: its HNSW graph
    /// is in memory only and "must be repopulated by re-inserting rows after
    /// each open". Handing the rows in is what makes the *data* outlive the
    /// process and the index be rebuilt from it.
    ///
    /// The rows are the two payload shadow tables, `(rowid, blob)` each,
    /// because that is the shape a table's storage takes in this engine -- an
    /// ordinary table per shadow table. Which two is the module's business;
    /// the engine passes what the module's `shadow_tables` implies and lets the
    /// module decide what it can use.
    ///
    /// The default forwards to [`VtabModule::connect`], so a module whose
    /// tables survive in the module needs nothing here. That is the right
    /// default rather than a required method: it is the honest answer for a
    /// module with no on-disk representation of its rows.
    fn connect_from_shadow(
        &self,
        name: &str,
        args: &str,
        vectors: &[(i64, Vec<u8>)],
        chunks: &[(i64, Option<Vec<u8>>)],
    ) -> Result<Box<dyn VtabInstance>, VtabError> {
        let _ = (vectors, chunks);
        self.connect(name, args)
    }

    /// The tables this module wants backing a virtual table, as
    /// `(name, CREATE TABLE text)` pairs, in creation order.
    ///
    /// The text is returned rather than a column list so the engine can store
    /// each shadow table's own definition in `sqlite_schema` the way it stores
    /// any other table's, and so a reopened database reads the same text back.
    fn shadow_tables(&self, name: &str, args: &str) -> Result<Vec<(String, String)>, VtabError>;

    /// The `CREATE TABLE` text a client should be told the virtual table has.
    ///
    /// This is what `SELECT * FROM v` reports its columns from, and what
    /// `PRAGMA table_info` parses, because a virtual table's real definition
    /// is `CREATE VIRTUAL TABLE ... USING module(...)` — which names no columns
    /// at all.
    fn declare(&self, name: &str, args: &str) -> Result<String, VtabError>;
}

/// One attached virtual table, as a query reads it.
pub trait VtabInstance {
    /// The column names, in declaration order.
    fn columns(&self) -> Vec<String>;

    /// Every row this table has, as `(rowid, values)`.
    ///
    /// Eager, not a cursor. The reference implementation materialises here too,
    /// and a `vec0` query computes distances over its whole index before
    /// returning anything, so a row-at-a-time cursor would be a shape the data
    /// does not have.
    fn scan(&self) -> Result<Vec<VtabRow>, VtabError>;
}

/// One row of a virtual table.
///
/// `Clone` because `VtabInstance::scan` takes `&self` and so has to hand back a
/// fresh `Vec` each call, and a handle that materialises its rows once -- the
/// reopen path does exactly that -- cannot answer twice without one.
#[derive(Debug, Clone, PartialEq)]
pub struct VtabRow {
    /// The row's identifier, which goes in the rowid and not in a column —
    /// `join::nested_loop` recovers rowids from the row's own field.
    pub rowid: i64,
    /// One value per declared column, in `columns()` order.
    pub values: Vec<Value>,
}

/// What a module can refuse with.
///
/// The variants are the ones a caller has to act on differently: a missing
/// module is a `no such module` error naming what was asked for, while the
/// rest are the module's own complaint and its text is what the user sees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VtabError {
    /// `USING <name>` where no module of that name is registered.
    NoSuchModule(String),
    /// The module read the arguments and could not use them.
    BadArguments { module: String, detail: String },
    /// Anything else the module wants to say, verbatim.
    Other(String),
}

impl std::fmt::Display for VtabError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VtabError::NoSuchModule(name) => write!(f, "no such module: {name}"),
            VtabError::BadArguments { module, detail } => write!(f, "{module}: {detail}"),
            VtabError::Other(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for VtabError {}

/// The modules a connection knows about.
///
/// Names are stored lowercased, because SQLite matches `USING` against a
/// module name case-insensitively — `USING VEC0` and `USING vec0` are the same
/// module. Lookup lowercases the query rather than the table, so a name that
/// is not registered is reported as the user wrote it.
#[derive(Default)]
pub struct VtabRegistry {
    modules: std::collections::HashMap<String, std::rc::Rc<dyn VtabModule>>,
}

impl VtabRegistry {
    /// An empty registry. A database with no virtual table in it never needs
    /// one, and this keeps the common case free of the `Rc` plumbing.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a module, replacing any module already under that name.
    pub fn register(&mut self, module: std::rc::Rc<dyn VtabModule>) {
        self.modules.insert(module.name().to_ascii_lowercase(), module);
    }

    /// The module of that name, if one is registered.
    pub fn lookup(&self, name: &str) -> Option<std::rc::Rc<dyn VtabModule>> {
        self.modules.get(&name.to_ascii_lowercase()).cloned()
    }

    /// Every registered module name, lowercased and sorted, which is what
    /// `PRAGMA module_list` reports.
    pub fn names(&self) -> Vec<String> {
        let mut v: Vec<String> = self.modules.keys().cloned().collect();
        v.sort();
        v
    }

    /// Whether anything is registered at all.
    pub fn is_empty(&self) -> bool {
        self.modules.is_empty()
    }
}
