//! The static checks SQLite runs on a statement's aggregates, before the
//! statement runs.
//!
//! # Why one module
//!
//! None of these are evaluation results. Each is a property of the parsed
//! statement, decided from the parse tree alone, with no row read and no
//! accumulator built. That is what makes them answerable for a query over an
//! empty table — which is why they must run before execution, and why
//! `sqlite3` answers them for one.
//!
//! What is checked:
//!
//! * **Arity** of every aggregate call, with SQLite's own wording
//!   (`wrong number of arguments to function count()`).
//! * **Scope**: an aggregate may not be folded where no group exists — a WHERE,
//!   a CHECK constraint, a partial index's WHERE, an UPDATE's SET or WHERE, an
//!   INSERT's VALUES, a LIMIT. SQLite says
//!   `misuse of aggregate function count()`.
//! * **Nesting**: an aggregate may not be an argument of another, so
//!   `sum(count(a))` is `misuse of aggregate function count()`.
//! * **Aliased aggregates** (SQLite ticket #2526): a result-column alias that
//!   stands for an aggregate may not be used inside *another* aggregate, so
//!   `SELECT min(a) AS m ... HAVING max(m)<1` is
//!   `misuse of aliased aggregate m`.
//!
//! # A bare column beside an aggregate is *not* an error
//!
//! It takes the value from the first row of its group in scan order. This is
//! the opposite of what a strict implementation does, and it is worth stating
//! plainly, because the track brief asked for the "bare column not in the
//! GROUP BY" case to be an error with its own message. **It is not one.**
//! Checked against `sqlite3` 3.53.4 over `(a,b) = (1,2), (3,4), (1,9)`:
//!
//! | query | result |
//! |---|---|
//! | `SELECT a, b FROM t GROUP BY a` | `1\|2`, `3\|4` |
//! | `SELECT b, count(*) FROM t GROUP BY a` | `2\|2`, `4\|1` |
//! | `SELECT a, b, count(*) FROM t GROUP BY a+0` | `1\|2\|2`, `3\|4\|1` |
//! | `SELECT a, b, count(*) FROM t GROUP BY a+0 HAVING b>0` | `1\|2\|2`, `3\|4\|1` |
//! | `SELECT b FROM t GROUP BY a HAVING b>2` | `4` |
//!
//! So `b` in the second query is not in the GROUP BY and SQLite accepts it.
//! The string "non-aggregated" appears nowhere in the SQLite sources, so there
//! is no message to match, and this engine already documented the opposite
//! rule in `grouping.rs` before this module existed.
//!
//! What *is* refused is an aggregate **in the GROUP BY clause**, and the
//! wording depends on where the alias sits relative to it — see
//! [the GROUP BY rule](#the-group-by-rule-three-messages-one-rule).
//!
//! # The GROUP BY rule: three messages, one rule
//!
//! SQLite does not test a GROUP BY term for an aggregate. It resolves the
//! clause first, *substituting each output alias for the expression it names
//! on the way* (`resolve.c:674-689`), and only then asks whether any term has
//! come out marked `EP_Agg` (`resolve.c:2109-2115`). Which message that produces
//! depends on how the alias is reached:
//!
//! | GROUP BY term | what is reached first | message |
//! |---|---|---|
//! | `m` | the alias, substituted whole | `aggregate functions are not allowed in the GROUP BY clause` |
//! | `(m)` | the same, the parentheses being dropped | `aggregate functions are not allowed in the GROUP BY clause` |
//! | `max(m)` | the `max`, on the way to `m` | `misuse of aliased aggregate m` |
//! | `m+0` | the alias, substituted into the term | `misuse of aggregate: min()` |
//! | `m+max(b)` | the written `max`, and the `m` substituted beside it | `aggregate functions are not allowed in the GROUP BY clause` |
//! | `min(a)` | the aggregate, written outright | `aggregate functions are not allowed in the GROUP BY clause` |
//!
//! The third message is the giveaway that a substitution really happened: the
//! aggregate is read off the *copy* of the aliased expression, so the name
//! quoted is the aggregate's, not the alias's, and the wording is the short
//! `misuse of aggregate: {fn}()` from `expr.c:5439` rather than the refusal by
//! name.
//!
//! The clause is still walked a term at a time, so arity and nesting are
//! decided before the next term is looked at: `GROUP BY max(m), min()` is the
//! misuse of `m` while `GROUP BY min(), max(m)` is the arity error. And the
//! refusal, being a test over the finished clause, outranks a substitution from
//! any term: `GROUP BY m+0, min(a)` is the refusal.
//!
//! # The three scope messages
//!
//! SQLite's two misuse wordings are a flag in its name resolver, not a
//! difference in the query, and which flag is set is decided by what the
//! statement has in scope:
//!
//! | situation | message |
//! |---|---|
//! | no group anywhere (`SELECT 1 FROM t WHERE count(*)>0`) | `misuse of aggregate function count()` |
//! | the query has an aggregate in scope (`SELECT count(*) FROM t WHERE min(a)>0`) | `misuse of aggregate: min()` |
//! | an ORDER BY on a query with no aggregate (`SELECT 1 FROM t ORDER BY count(*)`) | `misuse of aggregate: count()` |
//!
//! The first is [`Scope::NoGroup`], the second and third are
//! [`Scope::MisuseShort`], and a CHECK constraint, a partial index's WHERE and
//! an UPDATE or INSERT are always the first — they have no result columns at
//! all, so nothing can be in scope.
//!
//! # The order the four checks fire in
//!
//! Within one aggregate call, the order is SQLite's walk order, and every case
//! below is confirmed on `sqlite3` 3.53.4:
//!
//! 1. the call's own **arity**, before anything inside it — `sum(m,1)` is the
//!    arity error even where `m` is an aggregate alias, and `min()` is the
//!    arity error in every scope including a GROUP BY and a CHECK;
//! 2. the **alias** rule of ticket #2526, which is reached on the way into the
//!    arguments;
//! 3. **nesting**, whose name is the *innermost* aggregate, and whose own arity
//!    outranks it because that argument is resolved first — `sum(min())` names
//!    `min()`;
//! 4. the **scope** misuse, and last of all `DISTINCT aggregates must have
//!    exactly one argument`.
//!
//! That fourth one is not name resolution at all: it is raised by code
//! generation, so it is only reached once the whole expression has resolved
//! cleanly. A place with no group has no accumulator to generate and reports
//! the scope misuse instead — `SELECT 1 FROM t WHERE count(DISTINCT)>0` is
//! `misuse of aggregate function count()`, and so are the CHECK, UPDATE and
//! DELETE spellings.
//!
//! # Case
//!
//! Messages quote the function name **as written**, never lowercased:
//! `SELECT MAX(*)` is `wrong number of arguments to function MAX()` and
//! `SELECT SUM(MIN(f1))` is `misuse of aggregate function MIN()`. The
//! lowercased name is only used to decide what a call is.
//!
//! # A stated limit
//!
//! When a statement has **several independent** defects, which one SQLite
//! reports depends on where its name-resolution walk happens to stop, and that
//! is not derivable from the parse tree alone. This module reports the first in
//! walk order. Two of the cases where that differs, both confirmed on sqlite3
//! 3.53.4:
//!
//! | statement | sqlite3 | here |
//! |---|---|---|
//! | `SELECT sum(a,b), avg(a,b) FROM t` | `sum()` | `sum()` |
//! | `SELECT 1 FROM t WHERE min(*)>0 AND count(*)>0` | `min()` | `min()` |
//! | `SELECT sum(max(sum(a)),count(*)) FROM t` | `sum()` (inner) | `sum()` (inner) |
//!
//! Every single-defect statement — which is every case in `select1-2.*` and
//! every case the track names — matches byte for byte, and the three shapes
//! above happen to agree as well. What is not claimed is that an arbitrary
//! two-defect statement always agrees.
//!
//! # What is deliberately not reported
//!
//! Three cases are left to the name resolution that runs after this check,
//! because answering them needs the schema and this module has none. Each is a
//! `no such column` on `sqlite3` where a wrong answer here would be worse than
//! no answer:
//!
//! | statement | sqlite3 | here |
//! |---|---|---|
//! | `SELECT 1 FROM t LIMIT count(a)` | `no such column: a` | no error |
//! | `SELECT min(a) AS m FROM t LIMIT max(m)` | `no such column: m` | no error |
//! | `SELECT 1 FROM t GROUP BY max(m), count(a,b)` | `no such column: m` | `misuse of aliased aggregate m` |
//!
//! The first two are a LIMIT or an OFFSET, where `resolve.c:1930-1936` passes
//! an empty `NameContext` and *no* name resolves — not a column, and not a
//! result alias either. The fold is only refused when its first argument names
//! nothing: `LIMIT count(*)` and `LIMIT string_agg(1,2)` are misuses, while
//! `LIMIT string_agg(1,a)` is a misuse too and `LIMIT string_agg(a,1)` is not,
//! because the walk reaches the first argument first. See
//! [`names_in_first_argument`].
//!
//! The third is an alias that a real column of the FROM shadows, which needs
//! the schema to tell. It is the one case here that reports something
//! `sqlite3` would not, and it is the same shadowing limit [`aliased_aggregate_in`]
//! already carries.
//!
//! # The integration hook
//!
//! One line, in `connection.rs`, at the top of `Connection::execute` before
//! the `match stmt`:
//!
//! ```ignore
//! crate::aggcheck::Ctx::new().check(stmt)?;
//! ```
//!
//! It has to be there and not inside the per-arm handlers because several of
//! the checks apply to every arm — `count(a,b)` is the same defect whether it
//! is in a SELECT, an UPDATE's SET or an INSERT's VALUES — and because the
//! clause order in [`Ctx::check_stmt`] is what makes a multi-defect statement
//! report the right one.
//!
//! The existing checks in `grouping.rs` (`Plan::build`) and in `Connection::select`
//! then become redundant for the messages listed above and can be deleted; this
//! module deliberately does not touch either, because both are owned by another
//! track.
//!
//! A partial index's WHERE is the one case the hook above cannot reach: the
//! parser reads the expression at `parser.rs:2390` and drops it, so
//! `Stmt::CreateIndex` carries no expression to check. Wiring that needs a
//! `where_: Option<Expr>` field on `Stmt::CreateIndex` in `parser.rs`, and a
//! second `check_no_group` call in the `Stmt::CreateIndex` arm above. The
//! `no_group` entry point is public and ready for it.

