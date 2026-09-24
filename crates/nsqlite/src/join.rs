//! Running a FROM clause: name resolution, and the nested-loop join.
//!
//! SQLite resolves a query's names once, before it runs the statement, so an
//! ambiguous or unknown column is an error even when the query would have
//! matched no rows. This module does the same: it reads the catalog once,
//! decides what every name in the statement means, and only then walks the
//! tables. That ordering is the reason a bare `x` in `a JOIN b` fails even when
//! `a` and `b` are empty.
//!
//! The join itself is a nested loop, left row outermost, which is the order
//! SQLite produces without an index and the order a bare table scan produces
//! with one. For each row of everything joined so far, the next table is scanned
//! in full. A LEFT join keeps the left row when the scan found nothing, filling
//! the right side with NULL; every other kind drops it.
//!
//! # What a bound row holds
//!
//! The row an expression sees carries every column of every table in the FROM,
//! each tagged with the name that table is known by in this query. A bare column
//! resolves against the tags: exactly one match resolves, more than one is
//! ambiguous, none is "no such column". A qualified name resolves only against
//! the table it names, and only if that name is the one in scope — once a table
//! has an alias the original name stops resolving, which is what SQLite does.

use crate::catalog::Table;
use crate::error::{Error, Result, ResultCode};
use crate::parser::{Expr, FromItem, JoinKind, TableRef};
use crate::value::Value;

/// One table in a FROM clause, resolved against the catalog.
///
/// The name in scope is the alias where there is one, because an alias replaces
/// the table's own name for the rest of the statement. `name` is kept as well,
/// since the result columns of a star are named after the table's columns and
/// the rowid alias has to be recovered for the table the row came from.
#[derive(Debug, Clone)]
pub struct Source {
    /// The name this table answers to, which is the alias when there is one.
    pub name: String,
    /// The table as the catalog defines it.
    pub table: Table,
    /// How this table joins to everything to its left, and the constraint.
    pub join: Option<JoinKind>,
    pub on: Option<Expr>,
    /// The columns a USING clause named. They must be equal across the join and
    /// the right-hand one is dropped from the output of a star.
    pub using: Vec<String>,
}

impl Source {
    /// The name a qualified reference uses, which is the alias where there is
    /// one. A table that has been aliased answers to nothing else.
    pub fn scope_name(tref: &TableRef) -> &str {
        tref.alias.as_deref().unwrap_or(&tref.name)
    }
}

/// The FROM clause of a statement, resolved and ready to be walked.
#[derive(Debug, Clone)]
pub struct From {
    /// The sources, in the order they were written. The first is the left side
    /// of the join and the rest are folded in from the left.
    pub sources: Vec<Source>,
    /// The columns a star expands to, as (name, source index, column index).
    ///
    /// A star is not simply the concatenation of every table's columns: a USING
    /// column is shared by the two sides of the join and appears once, from the
    /// left side, which is what SQLite prints.
    pub star: Vec<(String, usize, usize)>,
}

impl From {
    /// The number of columns a star expands to.
    pub fn width(&self) -> usize {
        self.star.len()
    }

    /// Every column of every source, in source order, as
    /// (scope name, column name). This is the layout a bound row uses before
    /// the star's USING coalescing is applied.
    pub fn all_columns(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for s in &self.sources {
            for c in &s.table.columns {
                out.push((s.name.clone(), c.name.clone()));
            }
        }
        out
    }
}

