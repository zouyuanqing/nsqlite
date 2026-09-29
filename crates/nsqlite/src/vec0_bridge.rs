//! The engine side of `vec0`: what the engine must provide to host a virtual
//! table, and what it currently does instead.
//!
//! # Where this stands
//!
//! **The engine has no virtual table machinery.** This module does not add it.
//! What it does is state the integration contract precisely, and measure the
//! present behaviour so that "not yet" is a fact rather than an impression.
//!
//! # What was measured
//!
//! Against this engine, on the tree as it stands:
//!
//! ```text
//! $ CREATE VIRTUAL TABLE v USING vec0(a float[3]);
//! ERROR: virtual is not supported yet
//! ```
//!
//! The statement parses — `parser::parse_one` returns
//! `Stmt::Unsupported("virtual")`, because `Parser::create_statement` falls
//! through to its catch-all at `crates/nsqlite/src/parser.rs:2931` after
//! handling `CREATE TABLE`, `CREATE INDEX`, `CREATE VIEW`, and `CREATE TRIGGER`.
//! So the shape is already recognised and only the execution is missing. That
//! is a useful place to be: the parse half of the work is a matter of adding a
//! variant rather than of teaching the parser a grammar.
//!
//! Against the real `sqlite3` 3.53.4, the behaviour this module has to match:
//!
//! ```text
//! $ sqlite3 :memory: "CREATE VIRTUAL TABLE v USING vec0(a float[3]);"
//! Error in 2nd command line argument: no such module: vec0
//! ```
//!
//! and, for a module it *does* have, the shape a virtual table takes in the
//! catalog:
//!
//! ```text
//! type | name      | tbl_name | rootpage | sql
//! -----+-----------+----------+----------+--------------------------------------
//! table| geo       | geo      |         0|CREATE VIRTUAL TABLE geo USING rtree(...)
//! table| geo_rowid | geo_rowid|         2|CREATE TABLE "geo_rowid"(rowid INTEGER PRIMARY KEY,nodeno)
//! table| geo_node  | geo_node |         3|CREATE TABLE "geo_node"(nodeno INTEGER PRIMARY KEY,data)
//! ```
//!
//! Four things follow from that table, and they are the whole of what the
//! catalog side has to do:
//!
//! 1. a virtual table is a `sqlite_schema` row of `type = 'table'` with
//!    `rootpage = 0`;
//! 2. its `sql` is the original `CREATE VIRTUAL TABLE` text, verbatim, because a
//!    reopened connection reads the definition back from it;
//! 3. each shadow table is an **ordinary** table with its own rootpage and real
//!    `CREATE TABLE` text — `PRAGMA integrity_check` on such a file reports
//!    `ok`, measured, so nothing special is needed for them;
//! 4. a `CREATE` for a module the library lacks leaves no row behind, so a
//!    failed registration rolls back cleanly.
//!
//! # The contract
//!
//! [`Vec0EngineContract`] lists the fourteen items, each with the change it
//! names and, where one exists, the function in this crate that has to grow.
//! It is the same list the vector crate carries on its side; this one is
//! written from the engine's point of view, naming engine functions.
//!
//! Three of the items are worth calling out because they are the ones a
//! hand-rolled implementation gets wrong:
//!
//! * **`rootpage = 0` is load-bearing.** An implementation that opens a table's
//!   b-tree unconditionally will read page 0, which is the database header, not
//!   a table. See [`catalog::Catalog`] and `table_tree::TableTree::open`.
//! * **`MATCH` must survive the parser.** Measured: where no virtual table can
//!   take it, the reference says `unable to use function MATCH in the requested
//!   context`, and the engine's own message for the same mistake is
//!   `virtual is not supported yet`. Those are different faults, and collapsing
//!   them would make the second one impossible to diagnose.
//! * **`distance` is a hidden column.** Measured against `fts5`: `rank` is
//!   addressable in a query and *absent* from `PRAGMA table_info`. A column
//!   record carrying only a name and a type cannot express that, so
//!   `pragma::table_info` needs a hidden flag on whatever it reads columns out
//!   of.

use crate::value::Value;

