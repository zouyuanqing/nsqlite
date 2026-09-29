//! Binding every column reference in a statement before the statement runs.
//!
//! SQLite resolves a query's names once, against the schema, before it reads a
//! single row. That ordering is observable, and it is the whole point of this
//! module: `SELECT nosuchcol FROM t` is an error even when `t` is empty and even
//! when a WHERE would have matched nothing, because the name is rejected during
//! resolution rather than while a value is being read.
//!
//! Resolving per row instead produces a wrong answer rather than a wrong error,
//! which is the worst shape a gap can have: a query whose FROM is empty, or whose
//! WHERE keeps no row, never evaluates the projection at all, so a name that
//! would have failed simply becomes a column name in the result header and the
//! statement *succeeds*. The executor did that, and it is what
//! [`crate::connection`] is told to stop doing.
//!
//! # What resolves against what
//!
//! A reference has three shapes, and they resolve differently:
//!
//! * `t.c` names a column of the source spelled `t`, and of nothing else. Once
//!   `u AS t` is in scope the table `t` itself has no name, so `t.c` reads
//!   `u.c` and `u.c` is what fails. A qualifier that matches no source at all
//!   gives `no such column: t.c`, which is the same message as a column the
//!   source does not have -- SQLite does not distinguish "no such table" from
//!   "no such column" on the left of a dot.
//! * A bare `c` names a column of whichever sources in scope have it. Exactly
//!   one is a resolution; more than one is `ambiguous column name: c`.
//! * A bare name that no source has is an error, unless it is a result column
//!   alias, which is the one place a name is allowed not to be a column.
//!
//! # Aliases, and which clause they are legal in
//!
//! A result column alias is spelled `AS`, and only `AS`: the output name a
//! column gets for free -- `SELECT a+b` is called `a+b` -- is not an alias and
//! cannot be referred to. Only an explicit one is, and which clause accepts it
//! is not uniform:
//!
//! | clause   | a real column of the same name | otherwise |
//! |----------|---------------------------------|-----------|
//! | ORDER BY | the alias wins                   | the alias |
//! | WHERE    | the column wins                 | the alias is substituted |
//! | GROUP BY | the column wins                 | the alias |
//! | HAVING   | the column wins                 | the alias |
//! | LIMIT/OFFSET | neither: both are out of scope | the error |
//! | ON       | the column wins                 | the alias is substituted |
//!
//! The WHERE and ON rows are the surprising ones, and they are not a mistake in
//! this table. SQLite really does substitute a result alias into a WHERE term,
//! so `SELECT 1 AS z FROM t WHERE z=1` keeps every row, and into an ON one for
//! the same reason. It is a documented consequence of the historical rule that
//! those clauses cannot see the result columns, worked around by textual
//! substitution -- which is why it is a *fallback* and not a resolution: a real
//! column of the same name is read from the table first, and only a name the
//! tables do not have is substituted. It also does not rescue an unknown name
//! that is not an alias, because there is nothing to substitute:
//! `SELECT 1 AS z FROM t WHERE nosuch IS NULL` is `no such column: nosuch`.
//!
//! An alias is never reached through a qualifier. `SELECT a AS x ... WHERE t.x`
//! is `no such column: t.x`, because `t.x` is a column of `t` and `t` has no
//! column `x`, and no alias is a column of any table.
//!
//! An ambiguous name is an error even where an alias of that name exists,
//! except in ORDER BY, where the alias wins outright. So
//! `SELECT 5 AS a FROM t, u WHERE a=5` is `ambiguous column name: a` while
//! `SELECT 5 AS a FROM t, u ORDER BY a` is not an error at all. An alias cannot
//! resolve an ambiguity, because picking it would be picking a side, which is
//! the thing the error is reporting.
//!
//! # The order the clauses are checked in
//!
//! SQLite does not check the clauses in the order they are written, and a
//! statement with two bad names fails on whichever one is checked first, so the
//! order is observable. It was measured against sqlite3 3.53.4 over every pair
//! of clauses with a bad name in each, and it is not the order this file's
//! callers are likely to guess. From first to last:
//!
//! 1. LIMIT, then OFFSET. Both read the *outer* query, so neither sees the
//!    tables of this one at all: `SELECT 1 FROM t LIMIT a` is `no such column: a`
//!    even though `a` is a column of `t`. An alias is not visible here either.
//! 2. The projection. A bad result column outranks a bad name in every other
//!    clause, the HAVING included: `SELECT nosuchcol FROM t HAVING 1` reports
//!    the column, not the HAVING.
//! 3. HAVING, but only once the query is an aggregate query.
//! 4. WHERE.
//! 5. The ON constraints of the joins, in FROM order.
//! 6. ORDER BY.
//! 7. GROUP BY, which is the last to be reached.
//!
//! Two of those deserve their reasoning. LIMIT and OFFSET come first because
//! they are resolved before the SELECT is even walked -- they name something
//! outside it. GROUP BY comes last because SQLite resolves the rest of the
//! statement into a list of result columns and resolves the GROUP BY against
//! that, after everything else has been looked at.

use crate::error::{Error, Result, ResultCode};
use crate::join::{self, From};
use crate::parser::Expr;

/// The clauses of a SELECT, and the rule each one applies to a bare name that
/// no source in scope has.
///
/// The distinction is only reached for a name that is not a column anywhere, so
/// it is about what may stand in for one rather than about which column a name
/// is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// WHERE, and the ON of a join. A result alias may be substituted.
    Filter,
    /// GROUP BY. A real column wins; an alias stands in only when there is none.
    Group,
    /// HAVING. Same rule as GROUP BY.
    Having,
    /// ORDER BY. The alias wins outright, even over a real column of that name.
    Order,
    /// LIMIT and OFFSET. No alias, and no table either: these are resolved
    /// against the outer query.
    Outer,
    /// The projection, and anywhere else with no alias to fall back on.
    Default,
}