/// Resolves a FROM clause into a [`From`], ready to be walked.
///
/// The star is not simply every table's columns concatenated. A column named by
/// a USING clause is shared by the two sides of that join, and SQLite prints it
/// once, taken from the left side. So the star is built left to right and a
/// column already claimed by an earlier source is skipped, which is what makes
/// `a JOIN b USING (x)` expand to `x, a.y, b.y` rather than `x, a.y, x, b.y`.
pub fn resolve(sources: Vec<Source>) -> Result<From> {
    // A USING column has to exist on both sides of the join that names it. This
    // is checked before anything runs, so a missing column is an error even
    // when no row would have matched, which is where SQLite reports it.
    for (i, s) in sources.iter().enumerate() {
        if i == 0 {
            continue;
        }
        for name in &s.using {
            let on_left = sources[..i]
                .iter()
                .rev()
                .find(|p| has_column(p, name));
            let on_right = has_column(s, name);
            if on_left.is_none() || !on_right {
                return Err(Error::new(
                    ResultCode::Error,
                    format!(
                        "cannot join using column {name} - column not present in both tables"
                    ),
                ));
            }
        }
    }
    let mut star = Vec::new();
    // The columns a USING clause has already claimed, in the order they were
    // first seen. A three-way chain naming the same column twice still claims it
    // once, from the leftmost source.
    let mut claimed: Vec<String> = Vec::new();
    for (i, s) in sources.iter().enumerate() {
        for (j, c) in s.table.columns.iter().enumerate() {
            let is_using = s.using.iter().any(|u| u.eq_ignore_ascii_case(&c.name));
            if is_using && claimed.iter().any(|n| n.eq_ignore_ascii_case(&c.name)) {
                continue;
            }
            if is_using {
                claimed.push(c.name.clone());
            }
            star.push((c.name.clone(), i, j));
        }
    }
    Ok(From { sources, star })
}

/// Whether a source has a column by that name.
fn has_column(s: &Source, name: &str) -> bool {
    s.table.columns.iter().any(|c| c.name.eq_ignore_ascii_case(name))
}

/// Builds the sources for a FROM list, looking each table up by name.
pub fn sources_from(tables: &[Table], from: &[FromItem]) -> Result<Vec<Source>> {
    let mut out = Vec::with_capacity(from.len());
    for item in from {
        let FromItem::Table(tref) = item else {
            return Err(Error::new(
                ResultCode::Error,
                "a subquery in FROM is not supported yet",
            ));
        };
        let table = tables
            .iter()
            .find(|t| t.name.eq_ignore_ascii_case(&tref.name))
            .cloned()
            .ok_or_else(|| {
                Error::new(
                    ResultCode::Error,
                    format!("no such table: {}", display_table_name(tref)),
                )
            })?;
        out.push(Source {
            name: Source::scope_name(tref).to_ascii_lowercase(),
            table,
            join: tref.join,
            on: tref.on.clone(),
            using: tref.using.clone(),
        });
    }
    Ok(out)
}

/// The table name as SQLite writes it in a "no such table" message: the name
/// with a schema qualifier stripped down to the table part, lowercased if it
/// was written unquoted. The parser folds unquoted names already, so this only
/// has to drop the schema.
fn display_table_name(tref: &TableRef) -> String {
    match tref.name.rsplit_once('.') {
        Some((_, table)) => table.to_string(),
        None => tref.name.clone(),
    }
}

/// A name in a statement, resolved to the place in the joined row it reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ref {
    /// A column of one source, by its index within that source.
    Column { source: usize, column: usize },
}

/// A resolution failure, carrying SQLite's wording.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unresolved {
    pub message: String,
}

impl Unresolved {
    fn new(message: String) -> Unresolved {
        Unresolved { message }
    }
}

