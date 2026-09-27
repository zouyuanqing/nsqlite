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
//! The name cases are a larger instance of the same limit, and the shapes that
//! still differ are the ones with **two** aggregate defects in one clause. The
//! name-versus-aggregate ordering is settled for the single-defect half of
//! every pair, and the pairs themselves are not:
//!
//! | statement | sqlite3 | here |
//! |---|---|---|
//! | `SELECT 1 FROM t WHERE min() AND sum(count(*))` | `count()` | `min()` |
//! | `SELECT 1 FROM t WHERE min(a) AND count(*)` | `count()` | `min()` |
//! | `SELECT 1 FROM t WHERE count(*) AND min(a)` | `min()` | `count()` |
//!
//! All three are error-for-error, never accept-for-refuse, and all three are the
//! documented "several independent defects" case rather than a rule this module
//! gets wrong. Fixing them means modelling which of two aggregate verdicts
//! `sqlite3` reaches first across sibling nodes, which is the same walk-order
//! question raised once more and not settled by the parse tree.
//!
//! # What is deliberately not reported
//!
//! Three cases are left to the name resolution that runs after this check.
//! Each needs the schema, and each is a `no such column` on `sqlite3`:
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
//! already carries. [`Ctx::check_without_names`] is the entry point that leaves
//! all three alone; [`Ctx::check`] is the one the connection uses.
//!
//! # Names, and why they have to be handed in
//!
//! A name `sqlite3` cannot resolve is `no such column` (`resolve.c:785`), and
//! that outranks every message in this module — but only when the name is
//! reached *first*. `sqlite3ErrorMsg` keeps the **last** message written
//! (`util.c:268`) and the resolution walk stops as soon as a node has failed
//! (`resolve.c:1505`), so which of the two a statement has is decided by
//! walking the names and the aggregates together, in one order:
//!
//! | statement | sqlite3 |
//! |---|---|
//! | `SELECT count(a,b), nosuchcol FROM t` | `wrong number of arguments to function count()` |
//! | `SELECT nosuchcol, count(a,b) FROM t` | `no such column: nosuchcol` |
//! | `SELECT 1 FROM t WHERE nosuchcol AND count(*)>0` | `no such column: nosuchcol` |
//! | `SELECT 1 FROM t WHERE count(*)>0 AND nosuchcol` | `misuse of aggregate function count()` |
//!
//! So the checks have to walk the names in the same order as the aggregates,
//! and a static check has no schema to walk them against. [`Names`] is that
//! schema, for one statement's FROM, and [`names_for`] builds it from the
//! catalog. It is why the connection hands them to [`Ctx::check`]: without
//! them the walk is blind to a name, and every statement mixing one with an
//! aggregate defect is answered with the wrong one of the two.
//!
//! # The integration hook
//!
//! One line, in `connection.rs`, at the top of `Connection::execute` before
//! the `match stmt`:
//!
//! ```ignore
//! crate::aggcheck::Ctx::new().check(stmt, &self.queryable_tables())?;
//! ```
//!
//! It has to be there and not inside the per-arm handlers because several of
//! the checks apply to every arm — `count(a,b)` is the same defect whether it
//! is in a SELECT, an UPDATE's SET or an INSERT's VALUES — and because the
//! clause order in [`Ctx::check_stmt`] is what makes a multi-defect statement
//! report the right one.
//!
//! The catalog is the second argument and is not optional in practice. Without
//! it the walk cannot tell a name that resolves from one that does not, and
//! every statement mixing the two gets the aggregate answer where `sqlite3`
//! gives `no such column`; see the note on [`Names`]. `queryable_tables` is the
//! list the SELECT path resolves a FROM against, so the two agree on which
//! tables exist — including the schema table, which is not in the catalog.
//!
//! The existing checks in `grouping.rs` (`Plan::build`) and in `Connection::select`
//! then become redundant for the messages listed above and can be deleted; this
//! module deliberately does not touch either, because both are owned by another
//! track.
//!
//! A partial index's WHERE is the one case the hook above cannot reach: the
//! parser reads the expression at `parser.rs:2520` and drops it, so
//! `Stmt::CreateIndex` carries no expression to check. Wiring that needs a
//! `where_: Option<Expr>` field on `Stmt::CreateIndex` in `parser.rs`, bound
//! from that `self.expr()?`, and a second `check_no_group` call in the
//! `Stmt::CreateIndex` arm of `execute`. The `check_no_group` entry point is
//! public and ready for it.

