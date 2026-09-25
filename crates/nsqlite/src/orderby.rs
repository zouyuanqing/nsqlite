//! The ORDER BY key: what each term in an ORDER BY names, and where it reads.
//!
//! An ORDER BY term is not an ordinary expression. SQLite gives each one three
//! chances to name something, in an order that is easy to get wrong:
//!
//! 1. An **integer literal** is an *ordinal*: `ORDER BY 2` is the second result
//!    column, not the constant 2. Sorting on the constant would leave every row
//!    equal, so the direction would be invisible and the result would be the
//!    rows in the order they arrived.
//! 2. A **name that is exactly a result column** reads that projected value.
//!    The first result column with that name wins, whether it got the name from
//!    an explicit `AS` or from being a bare column reference.
//! 3. Otherwise the term is an **expression** over the FROM, and an *output
//!    alias* named anywhere inside it is replaced by the expression that alias
//!    stands for, before the expression is evaluated.
//!
//! Those three are not interchangeable, and the differences are all observable:
//!
//! * `SELECT a AS b FROM t ORDER BY b` sorts on the alias, because the term is
//!   exactly a name. `SELECT a AS b FROM t ORDER BY b+1` sorts on the *table*
//!   column `b`, because inside an expression a real column wins over an alias.
//! * `SELECT a AS zz FROM t ORDER BY zz+1` works at all only because there is no
//!   column `zz` to shadow: with no real column of that name, the alias is
//!   substituted into the expression.
//! * The ordinal is checked against the number of *result* columns, so a star
//!   counts as every column it expanded to.
//!
//! Everything here is decided once per query, before any row is read, because
//! none of it depends on a row: `SELECT ... WHERE 0 ORDER BY 9` has to fail the
//! same way as one that matches something.

use crate::error::{Error, Result, ResultCode};
use crate::join::From;
use crate::parser::{Expr, Literal, ResultColumn, Select, UnaryOp};
use crate::value::Value;

/// The largest ordinal whose range error SQLite settles in term order, beside
/// the names, rather than holding back for them.
///
/// The lower bound is a sign test, so every ordinal of zero or less is eager.
/// The upper bound is a 16-bit compare, so it only catches `0x10000` and up —
/// and only for an ordinal that is also out of the result width, since a width
/// that large is not a thing a statement has. The split is an artifact of
/// SQLite's two bounds being different widths, and it is only visible in a
/// statement carrying both faults at once:
///
/// ```text
/// SELECT a,b FROM t ORDER BY 9, 65535, zzz;  ->  no such column: zzz
/// SELECT a,b FROM t ORDER BY 9, 65536, zzz;  ->  1st ORDER BY term out of range
/// ```
///
/// Anything between the two bounds is a *deferred* ordinal: it is range-checked
/// only after every name in the statement has been resolved, and reported at the
/// first of them that is out of range.
const EAGER_UPPER: i64 = 0x1_0000;

/// How one ORDER BY term found the value it sorts on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyKind {
    /// An integer literal naming a result column, 1-based.
    Ordinal,
    /// A name that is exactly a result column, reading the projected value.
    OutputName,
    /// An expression, evaluated against the FROM with any alias substituted in.
    Expression,
}

/// One ORDER BY term, already decided.
#[derive(Debug, Clone)]
pub struct Key {
    /// Which of the three rules matched.
    pub kind: KeyKind,
    /// For an ordinal and an output name, the 0-based result column it names.
    /// Both are in range: the range check ran before this was built. It is
    /// `None` for an expression, which has no single column to point at.
    pub at: Option<usize>,
    /// The expression to evaluate per row. It is `None` for an ordinal and an
    /// output name, because both read a projected value instead.
    pub expr: Option<Expr>,
    /// `true` for ASC, which is the default, and `false` for DESC.
    pub ascending: bool,
}