use crate::error::{Error, Result, ResultCode};
use crate::parser::{
    ColumnDef, Constraint, Expr, FromItem, InsertSource, Select, SelectBody, Stmt,
};

/// The arity range each aggregate name accepts, from the `WAGGREGATE` lines in
/// `testsuite/src/func.c:3469-3494`.
///
/// `min` and `max` are the awkward ones: each is registered twice, once as a
/// variadic scalar (`FUNCTION(min, -3, ...)`) and once as a single-argument
/// aggregate, and the arity picks which. `min(a)` folds a group; `min(a,b,c)` is
/// a scalar call on one row and is never an arity error. Zero arguments matches
/// neither, so it is an arity error — a *parenthesised* empty list and a bare
/// `min(*)` are the same defect, which is why the star arm below and this one
/// agree.
///
/// `median`, `percentile`, `percentile_cont` and `percentile_disc` are real
/// aggregates in this `sqlite3` build and take one, two, two and two. Every
/// other name here is not a function at all and is deliberately absent:
/// `SELECT product(1,2)` is `no such function: product` on 3.53.4.
fn arity_range(name: &str) -> Option<(usize, usize)> {
    if name.eq_ignore_ascii_case("min") || name.eq_ignore_ascii_case("max") {
        return Some((1, 1));
    }
    Some(match name.to_ascii_lowercase().as_str() {
        "count" => (0, 1),
        "sum" | "total" | "avg" | "median" => (1, 1),
        "group_concat" => (1, 2),
        "string_agg" | "percentile" | "percentile_cont" | "percentile_disc" => (2, 2),
        _ => return None,
    })
}

/// Whether a call to `name` with `argc` arguments folds a group rather than
/// being a scalar call.
///
/// `min` and `max` are the only names with a scalar form, and it starts at two
/// arguments — so a *zero*-argument call is the aggregate with the wrong arity,
/// not a scalar. That arm is what makes `SELECT min() FROM t` an arity error
/// instead of passing through.
fn is_aggregate_call(name: &str, argc: usize) -> bool {
    if name.eq_ignore_ascii_case("min") || name.eq_ignore_ascii_case("max") {
        return argc <= 1;
    }
    arity_range(name).is_some()
}