use crate::error::{Error, Result, ResultCode};
use crate::parser::{
    BinOp, ColumnDef, Constraint, Expr, FromItem, InsertSource, Literal, Select, SelectBody, Stmt,
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

/// The columns a statement's FROM puts in scope, so the walk can tell a name
/// that resolves from one that does not.
///
/// A name `sqlite3` cannot resolve is `no such column`, which outranks every
/// message this module produces — but only where the walk reaches it first. A
/// static check cannot work that out on its own, so the caller hands the
/// columns in and the walk asks.
///
/// This is a list of *sources* rather than a flat set of columns, because a
/// qualified name is reported as itself: `t.a` is `no such column: t.a` and
/// `nosuchtbl.a` is `no such column: nosuchtbl.a`, whichever way it fails, so
/// answering one means knowing both which source it names and whether that
/// source is in the FROM. The three rowid names answer in every source that
/// is a rowid table, whether or not it declares a column of that name — and
/// treating them as unknown would turn `GROUP BY rowid, count(*)` into
/// `no such column: rowid`, which is what a *no* schema wrongly makes of every
/// bare name.
#[derive(Debug, Default, Clone)]
pub struct Names {
    /// Each source as `(the name a reference uses, its columns)`.
    sources: Vec<(String, Vec<String>)>,
    /// Whether any name in this statement can be judged. `false` means the
    /// question has no answer here and the walk must not ask it.
    known: bool,
    /// Whether an unresolvable source makes every name unresolvable, which is
    /// what an unknown table does: `sqlite3` refuses the table before it looks
    /// at anything else, so the answer is `no such table`, not this module's.
    dead: bool,
}

impl Names {
    /// Names for a statement whose FROM resolved to real tables.
    pub fn new(sources: Vec<(String, Vec<String>)>) -> Names {
        Names {
            sources,
            known: true,
            dead: false,
        }
    }

    /// Names for a statement this check cannot judge any name in, because it
    /// has no FROM to judge them against — an UPDATE, an INSERT, a DDL
    /// statement, or a caller that has no catalog.
    ///
    /// The aggregate checks still run and still answer; only the *name*
    /// question is off the table, so a statement whose only defect is an
    /// aggregate one is unaffected.
    pub fn unknown() -> Names {
        Names::default()
    }

    /// Names for a statement whose FROM names a table this connection does not
    /// have, so nothing in it can be judged and no aggregate check should
    /// report: `sqlite3` refuses the table before it looks at anything else,
    /// and that refusal is not this module's to make.
    pub fn unresolved() -> Names {
        Names {
            sources: Vec::new(),
            known: true,
            dead: true,
        }
    }

    /// Adds another scope's sources, for a subquery that also sees the outer
    /// query's FROM.
    ///
    /// This widens what resolves, never narrows it, so a name that already
    /// resolved still does and a name that did not is still reported. A scope
    /// that could not be judged contributes nothing, which leaves this one to
    /// decide alone -- the right answer when only one of the two is knowable.
    fn add(&mut self, other: &Names) {
        if !other.known {
            return;
        }
        if !self.known {
            *self = other.clone();
            return;
        }
        for (scope, cols) in &other.sources {
            if !self
                .sources
                .iter()
                .any(|(s, _)| s.eq_ignore_ascii_case(scope))
            {
                self.sources.push((scope.clone(), cols.clone()));
            }
        }
    }

    /// Whether this statement is one whose FROM names a table that is not
    /// there, so no aggregate answer is the right one either.
    fn is_dead(&self) -> bool {
        self.dead
    }

    /// Whether a name resolves against this FROM, or `None` when the question
    /// cannot be asked.
    ///
    /// A FROM with no source at all is a real answer and not an absence of one:
    /// a statement with no FROM resolves against nothing, so every name in it
    /// is `no such column` — which is what `SELECT nosuchcol HAVING 1` is on
    /// `sqlite3`. `None` is reserved for a statement this check has no schema
    /// for at all, which is what [`Names::unknown`] means.
    fn resolves(&self, qualifier: Option<&str>, name: &str) -> Option<bool> {
        if !self.known {
            return None;
        }
        Some(match qualifier {
            // A qualifier names one source, so the column is looked for in that
            // one alone. A qualifier that names no source fails too, and
            // `main.a` is one of those: `main` is a schema rather than a table,
            // and `sqlite3` says `no such column: main.a` for it.
            Some(q) => self
                .sources
                .iter()
                .find(|(s, _)| s.eq_ignore_ascii_case(q))
                .is_some_and(|(_, cols)| has_column(cols, name)),
            None => self.sources.iter().any(|(_, cols)| has_column(cols, name)),
        })
    }
}

/// The one side of an `AND` that survives constant folding, or `None` when the
/// clause has no such fold to make.
///
/// `sqlite3ExprSimplifiedAndOr` (`expr.c:2393`) drops a side of an `AND` when
/// the other is always true or always false: `x AND false` is `false` and
/// `x AND true` is `x`. The dropped side is not resolved at all, so a defect
/// in it is never found — which is what makes `WHERE nosuchcol AND 0` legal
/// and `WHERE nosuchcol AND 1` a `no such column`.
///
/// Only a literal counts. An arithmetic expression that happens to evaluate to
/// a constant is folded by `expr.c` too, but only when it is already known to
/// be constant, which needs a pass this check does not make; a bare literal is
/// the case the parser marks as one, and it is the one the suite reaches.
fn simplifies_and(expr: &Expr) -> Option<&Expr> {
    let Expr::Binary {
        op: BinOp::And,
        left,
        right,
        ..
    } = expr
    else {
        return None;
    };
    // `x AND false` and `false AND x` are both just `false`, so a false side
    // leaves the constant itself as the whole expression and the other side is
    // dropped without being resolved: that is what makes `nosuchcol AND 0` an
    // empty result and `0 AND nosuchcol` one too.
    //
    // `x AND true` is `x` and `true AND x` is `x`, so a true side is the one
    // dropped and the other survives, and that is what the walk is given.
    if always_true(left) == Some(true) {
        return Some(right);
    }
    if always_true(right) == Some(true) {
        return Some(left);
    }
    if always_true(left) == Some(false) || always_true(right) == Some(false) {
        return Some(&Expr::Literal(Literal::Integer(0)));
    }
    None
}

/// Whether an expression is a literal that is known to be true, or known to be
/// false. `None` when it is neither — a column, a call, or an expression this
/// check does not evaluate.
fn always_true(expr: &Expr) -> Option<bool> {
    let Expr::Literal(Literal::Integer(n)) = expr else {
        return None;
    };
    Some(*n != 0)
}

/// Whether a node's operands are folded to constants before the walk descends
/// into them, so a defect in one ends the walk of the clause rather than being
/// recorded and stepped over.
///
/// A *comparison* does: `expr.c` calls `sqlite3ExprCodeTarget` on one, and a
/// misused aggregate is not an aggregate (`resolve.c:1299` clears `is_agg`), so
/// `count(*)>0` is already the constant 0 by the time its operands would be
/// walked. That is what makes `WHERE count(*)>0 AND nosuchcol` the misuse and
/// never the name.
///
/// `AND` and `OR` do not: they are not code-generated during resolution, so
/// their operands are walked in the ordinary way and a misuse in one is
/// recorded and the walk carries on. `WHERE count(*) AND nosuchcol` is
/// `no such column: nosuchcol`.
///
/// A unary operator, a cast and an ordinary call are not folded either:
/// `WHERE -count(*) AND nosuchcol` and `WHERE abs(count(*)) AND nosuchcol` are
/// both `no such column: nosuchcol`.
fn folds_operands(expr: &Expr) -> bool {
    match expr {
        Expr::Binary { op, .. } => !matches!(op, BinOp::And | BinOp::Or),
        _ => false,
    }
}

/// Whether a source answers to `name`. The three rowid names count as columns
/// of every source, since a rowid table has them either way.
fn has_column(cols: &[String], name: &str) -> bool {
    cols.iter().any(|c| c.eq_ignore_ascii_case(name))
        || ROWID_NAMES.iter().any(|r| r.eq_ignore_ascii_case(name))
}

/// `no such column: a`, or `no such column: t.a` for a qualified one.
///
/// The two forms are `resolve.c:791-793`: a qualified and a bare name are
/// printed the same way whichever way the name failed to resolve, so the
/// message does not depend on *how* it was unresolvable — only on what it was
/// written as. A doubly-quoted name gets a different sentence
/// (`resolve.c:788-789`), which is the tokenizer's to choose and not this
/// module's, so the bare form stands for it too.
fn no_such_column(qualifier: Option<&str>, name: &str) -> String {
    match qualifier {
        Some(q) => format!("no such column: {q}.{name}"),
        None => format!("no such column: {name}"),
    }
}

/// The names a rowid table answers to besides its own columns. SQLite has
/// three, and any of them resolves in a GROUP BY like a column.
const ROWID_NAMES: [&str; 3] = ["rowid", "_rowid_", "oid"];

/// Builds the [`Names`] for a statement's FROM out of the catalog.
///
/// Every table named in the FROM contributes its columns under the name a
/// reference to it uses, which is its alias where it has one. A table the
/// catalog does not have makes the whole answer [`Names::unresolved`].
///
/// A subquery in FROM is not a table and contributes no columns; this engine
/// refuses one before any of this runs, so a query with one is answered by that
/// refusal.
pub fn names_for(tables: &[crate::catalog::Table], from: &[FromItem]) -> Names {
    let mut sources: Vec<(String, Vec<String>)> = Vec::new();
    for item in from {
        // A subquery in FROM contributes its own result columns, which this
        // cannot read without running it, and this engine refuses one outright
        // (`a subquery in FROM is not supported yet`). So no name in such a
        // statement can be judged: judging none leaves the aggregate checks
        // running and every name to the resolver, which is the only place that
        // can report either. Judging *any* of them would be reporting a
        // `no such column` for a statement whose FROM has not been resolved at
        // all, which is how `select1-18.2` came to answer `no such column: x`
        // where `sqlite3` answers `123`.
        let FromItem::Table(tref) = item else {
            return Names::unknown();
        };
        let Some(table) = find_table(tables, &tref.name) else {
            return Names::unresolved();
        };
        let scope = crate::join::Source::scope_name(tref).to_string();
        // The same source twice under one name is `ambiguous column name`, and
        // this cannot answer that, so it declines to judge any name.
        if sources.iter().any(|(s, _)| s.eq_ignore_ascii_case(&scope)) {
            return Names::unknown();
        }
        sources.push((
            scope,
            table.columns.iter().map(|c| c.name.clone()).collect(),
        ));
    }
    Names::new(sources)
}

/// A table by name, allowing the `main.` prefix that `sqlite3` accepts.
fn find_table<'a>(
    tables: &'a [crate::catalog::Table],
    name: &str,
) -> Option<&'a crate::catalog::Table> {
    tables
        .iter()
        .find(|t| t.name.eq_ignore_ascii_case(name))
        .or_else(|| {
            crate::join::strip_schema_qualifier(name)
                .and_then(|base| tables.iter().find(|t| t.name.eq_ignore_ascii_case(base)))
        })
}

