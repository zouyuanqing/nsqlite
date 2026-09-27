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
use crate::msg::Msg;
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
    /// Where each source's columns begin in a bound row, and how many there are.
    ///
    /// A bound row is every source's columns concatenated, so finding a source's
    /// slice means knowing where the ones before it ended. Summing the widths on
    /// each lookup turns every column read into a walk of the sources, and a
    /// cross product reads every column of every pair, so the offsets are
    /// worked out once here instead.
    layout: Vec<(usize, usize)>,
}

impl From {
    /// Where one source's columns sit in a bound row, and how many there are.
    ///
    /// The second element is the width, so a caller that only wants the end does
    /// not have to add the two up again.
    pub fn slice_bounds(&self, index: usize) -> (usize, usize) {
        self.layout[index]
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
            let on_left = sources[..i].iter().rev().find(|p| has_column(p, name));
            let on_right = has_column(s, name);
            if on_left.is_none() || !on_right {
                return Err(Error::new(
                    ResultCode::Error,
                    format!("cannot join using column {name} - column not present in both tables"),
                ));
            }
        }
    }
    let mut star = Vec::new();
    // The star is built left to right, and a column is skipped when the source
    // holding it declares a USING and an earlier source already printed the same
    // column. `a JOIN b USING (x)` therefore prints `a`'s `x` and drops `b`'s,
    // which is the one place the two copies meet. A three-way chain naming the
    // same column twice still prints it once, from the leftmost source, because
    // every source after the first that holds it has an earlier holder.
    for (i, s) in sources.iter().enumerate() {
        for (j, c) in s.table.columns.iter().enumerate() {
            let coalesced = s.using.iter().any(|u| u.eq_ignore_ascii_case(&c.name))
                && sources[..i].iter().any(|p| has_column(p, &c.name));
            if coalesced {
                continue;
            }
            star.push((c.name.clone(), i, j));
        }
    }
    let mut layout: Vec<(usize, usize)> = Vec::with_capacity(sources.len());
    let mut at = 0usize;
    for s in &sources {
        let width = s.table.len();
        layout.push((at, width));
        at += width;
    }
    Ok(From {
        sources,
        star,
        layout,
    })
}

/// Whether a source has a column by that name.
pub fn source_has_column(s: &Source, name: &str) -> bool {
    s.table
        .columns
        .iter()
        .any(|c| c.name.eq_ignore_ascii_case(name))
}

/// Whether a source has a column by that name.
fn has_column(s: &Source, name: &str) -> bool {
    source_has_column(s, name)
}

/// Whether a USING clause anywhere in the FROM names this column.
///
/// A column a USING names is shared by the two sides of its join, so a bare
/// name for it means the value they coalesce to rather than one side's copy,
/// and a star prints it from whichever side has a value on the row.
pub fn is_using_column(from: &From, name: &str) -> bool {
    from.sources
        .iter()
        .any(|s| s.using.iter().any(|u| u.eq_ignore_ascii_case(name)))
}

/// Builds the sources for a FROM list, looking each table up by name.
///
/// A NATURAL join's column list is derived here rather than in the parser,
/// because the parser does not read the catalog and the shared columns are a
/// fact about the two tables. The result is the same list a USING clause would
/// have named, so everything downstream — the equality test, the coalescing, the
/// star — is shared with USING and needs to know nothing about NATURAL.
pub fn sources_from(tables: &[Table], from: &[FromItem]) -> Result<Vec<Source>> {
    let mut out: Vec<Source> = Vec::with_capacity(from.len());
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
            .or_else(|| {
                // `main.t` and `main.T` both name `t`: the qualifier is checked
                // without regard to case, and so is the table name after it.
                // sqlite3 3.53.4 answers `SELECT * FROM main.q` with the rows
                // of `q`, and `MAIN.Q` and `main.Q` with the same. `temp` is
                // accepted the same way, for the reason `is_schema_table`
                // accepts it: this engine keeps its temp objects in the main
                // catalog, so there is no second schema to miss. Any OTHER
                // qualifier is not a match and falls through to "no such
                // table", which is what sqlite3 does for `nosuchdb.q`.
                let base = strip_schema_qualifier(&tref.name)?;
                tables.iter().find(|t| t.name.eq_ignore_ascii_case(base))
            })
            .cloned()
            .ok_or_else(|| {
                Error::new(
                    ResultCode::Error,
                    format!("no such table: {}", display_table_name(tref)),
                )
            })?;
        // The shared columns are the ones the two tables both have, matched
        // without regard to case, taken in the left table's column order. The
        // left is everything joined before this one, not just the table written
        // immediately before it, which is what makes a NATURAL join in a chain
        // compare against the whole left, as SQLite does.
        let using = if tref.natural {
            let left: Vec<&Source> = out.iter().collect();
            shared_columns(&left, &table)
        } else {
            tref.using.clone()
        };
        out.push(Source {
            name: Source::scope_name(tref).to_ascii_lowercase(),
            table,
            join: tref.join,
            on: tref.on.clone(),
            using,
        });
    }
    Ok(out)
}

