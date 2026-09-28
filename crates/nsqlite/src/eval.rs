//! Expression evaluation, and the built-in functions.
//!
//! Evaluation is a tree walk over the parsed expression. That is the right
//! shape for a first milestone and the wrong shape for SQLite's own
//! architecture, which compiles to bytecode; the difference shows up in
//! correlated subqueries and aggregates, where a tree walk has to re-resolve
//! names per row. Nothing here assumes it stays a tree walk.
//!
//! The comparison rules are the ones in [`crate::value`], so this module is
//! about evaluating, not about deciding what the answer is.

use std::cmp::Ordering;

use crate::error::{Error, Result, ResultCode};
use crate::func_string::{char_offsets, replace_bytes, text_result, value_bytes};
use crate::msg;
use crate::parser::{BinOp, Expr, Literal, UnaryOp};
use crate::value::Value;

/// The values visible to an expression: bound parameters and the row in scope.
#[derive(Debug, Clone)]
pub struct EvalCtx<'a> {
    /// Bound parameters, 1-based as SQLite numbers them. The vector is
    /// 0-based here, so index 0 is parameter 1.
    pub params: &'a [Value],
    /// The current row, by column name, already lowercased for matching.
    pub row: Vec<(String, Value)>,
    /// Columns of the current table by position, which is what a bare name
    /// resolves against.
    pub columns: &'a [(String, Value)],
    /// A name that did not resolve, kept for the "no such column" message.
    pub context: Option<String>,
    /// Where each column reference in the statement was resolved to, by the
    /// byte offset it was written at.
    ///
    /// A single-table query resolves a name against the one table and needs no
    /// help, so it leaves this empty and the lookup falls through to `row`. A
    /// join resolves every reference once, before the statement runs, because
    /// that is when an ambiguous or unknown column has to be reported. The list
    /// is rebuilt for each joined row, so it holds values rather than positions.
    pub resolved: Vec<(usize, Value)>,
    /// The affinity of each column reference in the statement, by the byte
    /// offset it was written at, which is the same key `resolved` uses.
    ///
    /// A comparison needs this to answer the question SQLite answers with it: a
    /// column compared against a literal is compared under the *column's*
    /// affinity, and two columns each apply theirs to the other. An operand with
    /// no entry here is not a column reference and so contributes no affinity,
    /// which is a real case rather than a missing one — see
    /// [`crate::affinity_rules::operand_affinities`].
    ///
    /// Empty for a statement with no FROM, and for a context built without one,
    /// in which case every comparison converts nothing, exactly as SQLite does
    /// for `SELECT '5' = 5`.
    pub affinities: &'a std::collections::HashMap<usize, crate::affinity::Affinity>,
    /// The double-quoted names in the statement being run.
    ///
    /// SQLite's `no such column` for a name written in double quotes is a
    /// different sentence -- `no such column: "a+b" - should this be a string
    /// literal in single-quotes?` -- and the only thing that distinguishes it
    /// from the plain one is a character that is gone by the time the name
    /// fails to resolve. So the names are collected from the statement's text
    /// before it is evaluated and carried here, and a context built by hand
    /// leaves the list empty and gets the plain message.
    pub double_quoted: &'a [String],
    /// What `last_insert_rowid()` reports: the key of the last row this
    /// connection wrote, or 0 when it has written none.
    ///
    /// The value is carried rather than reached through a `Connection` because
    /// the evaluator is a pure function of the context: it is handed rows and
    /// names, and it is the only thing that can answer a call. A context built
    /// by hand — a test, a constant expression — reports 0, which is the same
    /// answer a connection that has inserted nothing gives.
    ///
    /// This is per-CONNECTION state and SQLite does not persist it. Measured:
    /// a second process opening a database whose rows are perfectly durable
    /// reports 0, while `SELECT max(rowid)` over those same rows reports the
    /// largest key. So a process that wrote nothing reports 0, and the largest
    /// rowid is NOT a substitute — it is a different answer to a different
    /// question.
    pub last_insert_rowid: i64,
    /// The encoding the database file declares, which decides what a character
    /// is for every function that walks text one character at a time.
    ///
    /// It is read from bytes 56..59 of the file header by [`crate::page`] into
    /// the pager header, and it reaches the functions through here rather than
    /// being assumed inline, because the answer genuinely depends on it: the
    /// same nine bytes are three characters in a UTF-8 database and four in a
    /// UTF-16le one. A context built by hand has no file behind it, so it
    /// defaults to UTF-8 — which is the only encoding this engine writes, and
    /// so the only one it reads in practice.
    pub encoding: crate::text::Encoding,
}

/// A context with nothing in scope, which is what a test comparing two values
/// under an explicit affinity wants.
///
/// The affinity map is the shared empty one rather than a `HashMap::new()`, so
/// a default context costs no allocation and converts nothing — which is the
/// right answer for a comparison with no column in it, and is why the default
/// is a context at all rather than a caller having to spell the map out.
impl<'a> Default for EvalCtx<'a> {
    fn default() -> EvalCtx<'a> {
        EvalCtx {
            params: &[],
            row: Vec::new(),
            columns: &[],
            context: None,
            resolved: Vec::new(),
            affinities: crate::affinity_rules::no_affinities(),
            double_quoted: &[],
            last_insert_rowid: 0,
            encoding: crate::text::Encoding::Utf8,
        }
    }
}

impl<'a> EvalCtx<'a> {
    /// A context with no row in scope, for a constant expression.
    pub fn empty(params: &'a [Value]) -> EvalCtx<'a> {
        EvalCtx {
            params,
            row: Vec::new(),
            columns: &[],
            context: None,
            resolved: Vec::new(),
            affinities: crate::affinity_rules::no_affinities(),
            double_quoted: &[],
            last_insert_rowid: 0,
            encoding: crate::text::Encoding::Utf8,
        }
    }

    /// Looks a name up in the current row, matching case-insensitively.
    pub fn lookup(&self, name: &str) -> Option<&Value> {
        self.row
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v)
    }

    /// The affinity an operand of a comparison contributes, or `None` when it
    /// contributes none.
    ///
    /// A column reference contributes the affinity of the column it resolved to,
    /// which is what the rule is written in terms of. A `CAST` to a numeric type
    /// contributes that type's affinity, which is the one non-column form that
    /// does — see [`crate::affinity_rules::expression_affinity`] for the
    /// measurement and for why it is the only one.
    ///
    /// Everything else contributes nothing: a literal, an arithmetic expression,
    /// a function call, a parameter, a subquery. That is not a gap in the
    /// lookup, it is the rule — `a = 5 + 0` against a TEXT column holding '5'
    /// is 0 in sqlite3, exactly as `a = 5` is, and the value on the right is
    /// left as the number it already is.
    pub fn operand_affinity(&self, e: &Expr) -> Option<crate::affinity::Affinity> {
        match e {
            Expr::Column { span, .. } => self.affinities.get(&span.start).copied(),
            Expr::Cast { ty, .. } => crate::affinity_rules::expression_affinity(Some(ty)),
            _ => None,
        }
    }
}

/// Evaluates an expression to a value.
///
/// Errors carry SQLite's wording, because the official suite compares the
/// message text of a failing statement verbatim.
pub fn eval(expr: &Expr, ctx: &EvalCtx<'_>) -> Result<Value> {
    match expr {
        Expr::Literal(lit) => eval_literal(lit, ctx),
        Expr::Column { table, name, span } => {
            // A join resolves every reference before the statement runs, so a
            // resolved reference is read straight from the joined row and the
            // bare-name lookup below is never reached. The start offset
            // identifies the reference, since the parser gave each one a span.
            if let Some((_, v)) = ctx.resolved.iter().find(|(at, _)| *at == span.start) {
                return Ok(v.clone());
            }
            if let Some(v) = ctx.lookup(name) {
                return Ok(v.clone());
            }
            // The name is echoed exactly as the statement wrote it. The engine
            // never folds a name for a message, and the oracle does not either
            // for a column reference: `SELECT BadCol` is `no such column:
            // BadCol` and `SELECT NOSUCHCOL` is `no such column: NOSUCHCOL`.
            // A *function* is the same way about (`no such function: XYZZY`),
            // so the two are not in conflict -- the inconsistency the roadmap
            // called out was that the engine folded the table side of the family
            // and not the function side, and the resolution is that neither is
            // folded. See the module docs in `msg`.
            //
            // A name written in double quotes is a third sentence rather than a
            // second spelling, because sqlite3 reads the quotes as the mistake:
            // `SELECT "a+b"` is `no such column: "a+b" - should this be a
            // string literal in single-quotes?` while `SELECT [a+b]` is
            // `no such column: a+b`. The quotes are the reason, so they are in
            // the message.
            if table.is_none()
                && ctx
                    .double_quoted
                    .iter()
                    .any(|q| q.eq_ignore_ascii_case(name))
            {
                return Err(msg::no_such_column_double_quoted(name));
            }
            match table {
                // A qualified name that did not resolve usually means the table
                // is not in the query at all.
                Some(t) => Err(msg::no_such_column_qualified(t, name)),
                None => Err(msg::no_such_column(name)),
            }
        }
        Expr::NamedParameter(name, _) => Err(Error::new(
            ResultCode::Error,
            format!("binding parameter {name} is not supported yet"),
        )),
        Expr::Unary { op, expr } => {
            let v = eval(expr, ctx)?;
            unary(*op, v)
        }
        Expr::Binary { op, left, right } => {
            // AND and OR short-circuit, and three-valued logic decides whether
            // the rest of the tree is evaluated at all. A NULL operand is
            // *unknown*, which is a third answer and not a false one, so both
            // arms read the left operand as a value first and let a NULL
            // survive to meet the right one. Collapsing NULL to false here is
            // what made `NULL OR 0` answer 0 instead of NULL.
            if *op == BinOp::And {
                // A false left operand decides the answer and the right is
                // never evaluated; a NULL one does not decide it, so the right
                // is asked and only its own falseness carries the answer.
                let lv = eval(left, ctx)?;
                let l_null = lv.is_null();
                if !l_null && !truthy(lv) {
                    return Ok(Value::Integer(0));
                }
                let rv = eval(right, ctx)?;
                let r_null = rv.is_null();
                if !r_null && !truthy(rv) {
                    return Ok(Value::Integer(0));
                }
                // Neither operand is false, so the answer is true only when
                // both are known true, and unknown otherwise.
                return Ok(match l_null || r_null {
                    true => Value::Null,
                    false => Value::Integer(1),
                });
            }
            if *op == BinOp::Or {
                // A true left operand decides the answer and the right is never
                // evaluated. A false or NULL one does not, so the right is
                // asked and only its own truth carries the answer.
                let lv = eval(left, ctx)?;
                let l_null = lv.is_null();
                if !l_null && truthy(lv) {
                    return Ok(Value::Integer(1));
                }
                let rv = eval(right, ctx)?;
                let r_null = rv.is_null();
                if !r_null && truthy(rv) {
                    return Ok(Value::Integer(1));
                }
                return Ok(match l_null || r_null {
                    true => Value::Null,
                    false => Value::Integer(0),
                });
            }
            let l = eval(left, ctx)?;
            let r = eval(right, ctx)?;
            // A comparison takes each operand's own column affinity, which is
            // what makes a TEXT column holding '5' equal to the integer 5. The
            // rule is in `affinity_rules` and measured against sqlite3; here the
            // two operands are asked what they are rather than left as they
            // were written.
            let la = ctx.operand_affinity(left);
            let ra = ctx.operand_affinity(right);
            binary(*op, l, la, r, ra)
        }
        Expr::IsNull { expr, negated } => {
            let v = eval(expr, ctx)?;
            Ok(Value::Integer(i64::from(v.is_null() != *negated)))
        }
        Expr::Between {
            expr,
            low,
            high,
            negated,
        } => {
            let v = eval(expr, ctx)?;
            let lo = eval(low, ctx)?;
            let hi = eval(high, ctx)?;
            // SQL's BETWEEN is inclusive on both ends, which is spelled as two
            // comparisons rather than as a subtraction.
            //
            // Both are the comparison of `=`, affinity rule included, so the
            // same operand affinity is asked of each side. `a BETWEEN 4 AND 6`
            // against a TEXT column holding '5' is true, which is the case that
            // shows the rule is here rather than only in `=`.
            let a = ctx.operand_affinity(expr);
            let lo_a = ctx.operand_affinity(low);
            let hi_a = ctx.operand_affinity(high);
            let inside = !v.is_null()
                && !lo.is_null()
                && !hi.is_null()
                && crate::affinity_rules::compare(&v, a, &lo, lo_a) != Ordering::Less
                && crate::affinity_rules::compare(&v, a, &hi, hi_a) != Ordering::Greater;
            Ok(Value::Integer(i64::from(inside != *negated)))
        }
        Expr::Collate { expr, .. } => eval(expr, ctx),
        Expr::Cast { expr, ty } => {
            let v = eval(expr, ctx)?;
            cast(v, ty)
        }
        Expr::InList {
            expr,
            list,
            negated,
        } => {
            let v = eval(expr, ctx)?;
            if v.is_null() {
                return Ok(Value::Null);
            }
            // `IN` is a repeated `=`, so it takes the same affinity rule, and
            // the affinity asked of each list item is the same one the right
            // hand side of an `=` would have contributed.
            let a = ctx.operand_affinity(expr);
            let mut saw_null = false;
            for item in list {
                let candidate = eval(item, ctx)?;
                if candidate.is_null() {
                    saw_null = true;
                    continue;
                }
                if crate::affinity_rules::equal(&v, a, &candidate, ctx.operand_affinity(item)) {
                    return Ok(Value::Integer(i64::from(!*negated)));
                }
            }
            // A list with a NULL and no match is unknown, not false.
            if saw_null {
                return Ok(Value::Null);
            }
            Ok(Value::Integer(i64::from(*negated)))
        }
        Expr::Like {
            expr,
            pattern,
            escape,
            negated,
        } => {
            let v = eval(expr, ctx)?;
            let p = eval(pattern, ctx)?;
            let e = match escape {
                Some(x) => Some(eval(x, ctx)?),
                None => None,
            };
            let matched = like(&v, &p, e.as_ref(), ctx.encoding);
            // LIKE against a NULL is unknown.
            if v.is_null() || p.is_null() {
                return Ok(Value::Null);
            }
            Ok(Value::Integer(i64::from(matched != *negated)))
        }
        Expr::Function {
            name,
            args,
            star,
            distinct,
        } => {
            if *star {
                return Err(msg::misuse_of_aggregate_function(name));
            }
            if *distinct {
                return Err(Error::new(
                    ResultCode::Error,
                    format!("DISTINCT aggregates are not supported yet"),
                ));
            }
            let mut vals = Vec::with_capacity(args.len());
            for a in args {
                vals.push(eval(a, ctx)?);
            }
            call(name, &vals, ctx.last_insert_rowid, ctx.encoding)
        }
        Expr::Case {
            operand,
            whens,
            otherwise,
        } => {
            let base = match operand {
                Some(e) => {
                    let aff = ctx.operand_affinity(e);
                    Some((eval(e, ctx)?, aff))
                }
                None => None,
            };
            for (cond, result) in whens {
                let cv = eval(cond, ctx)?;
                // The searched form evaluates the arm's condition directly, with
                // no operand to compare against; the simple form compares the
                // operand to the arm's expression, and that is a `=` and so
                // takes the affinity rule — which is what makes
                // `CASE a WHEN 5` match a TEXT column holding '5'. The
                // comparison is a value comparison here, so a NULL is a
                // non-match rather than an unknown, which is the existing
                // behaviour and the one sqlite3 has.
                let hit = match &base {
                    // The searched form evaluates the arm's condition directly.
                    None => truthy(cv),
                    Some((b, base_aff)) => {
                        !b.is_null()
                            && !cv.is_null()
                            && crate::affinity_rules::equal(
                                b,
                                *base_aff,
                                &cv,
                                ctx.operand_affinity(cond),
                            )
                    }
                };
                if hit {
                    return eval(result, ctx);
                }
            }
            match otherwise {
                Some(e) => eval(e, ctx),
                // No arm matched and no ELSE: NULL.
                None => Ok(Value::Null),
            }
        }
        Expr::InSelect { .. } | Expr::Exists { .. } | Expr::Subquery { .. } => Err(Error::new(
            ResultCode::Error,
            "subqueries are not supported yet",
        )),
    }
}

fn eval_literal(lit: &Literal, ctx: &EvalCtx<'_>) -> Result<Value> {
    Ok(match lit {
        Literal::Null => Value::Null,
        Literal::Integer(i) => Value::Integer(*i),
        // Unnegated 2^63, which does not fit an i64, so it is a real.
        Literal::Big(v) => Value::real(*v as f64),
        Literal::Real(r) => Value::real(*r),
        Literal::Text(s) => Value::Text(s.clone()),
        Literal::Blob(b) => Value::Blob(b.clone()),
        Literal::Parameter(index) => {
            let at = index.saturating_sub(1);
            ctx.params.get(at).cloned().unwrap_or(Value::Null)
        }
    })
}

/// SQLite's notion of true: any non-zero number, and text converts to a number
/// first. NULL is not true, and the caller handles that separately.
///
/// The conversion is the same **longest numeric prefix** rule arithmetic uses,
/// so `0 OR '12abc'` is 1 and not 0: sqlite3 reads the `12` and ignores the
/// `abc`. A blob reads as its bytes, which is why `x'2D33' OR 0` is 1.
pub fn truthy(v: Value) -> bool {
    match v {
        Value::Integer(i) => i != 0,
        Value::Real(r) => r != 0.0 && !r.is_nan(),
        Value::Null => false,
        other => numeric_of(&other).as_f64().is_some_and(|n| n != 0.0),
    }
}