/// The check for one statement.
///
/// Building it runs nothing. A fresh `Ctx` is wanted per statement, since the
/// error it carries is a property of one statement.
#[derive(Debug, Default, Clone)]
pub struct Ctx<'a> {
    /// The first defect found, in walk order.
    message: Option<String>,
    /// The columns the statement's FROM puts in scope, so a name the walk
    /// reaches is known to resolve before an aggregate defect is reported.
    ///
    /// `None` until [`Ctx::check`] sets it. [`Ctx::check_no_group`] leaves it
    /// unset, which is right: a CHECK and a partial index's WHERE are resolved
    /// with no FROM at all, so a name in one is not this check's to judge.
    names: Option<Names>,
    /// The catalog a nested SELECT's own FROM is read out of, since a name in a
    /// subquery is judged against the subquery's FROM and not the enclosing
    /// one's.
    tables: &'a [crate::catalog::Table],
    /// The catalog as the caller passed it. Kept so a walk that points
    /// [`Ctx::tables`] at something else can put it back.
    outer_tables: &'a [crate::catalog::Table],
    /// Whether a `no such column` has been recorded, which stops the statement's
    /// resolution and so puts every later aggregate verdict out of reach.
    ///
    /// Kept apart from `message` because the two are reached at different
    /// times: an aggregate verdict is computed as the walk goes, while a name
    /// error is raised from the walk itself and outlives whatever came before
    /// it. See [`Ctx::name_error`].
    name_failed: bool,
    /// Whether the node now being walked sits under an operator or a call that
    /// would fold, so a defect found in it stops the walk rather than being
    /// recorded and stepped over.
    ///
    /// This is the `resolve.c:1299` rule made explicit: a misused aggregate is
    /// not an aggregate, so its parent folds to a constant and is never walked
    /// into, while a bare call is a node in its own right and the walk goes on
    /// to the next one. [`Ctx::walk`] threads it down, and [`Ctx::fail`] and
    /// [`Ctx::name_error`] set it.
    folded: bool,
    /// Whether the clause now being walked has already found a defect, so its
    /// remaining terms are not walked and a later name cannot overwrite what it
    /// found.
    clause_failed: bool,
    /// Whether the expression now being walked is in the result-column list,
    /// which SQLite resolves before every other clause and whose failure ends
    /// the statement.
    projection: bool,
}