/// Decides what every ORDER BY term of a query means.
///
/// `names` is the result list the caller will report, which is what the ordinal
/// is checked against and what a bare name is matched in. `columns` is the
/// SELECT list those names were built from, which is where a substituted alias
/// reads its replacement from; a star passes the single `*` that was written,
/// since the names are already its expansion and it has no written expressions
/// to substitute.
///
/// `from` is the statement's resolved FROM, and it is what decides the one
/// asymmetry in the rules: inside an expression a real column wins over an alias
/// of the same name, so `SELECT a AS b FROM t ORDER BY b+1` sorts on the table's
/// `b` rather than on the alias. Without a FROM there is no real column to
/// shadow anything, so every unmatched name is an alias.
///
/// The check runs over every term before the caller reads a row, which is what
/// makes the error independent of the data.
pub fn resolve_keys(
    sel: &Select,
    columns: &[ResultColumn],
    names: &[String],
    from: Option<&From>,
) -> Result<Vec<Key>> {
    let width = names.len();
    // An integer literal is an ordinal into the result columns, checked
    // against the width the caller will report, so a star counts as every
    // column it expanded to. The range check and the name lookup are settled in
    // two passes because SQLite settles them in two: an eager ordinal is
    // reported in the statement's own term order, competing with the names for
    // the same walk, while a deferred one waits until every name has been
    // resolved.
    let mut deferred: Option<usize> = None;
    let mut keys = Vec::with_capacity(sel.order_by.len());
    for (i, (expr, ascending)) in sel.order_by.iter().enumerate() {
        if let Some(n) = ordinal_of(expr) {
            if n < 1 || n > width as i64 {
                if is_eager(n) {
                    return Err(out_of_range(i + 1, width));
                }
                // The first deferred one is the one that will be reported, but
                // the walk continues: a name below it is reported first.
                deferred.get_or_insert(i + 1);
                keys.push(constant_key(*ascending, expr));
                continue;
            }
            keys.push(Key {
                kind: KeyKind::Ordinal,
                at: Some(n as usize - 1),
                expr: None,
                ascending: *ascending,
            });
            continue;
        }
        // A name that is exactly a result column reads the projection. It is
        // only a name if nothing else is wrapped around it: `bb` is the alias,
        // `bb+0` is an expression.
        if let Expr::Column {
            table: None, name, ..
        } = expr
        {
            if let Some(at) = position_under(columns, names, name) {
                keys.push(Key {
                    kind: KeyKind::OutputName,
                    at: Some(at),
                    expr: None,
                    ascending: *ascending,
                });
                continue;
            }
        }
        // Anything else is an expression, with the aliases substituted into it
        // so that a name no source column answers to still reads something. A
        // name that is left over once the substitution has run answers to
        // nothing at all, which SQLite reports rather than evaluating.
        let expr = substitute(columns, names, from, expr);
        if let Some(err) = unresolved(&expr, names, from) {
            return Err(err);
        }
        keys.push(Key {
            kind: KeyKind::Expression,
            at: None,
            expr: Some(expr),
            ascending: *ascending,
        });
    }
    // The names are all resolved, so a deferred ordinal has nothing to lose to
    // and the first of them is reported.
    if let Some(term) = deferred {
        return Err(out_of_range(term, width));
    }
    Ok(keys)
}