impl Scope {
    /// Whether an ambiguous name is fatal here.
    ///
    /// It is everywhere except ORDER BY, where the alias decides before the
    /// ambiguity is ever reached.
    pub fn ambiguity_is_error(self) -> bool {
        !matches!(self, Scope::Order)
    }

    /// Whether a result alias may stand in for a name no source has.
    ///
    /// It may in the clauses that read the projected row -- ORDER BY, GROUP BY
    /// and HAVING -- and in WHERE and ON, where SQLite substitutes it. It may
    /// not in the projection itself, and not in LIMIT and OFFSET:
    /// `SELECT 1 AS x ... LIMIT x` is `no such column: x`, because a SELECT
    /// cannot see its own output while it is still writing it.
    pub fn alias_is_fallback(self) -> bool {
        matches!(
            self,
            Scope::Filter | Scope::Group | Scope::Having | Scope::Order
        )
    }
}

/// What the resolution of a whole SELECT produced.
///
/// A statement that binds successfully never fails at this stage again, which
/// is what lets the executor stop reporting "no such column" per row.
#[derive(Debug, Clone, Default)]
pub struct Resolved {
    /// Nothing to report. Every reference in the statement resolved.
    pub checked: usize,
}

/// The aliases a projection declares, as (name, expression) in written order.
///
/// Only an explicit `AS` is an alias. The output name a column gets for free is
/// deliberately absent, because `SELECT a+b` is called `a+b` and that name is
/// not a column of anything; treating the rendered expression as an alias would
/// quietly accept a reference SQLite rejects.
pub fn aliases(columns: &[crate::parser::ResultColumn]) -> Vec<(String, Expr)> {
    columns
        .iter()
        .filter_map(|rc| rc.alias.as_ref().map(|a| (a.clone(), rc.expr.clone())))
        .collect()
}

/// The spelling the schema holds for a column of `table`, or `None` when no
/// such table or column is in the catalog.
///
/// A result column that is a direct reference is reported under the schema's
/// name, not the statement's: `SELECT Bb FROM Users` names the column `Bb`
/// whichever way the query wrote it, because the report is a description of
/// the schema rather than a quotation of the query. It is also what makes
/// `SELECT b AS bb FROM t ORDER BY BB` work -- the projection is matched
/// against the reported names -- so the two spellings have to agree.
///
/// The result-column name is not a message, so the case rule does not apply to
/// it; `no such column: Bb` and the reported column name `Bb` are the same
/// text for different reasons. See `connection::column_name_with`.
pub fn schema_column_name(
    catalog: &crate::catalog::Catalog,
    qualifier: &str,
    column: &str,
) -> Option<String> {
    // The qualifier is whatever the query called the source, so it is an alias
    // more often than not. The catalog holds tables, and there is no place in
    // it that records which alias a query gave one, so an alias is looked up
    // the only way it can be: through the FROM the caller has already
    // resolved, and failing that, through the table whose name the qualifier
    // is. A caller that has the FROM should resolve through it -- see
    // `join::From` - and this is the fallback for a caller that does not, which
    // is right whenever the query gave no alias.
    let base = crate::join::strip_schema_qualifier(qualifier).unwrap_or(qualifier);
    let t = catalog.get(base)?;
    t.columns
        .iter()
        .find(|c| c.name.eq_ignore_ascii_case(column))
        .map(|c| c.name.clone())
}

/// Every clause of a SELECT, as the resolver needs to see them.
///
/// The clauses are one value rather than six arguments because each carries both
/// an expression and the [`Scope`] that says how a name in it is allowed to fall
/// back to a result alias, and pairing those two at the call site is where they
/// get crossed.
#[derive(Debug, Clone, Copy, Default)]
pub struct Clauses<'a> {
    pub where_: Option<&'a Expr>,
    pub group_by: &'a [Expr],
    pub having: Option<&'a Expr>,
    pub order_by: &'a [(Expr, bool)],
    pub limit: Option<&'a Expr>,
    pub offset: Option<&'a Expr>,
}