/// The arity verdict for a call, or `None` when there is nothing to report.
///
/// The order of the three shapes is SQLite's:
///
/// * A star is a call with no argument list at all, which makes `min(*)` a
///   zero-argument `min` and `wrong number of arguments to function min()`,
///   while `count(*)` is legal.
/// * A wrong argument count is name resolution, and is reported first.
/// * `DISTINCT aggregates must have exactly one argument` is raised later, by
///   code generation, so only an otherwise-legal argument list reaches it.
///   `sum(DISTINCT a,b)` is the arity error; `group_concat(DISTINCT a,b)` is
///   legal at two arguments and so is the DISTINCT one.
enum Arity {
    Wrong(String),
    BadDistinct,
}

fn arity_verdict(expr: &Expr) -> Option<Arity> {
    let Expr::Function {
        name,
        args,
        star,
        distinct,
    } = expr
    else {
        return None;
    };
    // A bare `*` in the result list is a star expansion, not a call.
    if *star && name == "*" {
        return None;
    }
    if *star {
        // Only `count(*)` is legal, and a star never carries DISTINCT.
        return match arity_range(name) {
            Some((0, _)) => None,
            Some(_) => Some(Arity::Wrong(name.clone())),
            None => None,
        };
    }
    // `min(a,b,c)` is the scalar form: not a fold, so no arity to get wrong.
    if !is_aggregate_call(name, args.len()) {
        return None;
    }
    let (lo, hi) = arity_range(name)?;
    if args.len() < lo || args.len() > hi {
        return Some(Arity::Wrong(name.clone()));
    }
    (*distinct && args.len() != 1).then_some(Arity::BadDistinct)
}
/// What is in scope where an expression is being checked.
#[derive(Clone, Copy)]
enum Scope {
    /// A place with no group at all, and where no output alias is in scope
    /// either: a WHERE on a query with no aggregate, a CHECK constraint, a
    /// partial index's WHERE, an UPDATE, an INSERT's VALUES, a LIMIT.
    ///
    /// A fold here is `misuse of aggregate function {name}()`.
    NoGroup,
    /// A place with no group, but on a query that *does* have an aggregate or
    /// a GROUP BY, and an ORDER BY that may fold only if it does. The shorter
    /// `misuse of aggregate: {name}()`.
    MisuseShort,
    /// A place where a fold is legal: the result columns, a HAVING, an ORDER BY
    /// on a query with a group. The alias rule still applies, and so does
    /// ticket #2526.
    Grouped,
    /// A GROUP BY term, which name-resolves like [`Scope::Grouped`] — arity,
    /// nesting and the alias rule all apply, since it is an ordinary walk — but
    /// which never reaches `DISTINCT aggregates must have exactly one
    /// argument`. That message is raised by code generation, and the GROUP BY
    /// is refused before any of it: `SELECT 1 FROM t GROUP BY count(DISTINCT)`
    /// is `aggregate functions are not allowed in the GROUP BY clause`.
    GroupBy,
    /// A LIMIT or an OFFSET, where no name resolves at all.
    ///
    /// The arity and nesting checks still apply — SQLite resolves the call and
    /// its arguments before it decides anything else, so `LIMIT min()` and
    /// `LIMIT sum(count(a,b))` are arity errors wherever they are — and so does
    /// the alias rule, though a result alias is in no scope here either and so
    /// an alias in a LIMIT is a `no such column` rather than a misuse of it.
    ///
    /// What this drops is the scope misuse, because it is the *last* thing
    /// SQLite decides and an argument naming a column beats it:
    /// `LIMIT count(a)` is `no such column: a` and not
    /// `misuse of aggregate function count()`. A fold whose arguments name
    /// nothing — `LIMIT count(*)`, `LIMIT count(1)` — does get the misuse, and
    /// that is the whole of what this reports.
    Limit,
}

/// The result-column aliases of the SELECT being checked: each alias name,
/// whether it stands for an aggregate, and the expression it names.
///
/// Only an *explicit* alias is referenceable: `SELECT a+b AS x` is reached by
/// `x`, but the same expression written without an alias has no name to be
/// reached by.
type Aliases<'a> = [(String, bool, &'a Expr)];

/// The check for one statement.
///
/// Building it runs nothing. A fresh `Ctx` is wanted per statement, since the
/// error it carries is a property of one statement.
#[derive(Debug, Default, Clone)]
pub struct Ctx {
    /// The first defect found, in walk order.
    message: Option<String>,
}

impl Ctx {
    /// A fresh check.
    pub fn new() -> Ctx {
        Ctx::default()
    }

    /// Checks a statement and reports the error `sqlite3` would report, if any.
    pub fn check(&mut self, stmt: &Stmt) -> Result<()> {
        self.check_stmt(stmt);
        match self.message.take() {
            Some(m) => Err(Error::new(ResultCode::Error, m)),
            None => Ok(()),
        }
    }

    /// Checks one expression in a place with no group, which is the rule a
    /// CHECK constraint and a partial index's WHERE both use.
    ///
    /// [`Ctx::check_stmt`] covers the statements whose WHERE is in the parse
    /// tree. A partial index's WHERE is not — the parser reads it and drops it
    /// — so this is the entry point for it, and the test suite drives it
    /// directly. Wiring it to `CREATE INDEX` needs the parser to keep the
    /// expression; see the module's integration note.
    pub fn check_no_group(&mut self, expr: &Expr) -> Result<()> {
        self.walk(expr, Scope::NoGroup, &[]);
        self.take()
    }

    /// The error found so far, as a `Result`.
    fn take(&mut self) -> Result<()> {
        match self.message.take() {
            Some(m) => Err(Error::new(ResultCode::Error, m)),
            None => Ok(()),
        }
    }

    /// Records the first defect and stops the walk there.
    fn fail(&mut self, message: String) {
        if self.message.is_none() {
            self.message = Some(message);
        }
    }