/// The error for the first name in an ORDER BY expression that resolves to
/// nothing, once the aliases have been substituted into it.
///
/// The substitution has already run, so a name an alias stood for is gone and
/// only a name that is neither a source column, nor an alias, nor a result
/// column is left. A result column counts as one here because a bare name
/// already became an [`KeyKind::OutputName`] above and never reaches here; a
/// name inside an expression that also carries an alias for it does, and SQLite
/// reads the alias, so the name is not reported.
///
/// A qualified name is never an alias, so `ORDER BY t.zzz` is reported under its
/// written form, `no such column: t.zzz`, whatever the result is called.
fn unresolved(expr: &Expr, names: &[String], from: Option<&From>) -> Option<Error> {
    let mut found = None;
    let mut refs = Vec::new();
    collect_columns(expr, &mut refs);
    for (table, name) in refs {
        // A result column may stand in for a name, but only an unqualified one,
        // and only a name that is absent rather than ambiguous.
        if table.is_none() && position_of(names, &name).is_some() {
            continue;
        }
        let from = match from {
            Some(f) => f,
            // With no FROM there is nothing an expression can name but the
            // result columns, and a qualified name names no table at all.
            None => {
                found = Some(match table {
                    Some(t) => format!("no such column: {t}.{name}"),
                    None => format!("no such column: {name}"),
                });
                break;
            }
        };
        if let Err(e) = crate::join::resolve_ref(from, table.as_deref(), &name, false) {
            found = Some(e.message);
            break;
        }
    }
    found.map(|message| Error::new(ResultCode::Error, message))
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
            if let Some(e) = otherwise {
                collect_columns(e, out);
            }
        }
        // A literal, a parameter, and a subquery hold no column reference. A
        // subquery has its own FROM and resolves there, which is the caller's
        // job rather than this one's.
        Expr::Literal(_)
        | Expr::NamedParameter(..)
        | Expr::InSelect { .. }
        | Expr::Exists { .. }
        | Expr::Subquery { .. } => {}
    }
}

/// Whether an out-of-range ordinal is settled in term order, beside the names,
/// rather than after them. See [`EAGER_UPPER`].
///
/// The width is not consulted, because a statement never has enough result
/// columns for the upper bound to land inside the range: `EAGER_UPPER` is
/// 65536, and a SELECT list that long is not a thing a statement is written
/// with.
fn is_eager(n: i64) -> bool {
    n <= 0 || n >= EAGER_UPPER
}

/// A key on a constant, which is what an ordinal that was refused still sorts
/// on. The statement is refused before any row is read, so the key is never
/// used; it exists so that a caller which ignores the error still holds one key
/// per term.
fn constant_key(ascending: bool, expr: &Expr) -> Key {
    Key {
        kind: KeyKind::Expression,
        at: None,
        expr: Some(expr.clone()),
        ascending,
    }
}

/// The integer an ORDER BY term is an ordinal for, if it is one.
///
/// A literal integer is one, and so is a sign applied to one: the parser folds
/// `-1` and `-(-1)` while reading, so `ORDER BY +2` is out of range in a
/// one-column query just as `ORDER BY 2` is. Any other operator on the literal
/// (`~1`, `NOT 1`) is an ordinary expression, and so is anything with
/// arithmetic in it (`1+0`), which sorts on a constant.
///
/// A `Negate` node is not one, because the parser only leaves it when the sign
/// could not be folded into the literal, which is when the value depends on a
/// row: `ORDER BY -(a)` is an expression, not an ordinal.
pub fn ordinal_of(expr: &Expr) -> Option<i64> {
    match expr {
        Expr::Literal(Literal::Integer(n)) => Some(*n),
        Expr::Unary {
            op: UnaryOp::Plus,
            expr,
        } => ordinal_of(expr),
        _ => None,
    }
}

/// The `Nth ORDER BY term out of range` error for the `n`th term of a query
/// whose result has `width` columns.
pub fn out_of_range(n: usize, width: usize) -> Error {
    Error::new(
        ResultCode::Error,
        format!(
            "{} ORDER BY term out of range - should be between 1 and {}",
            ordinal_suffix(n),
            width
        ),
    )
}

/// A count as an English ordinal: 1st, 2nd, 3rd, 4th, 11th, 21st, 112th.
///
/// The teens are the exception the last digit alone would get wrong, so they
/// are checked before the digit decides.
fn ordinal_suffix(n: usize) -> String {
    if (11..=13).contains(&(n % 100)) {
        return format!("{n}th");
    }
    let suffix = match n % 10 {
        1 => "st",
        2 => "nd",
        3 => "rd",
        _ => "th",
    };
    format!("{n}{suffix}")
}