/// Resolves one name against the FROM clause.
///
/// A qualified name reads only the source it names, and only when that name is
/// the one in scope: once a table has an alias, the original name no longer
/// resolves and the qualified form fails like any other unknown name. An
/// unqualified name reads every source that has it, and more than one is
/// ambiguous.
///
/// A column named by USING resolves to the left-hand side's copy, which is the
/// single one the star prints and the one a bare name has to mean for the query
/// to be unambiguous.
///
/// `star` says the name came from expanding a `*` rather than from something
/// the user wrote. It only changes the wording of an ambiguity: a star is
/// resolved against the schema, so it names the table as `main.alias.column`,
/// where a name the user wrote is echoed back exactly as written.
pub fn resolve_ref(
    from: &From,
    table: Option<&str>,
    name: &str,
    star: bool,
) -> std::result::Result<Ref, Unresolved> {
    let matches: Vec<(usize, usize)> = from
        .sources
        .iter()
        .enumerate()
        .filter(|(_, s)| match table {
            Some(t) => s.name.eq_ignore_ascii_case(t),
            None => true,
        })
        .filter_map(|(i, s)| {
            s.table
                .columns
                .iter()
                .position(|c| c.name.eq_ignore_ascii_case(name))
                .map(|j| (i, j))
        })
        .collect();
    // A USING column names two columns but stands for one, so the copies from
    // the right side of the join are not candidates for a bare name.
    let live: Vec<(usize, usize)> = matches
        .iter()
        .copied()
        .filter(|(i, j)| {
            let s = &from.sources[*i];
            !s.using
                .iter()
                .any(|u| u.eq_ignore_ascii_case(&s.table.columns[*j].name))
        })
        .collect();
    let candidates = if live.is_empty() { matches } else { live };

    match candidates.len() {
        1 => Ok(Ref::Column {
            source: candidates[0].0,
            column: candidates[0].1,
        }),
        0 => Err(Unresolved::new(match table {
            Some(t) => format!("no such column: {t}.{name}"),
            None => format!("no such column: {name}"),
        })),
        _ => {
            // More than one source has the column. A star names the first
            // candidate through the schema, the way SQLite resolves one against
            // `sqlite_schema`; a name the user wrote is echoed as written.
            let shown = if star {
                let s = &from.sources[candidates[0].0];
                format!("main.{}.{}", s.name, name)
            } else {
                match table {
                    Some(t) => format!("{t}.{name}"),
                    None => name.to_string(),
                }
            };
            Err(Unresolved::new(format!(
                "ambiguous column name: {shown}"
            )))
        }
    }
}

/// A name that did not resolve, turned into the error the executor reports.
pub fn unresolved_error(e: Unresolved) -> Error {
    Error::new(ResultCode::Error, e.message)
}

/// One row of the join, as the values of every source's columns in order.
///
/// The layout is every source's columns concatenated, which is what a bound row
/// is built from. A source that contributed no row — the right side of a LEFT
/// join with no match — is all NULL.
#[derive(Debug, Clone, Default)]
pub struct JoinedRow {
    /// The values, source by source, each source occupying its own width.
    pub values: Vec<Value>,
    /// The rowid of each source's row, or `None` where there was no row. A
    /// rowid alias column is stored as NULL and recovered from here.
    pub rowids: Vec<Option<i64>>,
}

impl JoinedRow {
    /// The values of one source, by its index.
    pub fn source_slice(&self, from: &From, index: usize) -> &[Value] {
        let start: usize = from.sources[..index]
            .iter()
            .map(|s| s.table.len())
            .sum();
        let len = from.sources[index].table.len();
        &self.values[start..start + len]
    }

    /// Substitutes each source's rowid into its rowid-alias column.
    ///
    /// An INTEGER PRIMARY KEY column is stored as NULL and stands for the row's
    /// key, so the key has to be written back before any expression reads the
    /// column. A source with no row — the right side of a LEFT join that did not
    /// match — keeps the NULL, which is what makes the joined row read as all
    /// NULL on that side.
    pub fn recover_rowids(&mut self, from: &From) {
        for (i, s) in from.sources.iter().enumerate() {
            let Some(alias) = s.table.rowid_alias else {
                continue;
            };
            let start: usize = from.sources[..i].iter().map(|s| s.table.len()).sum();
            if let (Some(Some(rowid)), Some(slot)) =
                (self.rowids.get(i), self.values.get_mut(start + alias))
            {
                *slot = Value::Integer(*rowid);
            }
        }
    }
}