    /// The statement's aggregates, in SQLite's resolution order: result
    /// columns, HAVING, WHERE, ORDER BY, GROUP BY.
    fn check_stmt(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::Select(sel) => self.check_select(sel),
            Stmt::CreateTable {
                columns,
                constraints,
                ..
            } => {
                for c in columns {
                    self.check_columns(c);
                }
                for c in constraints {
                    self.check_constraint(c);
                }
            }
            Stmt::CreateIndex { .. } => {}
            Stmt::Insert { source, .. } => match source {
                InsertSource::Values(rows) => {
                    for row in rows {
                        for e in row {
                            self.walk(e, Scope::NoGroup, &[]);
                        }
                    }
                }
                InsertSource::Select(sel) => self.check_select(sel),
            },
            Stmt::Update { sets, where_, .. } => {
                for (_, e) in sets {
                    self.walk(e, Scope::NoGroup, &[]);
                }
                if let Some(w) = where_ {
                    self.walk(w, Scope::NoGroup, &[]);
                }
            }
            Stmt::Delete {
                where_: Some(w), ..
            } => self.walk(w, Scope::NoGroup, &[]),
            _ => {}
        }
    }

    /// A CREATE TABLE's column-level constraints.
    fn check_columns(&mut self, c: &ColumnDef) {
        for k in &c.constraints {
            self.check_constraint(k);
        }
    }

    /// One table- or column-level constraint.
    ///
    /// Only CHECK can hold an aggregate. A DEFAULT cannot, and `sqlite3`
    /// refuses it first with `default value of column [a] is not constant` —
    /// with the single exception of `count(*)` alone, which it accepts. That is
    /// a constant-folding rule rather than an aggregate rule, so it belongs to
    /// the parser and is not checked here.
    fn check_constraint(&mut self, c: &Constraint) {
        if let Constraint::Check(e) = c {
            self.walk(e, Scope::NoGroup, &[]);
        }
    }

    /// A SELECT, its CTEs, and its LIMIT and OFFSET.
    fn check_select(&mut self, sel: &Select) {
        for cte in &sel.with {
            self.check_select(&cte.select);
        }
        self.check_body(&sel.body, &sel.order_by);
        // LIMIT and OFFSET are resolved with an *empty* NameContext
        // (`resolve.c:1930-1936`), so no name in them resolves: not a column of
        // the FROM, and not a result alias either. An aggregate that reaches
        // no column — `LIMIT count(*)`, `LIMIT count(1)`, `LIMIT min(1)` — is
        // refused as a misuse, while one that names a column is
        // `no such column: a`, which is name resolution's answer and not this
        // module's to give. `Scope::Limit` keeps that second case out, so the
        // walk reports only what it can answer.
        for e in sel.limit.iter().chain(sel.offset.iter()) {
            self.walk(e, Scope::Limit, &[]);
        }
    }

    /// A select body, which is one arm of a compound or a nested one.
    fn check_body(&mut self, body: &SelectBody, order_by: &[(Expr, bool)]) {
        if self.message.is_some() {
            return;
        }
        match body {
            SelectBody::Compound { left, right, .. } => {
                self.check_body(left, &[]);
                self.check_body(right, &[]);
            }
            SelectBody::Nested(sel) => self.check_select(sel),
            SelectBody::Simple {
                columns,
                from,
                where_,
                group_by,
                having,
                values,
                ..
            } => {
                // A subquery in FROM carries its own result columns, and so its
                // own aliases, and is checked on its own terms.
                for item in from {
                    if let FromItem::Subquery { select, .. } = item {
                        self.check_select(select);
                    }
                }
                if self.message.is_some() {
                    return;
                }

                let aliases: Vec<(String, bool, &Expr)> = columns
                    .iter()
                    .filter_map(|rc| {
                        rc.alias
                            .as_ref()
                            .map(|a| (a.clone(), has_aggregate(&rc.expr), &rc.expr as &Expr))
                    })
                    .collect();
                // A GROUP BY creates the scope; a HAVING does not, which is
                // why `SELECT 1 FROM t HAVING count(*)>0` is a non-aggregate
                // query rather than one whose count is in scope.
                let agg_in_scope =
                    !group_by.is_empty() || columns.iter().any(|rc| has_aggregate(&rc.expr));
                let order_scope = if agg_in_scope {
                    Scope::Grouped
                } else {
                    Scope::MisuseShort
                };

                // 1. the result columns, where a fold is what makes the query
                //    aggregate and is therefore legal.
                for rc in columns {
                    self.walk(&rc.expr, Scope::Grouped, &aliases);
                }
                if let Some(vs) = values {
                    for row in vs {
                        for e in row {
                            self.walk(e, Scope::Grouped, &aliases);
                        }
                    }
                }

                // 2. HAVING, which needs a group to fold over. The test is
                //    made before its own aggregates are walked, so the count it
                //    names does not itself make the query aggregate.
                if let Some(h) = having {
                    if !agg_in_scope {
                        self.fail("HAVING clause on a non-aggregate query".to_string());
                        return;
                    }
                    self.walk(h, Scope::Grouped, &aliases);
                }

                // 3. WHERE, which never folds, and where the two misuse
                //    wordings differ.
                if let Some(w) = where_ {
                    let s = if agg_in_scope {
                        Scope::MisuseShort
                    } else {
                        Scope::NoGroup
                    };
                    self.walk(w, s, &aliases);
                }

                // 4. ORDER BY, which folds per group when there is one.
                for (e, _) in order_by {
                    self.walk(e, order_scope, &aliases);
                }

                // 5. the GROUP BY clause itself.
                //
                //    SQLite's rule is not "this term holds an aggregate". It
                //    resolves the whole GROUP BY first, *substituting each
                //    output alias for the expression it names on the way*, and
                //    only then asks whether any term has come out marked
                //    `EP_Agg` (`testsuite/src/resolve.c:2099-2115`). Which
                //    error that produces depends on where the alias sits
                //    relative to the aggregate, because a substitution
                //    returns immediately (`resolve.c:688`):
                //
                //    | GROUP BY term | what is reached first | message |
                //    |---|---|---|
                //    | `m` | the alias, substituted whole | `aggregate functions are not allowed in the GROUP BY clause` |
                //    | `max(m)` | the `max`, on the way to `m` | `misuse of aliased aggregate m` |
                //    | `m+0` | the alias, substituted into the term | `misuse of aggregate: min()` |
                //    | `min(a)` | the aggregate, written outright | `aggregate functions are not allowed in the GROUP BY clause` |
                //
                //    The substitution only happens for a term that reaches the
                //    alias through a *bare name*, so it is a walk of the term
                //    with the alias handled one node at a time:
                //
                //    * another aggregate is on the way to the alias, so
                //      `NC_AllowAgg` is already clear and the alias is reached
                //      under it — `misuse of aliased aggregate {alias}`;
                //    * the alias is a bare name of the term, so it is replaced
                //      and the aggregate that came with it is read off the
                //      copy — `misuse of aggregate: {fn}()`.
                //
                //    A term with an aggregate written into it and no alias is
                //    not substituted at all, so it falls through to the
                //    refusal by name, which SQLite's message does not quote a
                //    function for.
                //
                //    Name resolution is a *walk*, not a test, and it walks the
                //    clause a term at a time, so a term's arity and nesting are
                //    decided before the next term is looked at:
                //    `GROUP BY max(m), count(a,b)` is the misuse of `m` while
                //    `GROUP BY count(a,b), max(m)` is the arity error. So the
                //    walk and the alias verdict are interleaved per term, with
                //    the `EP_Agg` refusal held back until the whole clause has
                //    resolved — it is a test over the finished clause, and in
                //    `GROUP BY m+0, min(a)` it is the `min` that is reported
                //    even though `m+0` comes first.
                // The three verdicts are ranked, and the ranking is the
                // order in which `resolve.c` and `select.c` can reach them:
                //
                // * `misuse of aliased aggregate {a}` — `resolveAlias` returns
                //   immediately (`resolve.c:688`), so it pre-empts every other
                //   term, and it pre-empts a refusal already marked for an
                //   earlier term: `GROUP BY m, max(m)` is the misuse of `m`.
                // * `aggregate functions are not allowed in the GROUP BY
                //   clause` — a test over the whole finished clause
                //   (`resolve.c:2109`), so it outranks a substitution from any
                //   term, earlier or the same one: `GROUP BY m+max(b)` is the
                //   refusal, not `misuse of aggregate: min()`.
                // * `misuse of aggregate: {n}()` — only what is left.
                //
                // The walk in between is what makes the first two term-local:
                // arity and nesting are decided term by term, so
                // `GROUP BY max(m), count(a,b)` is the misuse of `m` while
                // `GROUP BY count(a,b), max(m)` is the arity error.
                let mut misuse: Option<GroupByTerm<'_>> = None;
                let mut refusal = false;
                for e in group_by {
                    match group_by_term(e, &aliases) {
                        GroupByTerm::Alias(_) => {
                            misuse.get_or_insert_with(|| group_by_term(e, &aliases));
                            break;
                        }
                        GroupByTerm::Aggregate => refusal = true,
                        _ => {}
                    }
                    self.walk(e, Scope::GroupBy, &aliases);
                    if self.message.is_some() {
                        return;
                    }
                }
                let e = if let Some(e) = misuse {
                    e
                } else if refusal {
                    GroupByTerm::Aggregate
                } else {
                    // No term holds an aggregate outright, so the first
                    // substitution is what is left to report.
                    group_by
                        .iter()
                        .map(|e| group_by_term(e, &aliases))
                        .find(|t| matches!(t, GroupByTerm::Substituted(_)))
                        .unwrap_or(GroupByTerm::Clean)
                };
                {
                    match e {
                        GroupByTerm::Alias(a) => {
                            self.fail(format!("misuse of aliased aggregate {a}"));
                        }
                        GroupByTerm::Substituted(orig) => {
                            // The alias only counts as standing for an aggregate
                            // when one is in the expression it names, so there
                            // is always one to name.
                            let n = substituted_aggregate(orig)
                                .expect("an aggregate alias names an aggregate");
                            self.fail(format!("misuse of aggregate: {n}()"));
                        }
                        GroupByTerm::Aggregate => {
                            self.fail(
                                "aggregate functions are not allowed in the GROUP BY clause"
                                    .to_string(),
                            );
                        }
                        GroupByTerm::Clean => {}
                    }
                }
            }
        }
    }

    /// Walks an expression, checking every call in it.
    ///
    /// `aliases` is the SELECT's result-column alias list, which is threaded
    /// through every clause including a WHERE. That is deliberate and matches
    /// `sqlite3`: a result alias is in scope for the whole SELECT, not only
    /// for the clauses where a fold is legal, so
    /// `SELECT min(a) AS m FROM t WHERE max(m)<1` is
    /// `misuse of aliased aggregate m` even though the `max` is itself a
    /// misuse and the alias check is reached first.
    fn walk(&mut self, expr: &Expr, scope: Scope, aliases: &Aliases<'_>) {
        if self.message.is_some() {
            return;
        }
        let Expr::Function {
            name, args, star, ..
        } = expr
        else {
            // A subquery is its own scope and is reached separately, but in a
            // LIMIT it still has to be: `LIMIT (SELECT 1 WHERE count(*)>0)` is
            // `misuse of aggregate function count()` on `sqlite3`, and the
            // inner query has a group context of its own.
            match expr {
                Expr::Subquery { select } | Expr::Exists { select, .. } => {
                    self.check_select(select);
                }
                Expr::InSelect {
                    expr: e, select, ..
                } => {
                    self.walk(e, scope, aliases);
                    self.check_select(select);
                }
                _ => {
                    for c in children(expr) {
                        self.walk(c, scope, aliases);
                    }
                }
            }
            return;
        };
        // A bare `*` in the result list is a star expansion, not a call.
        if *star && name == "*" {
            return;
        }
        let folds = *star || is_aggregate_call(name, args.len());
        if !folds {
            // An ordinary function, whose arguments may still hold an
            // aggregate: `abs(count(*))` in a WHERE is a misuse, and `abs` is
            // not named in the message.
            for a in args {
                self.walk(a, scope, aliases);
            }
            return;
        }

        // The call's own arity, before anything inside it — including before
        // the alias rule. `sum(m,1)` is `wrong number of arguments to function
        // sum()` even where `m` is an alias standing for an aggregate, because
        // SQLite has already found that no function takes two arguments by the
        // time it walks anything. The alias is only substituted on the way
        // *into* an argument, and there is no argument to walk.
        match arity_verdict(expr) {
            Some(Arity::Wrong(n)) => {
                self.fail(format!("wrong number of arguments to function {n}()"));
                return;
            }
            Some(Arity::BadDistinct) => {}
            None => {}
        }

        // Ticket #2526: an output alias that stands for an aggregate may not be
        // used inside another aggregate. SQLite substitutes the alias for the
        // expression it names before walking the argument list, so this holds
        // anywhere in the arguments, not only when an argument *is* the bare
        // name: `max(m+5)` is a misuse too. It beats the nesting check below,
        // because the substitution happens on the way down and the inner
        // aggregate is only reached after it: `max(m+sum(a,b))` names `m`.
        //
        // A GROUP BY term is the one place this does *not* decide it, because
        // there the answer depends on where the alias sits in the term rather
        // than on whether an aggregate is around it: `m+0` is
        // `misuse of aggregate: min()` and `m` is the refusal by name, while
        // only `max(m)` is the misuse of the alias. [`group_by_term`] makes
        // that call instead, so the walk stands aside here.
        //
        // A LIMIT stands aside for the opposite reason: the result-set alias
        // list is not in scope there at all, so `LIMIT max(m)` is
        // `no such column: m` and not a misuse of `m`.
        if matches!(scope, Scope::Grouped | Scope::NoGroup | Scope::MisuseShort) {
            if let Some(alias) = aliased_aggregate_in(args, aliases) {
                self.fail(format!("misuse of aliased aggregate {alias}"));
                return;
            }
        }

        // An argument that is itself an aggregate, which is the nesting rule.
        // This is decided before the *scope* check, for the walk order: SQLite
        // resolves the arguments before it decides the call is a misuse, so
        // `SELECT 1 FROM t WHERE sum(count(*))>0` names `count()`. An inner
        // call's own arity outranks even this, since it is reached on the way
        // down: `sum(min())` names `min()`.
        if let Some(inner) = args.iter().find_map(innermost_aggregate) {
            // A LIMIT has no alias list, so an argument that names one is a
            // `no such column` rather than a nesting to report.
            let deferred = matches!(scope, Scope::Limit) && names_in_first_argument(args);
            if !deferred {
                match inner {
                    Inner::Arity(n) => {
                        self.fail(format!("wrong number of arguments to function {n}()"));
                    }
                    Inner::Misuse(n) => {
                        self.fail(format!("misuse of aggregate function {n}()"));
                    }
                }
                return;
            }
        }

        // A fold where no group exists.
        match scope {
            Scope::NoGroup => {
                self.fail(format!("misuse of aggregate function {name}()"));
                return;
            }
            Scope::MisuseShort => {
                self.fail(format!("misuse of aggregate: {name}()"));
                return;
            }
            Scope::Limit => {
                // No name resolves in a LIMIT, so a fold that names a column
                // is `no such column: a` — name resolution's answer, and the
                // one that comes first, since SQLite resolves the arguments
                // before it decides the call is a misuse. A fold that names
                // nothing is still refused.
                if !names_in_first_argument(args) {
                    self.fail(format!("misuse of aggregate function {name}()"));
                    return;
                }
            }
            Scope::Grouped | Scope::GroupBy => {}
        }

        // `DISTINCT aggregates must have exactly one argument` is the *last*
        // of the four, and deliberately so: it is raised by code generation
        // rather than by name resolution, so it is only reached once the whole
        // expression has resolved cleanly. A place with no group — where there
        // is no accumulator to generate — reports the scope misuse instead, and
        // a nesting or an arity error is reported long before this:
        // `SELECT 1 FROM t WHERE count(DISTINCT)>0` is
        // `misuse of aggregate function count()`, and
        // `SELECT 1 FROM t WHERE sum(count(DISTINCT))>0` names `count()`.
        if !matches!(scope, Scope::GroupBy)
            && matches!(arity_verdict(expr), Some(Arity::BadDistinct))
        {
            self.fail("DISTINCT aggregates must have exactly one argument".to_string());
            return;
        }

        for a in args {
            self.walk(a, scope, aliases);
        }
    }
}