/// The first result column with that name, matching case-insensitively.
pub fn position_of(names: &[String], name: &str) -> Option<usize> {
    names.iter().position(|n| n.eq_ignore_ascii_case(name))
}

/// The result column a bare ORDER BY name reads, which is not always the first
/// one carrying it.
///
/// An explicit `AS` outranks a bare column reference wherever the two sit in the
/// list, so `SELECT b, a AS b FROM t ORDER BY b` reads the alias `a` even
/// though the table's own `b` is projected first. Among columns of the same
/// kind the first wins, so `SELECT b, a AS b` reads the alias and `SELECT a AS
/// b, b` reads the alias too; only a list whose name is carried entirely by bare
/// column references falls back to the first of those.
///
/// This is the one place a name is not simply "the first result column with
/// that name", and it is why `names` alone is not enough to answer it: `names`
/// says what each column is called, not whether it was called that by an author
/// or by being a column. The SELECT list is what settles that.
fn position_under(columns: &[ResultColumn], names: &[String], name: &str) -> Option<usize> {
    let at = position_of(names, name)?;
    // A star is a single result column reported under many names, and every one
    // of them is a table column, so there is no author-written name to prefer.
    if is_star(columns) {
        return Some(at);
    }
    let aliased = columns.iter().position(|rc| {
        rc.alias
            .as_ref()
            .is_some_and(|a| a.eq_ignore_ascii_case(name))
    });
    match aliased {
        Some(a) => Some(a),
        // Nothing carries the name under an explicit alias, so the name was
        // only ever a column of its own, and the first such column is it.
        None => Some(at),
    }
}

/// Replaces every output alias named inside an ORDER BY expression with the
/// expression that alias stands for.
///
/// SQLite resolves the aliases this way rather than by reading the projected
/// row, so `SELECT a+1 AS c FROM t ORDER BY c*2` sorts on `(a+1)*2` for every
/// row. It is why a bare `ORDER BY c` needs no substitution at all — that case
/// reads the projected value directly, which is the same number for the same
/// row, and is decided before this is reached.
///
/// A name is substituted only when no source column answers to it. Inside an
/// expression a real column wins, which is the one asymmetry with a bare name:
/// `SELECT a AS b FROM t ORDER BY b` reads the alias, but `ORDER BY b+1` reads
/// the table's `b`, and `SELECT a AS b, b FROM t ORDER BY b+1` sorts on the
/// table column too. So a name that resolves is left alone even when the result
/// also carries it under that name.
///
/// A star has no written expressions to substitute from, and every name it
/// reports is already a table column, so nothing is substituted and the
/// expression is left as it was. That is decided by the name landing on the one
/// `*` the SELECT list holds, which is the only result column without an
/// expression of its own to read.
fn substitute(
    columns: &[ResultColumn],
    names: &[String],
    from: Option<&From>,
    expr: &Expr,
) -> Expr {
    if is_star(columns) {
        return expr.clone();
    }
    let mut out = expr.clone();
    walk(&mut out, &mut |e: &mut Expr| {
        let Expr::Column {
            table: None, name, ..
        } = e
        else {
            return false;
        };
        // A qualified name always means a table's column, never an alias.
        if let Some(from) = from {
            if crate::join::resolve_ref(from, None, name, false).is_ok() {
                return false;
            }
        }
        // The first result column with that name is the one an alias names, so
        // `SELECT a AS b, b FROM t ORDER BY b+1` reads the table's b and
        // `SELECT a+1 AS c, c*2 ... ORDER BY c*2` reads the first c.
        let Some(at) = position_of(names, name) else {
            return false;
        };
        let Some(rc) = columns.get(at) else {
            return false;
        };
        *e = rc.expr.clone();
        true
    });
    out
}

/// Whether the SELECT list is a bare `*`, which carries no written expressions
/// for an alias to be substituted from.
fn is_star(columns: &[ResultColumn]) -> bool {
    matches!(columns, [only] if matches!(&only.expr, Expr::Function { name, star: true, .. } if name == "*"))
}