/// The columns the two sides of a NATURAL join have in common.
///
/// Everything joined so far is the left side, because a NATURAL join's columns
/// are the ones shared with the whole left rather than with one table. The left
/// table's column order decides the order of the result, and a column the right
/// side does not have is not shared.
///
/// Two tables with no column in common give an empty list, and an empty list is
/// no constraint, which is a cross product. That is what sqlite3 does: `p(a)`
/// and `q(b)` NATURAL joined over two rows each give the four pairs, the same as
/// `p CROSS JOIN q`.
fn shared_columns(left: &[&Source], right: &Table) -> Vec<String> {
    // A column is shared when the left side has it somewhere and the right has
    // it. The left is scanned in FROM order and the first holder decides, so a
    // three-way chain does not name the same column twice.
    let mut out: Vec<String> = Vec::new();
    for s in left {
        for c in &s.table.columns {
            if out.iter().any(|n| n.eq_ignore_ascii_case(&c.name)) {
                continue;
            }
            if right
                .columns
                .iter()
                .any(|r| r.name.eq_ignore_ascii_case(&c.name))
            {
                out.push(c.name.clone());
            }
        }
    }
    out
}

/// The table name as SQLite writes it in a "no such table" message: the name
/// exactly as it was written, schema qualifier and all.
///
/// The qualifier is NOT stripped. sqlite3 3.53.4 answers
/// `SELECT * FROM nosuchdb.t1` with `no such table: nosuchdb.t1` and
/// `SELECT * FROM t1.t2` with `no such table: t1.t2` -- the whole name, because
/// the qualifier is part of what was looked up and part of what was not found.
/// Dropping it would report `no such table: t1` for a name the user never
/// wrote, which is a message the suite does not contain.
///
/// The parser folds unquoted names to lower case before this is reached, so a
/// name written unquoted comes back lowercased, which is what SQLite does too.
fn display_table_name(tref: &TableRef) -> String {
    tref.name.clone()
}

/// The table name with a `main` or `temp` schema qualifier removed, or `None`
/// when the name carries no such qualifier or carries a different one.
///
/// The qualifier is matched without regard to case, and only the LAST dot
/// separates: `main.a.b` is a table `b` in the schema `main.a`, which is not a
/// schema this engine has, so it falls through to "no such table" rather than
/// matching a table called `a.b`.
pub fn strip_schema_qualifier(name: &str) -> Option<&str> {
    let (schema, base) = name.rsplit_once('.')?;
    (schema.eq_ignore_ascii_case("main") || schema.eq_ignore_ascii_case("temp")).then_some(base)
}