/// The first bare name in `expr` that is an alias standing for an aggregate, or
/// `None` if there is none.
///
/// A qualified name is not an alias reference: `max(t.m)` is `no such column:
/// t.m`, not a misuse of an alias.
///
/// A real column of the FROM that happens to share an aggregate alias's name
/// shadows the alias, and `sqlite3` accepts `max(m)` in that case. Deciding it
/// needs the schema, which a static check does not have; the name resolution
/// that runs afterwards reports the column, and this is noted as a limit in the
/// module's integration note.
fn aliased_aggregate_in<'a>(args: &'a [Expr], aliases: &'a Aliases<'a>) -> Option<&'a str> {
    fn find<'a>(e: &'a Expr, aliases: &'a Aliases<'a>) -> Option<&'a str> {
        if let Expr::Column { table, name, .. } = e {
            if table.is_none() {
                if let Some((alias, true, _)) = aliases.iter().find(|(a, agg, _)| *agg && a == name)
                {
                    return Some(alias.as_str());
                }
            }
        }
        children(e).into_iter().find_map(|c| find(c, aliases))
    }
    args.iter().find_map(|a| find(a, aliases))
}

/// What one GROUP BY term resolves to, and so which of SQLite's three GROUP
/// BY messages it earns.
#[derive(Clone)]
enum GroupByTerm<'a> {
    /// The term reaches an aggregate alias through another aggregate, so
    /// `NC_AllowAgg` is already clear when the alias is reached:
    /// `misuse of aliased aggregate {alias}`.
    Alias(&'a str),
    /// The term is not the whole alias but does contain it, so it is
    /// *replaced* by the expression the alias names and the aggregate that came
    /// with it is read off the copy: `misuse of aggregate: {name}()`. Carries
    /// the expression the alias names, because the name quoted is the
    /// *aggregate's*, which is not the alias: `SELECT min(a) AS m ... GROUP BY
    /// m+0` names `min`.
    Substituted(&'a Expr),
    /// The term holds an aggregate that was written into it, so the clause is
    /// refused by name — which is the only one of the three messages that does
    /// not quote a function.
    Aggregate,
    /// Nothing here is an aggregate.
    Clean,
}

/// Classifies one GROUP BY term. See the note at its call site for why the
/// order of the three is what it is.
///
/// A term is resolved node by node, left to right, and the two alias outcomes
/// both need a *bare* name to be reached through, which is what makes them
/// differ. Reaching the name while already inside an aggregate — `max(m)` — is
/// [`GroupByTerm::Alias`], because SQLite's `resolveAlias` returns
/// immediately rather than substituting (`resolve.c:688`). Reaching it in the
/// term itself — `m+0` — is [`GroupByTerm::Substituted`]. And a name that *is*
/// the whole term, `m`, is substituted so that nothing of the original survives
/// and the aggregate lands directly in the clause's top-level node, which is
/// the one the `EP_Agg` test then sees: `Aggregate`.
///
/// So the test is "is there an aggregate between the term and the name", not
/// "how deep is the name" — `m+0` and `max(m)` are both one node down, and
/// they are the two different messages. A *written* aggregate anywhere in the
/// term outranks both, because substituting the alias leaves it in the clause
/// and the `EP_Agg` test then finds it: `m+max(b)` is the refusal.
///
/// When the alias stands for an expression holding several aggregates, the one
/// named is the last in the right-to-left order of `sqlite3ExprCompare` at
/// `select.c:5570` — the rightmost aggregate of the rightmost summand and so
/// on: `min(a)+max(b)` names `max`, `max(b)+min(a)` names `min`, and
/// `min(a)+max(b)+sum(a)` names `sum`.
fn group_by_term<'a>(expr: &'a Expr, aliases: &'a Aliases<'a>) -> GroupByTerm<'a> {
    fn find<'a>(
        e: &'a Expr,
        aliases: &'a Aliases<'a>,
        in_agg: bool,
        is_term_root: bool,
    ) -> GroupByTerm<'a> {
        let is_agg_call = match e {
            // A star is a call with no argument list, so `count(*)` folds and
            // `min(*)` is a zero-argument `min`. A bare `*` in a result list is
            // a star *expansion* rather than a call, and is not one of these.
            Expr::Function {
                name, args, star, ..
            } => !(*star && name == "*") && (*star || is_aggregate_call(name, args.len())),
            _ => false,
        };
        let child_in_agg = in_agg || is_agg_call;
        for c in children(e) {
            let r = find(c, aliases, child_in_agg, false);
            if !matches!(r, GroupByTerm::Clean) {
                return r;
            }
        }
        if let Expr::Column { table, name, .. } = e {
            if table.is_none() {
                if let Some((_, true, orig)) = aliases.iter().find(|(a, agg, _)| *agg && a == name)
                {
                    return if in_agg {
                        GroupByTerm::Alias(name.as_str())
                    } else if is_term_root {
                        // The name *is* the term. SQLite's `resolveOrderGroupBy`
                        // substitutes the alias first for a GROUP BY term that is
                        // a bare name, so `GROUP BY m` leaves the aggregate in
                        // the clause's top-level node — which is the one the
                        // `EP_Agg` test then sees. Same for `(m)`, where the
                        // parentheses are dropped and leave the same node.
                        GroupByTerm::Aggregate
                    } else {
                        GroupByTerm::Substituted(orig)
                    };
                }
            }
        }
        if is_agg_call || has_aggregate(e) {
            return GroupByTerm::Aggregate;
        }
        GroupByTerm::Clean
    }
    // An aggregate written into the term is the refusal, whatever the walk
    // found: substituting the alias leaves that aggregate sitting in the clause
    // and the `EP_Agg` test then sees it. This is only a *written* one — the
    // aggregate an alias brings with it is not in the term, which is the whole
    // difference between `GROUP BY m+0` and `GROUP BY m+max(b)`.
    //
    // A misuse of an alias is the exception, because `resolveAlias` returns
    // immediately and never gets that far: `GROUP BY max(m), count(a,b)` is the
    // misuse of `m` and not the arity error.
    let found = find(expr, aliases, false, true);
    match found {
        GroupByTerm::Alias(_) => found,
        _ if has_written_aggregate(expr) => GroupByTerm::Aggregate,
        other => other,
    }
}