impl<'a> Ctx<'a> {
    /// A fresh check.
    pub fn new() -> Ctx<'a> {
        Ctx::default()
    }

    /// Checks a statement and reports the error `sqlite3` would report, if any.
    ///
    /// `tables` is the catalog, from which each SELECT's own FROM is read as the
    /// walk descends; see [`Names`]. A statement whose only defect is an
    /// aggregate one is unaffected by it, so [`Ctx::check_without_names`] is
    /// this call with no catalog at all.
    pub fn check(&mut self, stmt: &Stmt, tables: &'a [crate::catalog::Table]) -> Result<()> {
        // The statement's own FROM, read once: every name outside a nested
        // SELECT is judged against it.
        self.tables = tables;
        self.outer_tables = tables;
        // An *empty* catalog is not an unresolved FROM — it is a caller with no
        // schema, which is [`Ctx::check_without_names`]'s case and must not make
        // every FROM look unresolvable. A non-empty catalog is a real answer,
        // and a table missing from it really is missing.
        self.names = Some(if tables.is_empty() {
            Names::unknown()
        } else {
            match stmt {
                Stmt::Select(sel) => self.names_for_select(sel),
                _ => Names::unknown(),
            }
        });
        // A FROM that names a table this connection does not have is refused by
        // the FROM's own resolution, before any of this runs, and that refusal
        // is the answer — so this check stands aside entirely. Any other name
        // set is judged.
        if !self.names.as_ref().is_some_and(Names::is_dead) {
            self.check_stmt(stmt);
        }
        self.take()
    }

    /// Checks a statement with no schema, so an unresolvable name is invisible
    /// to the walk.
    ///
    /// This is what the tests in this module drive, and what a caller that has
    /// no catalog should use. It answers every statement whose only defect is
    /// an aggregate one, and is *not* what `sqlite3` says for a statement that
    /// mixes a bad name with one of those: see the module note on [`Names`].
    pub fn check_without_names(&mut self, stmt: &Stmt) -> Result<()> {
        self.check(stmt, &[])
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

    /// Records a `no such column`, which every other message loses to.
    ///
    /// This is a different kind of failure from [`Ctx::fail`], and the
    /// difference decides most of the answers. `sqlite3ErrorMsg` frees the old
    /// message and keeps the new one (`util.c:268`), so a `no such column`
    /// overwrites whatever was recorded before it — including an aggregate
    /// defect the walk had already found. The walk itself is *not* stopped,
    /// because `resolveExprStep` goes on to resolve the rest of the tree
    /// (`resolve.c:1354`); only the aggregate verdicts that run after the
    /// resolution as a whole — the `EP_Agg` test over the GROUP BY at
    /// `resolve.c:2109` — are out of reach.
    ///
    /// The two halves of that are both visible in the oracle:
    ///
    /// | statement | sqlite3 |
    /// |---|---|
    /// | `WHERE nosuchcol AND count(*)>0` | `no such column: nosuchcol` |
    /// | `WHERE count(*)>0 AND nosuchcol` | `misuse of aggregate function count()` |
    /// | `WHERE count(a,b) AND nosuchcol` | `no such column: nosuchcol` |
    /// | `ORDER BY nosuchcol, count(*)` | `no such column: nosuchcol` |
    /// | `GROUP BY nosuchcol, min()` | `no such column: nosuchcol` |
    ///
    /// The third and fourth rows are the two directions: in a WHERE a name
    /// later than an arity error still wins, because the walk continues to it;
    /// in an ORDER BY the clause-level refusal never runs at all, so it is not
    /// a ranking between messages but an ordering between *phases*.
    fn name_error(&mut self, message: String) {
        // A name error overwrites whatever came before it, because
        // `sqlite3ErrorMsg` frees the old message and keeps the new one
        // (`util.c:268`) and the name is reached *later* than an aggregate
        // verdict in the same clause: `WHERE count(*) AND nosuchcol` is
        // `no such column: nosuchcol`.
        //
        // Two names are the one exception, because the first one aborts the walk
        // and the second is never reached: `WHERE nosuchcol AND nosuchcol2` is
        // `no such column: nosuchcol`. That is what `name_failed` records.
        // A name error overwrites an *aggregate* message -- the name is the
        // later one and `sqlite3ErrorMsg` keeps the later message
        // (`util.c:268`) -- but not another name, which is the left-to-right
        // order of the walk: `WHERE nosuchcol AND nosuchcol2` is
        // `no such column: nosuchcol` and `WHERE nosuchcol2 AND nosuchcol` is
        // `no such column: nosuchcol2`.
        //
        // A name is also the one failure that *ends* the walk, since
        // `resolve.c:1505` returns `WRC_Abort` from the node that failed
        // whatever node that was. `folded` says whether the node was already
        // under a comparison, which does not change the message, only whether
        // the parent had already folded by the time it was recorded.
        if !self.name_failed {
            self.message = Some(message);
        }
        self.name_failed = true;
    }

    /// Whether a name resolves against the statement's FROM, or `None` when
    /// this check has no schema to answer with.
    ///
    /// A bare name that is a result-column alias resolves as well, whatever the
    /// FROM holds, so `aliases` is consulted first. A *qualified* name is
    /// never an alias — no alias is a column of any table — so only a bare one
    /// can be.
    fn resolves(&self, aliases: &Aliases<'_>, qualifier: Option<&str>, name: &str) -> Option<bool> {
        if qualifier.is_none() && aliases.iter().any(|(a, _, _)| a == name) {
            return Some(true);
        }
        self.names.as_ref()?.resolves(qualifier, name)
    }

    /// Records a defect found under an operator, which stops the walk.
    ///
    /// A misused aggregate is not an aggregate (`resolve.c:1299` clears
    /// `is_agg`), so an operator holding one folds to a constant before the
    /// walk descends into it, and the rest of the clause is never walked. A
    /// bare call is the opposite: it is a node of its own, so the walk records
    /// the verdict and carries on to the next node.
    fn fail(&mut self, message: String) {
        if self.message.is_none() {
            self.message = Some(message);
        }
        if self.folded {
            // The node this was found in is under something that folds, so the
            // walk of the clause ends here and nothing after it is reached.
            self.name_failed = true;
        }
    }

    /// Records an arity error, which never ends the walk.
    ///
    /// `resolveExprStep` decides a call's arity from the function's own
    /// registration, before the arguments are resolved, and the walk carries on
    /// to them (`resolve.c:1354`). So an arity error is a *node* verdict and a
    /// later name in the same clause still overwrites it:
    /// `WHERE min() AND nosuchcol` is `no such column: nosuchcol`, and
    /// `ORDER BY min(), nosuchcol` is the arity error only because the name is
    /// in a *sibling* term that the earlier abort skips.
    fn fail_arity(&mut self, message: String) {
        if self.message.is_none() {
            self.message = Some(message);
        }
        // A name in a *sibling term* of an ORDER BY or a GROUP BY does not
        // overwrite this, because those clauses are a list of terms resolved one
        // after another and the walk stops at the term that failed
        // (`resolve.c:1870`). So `ORDER BY min(), nosuchcol` is `min`'s arity
        // error and `ORDER BY nosuchcol, min()` is the name's.
        //
        // A name in the *same* expression still does, and a name in an earlier
        // clause never does: `SELECT count(a,b) FROM t WHERE nosuchcol` is the
        // arity error, because the projection is resolved before the WHERE
        // (`resolve.c:2001`) and the walk stops there.
        // The walk of the *expression* this was found in ends here
        // (`resolve.c:1505` returns `WRC_Abort` from the node that failed), and
        // what that covers is the expression and no more. A comma puts the rest
        // in a different expression, so the terms after it are still resolved:
        // `ORDER BY min(), nosuchcol` is the arity error and
        // `ORDER BY nosuchcol, min()` is the name. An `AND` does not, because
        // both sides are the one expression, so the walk carries on past the
        // bare node the verdict was recorded at: `WHERE min() AND nosuchcol` is
        // `no such column: nosuchcol` and so is `WHERE count(a,b) AND
        // nosuchcol`.
        self.clause_failed = true;
        // A projection is resolved on its own and the walk stops on failure
        // (`resolve.c:2001` into `resolve.c:1505`), so a name in a later clause
        // never overwrites what it found: `SELECT count(a,b) FROM t WHERE
        // nosuchcol` is the arity error.
        if self.projection {
            self.name_failed = true;
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
                InsertSource::Select(sel) => {
                    // The source query has a FROM of its own, so its names are
                    // read from the catalog. A table it does not have makes the
                    // whole check stand aside: `sqlite3` refuses the table
                    // before it looks at anything else, and
                    // `INSERT INTO t(a,b) SELECT x FROM nosuchtable` is
                    // `no such table: nosuchtable` and not `no such column: x`.
                    let saved = self.tables;
                    self.tables = self.outer_tables;
                    self.names = Some(self.names_for_select(sel));
                    if !self.names.as_ref().is_some_and(Names::is_dead) {
                        self.check_select(sel);
                    }
                    self.tables = saved;
                }
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
    ///
    /// `tables` is the catalog, needed because a SELECT nested in another one
    /// has a FROM of its own and so a set of names of its own. A LIMIT and an
    /// OFFSET are resolved with an *empty* NameContext
    /// (`resolve.c:1930-1936`), so no name in them resolves: not a column of
    /// the FROM, and not a result alias either. An aggregate that reaches
    /// no column — `LIMIT count(*)`, `LIMIT count(1)`, `LIMIT min(1)` — is
    /// refused as a misuse, while one that names a column is
    /// `no such column: a`. `Scope::Limit` keeps that second case in, so the
    /// walk reports it from the *enclosing* statement's columns, which is what
    /// `resolve.c` has at the point it resolves them: a LIMIT is resolved as
    /// part of the statement it belongs to, and only its *name context* is
    /// empty.
    fn check_select(&mut self, sel: &Select) {
        for cte in &sel.with {
            self.check_select(&cte.select);
        }
        // The names of *this* SELECT's FROM, which is what a name in its own
        // clauses is judged against. A nested SELECT replaces the outer set for
        // the length of its own walk, so a subquery's columns are its own and
        // not the enclosing query's.
        //
        // A `no such column` ends the resolution of the SELECT that reached it
        // and nothing else, so the flag is saved with the names: a subquery
        // that fails does not silence the enclosing statement's own checks, and
        // an enclosing one that has already failed does not silence the
        // subquery's.
        let saved = self.names.clone();
        let saved_failed = self.name_failed;
        self.names = Some(self.names_for_select(sel));
        self.name_failed = false;
        // LIMIT and OFFSET are resolved before every other clause of the
        // SELECT -- `resolve.c:1930` resolves them against an empty NameContext
        // before the body is walked at all -- so they come first here too. That
        // is what makes `ORDER BY nosuchcol LIMIT min()` the arity error and not
        // the name.
        let saved_names = self.name_failed;
        self.name_failed = false;
        for e in sel.limit.iter().chain(sel.offset.iter()) {
            if self.name_failed {
                break;
            }
            self.walk_clause(e, Scope::Limit, &[]);
        }
        if !self.name_failed {
            self.name_failed = saved_names;
        }
        self.check_body(&sel.body, &sel.order_by);
        self.names = saved;
        self.name_failed = saved_failed;
    }

    /// The names a SELECT's own FROM puts in scope.
    ///
    /// A body that is not a simple one — a compound arm, or a nested body
    /// reached through [`Ctx::check_body`] — contributes no FROM of its own, so
    /// the enclosing set stands.
    fn names_for_select(&self, sel: &Select) -> Names {
        // No catalog is a caller with no schema, so no name can be judged at
        // any depth — a nested SELECT's FROM is as unanswerable as the outer
        // one's, and asking it with an empty table list would call every name
        // unresolvable.
        if self.tables.is_empty() {
            return Names::unknown();
        }
        // A scalar subquery is parsed as a nested body wrapping the simple one
        // that holds the FROM (`parser.rs:844`), so the nesting is followed
        // rather than read at the outer shape only. Reading it that way is what
        // leaves an inner SELECT judging its names against the *enclosing*
        // query's columns, which is how `WHERE x BETWEEN (SELECT x FROM
        // (SELECT x FROM t2 WHERE x=c), t1 WHERE x=c) AND (c+1)` came to answer
        // `no such column: c` for a `c` that `t1` has.
        let mut body = &sel.body;
        while let SelectBody::Nested(inner) = body {
            body = &inner.body;
        }
        match body {
            // A subquery sees the *outer* query's FROM as well as its own: a
            // name it does not resolve against its own tables is looked for in
            // the enclosing ones, and that is what makes it correlated
            // (`resolveExprNames` walks the `pNext` chain, `resolve.c:699`).
            // `WHERE x IN (SELECT x FROM t2 WHERE x=c)` resolves `c` through the
            // outer `t1` and is legal, while the same subquery in a query with
            // no `t1` is `no such column: c`.
            //
            // So the two sets are unioned: a name in either resolves. A name in
            // neither is the answer, and the outer query's own names are still
            // in `self.names` here.
            SelectBody::Simple { from, .. } => {
                let mut n = names_for(self.tables, from);
                if let Some(outer) = &self.names {
                    n.add(outer);
                }
                n
            }
            // A compound has no FROM of its own here, so the enclosing set
            // stands: each of its arms is reached through `check_body`, which
            // installs that arm's own names.
            _ => self.names.clone().unwrap_or_default(),
        }
    }

    /// A select body, which is one arm of a compound or a nested one.
    fn check_body(&mut self, body: &SelectBody, order_by: &[(Expr, bool)]) {
        // A defect already recorded does not stop the statement: a later clause
        // may hold a `no such column`, and that one overwrites. What stops the
        // statement is a name error, because the phase it belongs to -- the
        // clause-level `EP_Agg` test -- is never reached after one.
        if self.name_failed {
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
                if self.name_failed {
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
                let saved_projection = self.projection;
                self.projection = true;
                for rc in columns {
                    self.walk_clause(&rc.expr, Scope::Grouped, &aliases);
                    if self.name_failed {
                        break;
                    }
                }
                self.projection = saved_projection;
                // A projection that failed ends the statement: the clauses after
                // it are never resolved (`resolve.c:2001` into `resolve.c:1505`),
                // so a name in one of them cannot overwrite what it found.
                if self.name_failed {
                    return;
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
                    self.walk_clause(h, Scope::Grouped, &aliases);
                    if self.name_failed {
                        return;
                    }
                }

                // 3. WHERE, which never folds, and where the two misuse
                //    wordings differ.
                if let Some(w) = where_ {
                    let s = if agg_in_scope {
                        Scope::MisuseShort
                    } else {
                        Scope::NoGroup
                    };
                    // Each clause is a walk of its own, so what an earlier one
                    // recorded does not carry into this one -- a bad name in the
                    // projection is `no such column` and the ORDER BY beside it
                    // is still resolved.
                    // Each clause is resolved on its own and the walk stops at
                    // The WHERE is a walk of its own (`resolve.c:2036`) and what
                    // it finds is kept: a later clause does not overwrite it,
                    // so `WHERE count(a,b) ORDER BY nosuchcol` is the arity
                    // error and `WHERE nosuchcol ORDER BY count(a,b)` is the
                    // name.
                    //
                    // A *bare* defect in the WHERE is the exception, because a
                    // bare node is stepped over rather than aborting: `WHERE
                    // count(*) AND nosuchcol` is the name. Only a defect that
                    // ended the walk of the clause carries out of it, and that
                    // is what `name_failed` and `clause_failed` mean here.
                    let saved = self.name_failed;
                    self.name_failed = false;
                    self.clause_failed = false;
                    self.walk_clause(w, s, &aliases);
                    // An arity error in the WHERE ends the statement the same
                    // way: `resolve.c:1505` returns `WRC_Abort` from the node
                    // that failed, and the ORDER BY is resolved after the WHERE
                    // (`resolve.c:2088`), so it is never reached. That is what
                    // makes `WHERE count(a,b) ORDER BY nosuchcol` the arity
                    // error while `WHERE nosuchcol ORDER BY count(a,b)` is the
                    // name.
                    if self.clause_failed || self.name_failed {
                        self.name_failed = true;
                    } else {
                        self.name_failed = saved;
                    }
                }

                // 3b. The ON constraints of the joins, in FROM order, which
                //     SQLite resolves after the WHERE and before the ORDER BY
                //     (`resolve.c:2072`). A name in one is a name, so this has
                //     to be walked for the same reason the WHERE is.
                for item in from {
                    if let FromItem::Table(tref) = item {
                        if let Some(on) = &tref.on {
                            let saved = self.name_failed;
                            self.name_failed = false;
                            self.clause_failed = false;
                            self.walk_clause(on, Scope::MisuseShort, &aliases);
                            if self.clause_failed || self.name_failed {
                                self.name_failed = true;
                            } else {
                                self.name_failed = saved;
                            }
                        }
                    }
                }
                if self.name_failed {
                    return;
                }

                // 4. ORDER BY, which folds per group when there is one.
                //
                //    A `no such column` reached anywhere in the clause ends the
                //    statement: SQLite resolves the ORDER BY in a walk of its
                //    own (`resolve.c:2088`) and a failure there returns
                //    `WRC_Abort` for the whole SELECT, so the `EP_Agg` test over
                //    the GROUP BY that follows (`resolve.c:2109`) is never
                //    reached. That is why `ORDER BY nosuchcol, count(*)` is
                //    `no such column: nosuchcol` and not `misuse of aggregate:
                //    count()`.
                //
                //    Every term is walked even after one has failed, because a
                //    *bare* defect is a node of the term and the next term is a
                //    node of its own: `ORDER BY count(*), nosuchcol` is
                //    `no such column: nosuchcol`, while `ORDER BY min(),
                //    nosuchcol` is the arity error, which ends the term.
                for (e, _) in order_by {
                    if self.name_failed {
                        break;
                    }
                    self.walk_clause(e, order_scope, &aliases);
                }
                if self.name_failed {
                    return;
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
                    // A name in the term that resolves to nothing is reached
                    // before the term's own verdict, because `sqlite3` walks
                    // the term's nodes and stops at the first one that fails:
                    // `GROUP BY nosuchcol, max(m)` is `no such column:
                    // nosuchcol` and not the misuse of `m`, while
                    // `GROUP BY max(m), nosuchcol` is the misuse, because the
                    // aggregate comes first in that one. The walk does this
                    // itself, so it has to run before the alias verdict below.
                    self.walk_clause(e, Scope::GroupBy, &aliases);
                    if self.clause_failed {
                        // This term failed, so `resolveOrderGroupBy` returns and
                        // the terms after it are never resolved: `GROUP BY a,
                        // min(), nosuchcol` is `min`'s arity error and not the
                        // name that follows it.
                        self.name_failed = true;
                        return;
                    }
                    if self.message.is_some() && !self.name_failed {
                        return;
                    }
                    if self.name_failed {
                        // The term failed to resolve, so the whole clause is
                        // abandoned and the `EP_Agg` test never runs. The terms
                        // after it are still walked, each being a node of its
                        // own, so a name in one of them is still found.
                        continue;
                    }
                    match group_by_term(e, &aliases) {
                        GroupByTerm::Alias(_) => {
                            misuse.get_or_insert_with(|| group_by_term(e, &aliases));
                            break;
                        }
                        GroupByTerm::Aggregate => refusal = true,
                        _ => {}
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
    fn walk_clause(&mut self, expr: &Expr, scope: Scope, aliases: &Aliases<'_>) {
        self.clause_failed = false;
        // A clause's top-level expression is walked as it is written, so the
        // fold of whatever clause came before it does not carry in: the first
        // node of a clause is never under an operator of this clause.
        self.folded = false;
        self.walk(expr, scope, aliases);
        if self.clause_failed {
            // A defect in a term of a list ends the walk of the terms after it,
            // and of the clause-level verdicts that would follow them.
            self.name_failed = true;
        }
    }

    /// Walks one expression, checking every call in it.
    fn walk(&mut self, expr: &Expr, scope: Scope, aliases: &Aliases<'_>) {
        // A name is asked about here, at the point the walk would reach it, and
        // it is the one failure that always ends the walk: `resolve.c:1505`
        // returns `WRC_Abort` from the node that failed whatever that node was.
        // Two names are therefore reported left to right and the first wins,
        // and a name beats any aggregate message because it is the later one
        // written and `sqlite3ErrorMsg` keeps the later message (`util.c:268`).
        if let Expr::Column { table, name, .. } = expr {
            if let Some(false) = self.resolves(aliases, table.as_deref(), name) {
                self.name_error(no_such_column(table.as_deref(), name));
                return;
            }
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
                    let before = self.names.clone();
                    self.check_select(select);
                    self.names = before;
                }
                Expr::InSelect {
                    expr: e, select, ..
                } => {
                    self.walk(e, scope, aliases);
                    self.check_select(select);
                }
                _ => {
                    // Only a *binary* operator folds its operands. `expr.c` calls
                    // `sqlite3ExprCodeTarget` on a comparison before the walk
                    // descends, and a misused aggregate is not an aggregate
                    // (`resolve.c:1299` clears `is_agg`), so `count(*)>0` is
                    // already the constant 0 by the time its operands are
                    // reached: `WHERE count(*)>0 AND nosuchcol` is the misuse
                    // and never reaches the name.
                    //
                    // A unary operator, a cast and an ordinary call are walked
                    // into as ordinary nodes, so a misuse in one is recorded and
                    // the walk carries on: `WHERE -count(*) AND nosuchcol` and
                    // `WHERE abs(count(*)) AND nosuchcol` are both
                    // `no such column: nosuchcol`.
                    // An `AND` with a constant on one side that decides it
                    // drops the other side entirely
                    // (`sqlite3ExprSimplifiedAndOr`, `expr.c:2393-2401`): `x AND
                    // false` is `false` and `x AND true` is `x`, so the dropped
                    // side is never resolved and a defect in it is never found.
                    // That is why `WHERE nosuchcol AND 0` is not an error while
                    // `WHERE nosuchcol AND 1` is.
                    if let Some(keep) = simplifies_and(expr) {
                        // The side that decides the `AND` is the one that
                        // survives, and the other is dropped without being
                        // resolved at all (`expr.c:2393`). So `nosuchcol AND 0`
                        // has no defect in it while `0 AND nosuchcol` is still
                        // resolved -- the constant is the *left* side there, and
                        // the right one is the one dropped.
                        let saved = self.folded;
                        self.folded = false;
                        self.walk(keep, scope, aliases);
                        self.folded = saved;
                        return;
                    }
                    let saved = self.folded;
                    self.folded = folds_operands(expr);
                    for c in children(expr) {
                        if self.name_failed {
                            break;
                        }
                        self.walk(c, scope, aliases);
                    }
                    self.folded = saved;
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
            // not named in the message. A call is a node of its own rather than
            // something that folds, so the walk goes on to the next node after a
            // defect in one of its arguments: `abs(nosuchcol) AND min()` is the
            // arity error of the `min`, and `min() AND abs(nosuchcol)` is the
            // `no such column`.
            let saved = self.folded;
            self.folded = false;
            for a in args {
                if self.name_failed || self.clause_failed {
                    break;
                }
                self.walk(a, scope, aliases);
            }
            self.folded = saved;
            return;
        }

        // The call's own arity, before anything inside it — including before
        // the alias rule. `sum(m,1)` is `wrong number of arguments to function
        // sum()` even where `m` is an alias standing for an aggregate, because
        // SQLite has already found that no function takes two arguments by the
        // time it walks anything. The alias is only substituted on the way
        // *into* an argument, and there is no argument to walk.
        //
        // The argument list is resolved *before* this message is written
        // (`resolve.c:1354` walks it after recording the verdict, and
        // `sqlite3ErrorMsg` keeps the later message), so a name inside the
        // arguments outranks an arity error on the call that holds them:
        // `count(nosuchcol, a)` is `no such column: nosuchcol` and
        // `min(nosuchcol, b)` is the same, while `count(a, b)` — every name
        // resolving — is the arity error.
        match arity_verdict(expr) {
            Some(Arity::Wrong(n)) => {
                let arity = format!("wrong number of arguments to function {n}()");
                for a in args {
                    self.walk(a, scope, aliases);
                }
                if self.name_failed {
                    return;
                }
                self.fail_arity(arity);
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
                        self.fail_arity(format!("wrong number of arguments to function {n}()"));
                    }
                    Inner::Misuse(n) => {
                        self.fail(format!("misuse of aggregate function {n}()"));
                    }
                }
                return;
            }
        }

        // A fold where no group exists. The arguments are walked first, because
        // `resolveExprStep` records the misuse and *then* resolves them
        // (`resolve.c:1354`), so a name in one is the later message and is what
        // `sqlite3ErrorMsg` keeps: `WHERE count(nosuchcol)` and
        // `ORDER BY count(nosuchcol)` are both `no such column: nosuchcol`.
        match scope {
            Scope::NoGroup | Scope::MisuseShort => {
                let msg = if matches!(scope, Scope::NoGroup) {
                    format!("misuse of aggregate function {name}()")
                } else {
                    format!("misuse of aggregate: {name}()")
                };
                for a in args {
                    if self.name_failed {
                        break;
                    }
                    self.walk(a, scope, aliases);
                }
                if self.name_failed {
                    return;
                }
                self.fail(msg);
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
