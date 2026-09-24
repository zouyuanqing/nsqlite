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
}

/// What a join produced, ready for the WHERE clause and the projection.
pub struct JoinOutput {
    /// Every row that survived the join, left-major.
    pub rows: Vec<JoinedRow>,
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
pub fn nested_loop<F>(
    from: &From,
    source_rows: &[Vec<crate::table_tree::Row>],
    mut eval_row: F,
) -> Result<Vec<JoinedRow>>
where
    F: FnMut(&JoinedRow) -> Result<bool>,
{
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
                if constraint_holds(from, i, &jr, &mut eval_row)? {
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

/// Whether a candidate pair satisfies the join's own constraint.
///
/// A join with no constraint keeps every pair. A USING constraint is an equality
/// on each named column, and unlike an ON expression it is a test the planner
/// applies itself: the columns must exist on both sides and must be equal, with
/// the same NULL rules as `=`.
fn constraint_holds<F>(
    from: &From,
    index: usize,
    row: &JoinedRow,
    eval_row: &mut F,
) -> Result<bool>
where
    F: FnMut(&JoinedRow) -> Result<bool>,
{
    let src = &from.sources[index];
    if let Some(on) = &src.on {
        return eval_row(row);
    }
    if src.using.is_empty() {
        return Ok(true);
    }
    // A USING column has to be on both sides. This is checked when the FROM is
    // resolved, where the error names the column, so reaching here means the
    // column exists on both sides and only the values are compared.
    let left_width: usize = from.sources[..index].iter().map(|s| s.table.len()).sum();
    for name in &src.using {
        let lv = lookup_in(row, from, index - 1, name);
        let rv = lookup_in(row, from, index, name);
        // The equality is the one an `=` would do, so NULL never matches NULL.
        let (Some(lv), Some(rv)) = (lv, rv) else {
            return Ok(true);
        };
        if lv.is_null() || rv.is_null() || !lv.eq_value(rv) {
            return Ok(false);
        }
    }
    let _ = left_width;
    Ok(true)
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