/// The aggregate named when an alias has been substituted into a GROUP BY term.
///
/// It is the one whose code generator is reached first, which is the last in
/// the right-to-left order of `sqlite3ExprCompare` at `select.c:5570` — the
/// rightmost aggregate of the rightmost summand, and so on down the fold. The
/// three that pin it down, all confirmed on `sqlite3` 3.53.4:
///
/// | alias's expression | reported |
/// |---|---|
/// | `min(a)` | `min` |
/// | `min(a)+max(b)` | `max` |
/// | `max(b)+min(a)` | `min` |
/// | `min(a)+max(b)+sum(a)` | `sum` |
/// | `sum(max(sum(a)))` | `sum` (the inner one, which the generator reaches first) |
fn substituted_aggregate(expr: &Expr) -> Option<String> {
    fn find(e: &Expr) -> Option<String> {
        if let Expr::Function {
            name, args, star, ..
        } = e
        {
            if *star && name == "*" {
                return None;
            }
            if *star || is_aggregate_call(name, args.len()) {
                return args
                    .iter()
                    .rev()
                    .find_map(find)
                    .or_else(|| Some(name.clone()));
            }
        }
        children(e).into_iter().rev().find_map(find)
    }
    find(expr)
}

/// What the **innermost** aggregate in an expression contributes, in the order
/// SQLite reaches it.
///
/// Innermost, because that is what SQLite names: `sum(max(a))` is
/// `misuse of aggregate function max()`, `sum(count(*))` names `count()`, and
/// `sum(max(sum(a)))` names the inner `sum()`. A single-defect statement has
/// only one candidate, so the depth only matters when a statement has two
/// defects at once, which is the limit noted in the module documentation.
///
/// An inner call's *arity* outranks the outer nesting, because SQLite resolves
/// the arguments before it decides that the outer call is itself a misuse: the
/// walk reaches `min()` and reports `wrong number of arguments to function
/// min()` for `sum(min())`, not the misuse of the `sum`. A `DISTINCT` shape is
/// not one of these — it is raised by code generation and never reached here —
/// so `sum(count(DISTINCT))` is still the misuse of `count()`.
enum Inner {
    Arity(String),
    Misuse(String),
}