/// The row a virtual table returns for one `xColumn` read.
///
/// Modelled here so the engine side of the contract is written in the engine's
/// own vocabulary. A `None` is a real SQL `NULL` and is distinct from a stored
/// zero — the distinction matters for the auxiliary columns, where "absent" and
/// "0.0" are different answers to a different question.
#[derive(Debug, Clone, PartialEq)]
pub struct VTabValue {
    /// The column index this value answers for, in the module's own numbering.
    pub column: usize,
    /// The value, or `None` for SQL `NULL`.
    pub value: Option<Value>,
}

impl VTabValue {
    /// A non-NULL value.
    pub fn some(column: usize, value: Value) -> Self {
        VTabValue {
            column,
            value: Some(value),
        }
    }

    /// A SQL `NULL`.
    pub fn null(column: usize) -> Self {
        VTabValue {
            column,
            value: None,
        }
    }

    /// Whether this is a SQL `NULL`.
    pub fn is_null(&self) -> bool {
        self.value.is_none()
    }
}

/// One column of a virtual table's declaration, as the engine holds it.
///
/// The `hidden` flag is the field that has no counterpart in an ordinary
/// table's column list, and it is the one a `vec0` table cannot be built
/// without. See the module docs.
#[derive(Debug, Clone, PartialEq)]
pub struct VTabColumn {
    /// Zero-based position in the module's column numbering.
    pub index: usize,
    /// The name a query uses.
    pub name: String,
    /// The declared type, for `PRAGMA table_info`.
    pub decl_type: String,
    /// Whether the column is addressable in a query but absent from
    /// `PRAGMA table_info`.
    ///
    /// **Measured**: the reference's `fts5` has a `rank` column that
    /// `SELECT rank FROM ft WHERE ft MATCH 'a'` reads and that
    /// `PRAGMA table_info(ft)` does not list. `distance` on a `vec0` table is
    /// the same kind of column.
    pub hidden: bool,
}

impl VTabColumn {
    /// An ordinary, visible column.
    pub fn visible(index: usize, name: impl Into<String>, decl_type: impl Into<String>) -> Self {
        VTabColumn {
            index,
            name: name.into(),
            decl_type: decl_type.into(),
            hidden: false,
        }
    }

    /// A hidden column: addressable in a query, absent from `table_info`.
    pub fn hidden(index: usize, name: impl Into<String>, decl_type: impl Into<String>) -> Self {
        VTabColumn {
            index,
            name: name.into(),
            decl_type: decl_type.into(),
            hidden: true,
        }
    }
}

/// A `CREATE VIRTUAL TABLE`, as the engine will need to see one.
///
/// The three fields are exactly what the contract's first item asks the parser
/// to produce, and the argument list is deliberately left as text: the module
/// owns that grammar, and a re-serialised argument list would lose the original
/// spelling, which is what the catalog has to store.
#[derive(Debug, Clone, PartialEq)]
pub struct CreateVirtualTable {
    /// The table name, unqualified.
    pub name: String,
    /// The module name — `vec0`.
    pub module: String,
    /// The module's argument list, verbatim, parentheses included.
    pub args: String,
    /// The whole statement's source text, which is what `sqlite_schema` stores.
    ///
    /// **Measured**: the reference keeps the original `CREATE VIRTUAL TABLE`
    /// text byte for byte, including the spacing. A reconstruction is not
    /// equivalent, because a reopened connection reads the definition back from
    /// this column.
    pub sql: String,
}

impl CreateVirtualTable {
    /// Builds a statement from a module name and its argument list, deriving
    /// `sql` the way the parser's own `CreateTable` does.
    pub fn new(name: &str, module: &str, args: &str) -> Self {
        let sql = format!("CREATE VIRTUAL TABLE {name} USING {module}{args}");
        CreateVirtualTable {
            name: name.to_string(),
            module: module.to_string(),
            args: args.to_string(),
            sql,
        }
    }
}

/// The fourteen things the engine has to provide to host a `vec0` table.
///
/// Each item names the change and, where one exists, the function in this crate
/// that has to grow. The vector crate carries the same list written from the
/// module's side; the two are meant to be read together, because a contract only
/// helps if both ends can see it.
pub struct Vec0EngineContract;