/// Checks every column reference in a SELECT against its FROM, before any row
/// is read.
///
/// This is the pass whose absence produced the suite's worst gap. The executor
/// used to resolve each reference while evaluating it, against a row, so a
/// statement that produced no rows -- an empty table, or a WHERE that kept
/// nothing -- never reached the evaluation that would have raised the error, and
/// the unresolved identifier was reported as a result column instead.
///
/// A statement with several bad names fails on the same one SQLite fails on,
/// which is the one the order documented on this module measures: LIMIT and
/// OFFSET first, then the projection, then the HAVING -- and, on a query with no
/// aggregate and no GROUP BY, the refusal of the HAVING itself, which sits in
/// exactly that slot -- then WHERE, then the ON constraints, then ORDER BY, then
/// GROUP BY.
///
/// # Why the HAVING is refused here rather than by the caller
///
/// SQLite raises `HAVING clause on a non-aggregate query` *before* it resolves
/// any name in the HAVING, and it raises it *after* binding the projection but
/// *before* the WHERE. So the check is not a guard the executor can run
/// beforehand: on `SELECT nosuchcol FROM t HAVING 1` the projection's bad column
/// wins, and on `SELECT 1 FROM t WHERE nosuchcol HAVING 1` the HAVING's refusal
/// wins. Both are only reachable from inside the ordered walk, which is why
/// this function owns the check rather than taking a flag saying whether the
/// query is an aggregate query.
///
/// The test for "is an aggregate query" is the one `grouping::Plan::build` uses
/// and the one the module docs there give the reason for: a HAVING does not make
/// a query an aggregate query, so `SELECT 1 FROM t HAVING count(*)>0` is still
/// refused. Only the result columns and a GROUP BY can.
pub fn check_select(
    from: &From,
    columns: &[crate::parser::ResultColumn],
    clauses: Clauses<'_>,
) -> Result<Resolved> {
    let aliases = aliases(columns);
    let mut count = 0usize;

    // LIMIT and OFFSET name something outside this SELECT, so they are checked
    // against no source at all: every name in them is unknown here, whatever the
    // FROM holds. They come first because SQLite resolves them first.
    if let Some(e) = clauses.limit {
        check_outer(e)?;
        count += count_refs(e);
    }
    if let Some(e) = clauses.offset {
        check_outer(e)?;
        count += count_refs(e);
    }

    // The projection next. A bad result column outranks a bad name anywhere
    // else, the HAVING included, because SQLite binds the output list before it
    // looks at the clauses that read it. A star was already expanded by the
    // caller, so there is nothing to check for it.
    for rc in columns {
        check_expr(from, &rc.expr, &aliases, Scope::Default)?;
        count += count_refs(&rc.expr);
    }

    // The HAVING, and the refusal of it. A query that is neither grouped nor
    // aggregated has its HAVING refused here, before the WHERE, and its names
    // are never looked at. A query that is one resolves them like any other
    // clause.
    if clauses.having.is_some() {
        if clauses.group_by.is_empty() && !columns.iter().any(|rc| folds(&rc.expr)) {
            return Err(Error::new(
                ResultCode::Error,
                "HAVING clause on a non-aggregate query",
            ));
        }
        let h = clauses.having.expect("just tested for one");
        check_expr(from, h, &aliases, Scope::Having)?;
        count += count_refs(h);
    }

    if let Some(pred) = clauses.where_ {
        check_expr(from, pred, &aliases, Scope::Filter)?;
        count += count_refs(pred);
    }

    // The ON constraints of the joins, in FROM order, so the first bad join
    // clause is the one reported. An ON sees the whole FROM -- the tables to
    // its left are in scope for it -- and an alias of the result may stand in
    // for a name no table has, exactly as in a WHERE.
    for s in from.sources.iter().skip(1) {
        if let Some(on) = &s.on {
            check_expr(from, on, &aliases, Scope::Filter)?;
            count += count_refs(on);
        }
    }

    for (e, _) in clauses.order_by {
        check_expr(from, e, &aliases, Scope::Order)?;
        count += count_refs(e);
    }
    for e in clauses.group_by {
        check_expr(from, e, &aliases, Scope::Group)?;
        count += count_refs(e);
    }

    Ok(Resolved { checked: count })
}

/// Checks every column reference in a SELECT, reading the clauses out of the
/// statement itself.
///
/// This is the call the executor makes, and the reason it exists is that the
/// clauses of a `Select` are spread across a destructuring match the caller
/// would otherwise have to repeat: `where_` is bound as `&Option<Expr>` by
/// `select`'s own destructure while [`Clauses`] wants `Option<&Expr>`, so
/// spelling the call at the call site means writing `where_.as_ref()` for three
/// clauses and getting the borrow wrong in a way the compiler reports as a type
/// mismatch rather than as a missing pass. Taking the `Select` makes the hook
/// one line that cannot be mistyped.
///
/// A compound or nested body has no FROM to resolve against and no clauses of
/// this shape; the executor refuses those before it gets here, so the body is
/// taken as simple and anything else resolves nothing rather than panicking.
pub fn check_statement(from: &From, sel: &crate::parser::Select) -> Result<Resolved> {
    let crate::parser::SelectBody::Simple {
        columns,
        where_,
        group_by,
        having,
        ..
    } = &sel.body
    else {
        return Ok(Resolved::default());
    };
    check_select(
        from,
        columns,
        Clauses {
            where_: where_.as_ref(),
            group_by,
            having: having.as_ref(),
            order_by: &sel.order_by,
            limit: sel.limit.as_ref(),
            offset: sel.offset.as_ref(),
        },
    )
}

/// Whether a result column mentions an aggregate, which is half of what makes a
/// query an aggregate query. The other half is a GROUP BY, which the caller
/// tests itself.
///
/// This asks `grouping` which names fold rather than keeping its own list, so
/// the two cannot drift apart: a name `grouping` learns to fold is one this
/// accepts here, and one it does not is one this does not.
fn folds(expr: &Expr) -> bool {
    if crate::grouping::Aggregate::of(expr).is_some() {
        return true;
    }
    children_of(expr).into_iter().any(folds)
}

/// The sub-expressions of an expression, in the shape the parser nests them.
fn children_of(expr: &Expr) -> Vec<&Expr> {
    use Expr::*;
    match expr {
        Unary { expr, .. } | IsNull { expr, .. } | Collate { expr, .. } | Cast { expr, .. } => {
            vec![expr]
        }
        Binary { left, right, .. } => vec![left, right],
        Between {
            expr, low, high, ..
        } => vec![expr, low, high],
        InList { expr, list, .. } => {
            let mut v = vec![&**expr];
            v.extend(list.iter());
            v
        }
        Like {
            expr,
            pattern,
            escape,
            ..
        } => {
            let mut v = vec![&**expr, &**pattern];
            v.extend(escape.as_deref());
            v
        }
        Function { args, .. } => args.iter().collect(),
        Case {
            operand,
            whens,
            otherwise,
        } => {
            let mut v: Vec<&Expr> = operand.iter().map(|b| &**b).collect();
            for (w, t) in whens {
                v.push(w);
                v.push(t);
            }
            v.extend(otherwise.iter().map(|b| &**b));
            v
        }
        // A subquery has its own aggregate scope; a literal and a parameter
        // have neither.
        Literal(_) | NamedParameter(..) | InSelect { .. } | Exists { .. } | Subquery { .. } => {
            Vec::new()
        }
        // A bare column never folds, but naming it keeps this match exhaustive
        // over `Expr` without a catch-all that would hide a new variant.
        Column { .. } => Vec::new(),
    }
}