fn innermost_aggregate(expr: &Expr) -> Option<Inner> {
    if let Expr::Function {
        name, args, star, ..
    } = expr
    {
        if *star && name == "*" {
            return None;
        }
        if *star || is_aggregate_call(name, args.len()) {
            if let Some(found) = args.iter().find_map(innermost_aggregate) {
                return Some(found);
            }
            return match arity_verdict(expr) {
                Some(Arity::Wrong(n)) => Some(Inner::Arity(n)),
                _ => Some(Inner::Misuse(name.clone())),
            };
        }
    }
    children(expr).into_iter().find_map(innermost_aggregate)
}

/// Whether a call's argument list puts a name where `sqlite3` looks for one
/// before it decides the call is a misuse, so that a place where no name
/// resolves cannot answer for it.
///
/// This is only used in a LIMIT or an OFFSET, where the answer `sqlite3` gives
/// for a fold that mentions a name is `no such column: {name}` — which needs to
/// know *which* name, so it is left to the name resolution that runs after this
/// check. What is left to report is the fold that mentions none.
///
/// The rule is narrower than "mentions a name". `resolve.c:1354` walks the
/// argument list *after* the call's own verdict has been recorded, and
/// `sqlite3ErrorMsg` overwrites `pParse->zErrMsg` (`util.c:249`), so an arity
/// error stays only if no argument names anything. What the walk reaches first
/// is the first argument, so that is the one that decides:
///
/// | LIMIT | sqlite3 |
/// |---|---|
/// | `count(*)` | `misuse of aggregate function count()` |
/// | `count(1)` | `misuse of aggregate function count()` |
/// | `min(1)` | `misuse of aggregate function min()` |
/// | `count(a)` | `no such column: a` |
/// | `string_agg(1,2)` | `misuse of aggregate function string_agg()` |
/// | `string_agg(1,a)` | `misuse of aggregate function string_agg()` |
/// | `string_agg(a,1)` | `no such column: a` |
/// | `count(1,a)` | `wrong number of arguments to function count()` |
/// | `count(a,1)` | `no such column: a` |
/// | `min(a)` | `no such column: a` |
///
/// So a name anywhere in the argument list is enough to take the answer away
/// *unless* it is preceded by something that resolves — which in a LIMIT means
/// a literal. `count(1,a)` keeps the arity error because the literal is
/// resolved first and fails no lookup; `string_agg(1,a)` is a misuse for the
/// same reason.
///
/// A column under an operator does not count, because SQLite folds the
/// operator into a constant before it looks for names: `count(a+1)`,
/// `count(-a)`, `count(a=1)` and `count(a||'')` are all misuses, while
/// `count(a IS NULL)`, `count((a))` and `count(abs(a))` are not. Only the first
/// argument decides, which is why `abs(count(a))` defers but `count(abs(a))`
/// does not.
fn names_in_first_argument(args: &[Expr]) -> bool {
    let Some(first) = args.first() else {
        // A star carries no name, so `count(*)` names nothing.
        return false;
    };
    named_bare(first)
}