impl Vec0EngineContract {
    /// The items, one per paragraph, in the order the engine meets them.
    pub const ITEMS: [&'static str; 14] = [
        // 1
        "PARSE. `Parser::create_statement` (crates/nsqlite/src/parser.rs, `create_statement`)
         handles CREATE TABLE, INDEX, VIEW, and TRIGGER, then falls through to
         `Stmt::Unsupported`. Add a `CreateVirtualTable` variant carrying
         (name, module, args-as-text, sql). The argument list stays text: the
         module owns that grammar. MEASURED: the engine already recognises the
         statement and reports `virtual is not supported yet`, so this is a
         variant plus a branch, not a new grammar.",
        // 2
        "CATALOG. `catalog::Catalog` must hold a table entry with root page 0
         and the original statement text. MEASURED: the reference writes
         type='table', rootpage=0, sql=the original CREATE VIRTUAL TABLE text.
         `sqlite_schema`'s own row for the vtab is an ordinary row; nothing
         about it is special to the catalog beyond the root page being 0.",
        // 3
        "DROP. `DROP TABLE` on a virtual table must drop its shadow tables. Four
         of them per table, named `<table>_vectors`, `_chunks`, `_rowids`,
         `_info`. Without this a CREATE/DROP cycle leaves a table behind each
         time and the file grows without bound.",
        // 4
        "DROP ORDER. The drop must consult the module before the pager, so the
         module can veto or report. The module owns the name of its shadow
         tables; the engine does not know them without asking.",
        // 5
        "PLAN. `xBestIndex` must be called with the statement's usable
         constraints and its `orderByConsumed` must be honoured. A vec0 query is
         `ORDER BY distance LIMIT k`, and the module's rows come out already in
         that order, so consuming it is the difference between one sort and two.
         See `orderby` for the sorter this hooks into.",
        // 6
        "FILTER ARGUMENTS. Each constraint's argument must reach `xFilter`. A
         literal arrives with the constraint; a bound parameter does not, and
         must travel in `idxStr`/`idxNum` so that `xFilter` can read it. This
         is the case the reference's own shape exercises with `k = ?`.",
        // 7
        "MATCH. `<table> MATCH <term>` must reach the planner as a function
         constraint rather than being rejected by the parser. MEASURED: the
         reference says `unable to use function MATCH in the requested context`
         where no virtual table can take it, so the parser must leave the
         decision to planning. See `resolve` for where a column would be bound.",
        // 8
        "HIDDEN COLUMNS. `pragma::table_info` and the column resolution in
         `resolve` need a hidden flag on a column. MEASURED: `rank` on an
         fts5 table is addressable in a query and absent from table_info;
         `distance` is the same kind of column. A name and a type cannot
         express that.",
        // 9
        "NULLS. A NULL returned by `xColumn` must become a real SQL `NULL`,
         distinct from a stored zero. `eval` and `value::Value::Null` already
         carry the distinction; the requirement is that a vtab result is built
         from the same `Value` type rather than a second one.",
        // 10
        "ROWID. After an INSERT, `last_insert_rowid()` must be the rowid the
         module assigned. MEASURED: the reference's rtree returns the vtab's
         rowid from `last_insert_rowid()`. `Connection::last_insert_rowid` is
         where that value is set, so `xUpdate` has to report what it assigned.",
        // 11
        "XUPDATE SHAPE. Supply both halves of `xUpdate`'s argv for an UPDATE and
         only argv[0] for an INSERT; argv[1] alone is a DELETE. Three shapes,
         and the module distinguishes them, so the engine has to pass them
         faithfully. See `insert_select` for the write path this feeds.",
        // 12
        "TRANSACTIONS. Nothing to do. A shadow-table write is an ordinary
         write, so `journal` already covers it. Listed so that its absence is a
         decision on the record rather than a gap found during a rollback test.",
        // 13
        "SHADOW TABLES. Run the module's four CREATE TABLE statements through
         the ordinary DDL path and read the rows back through the same path.
         MEASURED: the reference's shadow tables are ordinary tables with real
         CREATE TABLE text and their own root pages, and `PRAGMA
         integrity_check` on such a file reports `ok`. This is the reason the
         module's storage is a table per shadow table and not a struct.",
        // 14
        "NO B-TREE. Never open a virtual table's b-tree. There is none:
         rootpage is 0, and page 0 is the database header. `table_tree::
         TableTree::open` and `btree::TableBtree::new` both take a root page and
         would read the header as a page if handed 0.",
    ];