/// Checks one expression tree, reporting the first reference that does not
/// resolve.
///
/// The walk is exhaustive over the tree rather than stopping at the leaves that
/// look like names, because a reference hides at any depth: `f(CASE WHEN x
/// THEN y END)` has two, and only the first is ever evaluated when the first
/// row happens to take the other branch.
fn check_expr(from: &From, expr: &Expr, aliases: &[(String, Expr)], scope: Scope) -> Result<()> {
    match expr {
        Expr::Column { table, name, .. } => check_ref(from, table.as_deref(), name, aliases, scope),
        Expr::Unary { expr, .. }
        | Expr::IsNull { expr, .. }
        | Expr::Collate { expr, .. }
        | Expr::Cast { expr, .. } => check_expr(from, expr, aliases, scope),
        Expr::Binary { left, right, .. } => {
            check_expr(from, left, aliases, scope)?;
            check_expr(from, right, aliases, scope)
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            check_expr(from, expr, aliases, scope)?;
            check_expr(from, low, aliases, scope)?;
            check_expr(from, high, aliases, scope)
        }
        Expr::InList { expr, list, .. } => {
            check_expr(from, expr, aliases, scope)?;
            for item in list {
                check_expr(from, item, aliases, scope)?;
            }
            Ok(())
        }
        Expr::Like {
            expr,
            pattern,
            escape,
            ..
        } => {
            check_expr(from, expr, aliases, scope)?;
            check_expr(from, pattern, aliases, scope)?;
            if let Some(e) = escape {
                check_expr(from, e, aliases, scope)?;
            }
            Ok(())
        }
        // A star names every column of one source or of all of them, so it
        // resolves by construction. It is not a function call and its name is
        // not a column name: `count(*)` folds the rows, and `t.*` is every
        // column of `t`. Both carry no reference to check.
        Expr::Function { star, .. } if *star => Ok(()),
        Expr::Function { args, .. } => {
            for a in args {
                check_expr(from, a, aliases, scope)?;
            }
            Ok(())
        }
        Expr::Case {
            operand,
            whens,
            otherwise,
        } => {
            if let Some(o) = operand {
                check_expr(from, o, aliases, scope)?;
            }
            for (w, t) in whens {
                check_expr(from, w, aliases, scope)?;
                check_expr(from, t, aliases, scope)?;
            }
            match otherwise {
                Some(o) => check_expr(from, o, aliases, scope),
                None => Ok(()),
            }
        }
        // A subquery has its own FROM and resolves there, so its references are
        // not this statement's. A literal and a parameter hold no name.
        Expr::Literal(_)
        | Expr::NamedParameter(..)
        | Expr::InSelect { .. }
        | Expr::Exists { .. }
        | Expr::Subquery { .. } => Ok(()),
    }
}

/// Checks one column reference against the FROM.
///
/// A qualified name is decided on its own: it is a column of the source it
/// spells or it is nothing, and no alias can stand in for it. An unqualified
/// name is the one that consults the aliases, and it consults them only after
/// the sources have failed to produce exactly one answer.
fn check_ref(
    from: &From,
    table: Option<&str>,
    name: &str,
    aliases: &[(String, Expr)],
    scope: Scope,
) -> Result<()> {
    match join::resolve_ref(from, table, name, false) {
        Ok(_) => Ok(()),
        Err(e) => {
            // A qualifier is never an alias, so this is the whole story for a
            // qualified name: `t.x` reads the column `x` of `t`.
            if table.is_some() {
                return Err(join::unresolved_error(e));
            }
            // An ambiguity is a fact about the sources, and no alias resolves
            // one. ORDER BY is the single clause where the alias is consulted
            // first and so never reaches here.
            if e.message == format!("ambiguous column name: {name}") && scope.ambiguity_is_error() {
                return Err(join::unresolved_error(e));
            }
            if scope.alias_is_fallback()
                && aliases.iter().any(|(a, _)| a.eq_ignore_ascii_case(name))
            {
                return Ok(());
            }
            Err(join::unresolved_error(e))
        }
    }
}

/// Checks a LIMIT or an OFFSET, which are resolved against the outer query.
///
/// Neither sees the tables of the SELECT that carries them and neither sees its
/// result aliases, so every name in either is unknown. `SELECT 1 FROM t LIMIT a`
/// is `no such column: a` even though `a` is a column of `t`, which is the
/// whole reason this is a separate pass rather than a [`Scope`].
fn check_outer(expr: &Expr) -> Result<()> {
    // An empty FROM is what an outer query looks like from in here: a SELECT
    // with no sources evaluates against an empty row, so a bare name in it
    // resolves to nothing and is an error. Building it through `join::resolve`
    // rather than writing the struct out is what keeps this module from having
    // to know the layout `From` carries internally, which is join's business.
    let outer = join::resolve(Vec::new()).expect("an empty FROM always resolves");
    check_expr(&outer, expr, &[], Scope::Outer)
}