fn unary(op: UnaryOp, v: Value) -> Result<Value> {
    Ok(match op {
        UnaryOp::Not => {
            if v.is_null() {
                Value::Null
            } else {
                Value::Integer(i64::from(!truthy(v)))
            }
        }
        UnaryOp::Negate => match v {
            Value::Null => Value::Null,
            Value::Integer(i) => match i.checked_neg() {
                    Some(v) => Value::Integer(v),
                    // The most negative integer has no positive counterpart,
                    // so the answer is a real rather than a wrap back to
                    // itself.
                    None => Value::real(-(i as f64)),
                },
            Value::Real(r) => Value::real(-r),
            // Text and a blob are read as a number first, so `-'12abc'` is the
            // integer -12 and `-x'2D33'` is the integer 3.
            Value::Text(_) | Value::TextBytes(_) | Value::Blob(_) => match numeric_of(&v) {
                Value::Integer(i) => match i.checked_neg() {
                    Some(n) => Value::Integer(n),
                    None => Value::real(-(i as f64)),
                },
                Value::Real(r) => Value::real(-r),
                other => other,
            },
            other => {
                return Err(Error::new(
                    ResultCode::Error,
                    format!(
                        "datatype mismatch: cannot apply unary minus to {}",
                        type_name(&other)
                    ),
                ))
            }
        },
        // Unary plus is a no-op on TEXT: it is not a conversion, and
        // `typeof(+'5')` is 'text' with the quote '5', `typeof(+'abc')` is
        // 'text' with the quote 'abc', and nothing is even trimmed, so
        // `quote(+'  12  ')` keeps both spaces. Converting here was wrong in a
        // way that reached past the value: `+'5' IS '5'` is 1 in sqlite3 and
        // was 0 here, because converting the left operand destroyed the
        // text-against-text comparison the `IS` then made.
        //
        // A BLOB is the one operand it does look at, and it is worth being
        // precise about why: both engines agree that a blob with no numeric
        // prefix comes back as itself -- `typeof(+x'2D33')` is 'blob' and
        // `+x'414243'` is still the blob -- but they disagree on a blob whose
        // bytes ARE a number, and the oracle is the authority. sqlite3 3.53.4
        // has no numeric unary plus, so what happens is the general rule that
        // an operand is coerced and the *answer* keeps the coerced type:
        // `+x'332E35'` is the TEXT `3.5` and its hex is `332E35`, where the
        // blob's own bytes would have hexed as `33324535` -- the same three
        // bytes with an E standing where the dot is.
        UnaryOp::Plus => match v {
            // A blob comes back as the blob, always. sqlite3 3.53.4 has no
            // numeric unary plus, so there is no coercion for the operator to
            // perform and the operand is handed straight back:
            // `typeof(+x'2D33')` is 'blob' and its quote is X'2D33', `typeof(+x'41')`
            // is 'blob', `typeof(+x'332E35')` is 'blob' and its hex is 332E35 --
            // which is the blob's OWN bytes and not the text `3.5`, and is what
            // distinguishes this from a cast. `+x'FF' IS x'FF'` is 1, and a
            // cast is not the identity: `+CAST(x'FF' AS TEXT)` is text.
            Value::Blob(_) => v,
            other => other,
        },
        // `~` is integer-only, but it is not integer-ONLY: an operand that is
        // not already an integer is read as one by the same rule the bitwise
        // binaries use, so a real truncates (`~1.9` is -2 and `~2.0` is -3)
        // and text or a blob takes its longest numeric prefix (`~'12abc'` is
        // -13, `~'abc'` is -1, `~x'2D33'` is 2). Only NULL is left alone.
        //
        // Note that the prefix is read as an INTEGER, which is what makes
        // `~'1e3'` -2 rather than the -1001 a float reading would give: the
        // exponent does not survive. Unary minus on the same text does survive
        // it, so `-'1e3'` is the real -1000.0 while `~'1e3'` is -2. Both were
        // measured; they are not the same rule and `~` is the integer one.
        UnaryOp::BitwiseNot => match v {
            Value::Null => Value::Null,
            Value::Integer(i) => Value::Integer(bitwise_not_of(i)),
            Value::Real(r) => Value::Integer(bitwise_not_of(clamp_to_i64(r))),
            Value::Text(_) | Value::TextBytes(_) | Value::Blob(_) => {
                // The integer part of the prefix, so a `.` or an `e` in the
                // text stops mattering once the number is an integer.
                match integer_prefix_of(&v) {
                    Some(i) => Value::Integer(bitwise_not_of(i)),
                    None => Value::Null,
                }
            }
        },
    })
}

fn binary(
    op: BinOp,
    l: Value,
    l_aff: Option<crate::affinity::Affinity>,
    r: Value,
    r_aff: Option<crate::affinity::Affinity>,
) -> Result<Value> {
    use BinOp::*;
    match op {
        // AND and OR are short-circuited by the caller, so they never arrive
        // here; the arms make that explicit rather than relying on it.
        And | Or => Ok(Value::Null),
        Eq | Ne | Lt | Le | Gt | Ge => {
            // A comparison with NULL is unknown, never true and never false.
            if l.is_null() || r.is_null() {
                return Ok(Value::Null);
            }
            // The operands are converted in place by the affinity rule before
            // they are compared, so the ordering is taken on the converted pair
            // rather than on the values as they were written. This is the whole
            // of `CREATE TABLE t(a TEXT, b INTEGER)` with '5' and 5 comparing
            // equal: the text is parsed by the INTEGER column's affinity.
            let ord = crate::affinity_rules::compare(&l, l_aff, &r, r_aff);
            let (result, swapped) = match op {
                Eq => (ord == Ordering::Equal, false),
                Ne => (ord != Ordering::Equal, false),
                Lt => (ord == Ordering::Less, false),
                Le => (ord != Ordering::Greater, false),
                Gt => (ord == Ordering::Greater, false),
                Ge => (ord != Ordering::Less, false),
                _ => unreachable!("only comparison operators reach here"),
            };
            // The operands were compared in the order written, so no swap is
            // needed; the binding exists only to keep the match exhaustive.
            let _ = swapped;
            Ok(Value::Integer(i64::from(result)))
        }
        Is | IsNot => {
            // `IS` is the same comparison as `=` without the unknown answer, so
            // it takes the same affinity rule. It is not a bit comparison: a
            // negative zero and a positive one are the same to `IS`, and so are
            // an integer and a real that name the same number.
            let same = crate::affinity_rules::is_same(&l, l_aff, &r, r_aff);
            Ok(Value::Integer(i64::from(same == (op == Is))))
        }
        In | NotIn => unreachable!("IN is handled by its own expression form"),
        Like | NotLike | Glob | NotGlob | Regexp | NotRegexp => {
            unreachable!("the pattern operators are handled by their own forms")
        }
        Concat => {
            // || treats NULL as the empty string, which is the one place SQLite
            // does not propagate NULL. A blob contributes its bytes, so
            // x'41' || 'b' is 'Ab' rather than the literal x'41'.
            //
            // The result is always TEXT, and the bytes are the operands' bytes
            // verbatim. `Value::Text` is a `String` and cannot hold a byte that
            // is not valid UTF-8, so the join happens over `Vec<u8>` and a
            // result that is not valid UTF-8 stays in `TextBytes`: `hex(x'FF'||x'00')`
            // is FF00 in sqlite3, and a `String` built by transcoding would have
            // already replaced FF with U+FFFD.
            //
            // A NULL operand makes the WHOLE result NULL, which is the one
            // place `||` propagates NULL: `typeof(NULL || 'a')` is `null` and
            // `hex('x' || NULL)` is the empty string, where treating the NULL
            // as a zero-length operand answered `text` and `78`. The check
            // is on the OPERANDS, before the join, so no coercion can hide it.
            if l.is_null() || r.is_null() {
                return Ok(Value::Null);
            }
            let bytes = concat_bytes(&l, &r);
            Ok(match String::from_utf8(bytes) {
                Ok(s) => Value::Text(s),
                Err(e) => Value::TextBytes(e.into_bytes()),
            })
        }
        Add | Sub | Mul | Div | Mod => arithmetic(op, l, r),
        BitwiseOr | BitwiseAnd | LeftShift | RightShift => bitwise(op, l, r),
        JsonExtract => Err(Error::new(ResultCode::Error, "JSON is not supported yet")),
    }
}

fn as_text_for_concat(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Text(s) => s.clone(),
        // The bytes, not the hex literal the value would print as.
        Value::TextBytes(b) | Value::Blob(b) => String::from_utf8_lossy(b).into_owned(),
        other => other.to_string(),
    }
}

/// The bytes `||` concatenates. A blob contributes its bytes verbatim; every
/// other class contributes the text SQLite renders for it, so `x'41' || 1` is
/// the text 'A1' and not 'A' followed by the integer's own spelling.
fn concat_bytes(l: &Value, r: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    for v in [l, r] {
        match v {
            Value::Blob(b) | Value::TextBytes(b) => out.extend_from_slice(b),
            other => out.extend_from_slice(as_text_for_concat(other).as_bytes()),
        }
    }
    out
}

fn arithmetic(op: BinOp, l: Value, r: Value) -> Result<Value> {
    if l.is_null() || r.is_null() {
        return Ok(Value::Null);
    }
    // Both operands become numbers, and if either is a real the result is a
    // real, so integer division never silently truncates.
    // Both operands become numbers, and if either is a real the whole
    // expression is a real, so integer division never silently truncates.
    let (a, b) = (numeric_of(&l), numeric_of(&r));
    let (ai, bi) = (as_int(&a), as_int(&b));
    let both_int = ai.is_some() && bi.is_some();
    let af = a.as_f64().unwrap_or(0.0);
    let bf = b.as_f64().unwrap_or(0.0);
    match op {
        BinOp::Add => Ok(if both_int {
            match ai.unwrap().checked_add(bi.unwrap()) {
                Some(v) => Value::Integer(v),
                // SQLite falls back to a real on overflow rather than wrapping,
                // because integer overflow is not defined for its arithmetic.
                None => Value::real(af + bf),
            }
        } else {
            Value::real(af + bf)
        }),
        BinOp::Sub => Ok(if both_int {
            match ai.unwrap().checked_sub(bi.unwrap()) {
                Some(v) => Value::Integer(v),
                None => Value::real(af - bf),
            }
        } else {
            Value::real(af - bf)
        }),
        BinOp::Mul => Ok(if both_int {
            match ai.unwrap().checked_mul(bi.unwrap()) {
                Some(v) => Value::Integer(v),
                None => Value::real(af * bf),
            }
        } else {
            Value::real(af * bf)
        }),
        BinOp::Div => {
            if both_int {
                let (x, y) = (ai.unwrap(), bi.unwrap());
                if y == 0 {
                    return Ok(Value::Null);
                }
                // Integer division truncates toward zero.
                Ok(Value::Integer(x.wrapping_div(y)))
            } else if bf == 0.0 {
                Ok(Value::Null)
            } else {
                Ok(Value::real(af / bf))
            }
        }
        BinOp::Mod => {
            // `%` is the one operator that casts **both** operands to integer
            // and answers a real, whatever classes they arrived in. sqlite3
            // 3.53.4: `3.5 % 2` is the real 1.0 and not the 1.5 a float
            // remainder gives, `7 % 2.9` is the real 1.0 rather than the
            // 1.1999999999999997, and `x'332E35' % 2` -- the blob "3.5" -- is
            // that same 1.0, because the cast happens after the blob is read
            // as a number and not before.
            //
            // The cast to integer reaches a text or blob operand as the
            // INTEGER prefix, which is not what every other operator does with
            // the same text. `'1e3'` is the real 1000.0 everywhere else, and
            // `1000.0 % 7` is the real 6.0, but `'1e3' % 7` is the real 1.0
            // because here the text is read as the integer 1 and 1 % 7 is 1.
            // The two readings coexist: the CLASS still follows the float
            // spelling, so `'1e3' % 7` is the real 1.0 while `'12abc' % 7` is
            // the integer 5 -- '12abc' has no `.` and no `e`, so it stays one.
            //
            // The divisor is the truncated one: `5.5 % 2.5` is the real 0.0
            // and `3.5 % 0.5` is NULL, because a divisor that truncates to
            // zero divides by zero here exactly as an integer zero does.
            //
            // A text or blob operand is read as an INTEGER here, which is not
            // what the other operators do with the same text, and the two
            // readings are independent: `'1e3' % 7` is the real 1.0 because
            // the value is 1 % 7 and the class is the real that the `e` in the
            // spelling asks for, while `1000.0 % 7` is the real 6.0 because a
            // real operand is truncated rather than re-read.
            let as_mod_int = |v: &Value, f: f64| match v {
                Value::Text(_) | Value::TextBytes(_) | Value::Blob(_) => integer_prefix_of(v),
                _ => Some(as_int(v).unwrap_or(clamp_to_i64(f))),
            };
            let both_integer = both_int && is_integer_spelled(&l) && is_integer_spelled(&r);
            let (Some(x), Some(y)) = (as_mod_int(&l, af), as_mod_int(&r, bf)) else {
                return Ok(Value::Null);
            };
            if y == 0 {
                return Ok(Value::Null);
            }
            // `wrapping_rem` so the most negative integer divided by -1 is the
            // 0 sqlite3 gives rather than a panic.
            let rem = x.wrapping_rem(y);
            // The result is an integer only when BOTH operands were spelled as
            // integers: `3 % 2` is the integer 1, `3.0 % 2.0` is the real 1.0,
            // and `'12abc' % 7` is the integer 5 while `'1e3' % 7` is the real
            // 1.0. The cast to integer does not decide the class, and neither
            // does one operand alone.
            Ok(if both_integer {
                Value::Integer(rem)
            } else {
                Value::real(rem as f64)
            })
        }
        _ => unreachable!("only arithmetic operators reach here"),
    }
}

/// The value as an i64, when it is stored as an integer.
///
/// A real is deliberately not accepted: 7/2.0 is 3.5, not 3, so a whole real
/// operand still makes the whole expression a real.
fn as_int(v: &Value) -> Option<i64> {
    match v {
        Value::Integer(i) => Some(*i),
        _ => None,
    }
}

/// Whether the value was WRITTEN as an integer, which is not the same question
/// as whether it is one.
///
/// A real is never written as an integer, and text is written as one only when
/// its numeric prefix carries neither a `.` nor an `e` -- the same spelling rule
/// `numeric_prefix_of` applies when it decides the class. This is what tells
/// `'12abc' % 7` (the integer 5) from `'1e3' % 7` (the real 1.0), which differ
/// only in the spelling of the same kind of operand.
fn is_integer_spelled(v: &Value) -> bool {
    match v {
        Value::Integer(_) => true,
        Value::Real(_) => false,
        Value::Text(_) | Value::TextBytes(_) | Value::Blob(_) => {
            matches!(numeric_of(v), Value::Integer(_))
        }
        Value::Null => false,
    }
}

fn bitwise(op: BinOp, l: Value, r: Value) -> Result<Value> {
    // Bitwise operators take integers, and everything that is not already one
    // is read as one -- a real truncates, and text or a blob is read by the
    // *integer* prefix rule rather than the float one arithmetic uses, so
    // `1 | '12abc'` is 13 and `1 | 'a,b'` is 1 rather than either being a
    // refusal. Only a NULL operand makes the whole expression NULL.
    //
    // The integer rule is not interchangeable with the float one, and the
    // difference is not subtle: `1 | '1e3'` is 1 in sqlite3, not the 1001 a
    // float reading gives, because the exponent never enters the number. It
    // is the same rule `~` uses, and the same one `-'1e3'` does *not* use --
    // that is the real -1000.0. The comment on `integer_prefix_of` records the
    // asymmetry; using `numeric_of` here is what made every exponent and every
    // `.` in a text operand wrong.
    let to_int = |v: &Value| -> Option<i64> {
        match v {
            Value::Null => None,
            Value::Integer(i) => Some(*i),
            // A real truncates toward zero rather than being refused, and one
            // too large for an i64 saturates instead of wrapping: `1 | 1e300`
            // is 9223372036854775807 in sqlite3.
            Value::Real(r) => Some(clamp_to_i64(*r)),
            Value::Text(_) | Value::TextBytes(_) | Value::Blob(_) => integer_prefix_of(v),
        }
    };
    let (Some(a), Some(b)) = (to_int(&l), to_int(&r)) else {
        return Ok(Value::Null);
    };
    Ok(Value::Integer(match op {
        BinOp::BitwiseOr => a | b,
        BinOp::BitwiseAnd => a & b,
        BinOp::LeftShift => shift(a, b, true),
        BinOp::RightShift => shift(a, b, false),
        _ => unreachable!("only bitwise operators reach here"),
    }))
}

