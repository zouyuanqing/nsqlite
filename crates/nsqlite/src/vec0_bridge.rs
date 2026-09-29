//! The engine side of `vec0`: what the engine must provide to host a virtual
//! table, and what it currently does instead.
//!
//! # Where this stands
//!
//! **The engine has no `vec0` module.** It parses the statement and hosts the
//! registration; what it does not have is an implementation to hand the table
//! to. This module states the integration contract precisely and measures the
//! present behaviour so that "not yet" is a fact rather than an impression.
//!
//! # What was measured
//!
//! Against this engine, on the tree as it stands:
//!
//! ```text
//! $ printf 'CREATE VIRTUAL TABLE v USING vec0(a float[3]);\n' \
//!     | nsqlited --testsuite :memory:
//! E no such module: vec0
//! ```
//!
//! The statement parses — `parser::parse_one` returns
//! `Stmt::CreateVirtualTable`, because `Parser::create` handles CREATE TABLE,
//! CREATE INDEX, CREATE VIEW, CREATE TRIGGER, and CREATE VIRTUAL TABLE, and
//! then falls through to a catch-all that reports whatever it did not
//! recognise as `Stmt::Unsupported` (`crates/nsqlite/src/parser.rs:3077` for
//! the function, `:3111-3120` for the fall-through). So the parse half is done
//! and what is missing is an implementation to dispatch to.
//!
//! MEASURED after the parse half landed:
//!
//! ```text
//! $ printf 'CREATE VIRTUAL TABLE v USING vec0(a float[3]);\n' \
//!     | nsqlited --testsuite :memory:
//! E no such module: vec0
//! ```
//!
//! which is the reference's own sentence for a module the library does not
//! have, and the same one it gives for `vec0` — see
//! [`NO_SUCH_MODULE_PREFIX`]. The two engines now agree on this statement.
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
//! * **`MATCH` must survive the parser.** MEASURED: where no virtual table can
//!   take it, the reference says `unable to use function MATCH in the requested
//!   context`, and that error is a *runtime* one — `EXPLAIN SELECT 'a' MATCH
//!   'a'` prints a complete program against the real binary, and a `CASE` arm
//!   that is never taken answers normally. So the parser has to accept the
//!   operator and the refusal has to be raised when the expression is reached,
//!   not when it is planned and not when it is parsed. Item 7 below.
//! * **`distance` is a hidden column.** Measured against `fts5`: `rank` is
//!   addressable in a query and *absent* from `PRAGMA table_info`. A column
//!   record carrying only a name and a type cannot express that, so
//!   `pragma::table_info` needs a hidden flag on whatever it reads columns out
//!   of.