/// How many column references a tree holds, for the count a caller reports.
fn count_refs(expr: &Expr) -> usize {
    match expr {
        Expr::Column { .. } => 1,
        Expr::Unary { expr, .. }
        | Expr::IsNull { expr, .. }
        | Expr::Collate { expr, .. }
        | Expr::Cast { expr, .. } => count_refs(expr),
        Expr::Binary { left, right, .. } => count_refs(left) + count_refs(right),
        Expr::Between {
            expr, low, high, ..
        } => count_refs(expr) + count_refs(low) + count_refs(high),
        Expr::InList { expr, list, .. } => {
            count_refs(expr) + list.iter().map(count_refs).sum::<usize>()
        }
        Expr::Like {
            expr,
            pattern,
            escape,
            ..
        } => {
            count_refs(expr)
                + count_refs(pattern)
                + escape.as_ref().map(|e| count_refs(e)).unwrap_or(0)
        }
        Expr::Function { star, args, .. } => {
            if *star {
                0
            } else {
                args.iter().map(count_refs).sum()
            }
        }
        Expr::Case {
            operand,
            whens,
            otherwise,
        } => {
            operand.as_ref().map(|o| count_refs(o)).unwrap_or(0)
                + whens
                    .iter()
                    .map(|(w, t)| count_refs(w) + count_refs(t))
                    .sum::<usize>()
                + otherwise.as_ref().map(|o| count_refs(o)).unwrap_or(0)
        }
        Expr::Literal(_)
        | Expr::NamedParameter(..)
        | Expr::InSelect { .. }
        | Expr::Exists { .. }
        | Expr::Subquery { .. } => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::Table;
    use crate::join::Source;
    use crate::parser::{parse_one, ResultColumn, Stmt};

    /// A catalog over the fixture the oracle was asked about:
    /// `t(a,b)`, `u(a,c)`, and an empty `e(x)`.
    fn tables() -> Vec<Table> {
        let col = |n: &str| crate::catalog::Column {
            name: n.to_string(),
            declared_type: String::new(),
            affinity: crate::affinity::Affinity::Blob,
            not_null: false,
            default: None,
            rowid_alias: false,
        };
        let make = |name: &str, cols: &[&str]| Table {
            name: name.to_string(),
            columns: cols.iter().map(|c| col(c)).collect(),
            rowid_alias: None,
            unique_sets: Vec::new(),
            without_rowid: false,
            root_page: 0,
        // An ordinary table: no module hosts it.
        virtual_module: None,
        };
        vec![
            make("t", &["a", "b"]),
            make("u", &["a", "c"]),
            make("e", &["x"]),
        ]
    }

    /// The FROM of a statement, taken from the statement itself so a test
    /// cannot disagree with the SQL about which tables are in scope.
    fn from_of(sql: &str) -> From {
        let Stmt::Select(s) = parse_one(sql).expect("parses") else {
            panic!("not a select")
        };
        let crate::parser::SelectBody::Simple { from, .. } = &s.body else {
            panic!("not a simple select")
        };
        let items: Vec<Source> = from
            .iter()
            .map(|item| match item {
                crate::parser::FromItem::Table(tref) => Source {
                    name: Source::scope_name(tref).to_string(),
                    table: table_named(&tref.name),
                    // The join kind, the ON and the USING all belong to the
                    // source, and the resolver reads the ON out of them, so a
                    // helper that dropped them would quietly skip every ON
                    // clause and pass a test that the real executor would fail.
                    join: tref.join,
                    on: tref.on.clone(),
                    using: tref.using.clone(),
                },
                _ => panic!("subquery in FROM"),
            })
            .collect();
        join::resolve(items).expect("resolves")
    }

    /// One of the fixture tables, by name.
    fn table_named(name: &str) -> Table {
        tables()
            .into_iter()
            .find(|t| t.name.eq_ignore_ascii_case(name))
            .expect("fixture table")
    }

    fn columns(sql: &str) -> Vec<ResultColumn> {
        let Stmt::Select(s) = parse_one(sql).expect("parses") else {
            panic!("not a select")
        };
        match s.body {
            crate::parser::SelectBody::Simple { columns, .. } => columns,
            other => panic!("not a simple select: {other:?}"),
        }
    }

    /// Runs a whole SELECT through `check_select` and returns the message, or
    /// `Ok(())`. This is the check the executor makes, and every expectation
    /// below is the string `sqlite3` 3.53.4 printed for the same statement.
    fn check(sql: &str) -> std::result::Result<(), String> {
        let cols = columns(sql);
        let f = from_of(sql);
        let stmt = match parse_one(sql).expect("parses") {
            Stmt::Select(s) => s,
            other => panic!("not a select: {other:?}"),
        };
        let (where_, group_by, having) = match &stmt.body {
            crate::parser::SelectBody::Simple {
                where_,
                group_by,
                having,
                ..
            } => (where_.clone(), group_by.clone(), having.clone()),
            other => panic!("not a simple select: {other:?}"),
        };
        check_select(
            &f,
            &cols,
            Clauses {
                where_: where_.as_ref(),
                group_by: &group_by,
                having: having.as_ref(),
                order_by: &stmt.order_by,
                limit: stmt.limit.as_ref(),
                offset: stmt.offset.as_ref(),
            },
        )
        .map(|_| ())
        .map_err(|e| e.message)
    }

    // --- the gap this module closes -------------------------------------

    /// The statement docs/testing.md section 5.3 item 4 reports. Over a table
    /// with rows the old executor happened to raise this while evaluating the
    /// first row, so it looked right; the resolution is what makes it right, and
    /// these two show the difference.
    #[test]
    fn unknown_bare_column_is_rejected_whatever_the_table_holds() {
        assert_eq!(
            check("SELECT nosuchcol FROM t;"),
            Err("no such column: nosuchcol".into())
        );
        // An empty table. The old executor read no rows, so the projection never
        // ran and the name was reported as a result column instead.
        assert_eq!(
            check("SELECT nosuchcol FROM e;"),
            Err("no such column: nosuchcol".into())
        );
        // A WHERE that keeps nothing, on a table that has rows.
        assert_eq!(
            check("SELECT nosuchcol FROM t WHERE 1=0;"),
            Err("no such column: nosuchcol".into())
        );
    }

    #[test]
    fn a_qualified_unknown_names_the_qualifier() {
        assert_eq!(
            check("SELECT t.nosuchcol FROM t;"),
            Err("no such column: t.nosuchcol".into())
        );
        assert_eq!(
            check("SELECT t.nosuchcol;"),
            Err("no such column: t.nosuchcol".into())
        );
        assert_eq!(
            check("SELECT nosuchtbl.a FROM t;"),
            Err("no such column: nosuchtbl.a".into())
        );
    }

    /// An alias replaces the table's own name, so a qualified reference reads
    /// the alias's table and the original name stops resolving.
    #[test]
    fn an_alias_shadows_the_table_name() {
        // `t` here is `u`. `u` has an `a` and a `c`, so those resolve -- and
        // they read `u`'s values, which is what makes the shadowing observable
        // rather than merely legal.
        assert_eq!(check("SELECT t.a FROM u AS t;"), Ok(()));
        assert_eq!(check("SELECT t.c FROM u AS t;"), Ok(()));
        // `u` has no `b`, and `t`'s own `b` is out of scope.
        assert_eq!(
            check("SELECT t.b FROM u AS t;"),
            Err("no such column: t.b".into())
        );
        // The table's own name is gone, so `u` is now a name nothing answers to.
        assert_eq!(
            check("SELECT u.a FROM u AS t;"),
            Err("no such column: u.a".into())
        );
    }

    /// The message is `ambiguous column name: <name>` and nothing else. The task
    /// description says sqlite3 names both tables in it; it does not. Measured
    /// against 3.53.4, `SELECT a FROM alpha, beta` and `SELECT a FROM t, u,
    /// e` are both the bare `ambiguous column name: a`, so a caller matching the
    /// text verbatim cannot afford to add them.
    #[test]
    fn an_ambiguous_name_says_ambiguous_and_only_that() {
        assert_eq!(
            check("SELECT a FROM t, u;"),
            Err("ambiguous column name: a".into())
        );
        assert_eq!(
            check("SELECT a FROM t JOIN u;"),
            Err("ambiguous column name: a".into())
        );
        assert_eq!(
            check("SELECT a FROM t CROSS JOIN u;"),
            Err("ambiguous column name: a".into())
        );
        // The ambiguity is a property of the FROM, not of the projection, so it
        // is reported wherever the name is written.
        assert_eq!(
            check("SELECT 1 FROM t, u WHERE a=1;"),
            Err("ambiguous column name: a".into())
        );
        assert_eq!(
            check("SELECT count(a) FROM t, u;"),
            Err("ambiguous column name: a".into())
        );
        // A name only one side has is not ambiguous.
        assert_eq!(check("SELECT b FROM t, u;"), Ok(()));
        // A qualified reference picks its side, so it is not ambiguous even
        // though the bare name is.
        assert_eq!(check("SELECT t.a, u.a FROM t, u;"), Ok(()));
    }

    // --- aliases --------------------------------------------------------

    #[test]
    fn an_alias_may_be_referenced_in_order_by_group_by_and_having() {
        assert_eq!(check("SELECT a AS x FROM t ORDER BY x;"), Ok(()));
        assert_eq!(check("SELECT a AS x FROM t GROUP BY x;"), Ok(()));
        assert_eq!(
            check("SELECT a AS x, count(*) FROM t GROUP BY a HAVING x>0;"),
            Ok(())
        );
    }

    /// In the projection a result alias is not in scope, which is the one place
    /// a SELECT of its own result column fails.
    #[test]
    fn an_alias_is_not_in_scope_in_the_projection() {
        assert_eq!(
            check("SELECT a AS x, x FROM t;"),
            Err("no such column: x".into())
        );
        // Nor when the WHERE is statically false: resolution precedes execution.
        assert_eq!(
            check("SELECT a AS x, x FROM t WHERE 1=0;"),
            Err("no such column: x".into())
        );
        // A name that is neither a column nor an alias is still unknown.
        assert_eq!(check("SELECT x FROM t;"), Err("no such column: x".into()));
    }

    /// LIMIT and OFFSET read the outer query, not this one, so neither the
    /// tables nor the aliases of this SELECT are visible in them. `SELECT 1 FROM
    /// t LIMIT a` is an error even though `a` is a column of `t`.
    #[test]
    fn limit_and_offset_resolve_against_the_outer_query() {
        assert_eq!(
            check("SELECT 1 FROM t LIMIT a;"),
            Err("no such column: a".into())
        );
        assert_eq!(
            check("SELECT 1 FROM t LIMIT t.a;"),
            Err("no such column: t.a".into())
        );
        assert_eq!(
            check("SELECT 1 FROM t LIMIT 1 OFFSET a;"),
            Err("no such column: a".into())
        );
        // A star is not a name, so it is not an error.
        assert_eq!(check("SELECT 1 FROM t LIMIT count(*);"), Ok(()));
    }

    /// An explicit alias is no more visible in a LIMIT than any other name.
    #[test]
    fn an_alias_is_not_in_scope_in_limit_or_offset() {
        assert_eq!(
            check("SELECT 1 AS x FROM t LIMIT x;"),
            Err("no such column: x".into())
        );
        assert_eq!(
            check("SELECT 1 AS x FROM t LIMIT 1 OFFSET x;"),
            Err("no such column: x".into())
        );
    }

    /// Only an explicit `AS` is an alias. The output name a column gets for
    /// free is not one, so it cannot be referred to.
    #[test]
    fn only_an_explicit_as_is_an_alias() {
        assert_eq!(check("SELECT 1 AS x FROM t;"), Ok(()));
        assert_eq!(check("SELECT 1 FROM t;"), Ok(()));
        // `SELECT a+b` is called `a+b`, and that name is not a column of
        // anything, so the check rejects it. sqlite3 rejects it too, with a
        // longer message because a double-quoted name that resolves to nothing
        // is ambiguous between an identifier and a string literal; that wording
        // is raised when the name is lexed, and this module only sees the name
        // itself, which is the part it is responsible for. The divergence is
        // asserted in the integration test, which reaches the real engine.
        assert_eq!(
            check("SELECT 1 FROM t ORDER BY \"a+b\";"),
            Err("no such column: a+b".into())
        );
    }

    /// WHERE is the clause with the substitution rule, and it is a fallback:
    /// a real column of the same name is read from the table first.
    #[test]
    fn where_prefers_a_real_column_and_substitutes_an_alias_otherwise() {
        // `b` is a real column, so `b` is read as `b` even though the projection
        // calls something `b`.
        assert_eq!(check("SELECT a AS b FROM t WHERE b=1;"), Ok(()));
        // `z` is not a column, so the alias stands in.
        assert_eq!(check("SELECT a AS z FROM t WHERE z=1;"), Ok(()));
        // The substitution only rescues a name that IS an alias. An unknown name
        // has nothing to substitute and is reported.
        assert_eq!(
            check("SELECT a AS z FROM t WHERE nosuch IS NULL;"),
            Err("no such column: nosuch".into())
        );
    }

    /// The ON of a join substitutes an alias by the same rule a WHERE does,
    /// and an ambiguity in one is still an ambiguity.
    #[test]
    fn an_on_constraint_follows_the_where_rule() {
        assert_eq!(check("SELECT 1 AS z FROM t JOIN u ON z=1;"), Ok(()));
        assert_eq!(
            check("SELECT 1 AS z FROM t JOIN u ON nosuch=1;"),
            Err("no such column: nosuch".into())
        );
        // `a` is in both tables, so the alias cannot pick a side.
        assert_eq!(
            check("SELECT 1 AS a FROM t JOIN u ON a=1;"),
            Err("ambiguous column name: a".into())
        );
    }

    /// ORDER BY is the one clause where the alias wins over a real column, so
    /// an ambiguity is not even reached there.
    #[test]
    fn order_by_prefers_the_alias_over_a_column() {
        assert_eq!(check("SELECT 5 AS a FROM t, u ORDER BY a;"), Ok(()));
        // Everywhere else the ambiguity stands, alias or not.
        assert_eq!(
            check("SELECT 5 AS a FROM t, u WHERE a=5;"),
            Err("ambiguous column name: a".into())
        );
        assert_eq!(
            check("SELECT 5 AS a FROM t, u GROUP BY a;"),
            Err("ambiguous column name: a".into())
        );
    }

    #[test]
    fn an_alias_is_never_reached_through_a_qualifier() {
        assert_eq!(
            check("SELECT a AS x FROM t WHERE t.x=1;"),
            Err("no such column: t.x".into())
        );
        assert_eq!(
            check("SELECT a AS x FROM t ORDER BY t.x;"),
            Err("no such column: t.x".into())
        );
    }

    // --- resolution happens once ---------------------------------------

    /// The property that makes the whole pass worth having: the name is rejected
    /// whether or not the query would have produced a row.
    #[test]
    fn a_bad_name_fails_even_when_the_query_matches_no_rows() {
        // An empty table.
        assert_eq!(
            check("SELECT 1 FROM e WHERE nosuchcol=1;"),
            Err("no such column: nosuchcol".into())
        );
        assert_eq!(
            check("SELECT count(*) FROM e WHERE nosuchcol=1;"),
            Err("no such column: nosuchcol".into())
        );
        assert_eq!(
            check("SELECT nosuchcol FROM e, t;"),
            Err("no such column: nosuchcol".into())
        );
        assert_eq!(
            check("SELECT nosuchcol FROM e JOIN t;"),
            Err("no such column: nosuchcol".into())
        );
    }

    // --- the HAVING, and the check that runs before it ------------------

    /// On a query that is neither grouped nor aggregated, SQLite refuses the
    /// HAVING before it resolves any name in it, so the column in it is never
    /// reached. This pass raises that refusal itself, in the position the
    /// oracle puts it, so `SELECT 1 FROM t HAVING nosuchcol>0` is the HAVING's
    /// message and never `no such column: nosuchcol`.
    #[test]
    fn a_non_aggregate_having_is_refused_before_its_names_are_read() {
        assert_eq!(
            check("SELECT 1 FROM t HAVING nosuchcol>0;"),
            Err("HAVING clause on a non-aggregate query".into())
        );
        // The refusal outranks a bad name in the WHERE, because SQLite reaches
        // it first.
        assert_eq!(
            check("SELECT 1 FROM t WHERE nosuchcol HAVING 1;"),
            Err("HAVING clause on a non-aggregate query".into())
        );
        // A HAVING does not make a query an aggregate query, so a HAVING that
        // only names an aggregate is still refused.
        assert_eq!(
            check("SELECT 1 FROM t HAVING count(*)>0;"),
            Err("HAVING clause on a non-aggregate query".into())
        );
    }

    /// The refusal is not the first thing: a bad result column outranks it,
    /// because SQLite binds the output list before it looks at the HAVING.
    #[test]
    fn a_bad_result_column_outranks_the_having_refusal() {
        assert_eq!(
            check("SELECT nosuchcol FROM t HAVING 1;"),
            Err("no such column: nosuchcol".into())
        );
    }

    /// Once the query is an aggregate query the HAVING is resolved like any
    /// other clause, and its names are checked before the WHERE's.
    #[test]
    fn an_aggregate_having_is_resolved() {
        assert_eq!(
            check("SELECT count(*) FROM t HAVING nosuchcol>0;"),
            Err("no such column: nosuchcol".into())
        );
        // A GROUP BY makes a query an aggregate query too, so this HAVING is
        // reached and its name is reported.
        assert_eq!(
            check("SELECT 1 FROM t GROUP BY a HAVING nosuchcol>0;"),
            Err("no such column: nosuchcol".into())
        );
        // A GROUP BY does not have to come first: the HAVING is checked before
        // the GROUP BY, so its bad name is the one reported.
        assert_eq!(
            check("SELECT 1 FROM t GROUP BY nosuchg HAVING nosuchh>0;"),
            Err("no such column: nosuchh".into())
        );
    }

    // --- the order the clauses are checked in ---------------------------

    /// The order is observable, and it is not the order the clauses are written
    /// in. Every pair below was measured against sqlite3 3.53.4 with one bad
    /// name in each of the two clauses named.
    ///
    /// The names are spelled in lower case because the tokenizer folds an
    /// unquoted identifier, so `nosuchh` reaches this pass as `nosuchh` and
    /// `nosuchH` would reach it as `nosuchh` too -- and the message is built
    /// from what reached it. That the oracle echoes the case as *written* is a
    /// separate divergence of the tokenizer, which is not this module's file;
    /// it is noted in the integration test, which reaches the real engine.
    #[test]
    fn clauses_are_checked_in_sqlites_order() {
        // LIMIT and OFFSET are resolved first of all: they name the outer query.
        assert_eq!(
            check("SELECT nosuchcol FROM t LIMIT badl;"),
            Err("no such column: badl".into())
        );
        assert_eq!(
            check("SELECT nosuchcol, count(*) FROM t LIMIT 1 OFFSET badx;"),
            Err("no such column: badx".into())
        );
        assert_eq!(
            check("SELECT badp, count(*) FROM t LIMIT badl;"),
            Err("no such column: badl".into())
        );
        // The projection comes next, ahead of every other clause.
        assert_eq!(
            check("SELECT badp, count(*) FROM t WHERE badw;"),
            Err("no such column: badp".into())
        );
        assert_eq!(
            check("SELECT badp, count(*) FROM t ORDER BY bado;"),
            Err("no such column: badp".into())
        );
        // Then HAVING, ahead of WHERE.
        assert_eq!(
            check("SELECT 1, count(*) FROM t WHERE badw HAVING badh>0;"),
            Err("no such column: badh".into())
        );
        // Then WHERE, ahead of ORDER BY and GROUP BY.
        assert_eq!(
            check("SELECT 1, count(*) FROM t WHERE badw ORDER BY bado;"),
            Err("no such column: badw".into())
        );
        // Then the ON constraints, ahead of ORDER BY and GROUP BY, and checked
        // in FROM order so the first bad join is the one reported.
        assert_eq!(
            check("SELECT 1 FROM t JOIN u ON nosuchon=1 ORDER BY bado;"),
            Err("no such column: nosuchon".into())
        );
        assert_eq!(
            check("SELECT 1, count(*) FROM t JOIN u ON nosuchon=1 WHERE badw;"),
            Err("no such column: badw".into())
        );
        // Then ORDER BY, ahead of GROUP BY, which is last.
        assert_eq!(
            check("SELECT 1 FROM t GROUP BY badg ORDER BY bado;"),
            Err("no such column: bado".into())
        );
    }

    #[test]
    fn every_clause_is_checked() {
        assert_eq!(
            check("SELECT 1 FROM e GROUP BY nosuchcol;"),
            Err("no such column: nosuchcol".into())
        );
        assert_eq!(
            check("SELECT 1, count(*) FROM t HAVING nosuchcol>0;"),
            Err("no such column: nosuchcol".into())
        );
        assert_eq!(
            check("SELECT a FROM t ORDER BY nosuchcol;"),
            Err("no such column: nosuchcol".into())
        );
        assert_eq!(
            check("SELECT 1 FROM t JOIN u ON nosuchcol=1;"),
            Err("no such column: nosuchcol".into())
        );
    }

    /// A star is not a column name, so it resolves by construction and is not
    /// an error -- in the projection, in a function, and qualified.
    #[test]
    fn a_star_is_not_a_name_to_resolve() {
        assert_eq!(check("SELECT * FROM t;"), Ok(()));
        assert_eq!(check("SELECT * FROM e;"), Ok(()));
        assert_eq!(check("SELECT count(*) FROM t;"), Ok(()));
    }

    #[test]
    fn a_valid_query_reports_how_many_names_it_bound() {
        let f = from_of("SELECT a, b FROM t WHERE a>0;");
        let stmt = match parse_one("SELECT a, b FROM t WHERE a>0;").expect("parses") {
            Stmt::Select(s) => s,
            other => panic!("not a select: {other:?}"),
        };
        let (where_, group_by, having) = match &stmt.body {
            crate::parser::SelectBody::Simple {
                where_,
                group_by,
                having,
                ..
            } => (where_.clone(), group_by.clone(), having.clone()),
            other => panic!("not a simple select: {other:?}"),
        };
        let r = check_select(
            &f,
            &columns("SELECT a, b FROM t WHERE a>0;"),
            Clauses {
                where_: where_.as_ref(),
                group_by: &group_by,
                having: having.as_ref(),
                order_by: &[],
                limit: None,
                offset: None,
            },
        )
        .expect("resolves");
        // `a` and `b` in the projection, `a` in the WHERE.
        assert_eq!(r.checked, 3);
    }
}