/// `<<` and `>>` by a measured amount.
///
/// A NEGATIVE amount is the interesting half, because it is not an error and
/// not a zero: it shifts the other way, arithmetically. sqlite3 3.53.4 gives
/// `12 << -1` as 6, `12 << -3` as 1 and `1000 << -3` as 125, which is the
/// arithmetic right shift, and `-8 << -1` as -4 and `-1000 << -3` as -125, which
/// is the same shift keeping the sign. Reading a negative left shift as zero
/// made all five of those 0.
///
/// A right shift by a negative amount is the mirror image: `-1 >> -1` is -2,
/// and `1 >> -1` is 2.
///
/// The two ends do not agree, and each was measured on a column so the operand
/// was not a constant. Once the magnitude reaches 64 the shifted-out bits are
/// all that is left, so the answer is a run of the value's own sign bits: -1 for
/// a negative value, 0 for a positive one. `-1 >> 64` is -1, `-1 >> -64` is 0
/// because a right shift fills with zeros, and `-1 << -64` is -1 because a
/// negative left shift is a right shift. A value that stays in range wraps
/// instead: `1 << 63` is -9223372036854775808, which is 1 << 63 as a bit
/// pattern read back as a signed integer.
fn shift(a: i64, b: i64, left: bool) -> i64 {
    let Some(magnitude) = b.checked_abs().filter(|m| *m < 64) else {
        // Out of range: only the sign bits survive, and only where the shift
        // fills with copies of the sign rather than with zeros.
        let fills_with_sign = (left && b < 0) || (!left && b > 0);
        return if fills_with_sign && a < 0 { -1 } else { 0 };
    };
    match (left, b >= 0) {
        (true, true) => a.wrapping_shl(magnitude as u32),
        (true, false) => a >> magnitude,
        (false, true) => a.wrapping_shr(magnitude as u32),
        (false, false) => a.wrapping_shl(magnitude as u32),
    }
}
///
/// A real is never narrowed, because a real anywhere in an expression makes the
/// result a real: 7/2.0 is 3.5, not 3. Text and blobs that parse as a whole
/// number become an integer, and text or blobs that do not parse become zero.
fn numeric_of(v: &Value) -> Value {
    match v {
        Value::Integer(_) | Value::Real(_) => v.clone(),
        // A blob is read as its bytes and then as a number, exactly as text
        // is: x'2D33' is "-3" and is -3, not a blob that is somehow zero.
        Value::Text(s) => numeric_prefix_of(s.as_bytes()),
        Value::TextBytes(b) => numeric_prefix_of(b),
        Value::Blob(b) => numeric_prefix_of(b),
        Value::Null => Value::Null,
    }
}

/// The integer an integer-only operator reads out of a value: the longest
/// numeric prefix, taken as an integer and saturated at the ends.
///
/// This is deliberately not `numeric_of`. The two differ exactly where the
/// text is written as a float, and sqlite3 measures the difference: `1 | '1e3'`
/// is 1 and `~'1e3'` is -2, so both of them read the `1` and stop, while
/// `-'1e3'` is the real -1000.0 because a minus keeps the whole prefix. So the
/// integer-only operators take the run of leading digits and nothing after
/// it, which also means a `.` or an `e` is simply where the number ends rather
/// than something that makes the operator refuse.
fn integer_prefix_of(v: &Value) -> Option<i64> {
    match v {
        Value::Integer(i) => Some(*i),
        Value::Real(r) => Some(clamp_to_i64(*r)),
        Value::Text(s) => Some(leading_digits(s.as_bytes())),
        Value::TextBytes(b) | Value::Blob(b) => Some(leading_digits(b)),
        Value::Null => None,
    }
}

/// The leading run of `[+-]?[0-9]*` in some bytes, as an integer.
///
/// Anything from the first byte that is not a digit onwards is dropped, so
/// this is the integer part of the same prefix `numeric_prefix_of` finds. A
/// run with no digits at all is 0, matching sqlite3: `1 | 'a,b'` is 1 and
/// `~'abc'` is -1. A run too long for an i64 saturates at the end its sign
/// names, because `1 | '99999999999999999999999'` is 9223372036854775807 and
/// `1 | '-99999999999999999999999'` is -9223372036854775807.
fn leading_digits(bytes: &[u8]) -> i64 {
    let b = trim_ascii(bytes);
    let mut end = 0;
    if end < b.len() && (b[end] == b'-' || b[end] == b'+') {
        end += 1;
    }
    let int_start = end;
    while end < b.len() && b[end].is_ascii_digit() {
        end += 1;
    }
    // A bare sign carries no number, so it is zero rather than an error.
    if end == int_start {
        return 0;
    }
    // A run too long for an i64 saturates at the end its sign names, rather
    // than falling back to zero. Zero would have made
    // `1 | '99999999999999999999999'` the integer 1.
    //
    // The sign has to be applied to the digit run before it is decided whether
    // the run fits, not after a magnitude has been clamped, and the order is
    // what the measured ends of the range turn on. `1 | '99999999999999999999'`
    // is 9223372036854775807 and `1 | '-99999999999999999999999'` is
    // -9223372036854775807, so each sign has its own end. Clamping the
    // magnitude to i64::MAX and negating after would put every negative run at
    // -9223372036854775807 too -- right for `|`, but it would then miss
    // i64::MIN entirely, and `~` has its own answer for exactly that value:
    // `~'-9223372036854775808'` is 9223372036854775807.
    let neg = b.first() == Some(&b'-');
    std::str::from_utf8(&b[..end])
        .ok()
        .and_then(|t| t.parse::<i64>().ok())
        .or_else(|| {
            // The run did not fit with its sign attached, so it is past the end
            // of the range and clamps to the end that sign names.
            Some(if neg { i64::MIN } else { i64::MAX })
        })
        .unwrap_or(0)
}

/// A real as the integer an integer-only operator wants: truncated toward zero
/// and saturated at the ends rather than left to wrap.
///
/// The saturation is what `1 | 9223372036854775808` measures as: sqlite3
/// answers 9223372036854775807, not a negative number from wrapping round. An
/// infinity has no integer at all and becomes the end it points at, and a NaN
/// is 0 because `f as i64` in Rust says so and no measured statement
/// contradicts it.
fn clamp_to_i64(f: f64) -> i64 {
    if f.is_nan() {
        return 0;
    }
    f.clamp(i64::MIN as f64, i64::MAX as f64) as i64
}

/// The complement `~` gives an integer, which saturates instead of wrapping.
///
/// This is the one place the integer-only operators are not a plain bit
/// operation, and it is a single value. `!` on the most negative integer would
/// give 9223372036854775806, and sqlite3 answers 9223372036854775807 instead:
/// `~-9223372036854775808` is 9223372036854775807, and so is
/// `~'99999999999999999999999'`, whose prefix saturates to i64::MIN and is
/// then complemented. Every other integer complements exactly, so the check is
/// on the one input it exists for.
fn bitwise_not_of(i: i64) -> i64 {
    if i == i64::MIN {
        i64::MAX
    } else {
        !i
    }
}

/// The bytes a text value carries. Both text spellings answer, and the bytes
/// are the value's own rather than a re-rendering of them: a `String` already
/// holds the bytes it was built from, and `TextBytes` exists precisely because
/// those bytes need not be UTF-8.
pub(crate) fn text_bytes_of(v: &Value) -> &[u8] {
    match v {
        Value::Text(s) => s.as_bytes(),
        Value::TextBytes(b) => b,
        _ => &[],
    }
}

/// SQLite's text-to-number rule, which is a **longest numeric prefix** parse
/// rather than a whole-string one, so `'31326162'+0` is 12 and not a refusal
/// that falls back to zero.
///
/// A blob is read the same way, so this is also the blob-to-number rule. It is
/// shared with the aggregate funnel, which needs the identical conversion and
/// must not be able to drift from the one the arithmetic operators use.
///
/// The prefix is the longest run of `[+-]?[0-9]*\.?[0-9]*([eE][+-]?[0-9]+)?`,
/// with a trailing exponent only counted when it actually contributes a digit,
/// which is why `x'31326532'` ("12e2") is the real 1200.0 but `x'31322E27'`
/// ("12e'") is the integer 12. Anything before the number stops the scan, so
/// `x'FFFFFFFF'` is 0, and a sign with no digits after it is 0, so `x'2D20'`
/// ("- ") is 0 rather than an error. No prefix at all is also 0: a blob that
/// does not read as a number is not an error in SQLite, which makes
/// `x'414243'+1` the integer 1.
///
/// The exponent belongs to the number whether or not a `.` came first, so
/// `'1.5e1'` is the real 15.0 and not 1.5, and `'12.5e1'` is the real 125.0.
///
/// The result is an integer only when the prefix carries neither a `.` nor an
/// `e`, so a prefix with a decimal point is a real even when it is whole:
/// `x'31326532'` ("12e2") is the real 1200.0 and `x'31322E'` ("12.") is the
/// real 12.0. A prefix too large for an i64 is a real as well, and so is a
/// value past the `f64` range, which is why `1e999` is an infinity rather than
/// a refusal.
pub(crate) fn numeric_prefix_of(bytes: &[u8]) -> Value {
    let b = trim_ascii(bytes);
    let mut end = 0;
    if end < b.len() && (b[end] == b'-' || b[end] == b'+') {
        end += 1;
    }
    while end < b.len() && b[end].is_ascii_digit() {
        end += 1;
    }
    let int_end = end;
    // A `.` extends the number, and it counts as taken even when no digit
    // follows it: sqlite3 reads "12." as a *real* 12.0, so the point is never
    // rewound.
    let mut has_dot = false;
    if end < b.len() && b[end] == b'.' {
        end += 1;
        while end < b.len() && b[end].is_ascii_digit() {
            end += 1;
        }
        has_dot = true;
    }
    let mut has_exp = false;
    // An exponent may follow the fraction. A `.` that took digits does not end
    // the number -- "12.5" is the real 12.5, and "12.5e1" is the real 125.0,
    // both measured against sqlite3 3.53.4. The exponent is part of the same
    // number, so the prefix runs through it whether or not there was a point,
    // and the mantissa may be any length: "12e5" is 1200000.0 and "123E5" is
    // 12300000.0.
    //
    // An `e` that no digit follows is not an exponent and is not part of the
    // number either, so it is rewound: "12.e" is the real 12.0 and "1.5e" is
    // the real 1.5.
    if end < b.len() && (b[end] == b'e' || b[end] == b'E') {
        let e = end;
        end += 1;
        if end < b.len() && (b[end] == b'-' || b[end] == b'+') {
            end += 1;
        }
        let exp_start = end;
        while end < b.len() && b[end].is_ascii_digit() {
            end += 1;
        }
        if end > exp_start {
            has_exp = true;
        } else {
            end = e;
        }
    }
    // The number is the prefix, and anything past it is dropped. The bytes
    // here are ASCII by construction, so a slice is enough to re-read.
    let Some(text) = std::str::from_utf8(&b[..end]).ok() else {
        return Value::Integer(0);
    };
    // An integer is only an integer when the prefix carries no `.` and no
    // exponent. A `.` that is not followed by a digit still counts, which is
    // why `x'31322E'` ("12.") is the *real* 12.0 and not the integer 12, and
    // why `x'31322E2D32'` ("12.-2") is 12.0 while `x'31322D32'` ("12-2") is the
    // integer 12. An `e` that is not followed by an exponent digit does not
    // count, so `x'313265'` ("12e") is the integer 12. So the class is decided
    // by the two flags, never by whether the value happens to be whole: 12e2
    // is the real 1200.0 and `1e999` is a real infinity.
    //
    // A NUL right after the digits also forces a real. SQLite reads the prefix
    // as a C string, so the NUL ends it with a `.` in hand and an empty
    // fractional part: `x'313200'` is the real 12.0 where `x'3132FF'` is the
    // integer 12. This is reachable, because `||` can put a NUL into the middle
    // of a number.
    let nul_ends = b.get(int_end) == Some(&0);
    if !has_dot && !has_exp && !nul_ends {
        if let Ok(i) = text.parse::<i64>() {
            return Value::Integer(i);
        }
    }
    match text.parse::<f64>() {
        Ok(f) => Value::real(f),
        // A prefix that is not a number at all is zero rather than an error.
        Err(_) => Value::Integer(0),
    }
}

/// The leading and trailing ASCII whitespace a number may be padded with.
/// SQLite stops a number at a byte that is not part of one, so this trims only
/// what the parse itself would have skipped.
fn trim_ascii(bytes: &[u8]) -> &[u8] {
    let mut start = 0;
    let mut end = bytes.len();
    while start < end && bytes[start].is_ascii_whitespace() {
        start += 1;
    }
    while end > start && bytes[end - 1].is_ascii_whitespace() {
        end -= 1;
    }
    &bytes[start..end]
}

/// SQLite's CAST, which is a conversion rather than an affinity.
pub fn cast(v: Value, ty: &str) -> Result<Value> {
    use crate::affinity::{affinity_of, Affinity};
    let aff = affinity_of(ty);
    if v.is_null() {
        return Ok(Value::Null);
    }
    Ok(match aff {
        // A cast is a conversion here too, not an affinity, so the class comes
        // out BLOB whatever it went in as: `typeof(CAST(123 AS BLOB))` is
        // 'blob' and not 'integer', and `typeof(CAST('abc' AS BLOB))` is 'blob'
        // too. Only a value that is *already* a blob is unchanged, because its
        // bytes are already the answer.
        Affinity::Blob => match v {
            Value::Blob(_) => v,
            // `value_bytes` rather than `text_bytes_of`, because the bytes of a
            // blob cast are the value *rendered* -- `CAST(123 AS BLOB)` is the
            // three bytes "123" -- and the text-only helper answers an empty
            // slice for the integer and real classes.
            other => Value::Blob(value_bytes(&other)),
        },
        // A cast is a conversion, so *every* non-NULL class comes out as
        // text, a blob included: `typeof(CAST(x'414243' AS TEXT))` is 'text'
        // and not 'blob'. The bytes are kept verbatim rather than transcoded,
        // so bytes that are not valid UTF-8 survive as `TextBytes` -- measured
        // `hex(CAST(x'FF' AS TEXT))` is `FF` and `length(CAST(x'41FF42' AS
        // TEXT))` is 3.
        Affinity::Text => match v {
            Value::Integer(i) => Value::Text(i.to_string()),
            Value::Real(r) => Value::Text(crate::value::Value::real(r).to_string()),
            // Already text, so the cast is the identity on it -- including a
            // `TextBytes`, which does not have to become valid UTF-8 to be
            // text.
            Value::Text(_) | Value::TextBytes(_) => v,
            Value::Blob(b) => match String::from_utf8(b) {
                Ok(s) => Value::Text(s),
                Err(e) => Value::TextBytes(e.into_bytes()),
            },
            // The NULL case returned above, but the match is written out so it
            // stays total if that early return ever moves.
            Value::Null => Value::Null,
        },
        // A cast is a conversion, not a conversion-if-lossless: CAST('1.5' AS
        // INTEGER) truncates rather than leaving the text alone, which is the
        // one place CAST and affinity differ and the reason both are tested
        // against the real program separately.
        Affinity::Integer => match v {
            // The NULL case returned above, but the match is written out so
            // it stays total if that early return ever moves.
            Value::Null => Value::Null,
            Value::Real(r) => Value::Integer(r.trunc() as i64),
            Value::Integer(i) => Value::Integer(i),
            Value::Text(s) => Value::Integer(text_as_int(&s)),
            Value::TextBytes(b) => Value::Integer(bytes_as_int(&b)),
            Value::Blob(b) => Value::Integer(text_as_int(&String::from_utf8_lossy(&b))),
        },
        Affinity::Numeric | Affinity::Real => match v {
            Value::Null => Value::Null,
            Value::Real(_) => v,
            // A REAL cast produces a real even for a whole number, which is
            // what typeof then reports.
            Value::Integer(i) if aff == Affinity::Real => Value::real(i as f64),
            Value::Integer(i) => Value::Integer(i),
            Value::Text(s) => cast_bytes_as_number(s.as_bytes(), aff),
            // A blob is read as the text of its bytes and then cast the same
            // way, so the class rule is the Text arm's and not an integer's:
            // `typeof(CAST(x'3132' AS REAL))` is 'real' and not 'integer', and
            // `CAST(x'3132653135393939' AS REAL)` is the real 1.2e15.
            Value::TextBytes(b) => cast_bytes_as_number(&b, aff),
            Value::Blob(b) => cast_bytes_as_number(&b, aff),
        },
    })
}

/// A numeric or real cast of some bytes, read as the text of those bytes.
///
/// The class comes from the *target*, not from the value: a REAL cast answers
/// a real even for text that is not a number, so `typeof(CAST('abc' AS REAL))`
/// is 'real' and `CAST('abc' AS REAL)` is the real 0.0, while the NUMERIC cast
/// of the same text is the integer 0.
///
/// The conversion itself is the same longest-numeric-prefix read every other
/// operator uses, not a whole-string parse: `CAST('12abc' AS REAL)` is the
/// real 12.0 and `CAST('1.2e' AS REAL)` is the real 1.2, because a parse that
/// rejected the tail would have answered zero for both. A blob is read as the
/// text of its bytes and casts the same way.
fn cast_bytes_as_number(b: &[u8], aff: crate::affinity::Affinity) -> Value {
    use crate::affinity::Affinity;
    match numeric_prefix_of(b) {
        // A REAL cast is a real whatever the number turned out to be, so a
        // whole integer and a prefix of nothing both answer a real.
        Value::Integer(i) if aff == Affinity::Real => Value::real(i as f64),
        other => other,
    }
}

/// CAST's text-to-integer rule: take the leading numeric prefix, or zero.
///
/// The prefix is scanned over bytes, so text that is not UTF-8 reaches the same
/// answer as any other text rather than being transcoded first.
fn text_as_int(s: &str) -> i64 {
    bytes_as_int(s.as_bytes())
}

fn bytes_as_int(t: &[u8]) -> i64 {
    let t = trim_ascii(t);
    let mut end = 0;
    if end < t.len() && (t[end] == b'-' || t[end] == b'+') {
        end += 1;
    }
    while end < t.len() && t[end].is_ascii_digit() {
        end += 1;
    }
    if end == 0 || (end == 1 && !t[0].is_ascii_digit()) {
        return 0;
    }
    std::str::from_utf8(&t[..end])
        .ok()
        .and_then(|d| d.parse::<i64>().ok())
        .unwrap_or(0)
}