/// The nested-loop join.
///
/// The first source is scanned to make the initial rows, and each later source
/// is folded in: for every row so far, the new table is scanned in full and a
/// row is emitted for each pair that satisfies the constraint. That is the order
/// SQLite produces without an index, and it is what makes the result left-major.
///
/// A constraint is evaluated as a WHERE over the combined row, so an ON
/// expression and a WHERE expression are the same test here. The one difference
/// is what happens when it fails: for a LEFT join the left row survives with a
/// NULL right side, and for every other kind it is dropped. A join with no
/// constraint at all keeps every pair, which is the cross product — including a
/// bare `JOIN b` and a `CROSS JOIN b ON ...`, which SQLite executes as an inner
/// join, so the constraint is still what decides.
pub fn nested_loop(
    from: &From,
    source_rows: &[Vec<crate::table_tree::Row>],
    on_exprs: &[Option<(Expr, Vec<Bound>)>],
    params: &[Value],
) -> Result<Vec<JoinedRow>> {
    let widths: Vec<usize> = from.sources.iter().map(|s| s.table.len()).collect();
    let total: usize = widths.iter().sum();
    let mut rows: Vec<JoinedRow> = Vec::new();

    // The first source has no left side, so its rows are the starting set.
    if let Some(first) = source_rows.first() {
        for r in first {
            let mut jr = JoinedRow {
                values: r.values.clone(),
                rowids: vec![Some(r.rowid)],
            };
            jr.values.resize(total, Value::Null);
            rows.push(jr);
        }
    }

    for i in 1..from.sources.len() {
        let start: usize = widths[..i].iter().sum();
        let width = widths[i];
        let right = source_rows.get(i).map(|v| v.as_slice()).unwrap_or(&[]);
        let on = on_exprs.get(i - 1).and_then(|o| o.as_ref());
        let mut next: Vec<JoinedRow> = Vec::new();
        for left in &rows {
            let mut matched = false;
            for r in right {
                let mut jr = left.clone();
                // The right row overwrites its own slice and leaves the left
                // side alone, which is what a nested loop does.
                for k in 0..width {
                    jr.values[start + k] = r.values.get(k).cloned().unwrap_or(Value::Null);
                }
                jr.rowids.resize(i + 1, None);
                jr.rowids[i] = Some(r.rowid);
                if constraint_holds(from, i, &jr, on, params)? {
                    matched = true;
                    next.push(jr);
                }
            }
            // A LEFT join keeps the left row when nothing matched, with the
            // right side NULL. `None` for the operator means a comma, which is
            // a cross join and drops the row.
            let is_left = matches!(from.sources[i].join, Some(JoinKind::Left));
            if !matched && is_left {
                let mut jr = left.clone();
                jr.values.resize(total, Value::Null);
                for k in 0..width {
                    jr.values[start + k] = Value::Null;
                }
                jr.rowids.resize(i + 1, None);
                jr.rowids[i] = None;
                next.push(jr);
            }
        }
        rows = next;
    }
    Ok(rows)
}

/// Binds the ON expression of each join, alongside the references the statement
/// itself contains.
///
/// A join's ON is resolved against the same FROM as everything else, and it is
/// resolved before the loop runs so a name in it that does not resolve is an
/// error even when no pair would have been tested.
pub fn bind_constraints(from: &From) -> Result<Vec<Option<(Expr, Vec<Bound>)>>> {
    let mut out = Vec::with_capacity(from.sources.len());
    for s in from.sources.iter().skip(1) {
        match &s.on {
            Some(on) => {
                // A name in an ON clause is always a table column, so it is
                // resolved with no output aliases in scope. The references are
                // bound per join rather than shared with the statement, because
                // they are evaluated against a row that is still being built:
                // the tables to the right of this join are not in it yet.
                let bound = bind_all(from, &[on], &[])?;
                out.push(Some((on.clone(), bound)));
            }
            None => out.push(None),
        }
    }
    Ok(out)
}

/// The value a bound reference reads from a joined row.
pub fn read(row: &JoinedRow, from: &From, r: &Ref) -> Value {
    match r {
        Ref::Column { source, column } => row
            .source_slice(from, *source)
            .get(*column)
            .cloned()
            .unwrap_or(Value::Null),
    }
}