    /// The items as one block of text, for a report or a review.
    pub fn text() -> String {
        Vec0EngineContract::ITEMS
            .iter()
            .enumerate()
            .map(|(n, item)| format!("{}. {item}", n + 1))
            .collect::<Vec<_>>()
            .join("\n\n")
    }
}

/// The message the engine gives for a `CREATE VIRTUAL TABLE` today.
///
/// Measured, and worth pinning because it is the one a user sees first: it
/// says what the engine cannot do, and it is not the message a reader would
/// expect from a *parse* failure, because the statement parses fine.
pub const UNSUPPORTED_MESSAGE: &str = "virtual is not supported yet";

/// The message the reference gives for a module it does not have.
///
/// Measured: `no such module: vec0`. The engine will need the same message for
/// an unregistered module name, distinct from
/// [`UNSUPPORTED_MESSAGE`], because they are different faults — one is "this
/// library has no such extension", the other is "this extension has no
/// implementation in this engine".
pub const NO_SUCH_MODULE_PREFIX: &str = "no such module: ";

/// Builds the reference's message for a module the library does not have.
pub fn no_such_module(module: &str) -> String {
    format!("{NO_SUCH_MODULE_PREFIX}{module}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_contract_numbers_every_item_from_one() {
        let text = Vec0EngineContract::text();
        for n in 1..=14 {
            assert!(text.contains(&format!("{n}. ")), "item {n} is missing");
        }
    }

    #[test]
    fn the_two_module_errors_are_different_faults() {
        // Collapsing them would make "the library has no vec0" and "this engine
        // has no vec0" indistinguishable, which is the diagnostic the user
        // actually needs.
        assert_ne!(no_such_module("vec0"), UNSUPPORTED_MESSAGE);
        assert_eq!(no_such_module("vec0"), "no such module: vec0");
    }

    #[test]
    fn a_create_statement_keeps_its_original_text() {
        let stmt =
            CreateVirtualTable::new("docs", "vec0", "(embedding float[3], distance_metric=L2)");
        assert_eq!(
            stmt.sql,
            "CREATE VIRTUAL TABLE docs USING vec0(embedding float[3], distance_metric=L2)"
        );
        // The arguments stay verbatim, parentheses and all, because the module
        // owns that grammar and a reserialised list would lose the spelling.
        assert_eq!(stmt.args, "(embedding float[3], distance_metric=L2)");
    }

    #[test]
    fn a_create_statement_round_trips_through_the_engine_parser() {
        // A virtual table's text is stored and read back verbatim, so the text
        // this module builds has to be something the engine's tokenizer can
        // read. This used to assert the statement stayed `Unsupported` --
        // "the engine should recognise but not yet execute it" -- and that
        // assertion is what this pass removed: the engine now parses it into a
        // real `Stmt::CreateVirtualTable`, carrying back exactly the four
        // fields this struct holds. A reopened connection rebuilds the table
        // from that text, so what comes back out of the parser has to be what
        // went in.
        let stmt =
            CreateVirtualTable::new("docs", "vec0", "(embedding float[3], distance_metric=L2)");
        let parsed = crate::parser::parse_one(&stmt.sql).expect("the stored text must tokenize");
        let crate::parser::Stmt::CreateVirtualTable { name, module, args, sql } = parsed else {
            panic!("the engine should parse it as a virtual table, got {parsed:?}")
        };
        assert_eq!(name, stmt.name);
        assert_eq!(module, stmt.module);
        assert_eq!(args, stmt.args, "the argument list must survive verbatim");
        assert_eq!(sql, stmt.sql, "the stored text must survive verbatim");
    }

    #[test]
    fn a_hidden_column_is_a_column_the_table_info_does_not_list() {
        let visible = VTabColumn::visible(0, "embedding", "float[3]");
        let hidden = VTabColumn::hidden(3, "distance", "REAL");
        assert!(!visible.hidden);
        assert!(hidden.hidden);
        // Both are addressable; only the flag distinguishes them.
        assert_eq!(visible.index, 0);
        assert_eq!(hidden.index, 3);
    }

    #[test]
    fn a_null_vtab_value_is_distinct_from_a_zero() {
        let null = VTabValue::null(1);
        let zero = VTabValue::some(1, Value::Real(0.0));
        assert!(null.is_null());
        assert!(!zero.is_null());
        assert_ne!(null, zero);
    }
}