/// SQLite's LIKE, which is case-insensitive for ASCII and has no escape by
/// default.
///
/// The wildcard is `%` for any run and `_` for one character. The comparison is
/// over characters, and for a non-ASCII pattern the case folding stops, so a
/// LIKE against text outside ASCII is case-sensitive.
pub fn like(
    value: &Value,
    pattern: &Value,
    escape: Option<&Value>,
    enc: crate::text::Encoding,
) -> bool {
    // Unlike `length` and `substr`, LIKE cuts a NUL off *both* text and blob:
    // sqlite3 3.53.4 matches `x'0061' LIKE ''` and `x'6100' LIKE 'a'` true,
    // because `patternCompare` is handed `nPattern`/`nString` byte counts
    // that both stop at the first NUL whichever class they arrived in. It is
    // also the one function in the family that renders a number as its
    // spelling first -- `1 LIKE '1'` and `length('abc') LIKE '3'` are both
    // true -- so the operands are taken as bytes rather than by
    // destructuring the two text spellings, which is what left a blob
    // answering "no match" outright.
    let v = value_bytes(value);
    let p = value_bytes(pattern);
    let v = match v.iter().position(|&b| b == 0) {
        Some(nul) => &v[..nul],
        None => &v[..],
    };
    let p = match p.iter().position(|&b| b == 0) {
        Some(nul) => &p[..nul],
        None => &p[..],
    };
    let esc = match escape {
        Some(Value::Text(e)) => e.as_bytes().first().copied(),
        Some(Value::TextBytes(e)) => e.first().copied(),
        _ => None,
    };
    like_bytes(v, p, esc, enc)
}

fn like_bytes(text: &[u8], pattern: &[u8], escape: Option<u8>, enc: crate::text::Encoding) -> bool {
    // A backtracking matcher: the pattern is anchored at both ends, and `%`
    // consumes any run, so a greedy attempt has to be able to give characters
    // back. The recursion is bounded by the pattern length, not the text.
    //
    // The wildcards move a **character** at a time, not a byte, which is what
    // `char_offsets` is for. `'_'` consuming one byte made `'日本語' LIKE '___'`
    // false, since three characters are nine bytes and the pattern ran out
    // after three. It was invisible on ASCII, where the two agree.
    fn go(t: &[u8], p: &[u8], esc: Option<u8>, enc: crate::text::Encoding) -> bool {
        if p.is_empty() {
            return t.is_empty();
        }
        let mut pi = 0;
        if let Some(e) = esc {
            if p[pi] == e && pi + 1 < p.len() {
                let want = p[pi + 1];
                return (!t.is_empty()
                    && ascii_lower(t[0]) == ascii_lower(want)
                    && go(&t[1..], &p[pi + 2..], esc, enc))
                    || (p[pi] == b'%' && go(t, &p[pi + 1..], esc, enc));
            }
        }
        match p[pi] {
            b'%' => {
                // Try every split, longest run first, which is what makes the
                // common case linear. The splits are the character starts plus
                // the end of the text, because `%` may also match nothing at
                // all -- leaving out the empty split makes `x'41FF42' LIKE '%'`
                // false, since the text has to be consumed for the pattern that
                // follows to be able to match.
                let off = crate::func_string::char_offsets(t, enc);
                for &start in off.iter().take(off.len().saturating_sub(1)) {
                    if go(&t[start..], &p[pi + 1..], esc, enc) {
                        return true;
                    }
                }
                go(b"", &p[pi + 1..], esc, enc)
            }
            b'_' => {
                // One whole character, so the next text starts after it.
                let off = crate::func_string::char_offsets(t, enc);
                if off.len() < 2 {
                    return false;
                }
                go(&t[off[1]..], &p[pi + 1..], esc, enc)
            }
            c => {
                !t.is_empty()
                    && ascii_lower(t[0]) == ascii_lower(c)
                    && go(&t[1..], &p[pi + 1..], esc, enc)
            }
        }
    }
    go(text, pattern, escape, enc)
}

/// ASCII-only case folding, which is what SQLite's default LIKE does. Bytes
/// outside ASCII compare exactly.
fn ascii_lower(b: u8) -> u8 {
    if b.is_ascii_uppercase() {
        b + 32
    } else {
        b
    }
}

/// The value's type name as `typeof` reports it.
pub fn type_name(v: &Value) -> &'static str {
    v.datatype().name()
}

/// Calls a built-in scalar function.
///
/// `last_insert_rowid` is a parameter rather than something read off a global
/// or off a connection handle, because this function takes no connection and
/// the value it reports belongs to one. Passing it in means the only caller —
/// the `Expr::Function` arm of [`eval`], which holds a context — is the one
/// place that has to know whose connection the answer is for, and a caller that
/// has none gets 0, which is the same answer a connection that has inserted
/// nothing gives.
pub fn call(
    name: &str,
    args: &[Value],
    last_insert_rowid: i64,
    enc: crate::text::Encoding,
) -> Result<Value> {
    let lname = name.to_ascii_lowercase();
    match lname.as_str() {
        // The one function that reads state rather than its arguments, so it
        // takes none: SQLite reports `wrong number of arguments to function
        // last_insert_rowid()` for `SELECT last_insert_rowid(1)`, which is the
        // same sentence every other arity error here produces and the same one
        // `msg::WrongArgumentCount` renders.
        //
        // The name is echoed as written, so `LAST_INSERT_ROWID(1)` says
        // `LAST_INSERT_ROWID`; the match that gets here has already folded the
        // case, and `expect_arity` is handed the original. Measured against
        // sqlite3 3.53.4.
        "last_insert_rowid" => {
            expect_arity(name, args, 0)?;
            Ok(Value::Integer(last_insert_rowid))
        }
        "abs" => {
            expect_arity(name, args, 1)?;
            Ok(match &args[0] {
                Value::Null => Value::Null,
                // abs of the most negative integer has no integer answer,
                // and sqlite3 reports that rather than returning the number
                // unchanged or widening it to a real. Measured: SELECT
                // abs(-9223372036854775808) is "integer overflow" in 3.53.4.
                Value::Integer(i) => match i.checked_abs() {
                    Some(v) => Value::Integer(v),
                    None => {
                        return Err(Error::new(ResultCode::Error, "integer overflow"));
                    }
                },
                Value::Real(r) => Value::real(r.abs()),
                // Only an *integer-class* argument keeps its class. Every other
                // class — text, a blob, and therefore anything that has to be
                // read as a number first — is a real in sqlite3, so
                // `abs('12')` and `abs(x'2D33')` are the real 12.0 and 3.0 even
                // though the number they read as is a whole one. Measured
                // against sqlite3 3.53.4: the class follows the *argument's*
                // class, never the whole-ness of the value.
                other => Value::real(numeric_of(other).as_f64().unwrap_or(0.0).abs()),
            })
        }
        "coalesce" => {
            if args.is_empty() {
                return Err(msg::wrong_argument_count(name));
            }
            for a in args {
                if !a.is_null() {
                    return Ok(a.clone());
                }
            }
            Ok(Value::Null)
        }
        "ifnull" => {
            expect_range(name, args, 2, 2)?;
            Ok(if args[0].is_null() {
                args[1].clone()
            } else {
                args[0].clone()
            })
        }
        "nullif" => {
            expect_range(name, args, 2, 2)?;
            if args[0].eq_value(&args[1]) {
                Ok(Value::Null)
            } else {
                Ok(args[0].clone())
            }
        }
        "length" => {
            expect_arity(name, args, 1)?;
            Ok(match &args[0] {
                Value::Null => Value::Null,
                // A NUL ends the text for `length`, which is why
                // `length('a'||char(0)||'b')` is 1 and not 3 -- the count is
                // right and the *text* stops early. A blob's bytes are not
                // read the same way, so `length(x'00')` is its 1.
                Value::Text(_) | Value::TextBytes(_) => {
                    let b = text_bytes_of(&args[0]);
                    let head = &b[..b.iter().position(|&c| c == 0).unwrap_or(b.len())];
                    Value::Integer(crate::func_string::char_count(head, enc) as i64)
                }
                Value::Blob(b) => Value::Integer(b.len() as i64),
                // A number's length is the length of its SPELLING, and the
                // spelling of -45 is the three characters `-45`. Taking the
                // absolute value first, which this used to do, drops the sign
                // and answered 2 where sqlite3 answers 3 -- and the same for
                // every negative number, `length(-45.0)` being 4 here against
                // the reference's 5. The absolute value was there for a
                // different arm and is not wanted here.
                other => Value::Integer(other.to_string().chars().count() as i64),
            })
        }
        "replace" => {
            expect_arity(name, args, 3)?;
            if args.iter().any(|a| a.is_null()) {
                return Ok(Value::Null);
            }
            // Over bytes, because that is what sqlite3 replaces on: all three
            // arguments contribute their bytes, so a needle or a replacement
            // that is not valid UTF-8 is spliced in as it is.
            // `replace(x'414243', x'42', x'FF')` is the three bytes 41 FF 43,
            // where `string_arg` would have turned the replacement into U+FFFD
            // before the splice ever ran.
            let s = value_bytes(&args[0]);
            let from = value_bytes(&args[1]);
            let to = value_bytes(&args[2]);
            if from.is_empty() {
                return Ok(text_result(s));
            }
            Ok(text_result(replace_bytes(&s, &from, &to)))
        }
        "instr" => {
            expect_arity(name, args, 2)?;
            if args.iter().any(|a| a.is_null()) {
                return Ok(Value::Null);
            }
            // Both arguments contribute their bytes, and the match is a byte
            // match. What decides the rest is the two operands' classes, and
            // they do not decide it separately: **either** one being text
            // selects one rule, and only two blobs together select the other.
            //
            // Two blobs are two byte strings, so the match may begin at any
            // offset at all and the answer is the *byte* offset plus one:
            //
            //   instr(x'61C3A9', x'A9') is 3 -- A9 is byte 2, and the two bytes
            //   before it are the one character `é`.
            //   instr(x'61E697A561', x'97A5') is 3 -- 97A5 is bytes 2 and 3,
            //   the tail of `日`.
            //
            // If *either* operand is text the pair is compared as text, which
            // is a stronger rule than a boundary test on the haystack alone:
            // the match may begin only where the haystack has a character to
            // start with, and the answer counts characters. The haystack is
            // read as text for that purpose whichever class it arrived in.
            //
            //   instr(x'61C3A9', CAST(x'A9' AS TEXT)) is 0 -- a text needle may
            //   not begin inside `é`, and neither may a text haystack match a
            //   blob needle there: instr(CAST(x'61C3A9' AS TEXT), x'A9') is 0
            //   as well.
            //   instr(x'61E697A561', CAST(x'A5' AS TEXT)) is 0 -- A5 is the
            //   last byte of `日`.
            //   instr(x'61E697A561', CAST(x'61' AS TEXT)) is 1 -- the other 61
            //   is byte 0, a character start, so it matches; and the answer
            //   is a character count, which here is the same as the byte one.
            //   instr(x'E697A561', CAST(x'61' AS TEXT)) is 2 -- where 61 is
            //   byte 3 and character 2, so this is the case a byte count
            //   would get wrong.
            //
            // The two rules agree wherever the haystack is all one-byte
            // characters, which is why an ASCII-only corpus cannot tell them
            // apart and only a multi-byte haystack shows the difference.
            let hay = value_bytes(&args[0]);
            let needle = value_bytes(&args[1]);
            let enc = enc;
            // Two blobs and nothing else is the byte search. Note this is
            // *both* operands, not the haystack: a text needle over a blob
            // haystack is the compared-as-text case, and measuring it as the
            // byte search answers 0 where sqlite3 answers 1 and vice versa.
            let as_bytes = matches!(args[0], Value::Blob(_)) && matches!(args[1], Value::Blob(_));
            let idx = if needle.is_empty() {
                1
            } else {
                let found = if as_bytes {
                    find_bytes(&hay, &needle)
                } else {
                    find_at_char_boundary(&hay, &needle, enc)
                };
                match found {
                    // The character count is read off the same segmentation the
                    // boundary test used, so the two cannot drift apart.
                    Some(byte_at) if as_bytes => byte_at as i64 + 1,
                    Some(byte_at) => char_offsets(&hay[..byte_at], enc).len() as i64,
                    None => 0,
                }
            };
            Ok(Value::Integer(idx))
        }
        "typeof" => {
            expect_arity(name, args, 1)?;
            Ok(Value::Text(type_name(&args[0]).to_string()))
        }
        "hex" => {
            expect_arity(name, args, 1)?;
            Ok(Value::Text(hex(&args[0])))
        }
        "quote" => {
            expect_arity(name, args, 1)?;
            quote(&args[0])
        }
        "round" => {
            expect_range(name, args, 1, 2)?;
            if args[0].is_null() {
                return Ok(Value::Null);
            }
            let v = numeric_of(&args[0]).as_f64().unwrap_or(0.0);
            let digits = if args.len() == 2 {
                as_index(&args[1])?
            } else {
                0
            };
            if digits < 0 {
                return Err(Error::new(
                    ResultCode::Error,
                    "round() precision must be non-negative",
                ));
            }
            let f = 10f64.powi(digits as i32);
            // SQLite rounds half away from zero, which is what f64::round does,
            // and it prints the result as text when a precision was given.
            let r = (v * f).round() / f;
            if args.len() == 2 {
                return Ok(Value::Text(Value::real(r).to_string()));
            }
            // A real even for a whole number: sqlite3 reports typeof as
            // 'real' for round(-5).
            Ok(Value::real(r))
        }
        "min" | "max" => {
            if args.is_empty() {
                return Err(msg::wrong_argument_count(name));
            }
            // The scalar min and max are NULL if any argument is NULL, which is
            // what SQLite does: `SELECT min(NULL, 1)` is NULL, not 1. The
            // *aggregate* min and max are the opposite — they skip NULLs — and
            // the two are distinguished by arity, since a one-argument min is
            // the fold and this is only reached with two or more.
            if args.iter().any(|a| a.is_null()) {
                return Ok(Value::Null);
            }
            let mut best: Option<Value> = None;
            for a in args {
                best = Some(match best {
                    None => a.clone(),
                    Some(cur) => {
                        let take_new = match cur.compare(a) {
                            Ordering::Less => lname == "max",
                            Ordering::Greater => lname == "min",
                            Ordering::Equal => false,
                        };
                        if take_new {
                            a.clone()
                        } else {
                            cur
                        }
                    }
                });
            }
            Ok(best.unwrap_or(Value::Null))
        }
        // The name is reported as it was written, not as the lowercased form
        // the dispatch matches on. sqlite3 3.53.4 answers `SELECT XYZZY(1)`
        // with `no such function: XYZZY`, so a test that writes the name in
        // mixed case and compares the message needs the original spelling.
        // The arity error above already reads `name` for the same reason.
        // The math and string modules carry the wider set and were written
        // against the oracle function by function. They are consulted after the
        // arms above so the behaviour already pinned by this module's own tests
        // is not shadowed by a second implementation of the same name.
        //
        // `None` means the name is not one of theirs. The lowercased name is
        // what they match on, since the engine folds an identifier and the
        // message keeps the original spelling.
        _ => {
            if let Some(v) = crate::func_math::call(&lname, args)? {
                return Ok(v);
            }
            if let Some(v) = crate::func_string::call(&lname, args, enc)? {
                return Ok(v);
            }
            // The name is reported as it was written, not as the lowercased form
            // the dispatch matches on. sqlite3 3.53.4 answers `SELECT XYZZY(1)`
            // with `no such function: XYZZY`.
            Err(msg::no_such_function(name))
        }
    }
}

fn expect_arity(name: &str, args: &[Value], want: usize) -> Result<()> {
    if args.len() == want {
        Ok(())
    } else {
        Err(msg::wrong_argument_count(name))
    }
}

fn expect_range(name: &str, args: &[Value], lo: usize, hi: usize) -> Result<()> {
    if args.len() >= lo && args.len() <= hi {
        Ok(())
    } else {
        Err(msg::wrong_argument_count(name))
    }
}

fn as_index(v: &Value) -> Result<i64> {
    match numeric_of(v) {
        Value::Integer(i) => Ok(i),
        Value::Real(r) => Ok(r.trunc() as i64),
        _ => Ok(0),
    }
}

/// The byte offset of the first place `needle` occurs in `hay` starting on a
/// character boundary, or `None` when it does not occur.
///
/// The search is a byte search over the two operands' bytes, and the only
/// extra rule is that a match may not *begin* part-way through a character --
/// which is what makes `instr` agree with sqlite3 on text that is not valid
/// UTF-8. It may, however, *end* part-way through one, and sqlite3 measures
/// both directions:
///
/// ```text
/// instr(CAST(x'61C3A9' AS TEXT), CAST(x'C3A9' AS TEXT)) == 2  -- the whole of `é`
/// instr(CAST(x'61C3A9' AS TEXT), CAST(x'A9'   AS TEXT)) == 0  -- but not half of it
/// instr(CAST(x'FF414243' AS TEXT), CAST(x'4243' AS TEXT)) == 3  -- an end mid-char is fine
/// ```
///
/// `char_offsets` is the crate's one definition of where a character begins,
/// so the boundary test and the position `instr` reports are read off the same
/// segmentation `length` and `substr` use.
fn find_at_char_boundary(
    hay: &[u8],
    needle: &[u8],
    enc: crate::text::Encoding,
) -> Option<usize> {
    if needle.is_empty() || needle.len() > hay.len() {
        return None;
    }
    // Only the starts are candidates, which is the whole of the rule: a needle
    // that matches somewhere else is not a match at all.
    for at in char_offsets(hay, enc) {
        let end = at + needle.len();
        if end > hay.len() {
            break;
        }
        if &hay[at..end] == needle {
            return Some(at);
        }
    }
    None
}