//! # Why `xBestIndex` is not in the sequence
//!
//! [`Vec0EngineContract::ITEMS`] is in dependency order, and `xBestIndex` is
//! item 14 — last, and outside the KNN sequence that the other items build.
//! That placement is a decision, and the reasoning is worth keeping because it
//! was settled by reading a reference implementation that **failed at it
//! first**.
//!
//! > A `best_index`-style **rowid pushdown is for _selection_**. Anything that
//! > changes the **shape or value** of the result needs its own plan node.
//!
//! A reference implementation of a pure-Rust engine (`rsqlite-wasm`) tried to
//! fit KNN into a
//! `VtabFilterPlan { rowids: Vec<i64>, residual: Option<PlanExpr> }` and could
//! not: that struct has no channel for "the query vector is this expression", no
//! channel for a runtime `k`, and no channel for a distance value. It added a
//! separate `Plan::VecIndexNearest` plan node instead. Its `vec_index` module
//! **does not implement `best_index` at all**.
//!
//! Our `vec0` is in the same category, and the two facts that put it there are
//! in the tree:
//!
//! * `crates/nsqlite-vector/src/vtab.rs:1443` is a `Plan` type that already
//!   carries `query: Option<Vec<f64>>` and `k: usize` — the two things a filter
//!   plan has nowhere to put;
//! * `Cursor::is_ordered()` at `crates/nsqlite-vector/src/vtab.rs:1231` **is**
//!   `orderByConsumed`; the plan carries `order_by_distance: bool` and the
//!   cursor is what reports it.
//!
//! So `xBestIndex` on a `vec0` query is a *filtering* optimisation that is not
//! on the KNN path at all. It can be built after the read path works, with no
//! correctness consequence: with items 5-8 and 13 in place, a query returns the
//! right rows in the right order whether or not the planner callback exists.
//!
//! The distinction the rule draws, stated so it can be applied to the next
//! thing that looks like it belongs in `xBestIndex`:
//!
//! | the callback changes | it belongs in `xBestIndex` |
//! | -------------------- | ------------------------ |
//! | *which* rows are visited | yes — that is selection |
//! | *how many* rows are produced | no — that is the shape |
//! | *what a column* contains | no — that is the value |
//! | *in what order* rows arrive | only if the module already has them |
//!
//! The last row is the subtle one, and it is why item 13 (`ORDERING`) is
//! separate from item 14. A `vec0` table does know its rows are ordered, and
//! saying so is worth a sort's worth of work — but the module knows it, not the
//! planner, so the engine can be told by the cursor (item 13) without
//! consulting a callback at all.

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
    ///
    /// The order is a **dependency** order — what has to exist before what —
    /// and not a reading order. Item 14, `xBestIndex`, is the one that does not
    /// sit in the KNN sequence, and it is deliberately last: it is an
    /// optimisation on a path that is already correct without it. The reasoning
    /// is long enough to be worth its own section; see the module docs, "Why
    /// `xBestIndex` is not in the sequence".
    ///
    /// Two of the original fourteen paragraphs were merged rather than dropped,
    /// and both merges are visible here so nobody has to wonder where a claim
    /// went: the old items 3 and 4 (`DROP` and `DROP ORDER`) were two halves of
    /// one concern and are now item 4; the old item 12 (`TRANSACTIONS`, which
    /// records that there is *nothing to do*) is now a closing sentence of item
    /// 12, the write-path item it qualifies.
    pub const ITEMS: [&'static str; 14] = [
        // 1
        "PARSE. `Parser::create` (crates/nsqlite/src/parser.rs:3077) handles
         CREATE TABLE, INDEX, VIEW, and TRIGGER, then falls through to
         `Stmt::Unsupported` for whatever it did not recognise
         (crates/nsqlite/src/parser.rs:3111-3120). DONE: it now returns a
         `Stmt::CreateVirtualTable` carrying (name, module, args-as-text, sql).
         The argument list stays text: the module owns that grammar.",
        // 2
        "CATALOG. `catalog::Catalog` must hold a table entry with root page 0
         and the original statement text. MEASURED: the reference writes
         type='table', rootpage=0, sql=the original CREATE VIRTUAL TABLE text.
         `sqlite_schema`'s own row for the vtab is an ordinary row; nothing
         about it is special to the catalog beyond the root page being 0.
         DEPENDS ON: 1 — there is nothing to record until the parser produces
         the four fields.",
        // 3
        "SHADOW TABLES. Run the module's CREATE TABLE statements through the
         ordinary DDL path and read the rows back through the same path.
         MEASURED: the reference's shadow tables are ordinary tables with real
         CREATE TABLE text and their own root pages, and `PRAGMA
         integrity_check` on such a file reports `ok`. This is the reason the
         module's storage is a table per shadow table and not a struct.
         The COUNT is the module's, not the engine's: MEASURED on the
         reference's rtree, `CREATE VIRTUAL TABLE g USING rtree(id, minX, maxX)`
         makes THREE shadow tables (`g_rowid`, `g_node`, `g_parent`), so a
         fixed 'four' is a `vec0` fact and never an engine rule.
         DEPENDS ON: 2.",
        // 4
        "DROP, AND DROP ORDER. `DROP TABLE` on a virtual table must drop its
         shadow tables, and it must consult the module BEFORE the pager, so the
         module can veto or report and so it is the one that knows the names.
         Without the drop a CREATE/DROP cycle leaves a table behind each time
         and the file grows without bound. The module owns the name of its
         shadow tables; the engine does not know them without asking.
         DEPENDS ON: 2 and 3.",
        // 5
        "READ PATH. A cursor that walks the shadow tables and produces one row
         per neighbour, and an `xColumn` that reads a column out of it. This is
         the item the whole contract exists to make possible; everything after
         it improves a path that is already correct without it.
         DEPENDS ON: 3.",
        // 6
        "PLAN SHAPE. The KNN query needs its own plan node. The query vector and
         a runtime `k` are not filters over an existing row set — they are what
         the rows ARE — and a `best_index` rowid pushdown has no channel for
         either. See the module docs, 'Why `xBestIndex` is not in the sequence',
         for the reference implementation that tried to fit this into a filter
         plan and could not. The same shape is visible in the reference's own
         lowering of the operator: MEASURED, `EXPLAIN SELECT 'a' MATCH 'a'` is
         a single `Function` opcode reading `match(2)`, not a scan with a
         constraint attached. DEPENDS ON: 5.",
        // 7
        "MATCH. `<table> MATCH <term>` must reach the planner as a function
         constraint rather than being rejected by the parser. MEASURED: the
         reference says `unable to use function MATCH in the requested context`
         where no virtual table can take it, and that error is a RUNTIME one —
         `EXPLAIN SELECT 'a' MATCH 'a'` prints a whole program against the real
         binary, and `SELECT CASE WHEN 0 THEN ('a' MATCH 'a') ELSE 42 END`
         answers 42. So the parser must accept the operator and the refusal must
         be raised when the expression is evaluated, not when it is planned.
         DONE, and measured to agree byte for byte; see `Expr::Match` in
         `parser.rs`, `Msg::MatchNotInContext` in `msg.rs`, and `match_tests`.
         The column on the left binds through the ordinary resolver, which is
         what lets this item hand a resolved reference to item 6.
         DEPENDS ON: 5.",
        // 8
        "FILTER ARGUMENTS. Each constraint's argument must reach `xFilter`. A
         literal arrives with the constraint; a bound parameter does not, and
         must travel in `idxStr`/`idxNum` so that `xFilter` can read it. This
         is the case the reference's own shape exercises with `k = ?`.
         DEPENDS ON: 6.",
        // 9
        "HIDDEN COLUMNS. `pragma::table_info` and the column resolution in
         `resolve` need a hidden flag on a column. MEASURED: `rank` on an
         fts5 table is addressable in a query and absent from table_info;
         `distance` is the same kind of column. A name and a type cannot
         express that. DEPENDS ON: 5 — a hidden column is a read-path column
         the catalog chooses not to list.",
        // 10
        "NULLS. A NULL returned by `xColumn` must become a real SQL `NULL`,
         distinct from a stored zero. `eval` and `value::Value::Null` already
         carry the distinction; the requirement is that a vtab result is built
         from the same `Value` type rather than a second one. DEPENDS ON: 5.",
        // 11
        "NO B-TREE. Never open a virtual table's b-tree. There is none:
         rootpage is 0, and page 0 is the database header. `table_tree::
         TableTree::open` and `btree::TableBtree::new` both take a root page and
         would read the header as a page if handed 0. This is a RULE rather
         than a step, which is why it sits beside the read path instead of
         inside it: every item that opens rows depends on it holding.
         DEPENDS ON: 2.",
        // 12
        "ROWID, XUPDATE SHAPE, AND TRANSACTIONS. After an INSERT,
         `last_insert_rowid()` must be the rowid the module assigned — MEASURED
         on the reference's rtree, after `INSERT INTO g VALUES(1, 0.0, 1.0)` it
         is `1`, and `Connection::last_insert_rowid` is where the value is set.
         And `xUpdate` needs both halves of its argv for an UPDATE, only
         argv[0] for an INSERT, and argv[1] alone is a DELETE: three shapes that
         the module distinguishes, so the engine has to pass them faithfully.
         See `insert_select` for the write path this feeds. TRANSACTIONS needs
         nothing: a shadow-table write is an ordinary write, so `journal`
         already covers it, and it is named here so that its absence is a
         decision on the record rather than a gap found during a rollback test.
         DEPENDS ON: 3.",
        // 13
        "ORDERING. The rows come out sorted by ascending distance, and
         `orderByConsumed` says so, so the engine skips its own sort. This is
         what makes `ORDER BY distance LIMIT k` one pass rather than two. It is
         a property of the READ PATH and of nothing else, which is exactly why
         it is worth separating from item 14: the engine learns it from the
         cursor, not from a planner callback. DEPENDS ON: 5.",
        // 14
        "BEST INDEX — OPTIMISATION, NOT CORRECTNESS, AND NOT ON THE KNN PATH.
         `xBestIndex` must be called with the statement's usable constraints and
         its `orderByConsumed` must be honoured. It is LAST because it changes
         no answer: with items 5-8 and 13 done, a vec0 query returns the right
         rows in the right order, and this only removes work. A `best_index`
         rowid pushdown is for SELECTION; anything that changes the SHAPE or
         VALUE of the result needs its own plan node, which is item 6. See the
         module docs, 'Why `xBestIndex` is not in the sequence', for the
         reference implementation that failed at this first and what it had to
         do instead.",
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

/// The message this module used to give for a `CREATE VIRTUAL TABLE`, kept
/// because it is now HISTORY rather than behaviour.
///
/// It was `virtual is not supported yet`, and it was a sentence about the
/// *engine*: the statement parsed, and the engine then refused to execute it.
/// MEASURED, the engine no longer says it — the registry now answers
/// [`no_such_module`]'s message for a module it has not got, which is the
/// reference's own sentence and a claim about the same thing from the other
/// side:
///
/// ```text
/// $ printf 'CREATE VIRTUAL TABLE v USING vec0(a float[3]);\n' \
///     | nsqlited --testsuite :memory:
/// E no such module: vec0
/// ```
///
/// So the two are no longer different faults, and the distinction this constant
/// was kept for has collapsed. It is retained only so that a reader who meets
/// it in an older note can tell that the note is describing a past state.
pub const UNSUPPORTED_MESSAGE: &str = "virtual is not supported yet";

/// The message the reference gives for a module it does not have.
///
/// Measured: `no such module: vec0`. The engine now gives the same sentence for
/// an unregistered module name — see the note on [`UNSUPPORTED_MESSAGE`], which
/// is what this used to be distinguished from.
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
    fn the_two_module_errors_are_still_two_distinct_sentences() {
        // This test used to say the two were "different faults", which was true
        // when the engine answered `virtual is not supported yet` for a
        // `CREATE VIRTUAL TABLE` and the reference answered `no such module:
        // vec0`. It is no longer: the engine now answers the reference's
        // sentence, so the two faults have merged and only the sentences are
        // still distinct. What is asserted is the weaker true thing -- the two
        // strings are not interchangeable, so a test that expected one where
        // the other is given still fails -- and the comment says why the
        // stronger claim went away, so the next reader does not re-derive it.
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