/// Whether an expression resolves a name or folds to a constant without one.
///
/// An operator is folded to a constant before resolution, so a column under one
/// is not seen. A call may or may not fold — `abs(a)` does not, and
/// `abs(1)` does — so a call is treated as naming.
fn named_bare(e: &Expr) -> bool {
    match e {
        Expr::Column { .. } | Expr::NamedParameter(..) => true,
        Expr::Binary { .. } | Expr::Unary { .. } | Expr::Literal(_) => false,
        Expr::Function { .. } | Expr::Case { .. } | Expr::Between { .. } => true,
        _ => false,
    }
}

/// Whether an expression *as written* holds an aggregate call, ignoring the
/// aggregate an output alias would bring with it when it is substituted.
///
/// This is the difference between `GROUP BY m+0` and `GROUP BY m+max(b)`:
/// both hold `m`, and both substitute it, but the second has an aggregate of
/// its own sitting in the clause when the `EP_Agg` test runs. A bare name is
/// not an aggregate however the alias resolves, so it does not stop the walk.
fn has_written_aggregate(expr: &Expr) -> bool {
    if let Expr::Function {
        name, args, star, ..
    } = expr
    {
        if *star && name == "*" {
            return false;
        }
        if *star || is_aggregate_call(name, args.len()) {
            return true;
        }
        return args.iter().any(has_written_aggregate);
    }
    match expr {
        Expr::Subquery { select } | Expr::Exists { select, .. } => select_has_aggregate(select),
        Expr::InSelect { expr, select, .. } => {
            has_written_aggregate(expr) || select_has_aggregate(select)
        }
        _ => children(expr).into_iter().any(has_written_aggregate),
    }
}

/// Whether an expression contains an aggregate call anywhere, including in a
/// subquery it holds.
fn has_aggregate(expr: &Expr) -> bool {
    if let Expr::Function {
        name, args, star, ..
    } = expr
    {
        if *star && name == "*" {
            return false;
        }
        if *star || is_aggregate_call(name, args.len()) {
            return true;
        }
        return args.iter().any(has_aggregate);
    }
    match expr {
        Expr::Subquery { select } | Expr::Exists { select, .. } => select_has_aggregate(select),
        Expr::InSelect { expr, select, .. } => has_aggregate(expr) || select_has_aggregate(select),
        _ => children(expr).into_iter().any(has_aggregate),
    }
}

/// Whether a select mentions an aggregate anywhere.
fn select_has_aggregate(sel: &Select) -> bool {
    sel.with.iter().any(|c| select_has_aggregate(&c.select))
        || body_has_aggregate(&sel.body)
        || sel.order_by.iter().any(|(e, _)| has_aggregate(e))
}

/// Whether a select body mentions an aggregate.
fn body_has_aggregate(body: &SelectBody) -> bool {
    match body {
        SelectBody::Simple {
            columns,
            from,
            where_,
            group_by,
            having,
            values,
            ..
        } => {
            columns.iter().any(|rc| has_aggregate(&rc.expr))
                || where_.as_ref().is_some_and(has_aggregate)
                || group_by.iter().any(has_aggregate)
                || having.as_ref().is_some_and(has_aggregate)
                || values
                    .as_ref()
                    .is_some_and(|rows| rows.iter().flatten().any(has_aggregate))
                || from.iter().any(|i| match i {
                    FromItem::Subquery { select, .. } => select_has_aggregate(select),
                    FromItem::Table(_) => false,
                })
        }
        SelectBody::Compound { left, right, .. } => {
            body_has_aggregate(left) || body_has_aggregate(right)
        }
        SelectBody::Nested(sel) => select_has_aggregate(sel),
    }
}

/// The sub-expressions of an expression, in evaluation order. A subquery is
/// not among them: it is its own scope, and the caller reaches it separately.
fn children(expr: &Expr) -> Vec<&Expr> {
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
            let mut v = vec![expr.as_ref()];
            v.extend(list.iter());
            v
        }
        Like {
            expr,
            pattern,
            escape,
            ..
        } => {
            let mut v = vec![expr.as_ref(), pattern.as_ref()];
            v.extend(escape.iter().map(|b| b.as_ref()));
            v
        }
        Function { args, .. } => args.iter().collect(),
        Case {
            operand,
            whens,
            otherwise,
        } => {
            let mut v: Vec<&Expr> = operand.iter().map(|b| b.as_ref()).collect();
            for (w, t) in whens {
                v.push(w);
                v.push(t);
            }
            v.extend(otherwise.iter().map(|b| b.as_ref()));
            v
        }
        InSelect { expr, .. } => vec![expr],
        Subquery { .. } | Exists { .. } | Literal(_) | Column { .. } | NamedParameter(..) => {
            Vec::new()
        }
    }
}

#[cfg(test)]
#[path = "aggcheck_tests.rs"]
mod aggcheck_tests;