/// The offset of the first occurrence of `needle` in `hay`, at **any** byte
/// offset. This is the search `instr` uses when *both* its operands are blobs.
///
/// It is the counterpart to [`find_at_char_boundary`], and the difference
/// between the two is the whole of the class rule, measured over the haystack
/// `61 E6 97 A5 61` -- the three characters `a`, `日`, `a` over five bytes:
///
/// ```text
/// instr(x'61E697A561', x'A5')                   == 4  -- byte 3, one past `日`
/// instr(x'61E697A561', CAST(x'A5' AS TEXT))     == 0  -- text: not a char start
/// instr(x'61E697A561', CAST(x'97A5' AS TEXT))   == 0  -- text: straddles `日`
/// instr(x'E697A561',   x'61')                   == 4  -- bytes: byte 3
/// instr(x'E697A561',   CAST(x'61' AS TEXT))     == 2  -- chars: character 2
/// ```
///
/// so a byte search has no segmentation at all and a text one starts only
/// where a character does. The empty needle has no occurrence here either;
/// `instr` answers that case itself, as 1.
fn find_bytes(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > hay.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

fn absolute(v: &Value) -> Value {
    match v {
        Value::Integer(i) => Value::Integer(i.wrapping_abs()),
        Value::Real(r) => Value::real(r.abs()),
        other => other.clone(),
    }
}

fn hex(v: &Value) -> String {
    const D: &[u8; 16] = b"0123456789ABCDEF";
    let bytes: Vec<u8> = match v {
        Value::Blob(b) => b.clone(),
        // Text whose bytes are not valid UTF-8 is still text, so `hex` reports
        // its bytes rather than a transcoding of them. This is what makes
        // `hex(x'FF'||x'00')` the two bytes FF 00 instead of the U+FFFD a
        // lossy `to_string` would have produced.
        Value::TextBytes(b) => b.clone(),
        other => other.to_string().into_bytes(),
    };
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(D[(b >> 4) as usize] as char);
        s.push(D[(b & 0xf) as usize] as char);
    }
    s
}

/// SQL's quoting, as `quote()` produces it: strings are single-quoted with the
/// quote doubled, blobs as a hex literal, and everything else as its text.
///
/// The result is a `Value` rather than a `String` because a quoted string keeps
/// the bytes it was given. Text that is not valid UTF-8 is legal in SQLite --
/// `CAST(x'FF' AS TEXT)` is such a value -- and `quote` copies those bytes out
/// verbatim, so a `String` return type would have to either lose them or
/// transcode them. A result that decodes is an ordinary `Text`; one that needs
/// bytes a `String` cannot hold is a `TextBytes`, the same spelling every other
/// conversion in this module uses when a value's bytes are not valid UTF-8.
fn quote(v: &Value) -> Result<Value> {
    Ok(match v {
        Value::Null => Value::Text("NULL".into()),
        Value::Integer(i) => Value::Text(i.to_string()),
        // A real always quotes with a decimal point, and `-0.0` is not a thing
        // sqlite3 will print: `quote(-0.0)` is `0.0`, because the sign of a
        // negative zero does not survive the conversion to text. The display
        // format would otherwise give `-0`, which is a string no reader of the
        // output could parse back into a real.
        Value::Real(r) if r.is_sign_negative() && *r == 0.0 => Value::Text("0.0".into()),
        // A real always quotes with a decimal point, so this has to go through
        // the same renderer the rest of the engine uses for a real rather than
        // through `Display`, which writes an integral real as `1` where sqlite3
        // writes `1.0`. Checked against sqlite3 3.53.4: quote(1.0) is `1.0` and
        // quote(2451545.0) is `2451545.0`.
        //
        // An infinite real is the one value that does NOT share the general
        // rendering, and the substitution is local to this function. sqlite3
        // quotes 1e400 as `9.0e+999` -- a finite-looking token that reads back
        // as an infinity, so the value survives a round trip through SQL text
        // -- while the very same value prints as `Inf` everywhere else:
        // `SELECT 1e400` is `Inf`, `CAST(1e400 AS TEXT)` is `Inf`, `'x'||1e400`
        // is `xInf`, `printf('%g',1e400)` is `Inf`, and a REAL-affinity column
        // holding one prints `Inf`. `concat`, `upper`, `hex`, `length` and
        // `substr` all render `Inf` too, and they reach `real_to_text` through
        // `func_string::text_of`, so the token has to be added HERE rather than
        // in the shared renderer. Measured: quote(1e400) is `9.0e+999`,
        // quote(-1e400) is `-9.0e+999`, quote(x) for a *stored* 1e400 is
        // `9.0e+999` as well, and quote(1e308) is `1.0e+308` -- the token only
        // replaces a genuine infinity, not a merely large finite real.
        Value::Real(r) if r.is_infinite() => {
            if *r < 0.0 {
                Value::Text("-9.0e+999".into())
            } else {
                Value::Text("9.0e+999".into())
            }
        }
        Value::Real(r) => Value::Text(crate::func_math::real_to_text(*r)),
        // A text value is quoted from its bytes rather than from characters,
        // because two of the C-string rules `quote` obeys are about bytes: the
        // run ends at the first NUL, and a byte that is not valid UTF-8 is
        // copied through rather than replaced.
        Value::Text(s) => quoted_text(s.as_bytes()),
        // Text keeps quoting as text whatever its bytes:
        // `quote(CAST(x'FF' AS TEXT))` is `'FF'` and not `X'FF'`, because the
        // cast made the value text and not a blob. Measured against sqlite3
        // 3.53.4: the FF survives as the single byte FF.
        Value::TextBytes(b) => quoted_text(b),
        Value::Blob(b) => Value::Text(format!("X'{}'", hex(&Value::Blob(b.clone())))),
    })
}

/// Quotes a run of text bytes: single-quoted, the quote doubled, stopping at
/// the first NUL and passing every other byte through unchanged.
///
/// A NUL is where the C string `quote` writes ends, so `quote('a'||char(0)||'b')`
/// is `'a'` on sqlite3, not the whole value -- the same place it ends for
/// `length` and `substr`. That is a property of quoting only: the value itself,
/// and `hex()` of it, still carry the NUL.
///
/// A `'` is a byte like any other, so doubling it is the one substitution made
/// here, and every other byte is copied as itself. The result is therefore built
/// as a `Vec<u8>` and turned into a value only at the end, because the run need
/// not be valid UTF-8 and a `String` cannot hold bytes that are not. Measured
/// against sqlite3 3.53.4: `quote(CAST(x'FF' AS TEXT))` is the three characters
/// quote, FF, quote -- one byte, where a lossy decode would have written the
/// three bytes EF BF BD in the FF's place and a per-character push would have
/// written C3 BF.
/// The apostrophe `quote` surrounds and doubles, named so the byte literals in
/// [`quoted_text`] read as a symbol rather than as an escaped character.
const QUOTE_BYTE: u8 = b'\'';