/// Reads the value one key sorts a row on.
///
/// A key that names a result column reads that column of the row the projection
/// produced, which is why an ordinal and a bare output name behave the same way
/// once resolved. An expression is handed to `eval_expr`, which is the caller's
/// own evaluator so that a grouped query can fold aggregates into it first.
///
/// The index is one [`Key::at`] names, which [`resolve_keys`] has already
/// checked against the number of result columns, so it is in range for any row
/// the projection produced. A missing value is an internal inconsistency rather
/// than a user error, and is reported as such.
pub fn read(
    key: &Key,
    projected: &[Value],
    eval_expr: impl FnOnce(&Expr) -> Result<Value>,
) -> Result<Value> {
    match key.at {
        Some(at) if key.expr.is_none() => projected.get(at).cloned().ok_or_else(|| {
            Error::new(
                ResultCode::Internal,
                format!("ORDER BY key names column {at} of a row with none"),
            )
        }),
        _ => eval_expr(key.expr.as_ref().expect("an expression key carries one")),
    }
}

/// Sorts rows by keys that have already been computed, in place of the
/// caller's own ordering.
///
/// The keys are compared in term order and the first that differs decides, so
/// `ORDER BY 2 DESC, 1 ASC` sorts on the second column descending and breaks
/// ties on the first ascending. The sort is stable, which is what makes the
/// rows that tie keep the order they arrived in — SQLite leaves that order
/// unspecified, and stability is the one reading of "unspecified" that is
/// reproducible.
///
/// NULL sorts before every other value, so it comes first on ASC and last on
/// DESC. That is the opposite of most languages' default, and it is a property
/// of the value comparison rather than of the direction: reversing the
/// comparison to apply DESC is what moves NULL from the front to the back.
pub fn sort<T>(keys: &[Key], keyed: &mut [(Vec<Value>, T)]) {
    keyed.sort_by(|a, b| {
        for (i, key) in keys.iter().enumerate() {
            let ord = a.0[i].compare(&b.0[i]);
            if ord != std::cmp::Ordering::Equal {
                return if key.ascending { ord } else { ord.reverse() };
            }
        }
        std::cmp::Ordering::Equal
    });
}

/// Applies `f` to every node of an expression, depth first, stopping at a node
/// `f` replaced.
///
/// A replaced node is not descended into, so a substituted expression that
/// mentions another alias does not get that alias substituted in turn: SQLite
/// resolves an alias against the FROM as it stood when the key was written, and
/// `SELECT a AS c, c AS d FROM t ORDER BY d` is a `no such column: c` rather
/// than a chain of substitutions.
fn walk(expr: &mut Expr, f: &mut dyn FnMut(&mut Expr) -> bool) {
    if f(expr) {
        return;
    }
    match expr {
        Expr::Unary { expr, .. }
        | Expr::IsNull { expr, .. }
        | Expr::Collate { expr, .. }
        | Expr::Cast { expr, .. } => walk(expr, f),
        Expr::Binary { left, right, .. } => {
            walk(left, f);
            walk(right, f);
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            walk(expr, f);
            walk(low, f);
            walk(high, f);
        }
        Expr::InList { expr, list, .. } => {
            walk(expr, f);
            for e in list {
                walk(e, f);
            }
        }
        Expr::Like {
            expr,
            pattern,
            escape,
            ..
        } => {
            walk(expr, f);
            walk(pattern, f);
            if let Some(e) = escape {
                walk(e, f);
            }
        }
        Expr::Function { args, .. } => {
            for e in args {
                walk(e, f);
            }
        }
        Expr::Case {
            operand,
            whens,
            otherwise,
        } => {
            if let Some(c) = operand {
                walk(c, f);
            }
            for (c, r) in whens {
                walk(c, f);
                walk(r, f);
            }
            if let Some(e) = otherwise {
                walk(e, f);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
#[path = "orderby_tests.rs"]
mod orderby_tests;