/// A name in a statement, resolved to the place in the joined row it reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ref {
    /// A column of one source, by its index within that source.
    Column { source: usize, column: usize },
    /// A column several sources share because a USING clause named it, which
    /// stands for one value.
    ///
    /// The value is the first non-NULL among the sources that have the column,
    /// left to right, skipping a source that contributed no row. Which source
    /// that lands on depends on the row, so it cannot be resolved once up front
    /// the way a plain column can: a RIGHT or FULL join may preserve a right row
    /// and leave the left all NULL, and SQLite then reads the column from the
    /// right. The list is the sources that hold the column, in FROM order; the
    /// name is carried so the walk can find the column within each one.
    Coalesced { holders: Vec<usize>, name: String },
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
    // A column a USING clause names stands for one value across the sources that
    // hold it, so a bare name has to mean that value rather than one side's
    // copy. The right-hand copies are therefore not candidates on their own,
    // and what is left decides between a single holder and an ambiguity.
    //
    // Whether a USING names the column at all is a property of the FROM, not of
    // the row, so it is decided here. Which of the holders supplies the value
    // is a property of the row, and is left to `read`.
    let coalesced = table.is_none() && is_using_column(from, name) && matches.len() > 1;
    if coalesced {
        return Ok(Ref::Coalesced {
            holders: matches.iter().map(|(i, _)| *i).collect(),
            name: name.to_string(),
        });
    }
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
    let candidates = if live.is_empty() {
        matches.as_slice()
    } else {
        live.as_slice()
    };

    match candidates.len() {
        1 => Ok(Ref::Column {
            source: candidates[0].0,
            column: candidates[0].1,
        }),
        0 => Err(Unresolved::new(match table {
            // A three-part reference names the column first: `main.T.x` is the
            // column `x` of table `T` in schema `main`, and it has to be echoed
            // in that order. The schema is the part a query cannot spell any
            // other way -- it is a keyword -- so it needs no case handling.
            Some(t) => match t.split_once('.') {
                Some((schema, table)) => Msg::NoSuchColumnSchemaQualified.render(&[
                    schema.into(),
                    table.into(),
                    name.into(),
                ]),
                None => Msg::NoSuchColumnQualified.render(&[t.into(), name.into()]),
            },
            None => Msg::NoSuchColumn.render(&[name.into()]),
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
            Err(Unresolved::new(format!("ambiguous column name: {shown}")))
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
        let (start, len) = from.slice_bounds(index);
        &self.values[start..start + len]
    }

    /// Whether this source contributed a row, which is what a `None` rowid means.
    pub fn has_row(&self, index: usize) -> bool {
        self.rowids.get(index).copied().flatten().is_some()
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
            let (start, _) = from.slice_bounds(i);
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
/// is what happens when it fails: an outer join keeps the row it preserved the
/// side of, and for every other kind it is dropped. A join with no constraint
/// at all keeps every pair, which is the cross product — including a bare
/// `JOIN b` and a `CROSS JOIN b ON ...`, which SQLite executes as an inner
/// join, so the constraint is still what decides.
///
/// # The outer joins
///
/// LEFT keeps a left row that matched nothing. RIGHT is the same preservation
/// with the sides swapped, and it cannot be expressed in this left-major loop
/// directly: a right row that matched nothing is found only after every left
/// row has been tried against it. So a RIGHT fold tracks which right rows
/// matched, emits the matched pairs in left-major order as the loop runs, and
/// then appends each unmatched right row with a NULL left side. FULL is a LEFT
/// fold that additionally appends the unmatched right rows, so it emits the
/// matched pairs, then the unmatched left ones, then the unmatched right ones —
/// the order SQLite produces for all three.
///
/// The order of those three groups is not an accident: it is the order the
/// `l FULL JOIN r` probe above returned for every fixture, and a query that
/// counts rows does not notice it, so the tests here pin it explicitly.
///
/// An outer join whose right side is empty has no unmatched right row to
/// recover, so a RIGHT join over one yields nothing rather than every left row,
/// which is what SQLite does and what a naive swap of the preservation test
/// would get wrong.
pub fn nested_loop(
    from: &From,
    source_rows: &[Vec<crate::table_tree::Row>],
    on_exprs: &[Option<Constraint>],
    params: &[Value],
) -> Result<Vec<JoinedRow>> {
    let widths: Vec<usize> = from.sources.iter().map(|s| s.table.len()).collect();
    let total: usize = widths.iter().sum();
    let mut rows: Vec<JoinedRow> = Vec::new();

    // The first source has no left side, so its rows are the starting set.
    // A rowid alias is written back as each row is placed rather than after the
    // loop, because a later join's constraint may read the alias and has to see
    // the key rather than the NULL that stands in for it on disk.
    if let Some(first) = source_rows.first() {
        for r in first {
            let mut jr = JoinedRow {
                values: r.values.clone(),
                rowids: vec![Some(r.rowid)],
            };
            jr.values.resize(total, Value::Null);
            jr.recover_rowids(from);
            rows.push(jr);
        }
    }

    for i in 1..from.sources.len() {
        let start: usize = widths[..i].iter().sum();
        let width = widths[i];
        let right = source_rows.get(i).map(|v| v.as_slice()).unwrap_or(&[]);
        let on = on_exprs.get(i - 1).and_then(|o| o.as_ref());
        // An outer join that preserves the right side cannot decide that from
        // inside the left loop, so it remembers which right rows matched and
        // emits the leftovers once the loop is done. The two vectors are only
        // allocated for the joins that need them, which is every join but the
        // inner one -- the common case pays nothing.
        let kind = from.sources[i].join;
        let keeps_left = matches!(kind, Some(JoinKind::Left) | Some(JoinKind::Full));
        let keeps_right = matches!(kind, Some(JoinKind::Right) | Some(JoinKind::Full));
        let mut matched_right = if keeps_right {
            vec![false; right.len()]
        } else {
            Vec::new()
        };
        let mut next: Vec<JoinedRow> = Vec::new();
        // The rows whose right side was preserved are held back until after the
        // left loop, so a FULL join can put the unmatched left rows ahead of the
        // unmatched right ones. Nothing else separates them, so the common path
        // does not pay for a second vector.
        let mut unmatched_left: Vec<JoinedRow> = Vec::new();
        for left in &rows {
            let mut matched = false;
            for (j, r) in right.iter().enumerate() {
                let mut jr = left.clone();
                // The right row overwrites its own slice and leaves the left
                // side alone, which is what a nested loop does.
                for k in 0..width {
                    jr.values[start + k] = r.values.get(k).cloned().unwrap_or(Value::Null);
                }
                jr.rowids.resize(i + 1, None);
                jr.rowids[i] = Some(r.rowid);
                jr.recover_rowids(from);
                if constraint_holds(from, i, &jr, on, params)? {
                    matched = true;
                    if keeps_right {
                        matched_right[j] = true;
                    }
                    next.push(jr);
                }
            }
            // An outer join keeps the left row when nothing matched, with the
            // right side NULL. `None` for the operator means a comma, which is
            // a cross join and drops the row.
            if !matched && keeps_left {
                let mut jr = left.clone();
                jr.values.resize(total, Value::Null);
                for k in 0..width {
                    jr.values[start + k] = Value::Null;
                }
                jr.rowids.resize(i + 1, None);
                jr.rowids[i] = None;
                if keeps_right {
                    unmatched_left.push(jr);
                } else {
                    next.push(jr);
                }
            }
        }
        if keeps_right {
            next.append(&mut unmatched_left);
            for (j, r) in right.iter().enumerate() {
                if matched_right[j] {
                    continue;
                }
                // A right row that matched nothing is emitted against an
                // all-NULL left side. The left slice is zeroed rather than
                // resized, so the widths of the sources already laid down stay
                // where they are.
                let mut jr = JoinedRow {
                    values: vec![Value::Null; total],
                    rowids: vec![None; i + 1],
                };
                for k in 0..width {
                    jr.values[start + k] = r.values.get(k).cloned().unwrap_or(Value::Null);
                }
                jr.rowids[i] = Some(r.rowid);
                jr.recover_rowids(from);
                next.push(jr);
            }
        }
        rows = next;
    }
    Ok(rows)
}

/// A join's ON constraint, with the column references in it already resolved.
///
/// The references are bound per join rather than shared with the statement,
/// because a constraint is evaluated against a row that is still being built:
/// the tables to the right of that join are not in it yet, so a name there
/// would resolve to something the statement's own bindings never saw.
#[derive(Debug, Clone)]
pub struct Constraint {
    pub expr: Expr,
    pub bound: Vec<Bound>,
}

/// Binds the ON expression of each join, alongside the references the statement
/// itself contains.
///
/// A join's ON is resolved against the same FROM as everything else, and it is
/// resolved before the loop runs so a name in it that does not resolve is an
/// error even when no pair would have been tested.
pub fn bind_constraints(from: &From) -> Result<Vec<Option<Constraint>>> {
    let mut out = Vec::with_capacity(from.sources.len());
    for s in from.sources.iter().skip(1) {
        match &s.on {
            Some(on) => {
                // A name in an ON clause is always a table column, so it is
                // resolved with no output aliases in scope.
                let bound = bind_all(from, &[on], &[])?;
                out.push(Some(Constraint {
                    expr: on.clone(),
                    bound,
                }));
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
        // The holders are in FROM order, and the first that has a row supplies
        // the value. A source that contributed no row is a placeholder an outer
        // join left NULL, so it is skipped; a real NULL in a real row is a
        // value and stops the walk, which is what makes `a.k` NULL with `b.k`
        // NULL print NULL rather than reaching past a.
        Ref::Coalesced { holders, name } => {
            for i in holders {
                if !row.has_row(*i) {
                    continue;
                }
                if let Some(v) = lookup_in(row, from, *i, name) {
                    return v;
                }
            }
            Value::Null
        }
    }
}

/// Checks that every column a star expands to resolves.
///
/// The star is resolved against the schema rather than against the names the
/// query wrote, so an ambiguity here is reported as `main.alias.column` — the
/// form SQLite uses when the expansion is what collided, as opposed to a name
/// the statement typed. A table aliased twice is caught here: both copies
/// answer to the same name, so the first column either of them has is
/// ambiguous.
pub fn check_star(from: &From) -> Result<()> {
    for (name, i, _) in &from.star {
        let src = &from.sources[*i];
        let same = from.sources.iter().filter(|s| s.name == src.name).count();
        if same > 1 {
            return Err(Error::new(
                ResultCode::Error,
                format!("ambiguous column name: main.{}.{name}", src.name),
            ));
        }
    }
    Ok(())
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
                jr.source_slice(from, i)
                    .get(j)
                    .cloned()
                    .unwrap_or(Value::Null),
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
                out.push(Bound {
                    at: span.start,
                    r#ref: r,
                });
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

/// The schema's spelling of a column of the source `qualifier` names, or
/// `None` when the FROM has no such source or the source has no such column.
///
/// A result column that refers straight at a table column is reported under
/// the name the schema gave it, and the query may have reached that column
/// through an alias -- so the qualifier is resolved against the FROM rather
/// than against the catalog, which holds tables and no aliases. `SELECT p.x
/// FROM a AS p` reports `x`, and it is the source called `p` that holds it.
///
/// An **empty** qualifier is the unqualified reference, which the resolution
/// itself settles: the search is across every source, in FROM order, and the
/// first one holding the column owns it. `SELECT BB FROM Users` reports `Bb`
/// for the same reason `SELECT p.x` reports `x` -- the name is the schema's,
/// whichever way the statement wrote it.
pub fn schema_column_name(from: &From, qualifier: &str, column: &str) -> Option<String> {
    let base = strip_schema_qualifier(qualifier).unwrap_or(qualifier);
    let src = from
        .sources
        .iter()
        .find(|s| base.is_empty() || s.name.eq_ignore_ascii_case(base))?;
    src.table
        .columns
        .iter()
        .find(|c| c.name.eq_ignore_ascii_case(column))
        .map(|c| c.name.clone())
}

/// The name the schema holds for the table a source reads, or `None` when the
/// FROM has no source by that name.
///
/// A result column is reported under the *table's* name when `full_column_names`
/// is on, and a query may have reached the table through an alias -- so the
/// table is named by asking the source the alias resolved to, not by reading
/// the alias itself. `SELECT p.x FROM a AS p` is `a.x` under `full` and `p.x`
/// with `short_column_names` off, and the difference is exactly this.
///
/// An **empty** qualifier is the unqualified reference, and it resolves to the
/// first source holding the column -- the same source the reference itself
/// resolved to, which is what keeps `full_column_names` from naming a table
/// the reference never read.
pub fn source_table_name(from: &From, qualifier: &str) -> Option<String> {
    let base = strip_schema_qualifier(qualifier).unwrap_or(qualifier);
    from.sources
        .iter()
        .find(|s| base.is_empty() || s.name.eq_ignore_ascii_case(base))
        .map(|s| s.table.name.clone())
}

/// Binds a set of expressions that have already been resolved.
///
/// [`bind_all`] re-checks every reference, which is what a statement's own
/// terms want: an unknown name is reported before a row is read. A caller
/// holding terms that came out of `orderby::resolve_keys` does not want that
/// twice -- the names in those terms have already been resolved against the
/// FROM and the result list, and the aliases they may stand for are not in the
/// FROM at all, so the second check reads them as missing columns. `SELECT b AS
/// bb FROM t ORDER BY BB` is that case: the key the statement wrote names the
/// alias, and the key that comes back names the table's `b`.
pub fn bind_all_lenient(from: &From, exprs: &[&Expr]) -> Result<Vec<Bound>> {
    let mut out = Vec::new();
    for e in exprs {
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
        match resolve_ref(from, table.as_deref(), &name, false) {
            Ok(_) => continue,
            Err(e) => {
                // An alias may stand in for a name that is not a table column,
                // but only when the name is absent. A name that is ambiguous is
                // an error even when the result happens to carry that name:
                // `SELECT x FROM a, b` projects one `x`, and reading it would
                // have to pick a side, which is the ambiguity being reported.
                // A qualified name is never an alias: `t.c` always names a
                // table's column.
                if table.is_none()
                    && e.message == format!("no such column: {name}")
                    && aliases.iter().any(|a| a.eq_ignore_ascii_case(&name))
                {
                    continue;
                }
                return Err(unresolved_error(e));
            }
        }
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
    on: Option<&Constraint>,
    params: &[Value],
) -> Result<bool> {
    let src = &from.sources[index];
    if let Some(Constraint { expr: on, bound }) = on {
        let resolved = resolved_values(row, from, bound);
        // Every column reference in an ON clause was resolved when the statement
        // was bound, so `eval` reads them out of `resolved` and never consults
        // the named row. Materialising that row here would allocate a Vec of
        // (String, Value) for the whole joined row on every candidate pair, and
        // a cross product tests every pair, so the fallback stays empty. The one
        // thing a name still needs is the context, which is the right table's.
        let ctx = crate::eval::EvalCtx {
            params,
            row: Vec::new(),
            columns: &[],
            context: Some(src.name.clone()),
            resolved,
            ..crate::eval::EvalCtx::default()
        };
        return Ok(crate::eval::truthy(crate::eval::eval(on, &ctx)?));
    }
    if src.using.is_empty() {
        return Ok(true);
    }
    // A USING column was checked to exist on both sides when the FROM was
    // resolved, so only the values are compared here.
    //
    // The left operand is the *coalesced* value of that column over the join
    // tree to the left, which is not simply the immediately preceding source.
    // Two cases pin the difference, both checked against sqlite3:
    //
    //   `a LEFT JOIN b USING(k) JOIN c USING(k)` with a.k=2, b unmatched and
    //   c.k=2 keeps the row. The preceding source is b, whose side is all NULL,
    //   so reading b.k would reject the pair and lose the row. SQLite pairs on
    //   a's k, because a LEFT join preserves the left and the left is where the
    //   coalesced column comes from.
    //
    //   `a RIGHT JOIN b USING(k) JOIN c USING(k)` with b.k=5, a unmatched and
    //   c.k=5 also keeps the row, and EXPLAIN shows the c-join comparing
    //   `cursor 1 column 0` — b's. A RIGHT join preserves the right, so the
    //   coalesced value of the tree is the right side's, and the left's NULL
    //   does not win.
    //
    // Both fall out of one rule: read the column from every source that holds
    // it, from the leftmost up to the one before this join, and take the first
    // non-NULL. A source whose side was preserved as NULL contributes nothing,
    // which is exactly what makes the two cases differ, and a genuine NULL in a
    // real row then falls through to the right, which is also what SQLite's
    // output column does.
    for name in &src.using {
        let lv = coalesced_in(row, from, index, name);
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

/// The coalesced value of a USING column across everything joined so far.
///
/// Every source before `index` that has the column is read, left to right, and
/// the first non-NULL is the value. `None` means no source before the join has
/// the column, which `resolve` already rejects, so it does not occur in a query
/// that runs.
fn coalesced_in(row: &JoinedRow, from: &From, index: usize, name: &str) -> Option<Value> {
    for p in 0..index {
        if let Some(v) = lookup_in(row, from, p, name) {
            if !v.is_null() {
                return Some(v);
            }
        }
    }
    // Every holder read NULL, or none held the column at all. The value the
    // coalesced column shows is then NULL rather than the first NULL seen, so
    // a caller that needs "was it ever non-NULL" can tell the two apart.
    if (0..index).any(|p| has_column(&from.sources[p], name)) {
        Some(Value::Null)
    } else {
        None
    }
}

/// The value a coalesced USING column shows for a whole joined row.
///
/// The column is named by several sources and printed once. The value is the
/// first non-NULL among them, left to right, which is what SQLite prints for
/// every case the engine supports:
///
///   `a LEFT JOIN b USING(k)` shows a's k, and a row b did not match still
///   shows it, because a is the left and a LEFT join preserves it.
///
///   `a RIGHT JOIN b USING(k)` shows b's k for a row only b matched, because
///   a RIGHT join preserves the right and the left is all NULL there.
///
/// A source that contributed no row is skipped for the same reason its NULLs
/// are: it is a placeholder, not a value. A real NULL in a real row does not
/// get skipped, so `a.k` NULL with `b.k` NULL shows NULL, which is what sqlite3
/// prints for a USING join where both sides are NULL.
pub fn coalesced_column(row: &JoinedRow, from: &From, name: &str) -> Value {
    // A source with no row is skipped whatever the column holds, so the walk
    // goes over the holders and the value is the first real one.
    for i in 0..from.sources.len() {
        if !row.has_row(i) {
            continue;
        }
        if let Some(v) = lookup_in(row, from, i, name) {
            return v;
        }
    }
    Value::Null
}
