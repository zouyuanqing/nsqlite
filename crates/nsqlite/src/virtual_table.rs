//! Reading a virtual table's rows, from the engine's side of the contract.
//!
//! # Why this is not in `vtab`
//!
//! [`crate::vtab`] is the trait a module implements, and it deliberately knows
//! nothing about pages, shadow tables or the shape a file takes. This module is
//! the other half: it is the one place that turns what is on disk into what
//! [`VtabInstance::scan`] returns, and it is where the two halves meet.
//!
//! The split matters because of what a virtual table *is* in this engine. It
//! has no b-tree -- `root_page` is 0, and page 0 is the database header -- so
//! its rows cannot come from `TableTree`. They come from shadow tables that are
//! perfectly ordinary, and the module owns how they combine. The engine's job
//! is to hand over `(rowid, blob)` pairs and take back rows; the module's is to
//! know what a pair means.
//!
//! # The two paths, and why both exist
//!
//! In a session that created the table, [`VtabModule::connect`] works: the
//! module still holds what `create` registered. In a **reopened** process it
//! does not, because the module is new and empty -- which is why
//! [`VtabModule::connect_from_shadow`] exists and why
//! [`connect`] prefers it. The engine cannot tell the two cases apart, and
//! should not try: asking the module for a table and letting it say what it
//! can do is the shape that keeps the distinction inside the module.
//!
//! So [`connect`] asks first and falls back, rather than deciding. That is not
//! hedging; it is the same answer for both cases when the module handles it and
//! a clear error when it does not.

use crate::error::{Error, Result, ResultCode};
use crate::vtab::{VtabInstance, VtabRegistry};

/// Asks `registry` for a handle onto the table `name`, handing it the rows read
/// from the shadow tables.
///
/// `sql_text` is the table's own stored definition -- `CREATE VIRTUAL TABLE
/// <name> USING <module>(<args>)` -- and `args` is the parenthesised argument
/// list taken out of it. The registry is asked for the module `sql_text` names,
/// so a file whose module is not registered answers `no such module: <name>`
/// rather than failing somewhere further in.
///
/// The module's `connect_from_shadow` is the **only** path, with no fallback to
/// `connect`.
///
/// That was tried and it hid the fault rather than covering it: when
/// `connect_from_shadow` failed for any reason, the fallback ran and reported
/// `no such vtable: v`, which is a different problem from the one that had
/// actually happened and sent the diagnosis somewhere else entirely.
///
/// A module whose rows live only in itself overrides nothing and gets the
/// default, which forwards to `connect` -- so it loses nothing by this being
/// the only path. What is gone is the case where a module *could* answer but
/// its own answer was preferred over a correct one.
pub fn connect(
    registry: &VtabRegistry,
    name: &str,
    sql_text: &str,
    args: &str,
    vectors: &[(i64, Vec<u8>)],
    chunks: &[(i64, Option<Vec<u8>>)],
) -> Result<Box<dyn VtabInstance>> {
    let module = module_of(registry, sql_text)?;
    module
        .connect_from_shadow(name, args, vectors, chunks)
        .map_err(|e| Error::new(ResultCode::Error, e.to_string()))
}

/// The module a stored `CREATE VIRTUAL TABLE` statement names.
///
/// The statement is handed to the engine's **own parser** rather than being
/// scanned by hand. That is the point: this crate already has a grammar that
/// produces `Stmt::CreateVirtualTable { name, module, args, .. }`, and a
/// second, hand-written reading of the same SQL is a second thing to get wrong.
/// The scan that was here first did get it wrong -- it found the `USING` inside
/// `distance_metric=USING` for an argument list that used that as a value, and
/// then took an offset from the uppercased text against the original and
/// skipped past the `(`.
///
/// The parser is also *more* correct than the scan would have been. `USING`
/// needs no uppercase copy here because `keyword_eq!` folds case itself, so a
/// module name spelled in any case comes back as written and
/// [`VtabRegistry::lookup`] lowercases it -- the same comparison the engine
/// does everywhere else.
fn module_of(
    registry: &VtabRegistry,
    sql_text: &str,
) -> Result<std::rc::Rc<dyn crate::vtab::VtabModule>> {
    let module = using_module(sql_text).ok_or_else(|| {
        Error::new(
            ResultCode::Error,
            format!("not a CREATE VIRTUAL TABLE statement: {sql_text:?}"),
        )
    })?;
    registry
        .lookup(&module)
        .ok_or_else(|| crate::msg::no_such_module(&module))
}