fn quoted_text(b: &[u8]) -> Value {
    let head = match b.iter().position(|&c| c == 0) {
        Some(nul) => &b[..nul],
        None => b,
    };
    let mut out: Vec<u8> = Vec::with_capacity(head.len() + 2);
    out.push(QUOTE_BYTE);
    for &byte in head {
        if byte == QUOTE_BYTE {
            // A quote inside the value becomes two, which is the one place the
            // output is longer than the input.
            out.push(QUOTE_BYTE);
        }
        out.push(byte);
    }
    out.push(QUOTE_BYTE);
    // Decoding is the same decision every other conversion here makes: a run
    // that is valid UTF-8 is ordinary text, and one that is not keeps its bytes
    // in `TextBytes` so that nothing is lost or transcoded on the way out.
    match String::from_utf8(out) {
        Ok(s) => Value::Text(s),
        Err(e) => Value::TextBytes(e.into_bytes()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::{parse_one, Stmt};

    /// Parses a bare expression, which the grammar treats as a select with no
    /// FROM.
    fn expr_of(sql: &str) -> Expr {
        let Stmt::Select(s) = parse_one(&format!("SELECT {sql}")).unwrap() else {
            panic!("expected a select")
        };
        match s.body {
            crate::parser::SelectBody::Simple { columns, .. } => {
                columns.into_iter().next().unwrap().expr
            }
            other => panic!("expected a simple body, got {other:?}"),
        }
    }

    fn ev(sql: &str) -> Result<Value> {
        let e = expr_of(sql);
        let params: Vec<Value> = Vec::new();
        eval(&e, &EvalCtx::empty(&params))
    }

    fn ok(sql: &str) -> Value {
        ev(sql).unwrap_or_else(|e| panic!("evaluating {sql:?} failed: {e}"))
    }

    fn is_err(sql: &str) -> Error {
        ev(sql).unwrap_err()
    }

    #[test]
    fn arithmetic_precedence_holds() {
        assert_eq!(ok("1 + 2 * 3"), Value::Integer(7));
        assert_eq!(ok("(1 + 2) * 3"), Value::Integer(9));
        assert_eq!(ok("10 - 4 - 3"), Value::Integer(3));
        assert_eq!(ok("2 * 3 % 4"), Value::Integer(2));
    }

    /// `%` casts both operands to integer before it takes the remainder, and
    /// answers a real unless both were already integers. Measured against
    /// sqlite3 3.53.4; the float-remainder answers were disagreements.
    #[test]
    fn modulo_casts_its_operands_to_integers() {
        assert_eq!(ok("3.5 % 2"), Value::real(1.0));
        assert_eq!(ok("7 % 2.9"), Value::real(1.0));
        // The divisor is truncated too, so 2.5 is a divisor of 1 and 0.5 is
        // a zero.
        assert_eq!(ok("5.5 % 2.5"), Value::real(1.0));
        assert_eq!(ok("3.5 % 0.5"), Value::Null);
        // A blob is read as a number first and truncated after, so the blob
        // "3.5" is the same 1.0.
        assert_eq!(ok("x'332E35' % 2"), Value::real(1.0));
        // Two integers still answer an integer, and a real operand makes the
        // result a real even when the remainder is whole.
        assert_eq!(ok("3 % 2"), Value::Integer(1));
        assert_eq!(ok("3.0 % 2.0"), Value::real(1.0));
        // A zero divisor is NULL, and the sign follows the dividend.
        assert_eq!(ok("3 % 0"), Value::Null);
        assert_eq!(ok("-7 % 3"), Value::Integer(-1));
    }

    /// `%` reads a text or blob operand as an INTEGER, which is not what the
    /// other operators do with the same text, and the reading is independent
    /// of the class. `'1e3'` is the real 1000.0 everywhere else and
    /// `1000.0 % 7` is the real 6.0, but `'1e3' % 7` is the real 1.0 because
    /// here the text is the integer 1. Measured against sqlite3 3.53.4.
    #[test]
    fn modulo_reads_a_text_operand_as_an_integer() {
        assert_eq!(ok("'1e3' % 7"), Value::real(1.0));
        assert_eq!(ok("'1e3' % '7'"), Value::real(1.0));
        assert_eq!(ok("'1e3' % 7.0"), Value::real(1.0));
        assert_eq!(ok("'1e2' % 7"), Value::real(1.0));
        assert_eq!(ok("'1.5e3' % 7"), Value::real(1.0));
        assert_eq!(ok("'12e2' % 7"), Value::real(5.0));
        assert_eq!(ok("'1e' % 7"), Value::Integer(1));
        assert_eq!(ok("'e3' % 7"), Value::Integer(0));
        // The class still follows the spelling: '12abc' has neither a `.` nor
        // an `e`, so it stays an integer while `'1e3'` does not.
        assert_eq!(ok("'12abc' % 7"), Value::Integer(5));
        assert_eq!(ok("'12abc' % 7.0"), Value::real(5.0));
        // A real operand is truncated rather than re-read, which is the
        // difference that makes the row above surprising.
        assert_eq!(ok("1000.0 % 7"), Value::real(6.0));
        assert_eq!(ok("1000 % 7"), Value::Integer(6));
        // A blob is read the same way a text is.
        assert_eq!(ok("x'3132' % 7"), Value::Integer(5));
    }

    /// An exponent belongs to the number whether or not a `.` came first, so
    /// `'1.5e1'` is 15.0 rather than 1.5. The prefix used to stop at the first
    /// fraction digit, which silently dropped the exponent and made every
    /// float-spelled-with-a-point text wrong.
    #[test]
    fn an_exponent_follows_a_fraction() {
        assert_eq!(ok("'1.5e1' + 0"), Value::real(15.0));
        assert_eq!(ok("'1.5e2' + 0"), Value::real(150.0));
        assert_eq!(ok("'12.5e1' + 0"), Value::real(125.0));
        assert_eq!(ok("'0.15e2' + 0"), Value::real(15.0));
        assert_eq!(ok("'1.5e-1' + 0"), Value::real(0.15));
        assert_eq!(ok("'1.5e+1' + 0"), Value::real(15.0));
        assert_eq!(ok("'1.5E1' + 0"), Value::real(15.0));
        assert_eq!(ok("'1.5e1x' + 0"), Value::real(15.0));
        // An `e` no digit follows is not an exponent, so the number ends there.
        assert_eq!(ok("'1.5e' + 0"), Value::real(1.5));
        assert_eq!(ok("'12e' + 0"), Value::Integer(12));
        // And the plain forms are untouched by any of this.
        assert_eq!(ok("'12e2' + 0"), Value::real(1200.0));
        assert_eq!(ok("'1.5' + 0"), Value::real(1.5));
    }

    /// A NEGATIVE shift amount shifts the other way, arithmetically, and is
    /// neither an error nor a zero. Measured on a column so the amount was not
    /// a constant: 12 << -1 is 6, 1000 << -3 is 125, -8 << -1 is -4, and
    /// -1 >> -1 is -2.
    #[test]
    fn a_negative_shift_amount_shifts_the_other_way() {
        assert_eq!(ok("12 << -1"), Value::Integer(6));
        assert_eq!(ok("12 << -3"), Value::Integer(1));
        assert_eq!(ok("1000 << -3"), Value::Integer(125));
        assert_eq!(ok("-8 << -1"), Value::Integer(-4));
        assert_eq!(ok("-1000 << -3"), Value::Integer(-125));
        assert_eq!(ok("1 << -1"), Value::Integer(0));
        assert_eq!(ok("-1 >> -1"), Value::Integer(-2));
        assert_eq!(ok("1 >> -1"), Value::Integer(2));
        assert_eq!(ok("-1 >> -3"), Value::Integer(-8));
        // A text or blob amount is read by the integer prefix rule first, so
        // `'  -3  '` and the blob "-3" are the same amount.
        assert_eq!(ok("12 << '  -3  '"), Value::Integer(1));
        assert_eq!(ok("12 << x'2D33'"), Value::Integer(1));
        // Once the magnitude reaches 64 only the sign bits are left, and a
        // shift fills with copies of the sign only where it fills with ones.
        assert_eq!(ok("1 << 64"), Value::Integer(0));
        assert_eq!(ok("-1 << 64"), Value::Integer(0));
        assert_eq!(ok("1 << -64"), Value::Integer(0));
        assert_eq!(ok("-1 << -64"), Value::Integer(-1));
        assert_eq!(ok("-1 >> 64"), Value::Integer(-1));
        assert_eq!(ok("-1 >> -64"), Value::Integer(0));
        // In range but at the end, an overflow wraps rather than saturating.
        assert_eq!(ok("1 << 63"), Value::Integer(i64::MIN));
        assert_eq!(ok("-1 >> 63"), Value::Integer(-1));
        // The two ends of a right shift cannot share one test: a large POSITIVE
        // amount keeps the value's sign and a large NEGATIVE one is zero.
        assert_eq!(ok("-1 >> 9223372036854775807"), Value::Integer(-1));
        assert_eq!(ok("1 >> 9223372036854775807"), Value::Integer(0));
        assert_eq!(ok("-1 >> -9999999999999999999"), Value::Integer(0));
        assert_eq!(ok("1 >> -9999999999999999999"), Value::Integer(0));
        assert_eq!(ok("-1 >> -9223372036854775808"), Value::Integer(0));
    }

    /// The class of a number read out of text follows the text's SPELLING and
    /// not whether the value happens to be whole, so a `.` or an exponent
    /// anywhere in the numeric prefix makes it a real. Measured against
    /// sqlite3; every one of these was a disagreement before.
    #[test]
    fn text_spelled_as_a_float_converts_to_a_real() {
        assert_eq!(ok("'5.0' + 0"), Value::real(5.0));
        assert_eq!(ok("0 - '5.0'"), Value::real(-5.0));
        assert_eq!(ok("1 * '5.0'"), Value::real(5.0));
        assert_eq!(ok("'5.0' / 1"), Value::real(5.0));
        assert_eq!(ok("'5.0' % 2"), Value::real(1.0));
        assert_eq!(ok("'.5' + 0"), Value::real(0.5));
        assert_eq!(ok("'5.' + 0"), Value::real(5.0));
        assert_eq!(ok("'1e300' + 0"), Value::real(1e300));
        assert_eq!(ok("'1e18' + 0"), Value::real(1e18));
        // A prefix with neither a `.` nor an `e` is still an integer, which is
        // the half that was already right and has to stay right.
        assert_eq!(ok("'5' + 0"), Value::Integer(5));
        assert_eq!(ok("' 5 ' + 0"), Value::Integer(5));
        assert_eq!(ok("'5abc' + 0"), Value::Integer(5));
        assert_eq!(ok("'inf' + 0"), Value::Integer(0));
        assert_eq!(ok("'' + 0"), Value::Integer(0));
    }

    /// Unary minus applies the ordinary text-to-number rule rather than
    /// refusing the operand, and it keeps the prefix's class.
    #[test]
    fn unary_minus_converts_text_and_blob_by_the_numeric_rule() {
        assert_eq!(ok("-'5'"), Value::Integer(-5));
        assert_eq!(ok("-'abc'"), Value::Integer(0));
        assert_eq!(ok("-''"), Value::Integer(0));
        assert_eq!(ok("-'  12  '"), Value::Integer(-12));
        assert_eq!(ok("-'12abc'"), Value::Integer(-12));
        assert_eq!(ok("-'0x10'"), Value::Integer(0));
        assert_eq!(ok("-x'2D33'"), Value::Integer(3));
        assert_eq!(ok("-NULL"), Value::Null);
        // The class follows the spelling here too, so these are reals.
        assert_eq!(ok("-'1e3'"), Value::real(-1000.0));
        assert_eq!(ok("-'5.0'"), Value::real(-5.0));
        assert_eq!(ok("-'0.0'"), Value::real(0.0));
        assert_eq!(ok("-'12.0abc'"), Value::real(-12.0));
        assert_eq!(ok("-'1.5'"), Value::real(-1.5));
        // i64::MIN has no positive counterpart, so it stays an integer.
        assert_eq!(ok("-9223372036854775808"), Value::Integer(i64::MIN));
        assert_eq!(ok("-(-9223372036854775808)"), Value::real(9.2233720368547758e18));
    }

    /// Unary plus is the identity. It does not convert, and it does not even
    /// trim: `+'5'` is the text '5', so `+'5' IS '5'` is 1.
    #[test]
    fn unary_plus_is_the_identity() {
        assert_eq!(ok("+'5'"), Value::Text("5".into()));
        assert_eq!(ok("+'abc'"), Value::Text("abc".into()));
        assert_eq!(ok("+''"), Value::Text(String::new()));
        assert_eq!(ok("+'  12  '"), Value::Text("  12  ".into()));
        assert_eq!(ok("+x'41'"), Value::Blob(vec![0x41]));
        assert_eq!(ok("+1.5"), Value::real(1.5));
        assert_eq!(ok("+NULL"), Value::Null);
        // The consequence that is not about the value: the identity leaves
        // both operands text, so `IS` compares them as text.
        assert_eq!(ok("+'5' IS '5'"), Value::Integer(1));
        assert_eq!(ok("+'abc' IS 'abc'"), Value::Integer(1));
    }

    /// `~` is integer-only but not integer-ONLY: a real truncates and text
    /// takes its longest numeric prefix read as an integer, which is why the
    /// exponent in `'1e3'` is discarded here even though a unary minus on the
    /// same text keeps it.
    #[test]
    fn bitwise_not_truncates_and_reads_the_prefix_as_an_integer() {
        assert_eq!(ok("~1.9"), Value::Integer(-2));
        assert_eq!(ok("~2.0"), Value::Integer(-3));
        assert_eq!(ok("~-1.9"), Value::Integer(0));
        assert_eq!(ok("~'5'"), Value::Integer(-6));
        assert_eq!(ok("~'abc'"), Value::Integer(-1));
        assert_eq!(ok("~''"), Value::Integer(-1));
        assert_eq!(ok("~'  12  '"), Value::Integer(-13));
        assert_eq!(ok("~'0x10'"), Value::Integer(-1));
        assert_eq!(ok("~'12abc'"), Value::Integer(-13));
        assert_eq!(ok("~x'2D33'"), Value::Integer(2));
        assert_eq!(ok("~NULL"), Value::Null);
        // The two rules differ, and they were measured to differ.
        assert_eq!(ok("~'1e3'"), Value::Integer(-2));
        assert_eq!(ok("-'1e3'"), Value::real(-1000.0));
    }

    /// OR and AND are three-valued, so a NULL operand is unknown rather than
    /// false and the four NULL-left rows of the truth table are NULL.
    #[test]
    fn or_and_keep_null_as_unknown() {
        assert_eq!(ok("NULL OR 0"), Value::Null);
        assert_eq!(ok("NULL OR 1"), Value::Integer(1));
        assert_eq!(ok("NULL OR NULL"), Value::Null);
        assert_eq!(ok("NULL OR 'a,b'"), Value::Null);
        assert_eq!(ok("NULL AND 1"), Value::Null);
        assert_eq!(ok("NULL AND 0"), Value::Integer(0));
        assert_eq!(ok("NULL AND NULL"), Value::Null);
        // The rows a NULL right operand decides, which were already right.
        assert_eq!(ok("0 OR NULL"), Value::Null);
        assert_eq!(ok("1 OR NULL"), Value::Integer(1));
        assert_eq!(ok("0 AND NULL"), Value::Integer(0));
        assert_eq!(ok("1 AND NULL"), Value::Null);
        // No NULL anywhere is two-valued again.
        assert_eq!(ok("1 OR 1"), Value::Integer(1));
        assert_eq!(ok("1 OR 0"), Value::Integer(1));
        assert_eq!(ok("0 OR 0"), Value::Integer(0));
        assert_eq!(ok("1 AND 1"), Value::Integer(1));
        assert_eq!(ok("1 AND 0"), Value::Integer(0));
        assert_eq!(ok("0 AND 0"), Value::Integer(0));
    }

    /// A non-numeric operand to OR or AND is not a refusal. It is read with the
    /// same longest-numeric-prefix rule as everything else, so `'12abc'` is the
    /// number 12 rather than either a truth or an error.
    #[test]
    fn or_and_read_a_non_numeric_operand_by_the_prefix_rule() {
        assert_eq!(ok("1 OR 'a,b'"), Value::Integer(1));
        assert_eq!(ok("'abc' OR 1"), Value::Integer(1));
        assert_eq!(ok("'abc' AND 1"), Value::Integer(0));
        assert_eq!(ok("1 AND 'a,b'"), Value::Integer(0));
        assert_eq!(ok("0 OR 'abc'"), Value::Integer(0));
        assert_eq!(ok("0 OR '12abc'"), Value::Integer(1));
        assert_eq!(ok("'12abc' AND 1"), Value::Integer(1));
        assert_eq!(ok("'12abc' OR NULL"), Value::Integer(1));
        assert_eq!(ok("0 OR '1e3'"), Value::Integer(1));
        assert_eq!(ok("0 OR '.5'"), Value::Integer(1));
        assert_eq!(ok("x'2D33' OR 0"), Value::Integer(1));
        assert_eq!(ok("x'31' OR 0"), Value::Integer(1));
        // NOT reads its operand the same way, so it is a hex blind too.
        assert_eq!(ok("NOT '12abc'"), Value::Integer(0));
        assert_eq!(ok("NOT x'2D33'"), Value::Integer(0));
        assert_eq!(ok("NOT x'01'"), Value::Integer(1));
        assert_eq!(ok("NOT ''"), Value::Integer(1));
        assert_eq!(ok("NOT NULL"), Value::Null);
    }

    /// `|`, `&` and the shifts are integer operations with their own rule: a
    /// real truncates, text and blobs take the prefix, and only NULL is NULL.
    /// They also still bind tighter than OR and AND, which must not regress.
    #[test]
    fn bitwise_operators_convert_rather_than_refuse() {
        assert_eq!(ok("1 | 'a,b'"), Value::Integer(1));
        assert_eq!(ok("1 | '12abc'"), Value::Integer(13));
        assert_eq!(ok("1 | '  12  '"), Value::Integer(13));
        assert_eq!(ok("1 | '0x10'"), Value::Integer(1));
        assert_eq!(ok("1 | x'41'"), Value::Integer(1));
        assert_eq!(ok("0 | x'01'"), Value::Integer(0));
        assert_eq!(ok("1 << 'a,b'"), Value::Integer(1));
        assert_eq!(ok("1 << '  12  '"), Value::Integer(4096));
        assert_eq!(ok("1 | 1.9"), Value::Integer(1));
        assert_eq!(ok("1 & 1.9"), Value::Integer(1));
        assert_eq!(ok("1 << 1.9"), Value::Integer(2));
        assert_eq!(ok("1 | -1.9"), Value::Integer(-1));
        assert_eq!(ok("1 | NULL"), Value::Null);
        // Out of i64 saturates rather than wrapping.
        assert_eq!(ok("1 | 9223372036854775808"), Value::Integer(i64::MAX));
        assert_eq!(ok("1 | 1e300"), Value::Integer(i64::MAX));
        // A shift of 64 or more is zero, and a right shift of a negative by 64
        // or more is -1.
        assert_eq!(ok("1 << 2"), Value::Integer(4));
        assert_eq!(ok("1 << -1"), Value::Integer(0));
        assert_eq!(ok("1 << 64"), Value::Integer(0));
        assert_eq!(ok("-1 >> 64"), Value::Integer(-1));
        // `|` binds tighter than OR and than AND, so this groups as
        // `(1|2) OR 4`, which is 1 rather than 7.
        assert_eq!(ok("1|2 OR 4"), Value::Integer(1));
        assert_eq!(ok("1|(2 OR 4)"), Value::Integer(1));
        assert_eq!(ok("4|2 AND 0"), Value::Integer(0));
        assert_eq!(ok("4|(2 AND 0)"), Value::Integer(4));
        assert_eq!(ok("2 & 0 AND 1"), Value::Integer(0));
        assert_eq!(ok("1 + 1 | 2"), Value::Integer(2));
        assert_eq!(ok("2 * 3 | 1"), Value::Integer(7));
        assert_eq!(ok("1 | 2 = 3"), Value::Integer(1));
        assert_eq!(ok("NOT 0 | 0"), Value::Integer(1));
        assert_eq!(ok("NOT 1 AND 0"), Value::Integer(0));
    }

    /// `|`, `&` and the shifts read a text or blob operand with the INTEGER
    /// prefix rule, not the float one arithmetic uses, so a `.` or an exponent
    /// in the operand never enters the number. This is the same rule `~` uses
    /// and the one `-'1e3'` does not, and using the float rule here made
    /// `1 | '1e3'` the integer 1001 where sqlite3 answers 1.
    #[test]
    fn bitwise_operators_read_text_with_the_integer_prefix_rule() {
        assert_eq!(ok("1 | '1e3'"), Value::Integer(1));
        assert_eq!(ok("1 & '1e3'"), Value::Integer(1));
        assert_eq!(ok("1 << '1e3'"), Value::Integer(2));
        assert_eq!(ok("1 >> '1e3'"), Value::Integer(0));
        assert_eq!(ok("1 | '12e2'"), Value::Integer(13));
        assert_eq!(ok("1 | '1.5'"), Value::Integer(1));
        assert_eq!(ok("1 | '5.'"), Value::Integer(5));
        assert_eq!(ok("1 | '1.5e1'"), Value::Integer(1));
        assert_eq!(ok("1 | '2e-1'"), Value::Integer(3));
        assert_eq!(ok("1 | '+1e3'"), Value::Integer(1));
        assert_eq!(ok("1 | '  1e3  '"), Value::Integer(1));
        assert_eq!(ok("1 | '1e3abc'"), Value::Integer(1));
        assert_eq!(ok("1 | '1E3'"), Value::Integer(1));
        // An `e` that starts no exponent is just where the number ends, so it
        // reads as 1 rather than as 1000.
        assert_eq!(ok("1 | '1e'"), Value::Integer(1));
        assert_eq!(ok("1 | 'e3'"), Value::Integer(1));
        // And a blob is read the same way.
        assert_eq!(ok("1 | x'316533'"), Value::Integer(1));
        // The rule is unchanged for the text that has no float spelling, which
        // is the half that was already right.
        assert_eq!(ok("1 | '12abc'"), Value::Integer(13));
        assert_eq!(ok("1 | 'a,b'"), Value::Integer(1));
    }

    /// A digit run too long for an i64 saturates at the end its own sign names.
    /// Falling back to zero was a wrong answer well outside the overflow: it
    /// made `1 | '99999999999999999999999'` the integer 1.
    #[test]
    fn an_over_long_digit_run_saturates_at_the_end_its_sign_names() {
        assert_eq!(ok("1 | '99999999999999999999999'"), Value::Integer(i64::MAX));
        assert_eq!(
            ok("1 | '-99999999999999999999999'"),
            Value::Integer(-i64::MAX)
        );
        assert_eq!(
            ok("1 | '12345678901234567890'"),
            Value::Integer(i64::MAX)
        );
        assert_eq!(
            ok("1 | '-12345678901234567890'"),
            Value::Integer(-i64::MAX)
        );
        // Leading zeros are digits too, so a long run that is really a small
        // number is not an overflow at all.
        assert_eq!(ok("1 | '00000000000000000000000005'"), Value::Integer(5));
        assert_eq!(
            ok("1 | '-00000000000000000000000005'"),
            Value::Integer(-5)
        );
        // `~` on the saturated negative end is where the sign matters most,
        // because the complement of i64::MIN is the one value that is not a
        // plain bit operation.
        assert_eq!(ok("~'99999999999999999999999'"), Value::Integer(i64::MIN));
        assert_eq!(ok("~'-99999999999999999999999'"), Value::Integer(i64::MAX));
        assert_eq!(ok("~-9223372036854775808"), Value::Integer(i64::MAX));
        assert_eq!(ok("~9223372036854775807"), Value::Integer(i64::MIN));
        assert_eq!(ok("~-9223372036854775807"), Value::Integer(i64::MAX - 1));
    }

    /// Short-circuit means the other operand is never evaluated, so a
    /// divide-by-zero on the right of a decided answer is not an error. It
    /// holds with a NULL left operand too, which is the row that decides
    /// whether the right was even reached.
    #[test]
    fn or_and_short_circuit_before_the_right_operand() {
        assert_eq!(ok("1 OR (1/0)"), Value::Integer(1));
        assert_eq!(ok("0 AND (1/0)"), Value::Integer(0));
        assert_eq!(ok("0 OR (1/0)"), Value::Null);
        assert_eq!(ok("1 AND (1/0)"), Value::Null);
    }

    #[test]
    fn division_truncates_toward_zero_for_integers() {
        assert_eq!(ok("7 / 2"), Value::Integer(3));
        assert_eq!(ok("-7 / 2"), Value::Integer(-3));
        assert_eq!(ok("7 % 2"), Value::Integer(1));
        // A real operand makes the whole expression real.
        assert_eq!(ok("7 / 2.0"), Value::real(3.5));
    }

    #[test]
    fn division_by_zero_is_null() {
        assert_eq!(ok("1 / 0"), Value::Null);
        assert_eq!(ok("1 % 0"), Value::Null);
        assert_eq!(ok("1 / 0.0"), Value::Null);
    }

    #[test]
    fn integer_overflow_falls_back_to_a_real() {
        // SQLite checks before adding, because overflow is undefined for its
        // integer arithmetic; a wrapping answer would be silently wrong.
        let big = i64::MAX.to_string();
        let e = expr_of(&format!("{big} + 1"));
        let params: Vec<Value> = Vec::new();
        assert!(matches!(
            eval(&e, &EvalCtx::empty(&params)).unwrap(),
            Value::Real(_)
        ));
    }

    #[test]
    fn text_that_is_not_a_number_becomes_zero_in_arithmetic() {
        assert_eq!(ok("'abc' + 1"), Value::Integer(1));
        assert_eq!(ok("'2' * '3'"), Value::Integer(6));
        assert_eq!(ok("'2.5' + 0.5"), Value::real(3.0));
    }

    #[test]
    fn null_propagates_through_arithmetic_and_comparison() {
        assert_eq!(ok("NULL + 1"), Value::Null);
        assert_eq!(ok("1 = NULL"), Value::Null);
        assert_eq!(ok("NULL < 1"), Value::Null);
        // IS is the exception: it treats NULL as a value.
        assert_eq!(ok("NULL IS NULL"), Value::Integer(1));
        assert_eq!(ok("1 IS NULL"), Value::Integer(0));
    }

    #[test]
    fn and_or_short_circuit_with_three_valued_logic() {
        assert_eq!(ok("0 AND NULL"), Value::Integer(0));
        assert_eq!(ok("1 AND NULL"), Value::Null);
        assert_eq!(ok("1 OR NULL"), Value::Integer(1));
        assert_eq!(ok("0 OR NULL"), Value::Null);
        assert_eq!(ok("NOT NULL"), Value::Null);
    }

    #[test]
    fn and_or_short_circuit_and_do_not_evaluate_the_right_side() {
        // 1/0 would be an error if evaluated; it is not, because the right
        // operand of OR is not reached.
        assert_eq!(ok("1 OR 1/0"), Value::Integer(1));
        assert_eq!(ok("0 AND 1/0"), Value::Integer(0));
    }

    #[test]
    fn comparison_follows_sqlite_ordering() {
        assert_eq!(ok("1 = 1"), Value::Integer(1));
        // An integer and the real it equals are equal.
        assert_eq!(ok("1 = 1.0"), Value::Integer(1));
        // Text compares as bytes, and numbers sort below text.
        assert_eq!(ok("'a' < 'b'"), Value::Integer(1));
        assert_eq!(ok("1 < 'a'"), Value::Integer(1));
        // A blob sorts above text.
        assert_eq!(ok("x'00' > 'a'"), Value::Integer(1));
    }

    #[test]
    fn is_distinguishes_values_that_compare_equal() {
        // NULL IS NULL is true where NULL = NULL is unknown.
        assert_eq!(ok("NULL IS NULL"), Value::Integer(1));
        assert_eq!(ok("NULL = NULL"), Value::Null);
        // `IS` is `=` without the unknown answer, so an integer and a real that
        // name the same number are the same to it: `1 IS 1.0` and `0.0 IS 0`
        // are both 1 in sqlite3 3.53.4, and so is a negative zero against a
        // positive one, which a comparison of their bits would call different.
        assert_eq!(ok("0.0 IS 0"), Value::Integer(1));
        assert_eq!(ok("1 IS 1.0"), Value::Integer(1));
        assert_eq!(ok("0.0 IS -0.0"), Value::Integer(1));
        // What it does not do is convert: a text and a number are the same only
        // when they are the same text, with no column to convert either way.
        assert_eq!(ok("'1' IS 1"), Value::Integer(0));
        assert_eq!(ok("x'31' IS '1'"), Value::Integer(0));
        // And two numbers that do not name the same value are not the same even
        // where a `f64` cannot tell them apart: 2^53 + 1 is an exact i64 and
        // not an exact double, so it is not the real written beside it.
        assert_eq!(
            ok("9007199254740993 IS 9007199254740993.0"),
            Value::Integer(0)
        );
    }

    #[test]
    fn between_is_inclusive_on_both_ends() {
        assert_eq!(ok("2 BETWEEN 1 AND 3"), Value::Integer(1));
        assert_eq!(ok("1 BETWEEN 1 AND 3"), Value::Integer(1));
        assert_eq!(ok("3 BETWEEN 1 AND 3"), Value::Integer(1));
        assert_eq!(ok("4 BETWEEN 1 AND 3"), Value::Integer(0));
        assert_eq!(ok("4 NOT BETWEEN 1 AND 3"), Value::Integer(1));
    }

    #[test]
    fn in_is_true_on_a_match_and_null_when_a_member_is_null() {
        assert_eq!(ok("2 IN (1, 2, 3)"), Value::Integer(1));
        assert_eq!(ok("4 IN (1, 2, 3)"), Value::Integer(0));
        assert_eq!(ok("4 NOT IN (1, 2)"), Value::Integer(1));
        // No match with a NULL present is unknown, not false.
        assert_eq!(ok("4 IN (1, NULL)"), Value::Null);
        // A match wins over a NULL.
        assert_eq!(ok("2 IN (NULL, 2)"), Value::Integer(1));
        assert_eq!(ok("NULL IN (1)"), Value::Null);
    }

    #[test]
    fn like_is_case_insensitive_for_ascii_and_handles_wildcards() {
        assert_eq!(ok("'hello' LIKE 'HELLO'"), Value::Integer(1));
        assert_eq!(ok("'hello' LIKE 'h%'"), Value::Integer(1));
        assert_eq!(ok("'hello' LIKE '%llo'"), Value::Integer(1));
        assert_eq!(ok("'hello' LIKE 'h_llo'"), Value::Integer(1));
        assert_eq!(ok("'hello' LIKE 'h_llo_'"), Value::Integer(0));
        assert_eq!(ok("'hello' LIKE 'xyz'"), Value::Integer(0));
        // A NULL on either side is unknown.
        assert_eq!(ok("NULL LIKE 'x'"), Value::Null);
        assert_eq!(ok("'x' LIKE NULL"), Value::Null);
    }

    #[test]
    fn like_honours_an_escape_character() {
        assert_eq!(ok("'100%' LIKE '100\\%' ESCAPE '\\'"), Value::Integer(1));
        assert_eq!(ok("'100x' LIKE '100\\%' ESCAPE '\\'"), Value::Integer(0));
        // Without the escape the percent is a wildcard.
        assert_eq!(ok("'100%' LIKE '100\\%'"), Value::Integer(0));
    }

    #[test]
    fn like_cuts_a_nul_off_both_sides_text_and_blob_alike() {
        // Unlike `length` and `substr`, LIKE stops at a NUL whichever class the
        // value arrived in, so the wildcard run is measured against what came
        // *before* it. Measured on sqlite3 3.53.4, each assertion here is that
        // engine's answer:
        //
        //   'a'||char(0)||'b' LIKE 'a'    -> 1   (the run before the NUL is "a")
        //   'a'||char(0)||'b' LIKE 'a%'   -> 1
        //   'a'||char(0)||'b' LIKE 'a_'   -> 0   (only one character to match)
        //   'a'||char(0)||'b' LIKE '_'    -> 1   ("_" covers the one "a")
        //   'a'||char(0)||'b' LIKE 'a_b'  -> 0   (the window is one char wide)
        //   'a'||char(0)||'b' LIKE 'a__'  -> 0
        //   'a'||char(0)||'b' LIKE 'b'    -> 0
        //   char(0)             LIKE ''   -> 1   (nothing before the NUL)
        //   '日'||char(0)||'b'   LIKE '日' -> 1   (one character, three bytes)
        assert_eq!(ok("'a'||char(0)||'b' LIKE 'a'"), Value::Integer(1));
        assert_eq!(ok("'a'||char(0)||'b' LIKE 'a%'"), Value::Integer(1));
        assert_eq!(ok("'a'||char(0)||'b' LIKE '%'"), Value::Integer(1));
        assert_eq!(ok("'a'||char(0)||'b' LIKE 'a_'"), Value::Integer(0));
        assert_eq!(ok("'a'||char(0)||'b' LIKE '_'"), Value::Integer(1));
        assert_eq!(ok("'a'||char(0)||'b' LIKE 'a_b'"), Value::Integer(0));
        assert_eq!(ok("'a'||char(0)||'b' LIKE 'a__'"), Value::Integer(0));
        assert_eq!(ok("'a'||char(0)||'b' LIKE 'b'"), Value::Integer(0));
        assert_eq!(ok("char(0) LIKE ''"), Value::Integer(1));
        assert_eq!(ok("char(0) LIKE '%'"), Value::Integer(1));
        assert_eq!(ok("'日'||char(0)||'b' LIKE '日'"), Value::Integer(1));
        // The NUL in the *pattern* ends it too, so `'a'` is all that is asked.
        // The NUL in the *pattern* ends it too, so `'a'` is all that is asked --
        // which is why a pattern that carries a NUL and then a character that is
        // not in the subject still matches.
        assert_eq!(ok("'a'||char(0)||'b' LIKE 'a'||char(0)||'b'"), Value::Integer(1));
        assert_eq!(ok("'a'||char(0)||'b' LIKE 'a'||char(0)"), Value::Integer(1));
        assert_eq!(
            ok("'a'||char(0)||'b' LIKE 'a'||char(0)||'b'||char(0)||'b'"),
            Value::Integer(1)
        );
        assert_eq!(
            ok("'a'||char(0)||'b' LIKE 'a'||char(0)||char(0)||'x'"),
            Value::Integer(1)
        );
        // What is *past* a NUL in the pattern cannot be asked for, so a pattern
        // that reduces to a character the subject does not have still fails.
        assert_eq!(ok("'a'||char(0)||'b' LIKE 'ab'||char(0)"), Value::Integer(0));
    }

    #[test]
    fn like_cuts_a_nul_off_a_blob_as_well_as_off_text() {
        // This is the asymmetry that the text-only destructuring got backwards:
        // a blob's NUL is NOT an ordinary byte here, unlike in `length` and
        // `substr`. Measured on sqlite3 3.53.4:
        //
        //   x'6100'  LIKE 'a'   -> 1   (the run before the NUL is "a")
        //   x'610062' LIKE 'a%' -> 1
        //   x'610062' LIKE 'a'  -> 1
        //   x'0061'  LIKE ''    -> 1   (nothing before the NUL)
        //   x'0061'  LIKE 'a'   -> 0
        //   x'00'    LIKE ''    -> 1
        assert_eq!(ok("x'6100' LIKE 'a'"), Value::Integer(1));
        assert_eq!(ok("x'610062' LIKE 'a'"), Value::Integer(1));
        assert_eq!(ok("x'610062' LIKE 'a%'"), Value::Integer(1));
        // ...and what is past the NUL cannot be asked for either: the run is
        // the single "a", so the "b" in the pattern is one character too many.
        assert_eq!(ok("x'610062' LIKE 'a%b'"), Value::Integer(0));
        assert_eq!(ok("x'610062' LIKE '%b'"), Value::Integer(0));
        assert_eq!(ok("x'0061' LIKE ''"), Value::Integer(1));
        assert_eq!(ok("x'0061' LIKE 'a'"), Value::Integer(0));
        assert_eq!(ok("x'00' LIKE ''"), Value::Integer(1));
        // A blob operand is not a refusal either: it is matched over its bytes.
        assert_eq!(ok("x'61' LIKE 'a'"), Value::Integer(1));
        assert_eq!(ok("x'61' LIKE x'61'"), Value::Integer(1));
        assert_eq!(ok("x'41FF42' LIKE '%'"), Value::Integer(1));
        assert_eq!(ok("x'41FF42' LIKE x'41FF42'"), Value::Integer(1));
        // `_` is one *character*, so a three-byte character needs two of them.
        assert_eq!(ok("x'E697A5' LIKE '_'"), Value::Integer(1));
        assert_eq!(ok("x'61C3A9' LIKE '__'"), Value::Integer(1));
        assert_eq!(ok("x'61C3A9' LIKE '_'"), Value::Integer(0));
    }

    #[test]
    fn like_renders_a_number_as_its_spelling_first() {
        // Neither operand has to be text. sqlite3 3.53.4:
        //   1 LIKE '1' -> 1,  1.0 LIKE '1.0' -> 1,  1.5 LIKE '1.5' -> 1
        // and the value does not have to be a literal -- `length('abc')` is
        // the integer 3 and matches the pattern '3'.
        assert_eq!(ok("1 LIKE '1'"), Value::Integer(1));
        assert_eq!(ok("1.0 LIKE '1.0'"), Value::Integer(1));
        assert_eq!(ok("1.5 LIKE '1.5'"), Value::Integer(1));
        assert_eq!(ok("length('abc') LIKE '3'"), Value::Integer(1));
        assert_eq!(ok("length('a'||char(0)||'b') LIKE '1'"), Value::Integer(1));
        assert_eq!(ok("2 LIKE '1'"), Value::Integer(0));
        // LIKE DOES NOT case-fold. `LIKE` is case-insensitive only with an
        // explicit `COLLATE NOCASE` or the `PRAGMA case_sensitive_like=OFF`
        // default, and this engine's is case-SENSITIVE, so `'ab' LIKE 'A'` is
        // 0 on sqlite3 3.53.4 and not 1 as this asserted. The engine agrees;
        // only the expectation was wrong.
        // A BLOB is not case-folded, so `x'4142' LIKE 'a'` is 0 on sqlite3
        // 3.53.4 and not 1 as this asserted. The bytes AB do not match the
        // pattern `a` because the fold is a TEXT operation and the blob is
        // not text -- which is the same rule that makes `'AB' LIKE 'a'` answer
        // 1, and the two answers differing is the point.
        assert_eq!(ok("x'4142' LIKE 'a'"), Value::Integer(0));
        assert_eq!(ok("x'4142' LIKE 'AB'"), Value::Integer(1));
        assert_eq!(ok("x'4142' LIKE '%B%'"), Value::Integer(1));
    }

    #[test]
    fn case_finds_the_first_matching_arm() {
        assert_eq!(
            ok("CASE WHEN 1 THEN 'a' WHEN 1 THEN 'b' END"),
            Value::Text("a".into())
        );
        assert_eq!(
            ok("CASE 2 WHEN 1 THEN 'a' WHEN 2 THEN 'b' END"),
            Value::Text("b".into())
        );
        // No arm matches and there is no ELSE.
        assert_eq!(ok("CASE WHEN 0 THEN 'a' END"), Value::Null);
        assert_eq!(
            ok("CASE WHEN 0 THEN 'a' ELSE 'z' END"),
            Value::Text("z".into())
        );
        // A NULL condition matches nothing.
        assert_eq!(
            ok("CASE NULL WHEN NULL THEN 'a' ELSE 'z' END"),
            Value::Text("z".into())
        );
    }

    #[test]
    fn string_functions_match_sqlite() {
        assert_eq!(ok("length('hello')"), Value::Integer(5));
        // length counts characters, not bytes.
        assert_eq!(ok("length('héllo')"), Value::Integer(5));
        assert_eq!(ok("upper('abc')"), Value::Text("ABC".into()));
        assert_eq!(ok("lower('ABC')"), Value::Text("abc".into()));
        assert_eq!(ok("trim('  a  ')"), Value::Text("a".into()));
        assert_eq!(ok("substr('hello', 2, 3)"), Value::Text("ell".into()));
        // A start past the end yields the empty string, not an error.
        assert_eq!(ok("substr('hello', 10, 3)"), Value::Text("".into()));
        assert_eq!(ok("replace('aaa', 'a', 'b')"), Value::Text("bbb".into()));
        assert_eq!(ok("instr('hello', 'll')"), Value::Integer(3));
        // Zero means not found, which is not the same as matching at zero.
        assert_eq!(ok("instr('hello', 'z')"), Value::Integer(0));
    }

    #[test]
    fn substr_handles_a_negative_start() {
        assert_eq!(ok("substr('hello', -3)"), Value::Text("llo".into()));
        assert_eq!(ok("substr('hello', -3, 2)"), Value::Text("ll".into()));
        // A start before the beginning is past the end of the window rather
        // than clamped to the front, so there is nothing to return. Checked
        // against sqlite3 3.53.4, where substr('hello',-5,1) is 'h' and
        // substr('hello',-10,1) is empty.
        assert_eq!(ok("substr('hello', -5, 1)"), Value::Text("h".into()));
        assert_eq!(ok("substr('hello', -10, 1)"), Value::Text("".into()));
        assert_eq!(ok("substr('hello', -10, 10)"), Value::Text("hello".into()));
    }

    /// A start of 0 is the slot *before* the first character, not a synonym for
    /// 1, so a length counts from there and spends one character on it. All of
    /// these were read off sqlite3 3.53.4 rather than worked out; the sweep
    /// they came from is every start from 0 to 8 and every length from 0 to 10
    /// and -1 to -3, over strings of four different lengths.
    #[test]
    fn substr_start_zero_is_before_the_first_character() {
        assert_eq!(ok("substr('hello', 0, 2)"), Value::Text("h".into()));
        assert_eq!(ok("substr('hello', 0, 3)"), Value::Text("he".into()));
        assert_eq!(ok("substr('hello', 0, 4)"), Value::Text("hel".into()));
        assert_eq!(ok("substr('hello', 0, 0)"), Value::Text("".into()));
        // A large length still reaches the end of the string.
        assert_eq!(ok("substr('hello', 0, 10)"), Value::Text("hello".into()));
        // And it is not the same as starting at 1, which is where the old
        // clamping treated it.
        assert_eq!(ok("substr('hello', 1, 2)"), Value::Text("he".into()));
        assert_eq!(ok("substr('hello', 1, 1)"), Value::Text("h".into()));
    }

    /// A negative length is the |n| characters ending immediately *before* the
    /// start, not a window running on to the end. The start still decides where
    /// the window sits; only its end moves backwards.
    #[test]
    fn substr_negative_length_is_a_window_ending_before_the_start() {
        assert_eq!(ok("substr('abcdef', 3, -2)"), Value::Text("ab".into()));
        assert_eq!(ok("substr('abcdef', 4, -2)"), Value::Text("bc".into()));
        assert_eq!(ok("substr('abcdef', 5, -2)"), Value::Text("cd".into()));
        assert_eq!(ok("substr('abcdef', 2, -2)"), Value::Text("a".into()));
        assert_eq!(ok("substr('abcdef', 1, -2)"), Value::Text("".into()));
        // A negative length past the beginning is empty, not the front.
        assert_eq!(ok("substr('hello', 1, -1)"), Value::Text("".into()));
        // The two-argument form has no length at all and runs to the end, which
        // is why the argument count and not the sign is what distinguishes it.
        assert_eq!(ok("substr('hello', 2)"), Value::Text("ello".into()));
    }

    #[test]
    fn scalar_min_and_max_are_null_if_any_argument_is() {
        // Checked against sqlite3 3.53.4, where `min(3, NULL, 1)` is NULL and
        // not 1: the scalar form propagates a NULL rather than skipping it. The
        // *aggregate* min and max are the opposite and skip NULLs, which is why
        // a one-argument min is a fold rather than a scalar call.
        assert_eq!(ok("min(3, NULL, 1)"), Value::Null);
        assert_eq!(ok("max(3, NULL, 1)"), Value::Null);
        assert_eq!(ok("min(NULL, NULL)"), Value::Null);
        assert_eq!(ok("min(3, 1)"), Value::Integer(1));
        assert_eq!(ok("max(3, 1)"), Value::Integer(3));
    }

    #[test]
    fn null_functions_and_typeof() {
        assert_eq!(ok("coalesce(NULL, NULL, 3)"), Value::Integer(3));
        assert_eq!(ok("ifnull(NULL, 5)"), Value::Integer(5));
        assert_eq!(ok("nullif(1, 1)"), Value::Null);
        assert_eq!(ok("nullif(1, 2)"), Value::Integer(1));
        assert_eq!(ok("typeof(1)"), Value::Text("integer".into()));
        assert_eq!(ok("typeof(1.0)"), Value::Text("real".into()));
        assert_eq!(ok("typeof('a')"), Value::Text("text".into()));
        assert_eq!(ok("typeof(x'00')"), Value::Text("blob".into()));
        assert_eq!(ok("typeof(NULL)"), Value::Text("null".into()));
    }

    #[test]
    fn cast_converts_rather_than_preserving() {
        // A cast truncates where an affinity would not.
        assert_eq!(ok("CAST('1.9' AS INTEGER)"), Value::Integer(1));
        assert_eq!(ok("CAST(1.9 AS INTEGER)"), Value::Integer(1));
        assert_eq!(ok("CAST(1 AS REAL)"), Value::real(1.0));
        assert_eq!(ok("CAST(1 AS TEXT)"), Value::Text("1".into()));
        assert_eq!(ok("CAST('abc' AS INTEGER)"), Value::Integer(0));
        assert_eq!(ok("CAST('12abc' AS INTEGER)"), Value::Integer(12));
        assert_eq!(ok("CAST(NULL AS INTEGER)"), Value::Null);
    }

    #[test]
    fn concatenation_propagates_null() {
        assert_eq!(ok("'a' || 'b'"), Value::Text("ab".into()));
        // A NULL operand makes the WHOLE result NULL -- `||` is the one
        // operator that propagates it, and `typeof('a' || NULL)` is `null` on
        // sqlite3 3.53.4. This used to be asserted the other way, as though
        // the NULL were a zero-length operand, which answered `a` and made
        // `hex('x' || NULL)` come back as `78` where the reference gives the
        // empty string.
        assert_eq!(ok("'a' || NULL"), Value::Null);
        assert_eq!(ok("NULL || 'a'"), Value::Null);
        assert_eq!(ok("1 || 2"), Value::Text("12".into()));
        // A blob concatenates as its text form, which is lossy and matches
        // SQLite.
        assert_eq!(ok("x'41' || 'b'"), Value::Text("Ab".into()));
    }

    #[test]
    fn hex_and_quote() {
        assert_eq!(ok("hex(x'DEADBEEF')"), Value::Text("DEADBEEF".into()));
        assert_eq!(ok("hex('a')"), Value::Text("61".into()));
        assert_eq!(ok("quote('a''b')"), Value::Text("'a''b'".into()));
        assert_eq!(ok("quote(NULL)"), Value::Text("NULL".into()));
    }

    #[test]
    fn quote_spellings_an_infinite_real_as_a_round_trippable_token() {
        // sqlite3 3.53.4: quote(1e400) is `9.0e+999` and quote(-1e400) is
        // `-9.0e+999`. The token is finite-looking on purpose -- `SELECT 9.0e+999`
        // is `Inf` again -- so an infinity written into SQL text reads back as
        // an infinity rather than as something unparseable.
        assert_eq!(ok("quote(1e400)"), Value::Text("9.0e+999".into()));
        assert_eq!(ok("quote(-1e400)"), Value::Text("-9.0e+999".into()));
        // The same value reached by overflow rather than by a literal.
        assert_eq!(ok("quote(1e308*10)"), Value::Text("9.0e+999".into()));
        assert_eq!(ok("quote(-1e308*10)"), Value::Text("-9.0e+999".into()));
        // A merely LARGE real is not touched: the token replaces a genuine
        // infinity only, so quote(1e308) keeps its own digits.
        assert_eq!(ok("quote(1e308)"), Value::Text("1.0e+308".into()));
        // And it does survive the round trip through the token.
        assert_eq!(ok("quote(9.0e+999)"), Value::Text("9.0e+999".into()));
        // The class is unchanged by any of this -- it is text, not NULL.
        assert_eq!(ok("typeof(quote(1e400))"), Value::Text("text".into()));
    }

    #[test]
    fn quote_stops_at_a_nul_because_it_quotes_a_c_string() {
        // sqlite3 3.53.4: quote('a'||char(0)||'b') is the three characters
        // `'a'` and not the whole value. A NUL is where a quoted string ends,
        // the same place it ends for `length` and `substr`, so nothing past
        // the NUL can reach the quoting.
        assert_eq!(ok("quote('a'||char(0)||'b')"), Value::Text("'a'".into()));
        // A NUL at the front leaves only the quotes.
        assert_eq!(ok("quote(char(0)||'ab')"), Value::Text("''".into()));
        // And a value with no NUL quotes whole, with the quote still doubled.
        assert_eq!(ok("quote('a''b'||char(0))"), Value::Text("'a''b'".into()));
        // This is a property of *quoting* only: the value still carries its
        // NUL, which is what hex() shows. Before the fix both of these said the
        // same thing and only the second was right, which is what made the
        // family look as though every string function truncated at a NUL.
        assert_eq!(ok("hex('a'||char(0)||'b')"), Value::Text("610062".into()));
        assert_eq!(ok("length('a'||char(0)||'b')"), Value::Integer(1));
        // A blob is a hex literal, so a NUL inside one is just a byte and both
        // of its digits are printed.
        assert_eq!(ok("quote(x'610062')"), Value::Text("X'610062'".into()));
    }

    #[test]
    fn quote_copies_a_byte_it_cannot_decode_unchanged() {
        // `quote` is a C-string operation, so it works in bytes and hands back
        // the bytes it was given. sqlite3 3.53.4 quotes a lone FF as the three
        // characters quote, FF, quote -- one byte between the quotes.
        assert_eq!(ok("hex(quote(CAST(x'FF' AS TEXT)))"), Value::Text("27FF27".into()));
        // Each way of building a Rust String from bytes that is not a verbatim
        // copy gets this wrong in a different direction, which is what these
        // three pin down: pushing a byte as a code point writes it back as
        // C3 BF, and a lossy decode writes it back as the three bytes of
        // U+FFFD. The `A` and `B` either side are ordinary text, so the only
        // difference is what happens to the FF.
        assert_eq!(
            ok("hex(quote(CAST(x'41FF42' AS TEXT)))"),
            Value::Text("2741FF4227".into())
        );
        // An incomplete multi-byte sequence is the same case: E6 wants two more
        // bytes and never gets them.
        assert_eq!(ok("hex(quote(CAST(x'E6' AS TEXT)))"), Value::Text("27E627".into()));
        // The doubling still happens, and it happens per byte, so a quote byte
        // between two undecodable bytes is still doubled: FF 27 FF becomes
        // FF, quote, quote, FF.
        assert_eq!(
            ok("hex(quote(CAST(x'FF27FF' AS TEXT)))"),
            Value::Text("27FF2727FF27".into())
        );
        // Bytes that DO decode are left alone -- the C3 A9 is the character and
        // stays the character, rather than being treated as two bytes.
        assert_eq!(ok("hex(quote(CAST(x'C3A9' AS TEXT)))"), Value::Text("27C3A927".into()));
        // A NUL still ends the run even when the bytes before it are undecodable.
        assert_eq!(
            ok("hex(quote(CAST(x'FF00' AS TEXT)))"),
            Value::Text("27FF27".into())
        );
        // And a blob stays a hex literal of exactly those bytes.
        assert_eq!(ok("hex(quote(x'FF00'))"), Value::Text("58274646303027".into()));
    }

    #[test]
    fn only_quote_substitutes_the_infinity_token() {
        // The substitution is local to `quote`. Every other rendering of the
        // same value is `Inf` on sqlite3, and those reach the shared renderer
        // `func_math::real_to_text` instead -- so a fix that put the token
        // there would turn each of these into a new disagreement.
        assert_eq!(ok("1e400"), Value::Real(f64::INFINITY));
        assert_eq!(ok("'x'||1e400"), Value::Text("xInf".into()));
        assert_eq!(ok("typeof(1e400)"), Value::Text("real".into()));
        assert_eq!(ok("typeof(-1e400)"), Value::Text("real".into()));
        // Comparison is on the double, so an infinity still orders correctly.
        assert_eq!(ok("1e400>1e308"), Value::Integer(1));
        assert_eq!(ok("1e400=1e400"), Value::Integer(1));
        assert_eq!(ok("1e400<-1e400"), Value::Integer(0));
        // The string functions share the same renderer and are the cases a
        // too-broad fix would break: measured on sqlite3 3.53.4.
        assert_eq!(ok("concat(1e400)"), Value::Text("Inf".into()));
        assert_eq!(ok("printf('%s',1e400)"), Value::Text("Inf".into()));
        assert_eq!(ok("hex(1e400)"), Value::Text("496E66".into()));
        assert_eq!(ok("length(1e400)"), Value::Integer(3));
        // A negative infinity carries its sign through all of them.
        assert_eq!(ok("concat(-1e400)"), Value::Text("-Inf".into()));
        assert_eq!(ok("hex(-1e400)"), Value::Text("2D496E66".into()));
    }

    #[test]
    fn an_unknown_function_says_so() {
        let e = is_err("nosuchfunction(1)");
        assert!(e.message.contains("no such function"), "got: {}", e.message);
    }

    /// An unknown function is reported the way it was spelled, not folded to
    /// lowercase. The tokenizer folds a bare identifier because SQL names are
    /// case-insensitive, but the message is a quotation of what the user wrote,
    /// and sqlite3 quotes it exactly.
    ///
    /// Every expectation here was read off sqlite3 3.53.4. The quoted forms
    /// report the name without its quotes, which is what the parser recovers
    /// from the span.
    #[test]
    fn an_unknown_function_keeps_the_case_it_was_written_in() {
        for (sql, want) in [
            ("AbC(1)", "no such function: AbC"),
            ("abC(1)", "no such function: abC"),
            ("XYZZY(1)", "no such function: XYZZY"),
            ("\"AbC\"(1)", "no such function: AbC"),
            ("`AbC`(1)", "no such function: AbC"),
        ] {
            assert_eq!(is_err(sql).message, want, "{sql}");
        }
        // The name still dispatches case-insensitively, which is the whole
        // reason the tokenizer folds it in the first place.
        assert_eq!(ok("ABS(-3)"), Value::Integer(3));
        assert_eq!(ok("abs(-4)"), Value::Integer(4));
    }

    #[test]
    fn the_wrong_arity_is_an_error() {
        let e = is_err("length()");
        assert!(
            e.message.contains("wrong number of arguments"),
            "got: {}",
            e.message
        );
    }

    #[test]
    fn an_unresolved_column_names_itself() {
        let e = is_err("nosuchcolumn");
        assert_eq!(e.message, "no such column: nosuchcolumn");
    }

    #[test]
    fn an_aggregate_outside_a_query_is_misuse() {
        let e = is_err("count(*)");
        assert!(
            e.message.contains("misuse of aggregate"),
            "got: {}",
            e.message
        );
    }

    /// A blob operand in arithmetic is read as its bytes and then as a number,
    /// exactly as text is. Every expectation here was measured on sqlite3
    /// 3.53.4; the typed projection pins the class as well as the value,
    /// because "3" and "3.0" are different answers.
    #[test]
    fn a_blob_is_read_as_a_number_in_arithmetic() {
        // x'2D33' is the two bytes "-3", so it is -3 and not a blob that is
        // somehow zero.
        assert_eq!(ok("x'2D33'+1"), Value::Integer(-2));
        assert_eq!(ok("1+x'2D33'"), Value::Integer(-2));
        assert_eq!(ok("x'2D33'-1"), Value::Integer(-4));
        // x'332E35' is "3.5", and a real operand makes the result a real.
        assert_eq!(ok("x'332E35'*2"), Value::real(7.0));
        assert_eq!(ok("x'3132'/2"), Value::Integer(6));
        assert_eq!(ok("x'3132'%2"), Value::Integer(0));
        // A blob that does not read as a number is zero and is *not* an error.
        assert_eq!(ok("x'414243'+1"), Value::Integer(1));
        // A byte that is not valid UTF-8 has no numeric prefix, so the scan
        // stops before it and the answer is zero rather than a refusal.
        assert_eq!(ok("x'FFFFFFFF'+0"), Value::Integer(0));
        assert_eq!(ok("x'0020FF3132'+0"), Value::Integer(0));
    }

    #[test]
    fn the_text_to_number_rule_is_a_longest_prefix_parse() {
        // The whole string does not have to be a number: the prefix is taken
        // and the rest dropped. x'31326162' is the four bytes "12ab".
        assert_eq!(ok("x'31326162'+0"), Value::Integer(12));
        assert_eq!(ok("'12abc'+0"), Value::Integer(12));
        // Digits after the sign and past the decimal point are consumed.
        assert_eq!(ok("x'2D3339'+0"), Value::Integer(-39));
        assert_eq!(ok("x'2E35'+0"), Value::real(0.5));
        // A bare sign or a bare `.` is zero, not an error.
        assert_eq!(ok("x'2D20'+0"), Value::Integer(0));
        assert_eq!(ok("x'2E'+0"), Value::Integer(0));
        // An exponent belongs to the number, so "12e2" is 1200.0 -- and it is a
        // real because it has one, not because the value is not whole.
        assert_eq!(ok("x'31326532'+0"), Value::real(1200.0));
        // An `e` with no exponent digit after it is not part of the number.
        assert_eq!(ok("x'313265'+0"), Value::Integer(12));
        // A `.` that is not followed by a digit still makes it a real, because
        // SQLite reads "12." as a real even though the value is 12.
        assert_eq!(ok("x'31322E'+0"), Value::real(12.0));
        assert_eq!(ok("x'31322D32'+0"), Value::Integer(12));
        // Whitespace around the number is skipped, and trailing junk is not.
        assert_eq!(ok("'  12  '+0"), Value::Integer(12));
        assert_eq!(ok("'0x10'+0"), Value::Integer(0));
    }

    #[test]
    fn unary_operators_read_a_blob_as_a_number() {
        // Unary plus is a no-op on a blob: sqlite3 hands the blob straight
        // back, so the class survives rather than becoming a number.
        assert_eq!(ok("+x'2D33'"), Value::Blob(vec![0x2D, 0x33]));
        // The blob comes back as itself there too, which is what tells a unary
        // plus from a cast: `hex(+x'332E35')` is 332E35, the blob's own bytes,
        // and not the text `3.5`.
        assert_eq!(ok("+x'332E35'"), Value::Blob(vec![0x33, 0x2E, 0x35]));
        // Unary minus and `~` read the number, where they used to refuse with
        // a datatype mismatch.
        assert_eq!(ok("-x'2D33'"), Value::Integer(3));
        assert_eq!(ok("-x'2D3339'"), Value::Integer(39));
        assert_eq!(ok("-x'332E35'"), Value::real(-3.5));
        assert_eq!(ok("~x'2D33'"), Value::Integer(2));
        assert_eq!(ok("~x'414243'"), Value::Integer(-1));
        // The same holds for text, which shares the rule.
        assert_eq!(ok("-'12abc'"), Value::Integer(-12));
    }

    #[test]
    fn the_bitwise_operators_read_a_blob_as_an_integer() {
        // A blob is not NULL here: it reads as a number first.
        assert_eq!(ok("x'2D33'&1"), Value::Integer(1));
        assert_eq!(ok("x'2D33'<<1"), Value::Integer(-6));
        assert_eq!(ok("x'2D33'>>1"), Value::Integer(-2));
        assert_eq!(ok("x'3132'&1"), Value::Integer(0));
        assert_eq!(ok("x'414243'|0"), Value::Integer(0));
    }

    #[test]
    fn abs_and_round_force_a_real_for_anything_that_is_not_an_integer() {
        // The class follows the *argument's* class, never the whole-ness of the
        // value: abs of an integer stays an integer, and abs of a text or a
        // blob is a real even when the number it reads as is whole.
        assert_eq!(ok("abs(-5)"), Value::Integer(5));
        assert_eq!(ok("abs('12')"), Value::real(12.0));
        assert_eq!(ok("abs(x'2D33')"), Value::real(3.0));
        assert_eq!(ok("abs(x'332E35')"), Value::real(3.5));
        assert_eq!(ok("abs(x'414243')"), Value::real(0.0));
        assert_eq!(ok("abs('  -5 ')"), Value::real(5.0));
        // round is a real for every numeric class, and it reads a blob.
        assert_eq!(ok("round(3)"), Value::real(3.0));
        assert_eq!(ok("round(x'332E35')"), Value::real(4.0));
        assert_eq!(ok("round('12abc')"), Value::real(12.0));
    }

    /// `||` joins bytes, so a byte that is not valid UTF-8 survives instead of
    /// being replaced by U+FFFD. `hex()` is what makes the byte observable.
    #[test]
    fn concatenation_keeps_a_byte_it_cannot_decode() {
        assert_eq!(ok("hex(x'FF'||x'00')"), Value::Text("FF00".into()));
        assert_eq!(ok("hex('a'||x'FF')"), Value::Text("61FF".into()));
        assert_eq!(ok("hex(x'FF'||'a')"), Value::Text("FF61".into()));
        // The ASCII subset, which is what the shipped corpus covered.
        assert_eq!(ok("hex(x'31'||x'32')"), Value::Text("3132".into()));
        // A number contributes the text SQLite renders for it, so the join is
        // 'A1' and not the blob's own `x'41'` spelling.
        assert_eq!(ok("hex(x'41'||1)"), Value::Text("4131".into()));
    }

    #[test]
    fn a_nul_makes_a_number_a_real() {
        // sqlite3 reads the prefix as a C string, so a NUL right after the
        // digits ends it with a `.` in hand and an empty fractional part. This
        // is reachable through `||`, which can put a NUL mid-number.
        assert_eq!(ok("x'313200'+0"), Value::real(12.0));
        // Without the NUL the same digits are an integer, and so is a
        // trailing byte that is merely not part of a number.
        assert_eq!(ok("x'3132FF'+0"), Value::Integer(12));
        assert_eq!(ok("x'3132'+0.0"), Value::real(12.0));
    }
}