/// The bound values of a joined row, as the evaluator wants them: the offset
/// each reference was written at, and the value it reads.
pub fn resolved_values(row: &JoinedRow, from: &From, bound: &[Bound]) -> Vec<(usize, Value)> {
    bound
        .iter()
        .map(|b| (b.at, read(row, from, &b.r#ref)))
        .collect()
}

/// A bound row keyed by name, which is what a name-based lookup falls back to.
///
/// The names are qualified, so `x` resolves against the first source that has
/// it. That fallback is only reached when nothing was pre-resolved, which is the
/// single-table path this executor replaced; the join path pre-resolves
/// everything, so a bare name that is ambiguous never reaches here.
pub fn named_row(from: &From, jr: &JoinedRow) -> Vec<(String, Value)> {
    let mut out = Vec::new();
    for (i, s) in from.sources.iter().enumerate() {
        for (j, c) in s.table.columns.iter().enumerate() {
            out.push((
                c.name.clone(),
                jr.source_slice(from, i).get(j).cloned().unwrap_or(Value::Null),
            ));
        }
    }
    out
}

/// The value of one named column of one source, if that source has it.
fn lookup_in(row: &JoinedRow, from: &From, index: usize, name: &str) -> Option<Value> {
    let src = &from.sources[index];
    let j = src
        .table
        .columns
        .iter()
        .position(|c| c.name.eq_ignore_ascii_case(name))?;
    row.source_slice(from, index).get(j).cloned()
}

/// A column reference, resolved to where it reads from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bound {
    /// The byte offset the reference was written at, which is how the
    /// evaluator finds it again for each row.
    pub at: usize,
    pub r#ref: Ref,
}

/// Resolves every column reference an expression contains.
///
/// The walk is exhaustive over the expression tree, because a name can hide at
/// any depth and an unvisited one would fall through to the bare-name lookup and
/// silently read the wrong table. A subquery or a `CASE` operand is walked the
/// same way, since a column reference inside one resolves against the same FROM.
///
/// A reference to a result column alias is not a table column and is left for
/// the ORDER BY, which is where SQLite resolves an alias; the caller decides
/// which names those are.
pub fn bind_expr(from: &From, expr: &Expr, out: &mut Vec<Bound>) {
    match expr {
        Expr::Column { table, name, span } => {
            // A name that is not a table column is left alone: it may be an
            // output alias, and `check_unresolved` has already decided whether
            // that is allowed.
            if let Ok(r) = resolve_ref(from, table.as_deref(), name, false) {
                out.push(Bound { at: span.start, r#ref: r });
            }
        }
        Expr::Unary { expr, .. }
        | Expr::IsNull { expr, .. }
        | Expr::Collate { expr, .. }
        | Expr::Cast { expr, .. } => bind_expr(from, expr, out),
        Expr::Binary { left, right, .. } => {
            bind_expr(from, left, out);
            bind_expr(from, right, out)
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            bind_expr(from, expr, out);
            bind_expr(from, low, out);
            bind_expr(from, high, out)
        }
        Expr::InList { expr, list, .. } => {
            bind_expr(from, expr, out);
            for item in list {
                bind_expr(from, item, out);
            }
        }
        Expr::Like {
            expr,
            pattern,
            escape,
            ..
        } => {
            bind_expr(from, expr, out);
            bind_expr(from, pattern, out);
            if let Some(e) = escape {
                bind_expr(from, e, out);
            }
        }
        Expr::Function { args, .. } => {
            for a in args {
                bind_expr(from, a, out);
            }
        }
        Expr::Case {
            operand,
            whens,
            otherwise,
        } => {
            if let Some(o) = operand {
                bind_expr(from, o, out);
            }
            for (w, t) in whens {
                bind_expr(from, w, out);
                bind_expr(from, t, out);
            }
            if let Some(o) = otherwise {
                bind_expr(from, o, out);
            }
        }
        // A literal, a parameter, and a subquery hold no column reference. A
        // subquery has its own FROM and would resolve there, but the engine does
        // not execute one yet.
        Expr::Literal(_)
        | Expr::NamedParameter(..)
        | Expr::InSelect { .. }
        | Expr::Exists { .. }
        | Expr::Subquery { .. } => {}
    }
}

/// Resolves every reference in a list of expressions, reporting the first that
/// does not resolve.
///
/// `aliases` names the result columns, which a reference may name instead of a
/// table column. A name in that list is not an error, because the caller reads
/// it from the projected row rather than from the joined one.
pub fn bind_all(from: &From, exprs: &[&Expr], aliases: &[String]) -> Result<Vec<Bound>> {
    let mut out = Vec::new();
    for e in exprs {
        // Every reference is checked first, so an unknown or ambiguous name is
        // reported before any row is read. The check and the collection are two
        // passes over the same tree because the first one has to fail on the
        // first bad name while the second collects all of them.
        check_unresolved(from, e, aliases)?;
        bind_expr(from, e, &mut out);
    }
    Ok(out)
}

/// Reports the first column reference in an expression that resolves to
/// nothing and is not an output alias.
fn check_unresolved(from: &From, expr: &Expr, aliases: &[String]) -> Result<()> {
    let mut refs = Vec::new();
    collect_columns(expr, &mut refs);
    for (table, name) in refs {
        if resolve_ref(from, table.as_deref(), &name, false).is_ok() {
            continue;
        }
        if table.is_none() && aliases.iter().any(|a| a.eq_ignore_ascii_case(&name)) {
            continue;
        }
        let e = resolve_ref(from, table.as_deref(), &name, false).unwrap_err();
        return Err(unresolved_error(e));
    }
    Ok(())
}

/// Every column reference in an expression, with its qualifier.
fn collect_columns(expr: &Expr, out: &mut Vec<(Option<String>, String)>) {
    match expr {
        Expr::Column { table, name, .. } => out.push((table.clone(), name.clone())),
        Expr::Unary { expr, .. }
        | Expr::IsNull { expr, .. }
        | Expr::Collate { expr, .. }
        | Expr::Cast { expr, .. } => collect_columns(expr, out),
        Expr::Binary { left, right, .. } => {
            collect_columns(left, out);
            collect_columns(right, out);
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            collect_columns(expr, out);
            collect_columns(low, out);
            collect_columns(high, out);
        }
        Expr::InList { expr, list, .. } => {
            collect_columns(expr, out);
            for i in list {
                collect_columns(i, out);
            }
        }
        Expr::Like {
            expr,
            pattern,
            escape,
            ..
        } => {
            collect_columns(expr, out);
            collect_columns(pattern, out);
            if let Some(e) = escape {
                collect_columns(e, out);
            }
        }
        Expr::Function { args, .. } => {
            for a in args {
                collect_columns(a, out);
            }
        }
        Expr::Case {
            operand,
            whens,
            otherwise,
        } => {
            if let Some(o) = operand {
                collect_columns(o, out);
            }
            for (w, t) in whens {
                collect_columns(w, out);
                collect_columns(t, out);
            }
            if let Some(o) = otherwise {
                collect_columns(o, out);
            }
        }
        _ => {}
    }
}

/// Whether a candidate pair satisfies the join's own constraint.
///
/// A join with no constraint keeps every pair. A USING constraint is an equality
/// on each named column, applied by the planner rather than evaluated as an
/// expression, and it uses the same NULL rule as `=`: NULL never matches NULL,
/// not even against itself.
///
/// An ON expression is evaluated as a WHERE over the combined row. A constraint
/// that is not true — including one that is NULL — rejects the pair, which for
/// a LEFT join is what leaves the left row unmatched.
fn constraint_holds(
    from: &From,
    index: usize,
    row: &JoinedRow,
    on: Option<&(Expr, Vec<Bound>)>,
    params: &[Value],
) -> Result<bool> {
    let src = &from.sources[index];
    if let Some((on, bound)) = on {
        let resolved = resolved_values(row, from, bound);
        let ctx = crate::eval::EvalCtx {
            params,
            row: named_row(from, row),
            columns: &[],
            context: Some(src.name.clone()),
            resolved,
        };
        return Ok(crate::eval::truthy(crate::eval::eval(on, &ctx)?));
    }
    if src.using.is_empty() {
        return Ok(true);
    }
    // A USING column was checked to exist on both sides when the FROM was
    // resolved, so only the values are compared here.
    for name in &src.using {
        let lv = lookup_in(row, from, index - 1, name);
        let rv = lookup_in(row, from, index, name);
        let (Some(lv), Some(rv)) = (lv, rv) else {
            return Ok(true);
        };
        if lv.is_null() || rv.is_null() || !lv.eq_value(&rv) {
            return Ok(false);
        }
    }
    Ok(true)
}

/// The name-resolution check, exposed for the ON clauses, which resolve against
/// the FROM with no output aliases in scope.
pub fn check_unresolved_pub(from: &From, expr: &Expr, aliases: &[String]) -> Result<()> {
    check_unresolved(from, expr, aliases)
}