/// The module name in a `CREATE VIRTUAL TABLE ... USING <module>(...)`, if the
/// text is that kind of statement.
///
/// Returns `None` for anything else. A schema row whose text is some other kind
/// of statement is not a fault to guess at: a reopened database may hold rows
/// this engine does not rebuild, and `load_schema` already skips those -- so the
/// caller gets "not a CREATE VIRTUAL TABLE" and can say so.
pub fn using_module(sql_text: &str) -> Option<String> {
    match crate::parser::parse_one(sql_text) {
        Ok(crate::parser::Stmt::CreateVirtualTable { module, .. }) => Some(module),
        _ => None,
    }
}

/// The parenthesised argument list out of a `CREATE VIRTUAL TABLE` statement.
///
/// **Verbatim, parentheses included**, because that is what
/// `Stmt::CreateVirtualTable` stores and what the module's own `Schema::parse`
/// expects: it strips the parentheses itself, so stripping them here would make
/// it refuse a list it should have accepted.
///
/// It comes from the same parse as [`using_module`] -- one statement read
/// once, by the grammar that already defines it. A hand-written scan for the
/// matching `(` and `)` was here first and is exactly the sort of thing the
/// parser exists to replace: it has to guess at quoting, at nesting, and at
/// where the list ends, and all three are decisions this crate has already
/// made once.
pub fn virtual_table_args(sql_text: &str) -> Option<String> {
    match crate::parser::parse_one(sql_text) {
        Ok(crate::parser::Stmt::CreateVirtualTable { args, .. }) => Some(args),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_module_name_and_the_argument_list_are_read_by_the_parsers_own_grammar() {
        let sql = "CREATE VIRTUAL TABLE v USING vec0(a float[3], distance_metric=L2)";
        assert_eq!(using_module(sql).as_deref(), Some("vec0"));
        assert_eq!(
            virtual_table_args(sql).as_deref(),
            Some("(a float[3], distance_metric=L2)")
        );

        // Case-insensitive, because `USING` is a keyword and the module name is
        // matched case-insensitively too.
        let lower = "create virtual table v using VEC0(a float[3], distance_metric=L2)";
        assert_eq!(using_module(lower).as_deref(), Some("VEC0"));
        assert_eq!(
            virtual_table_args(lower).as_deref(),
            Some("(a float[3], distance_metric=L2)")
        );

        // `IF NOT EXISTS` is NOT supported for `CREATE VIRTUAL TABLE` by this
        // engine's parser, and the test below documents that rather than
        // pretending otherwise.
        //
        // MEASURED on the reference (sqlite3 3.53.4), which does support it:
        //
        //     $ sqlite3 :memory: "CREATE VIRTUAL TABLE IF NOT EXISTS g USING rtree(...);"
        //     $ sqlite3 in.db "SELECT sql FROM sqlite_schema WHERE name='g';"
        //     CREATE VIRTUAL TABLE g USING rtree(id,minx,maxx,miny,maxy)
        //
        // Two things follow. The clause is accepted by the reference, so this
        // engine's parser has a gap. And the reference **strips it from the
        // stored text**, so a reopen never sees it -- which is why this read
        // path does not have to cope with the clause at all, and why adding
        // support for it later belongs in the parser rather than here.
        assert_eq!(
            using_module("CREATE VIRTUAL TABLE IF NOT EXISTS v USING vec0(a float[3])"),
            None,
            "this engine's parser does not accept IF NOT EXISTS here; if that changes, \\
             the reopen path is unaffected because the reference strips it before storing"
        );

        // Not a virtual table statement at all.
        assert_eq!(using_module("CREATE TABLE v(a)"), None);
        assert_eq!(virtual_table_args("CREATE TABLE v(a)"), None);
    }

    /// The failure the hand-written scan had. It found the `USING` inside an
    /// argument *value*, and then indexed the original text with an offset taken
    /// from the uppercased copy, so it skipped past the opening paren and
    /// reported the whole statement as not being a virtual table.
    #[test]
    fn using_inside_an_argument_value_is_not_the_clause() {
        let sql = "CREATE VIRTUAL TABLE v USING vec0(a float[3], default USING)";
        assert_eq!(using_module(sql).as_deref(), Some("vec0"));
        assert_eq!(
            virtual_table_args(sql).as_deref(),
            Some("(a float[3], default USING)")
        );
    }

    /// A parenthesised value inside the argument list must not truncate it. The
    /// list runs to the closing paren that matches the opening one, which is
    /// what the parser's span capture gives.
    #[test]
    fn a_parenthesis_inside_the_argument_list_does_not_truncate_it() {
        let sql = "CREATE VIRTUAL TABLE v USING vec0(a float[3], b text default 'x(y', distance_metric=L2)";
        assert_eq!(
            virtual_table_args(sql).as_deref(),
            Some("(a float[3], b text default 'x(y', distance_metric=L2)")
        );
    }
}
