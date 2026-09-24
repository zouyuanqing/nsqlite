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
use crate::parser::{BinOp, Expr, Literal, UnaryOp};
use crate::value::Value;

/// The values visible to an expression: bound parameters and the row in scope.
#[derive(Debug, Default, Clone)]
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
        }
    }

    /// Looks a name up in the current row, matching case-insensitively.
    pub fn lookup(&self, name: &str) -> Option<&Value> {
        self.row
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v)
    }
}

/// Evaluates an expression to a value.
///
/// Errors carry SQLite's wording, because the official suite compares the
/// message text of a failing statement verbatim.
pub fn eval(expr: &Expr, ctx: &EvalCtx<'_>) -> Result<Value> {
    match expr {
        Expr::Literal(lit) => eval_literal(lit, ctx),
        Expr::Column {
            table,
            name,
            span,
        } => {
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
            match table {
                // A qualified name that did not resolve usually means the table
                // is not in the query at all.
                Some(t) => Err(Error::new(
                    ResultCode::Error,
                    format!("no such column: {t}.{name}"),
                )),
                None => Err(Error::new(
                    ResultCode::Error,
                    format!("no such column: {name}"),
                )),
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
            // the rest of the tree is evaluated at all.
            if *op == BinOp::And {
                // A false left operand decides the answer, but a NULL one does
                // not: 0 AND NULL is false because the left was false, while
                // 1 AND NULL is unknown.
                let lv = eval(left, ctx)?;
                if !lv.is_null() && !truthy(lv.clone()) {
                    return Ok(Value::Integer(0));
                }
                let rv = eval(right, ctx)?;
                if rv.is_null() {
                    return Ok(Value::Null);
                }
                return Ok(Value::Integer(i64::from(truthy(lv) && truthy(rv))));
            }
            if *op == BinOp::Or {
                // Short-circuit only on a true left operand. A false one does
                // not decide the answer: 0 OR NULL is unknown, not false,
                // because the right operand was never false either.
                let l = truthy(eval(left, ctx)?);
                if l {
                    return Ok(Value::Integer(1));
                }
                let rv = eval(right, ctx)?;
                if rv.is_null() {
                    return Ok(Value::Null);
                }
                return Ok(Value::Integer(i64::from(truthy(rv))));
            }
            let l = eval(left, ctx)?;
            let r = eval(right, ctx)?;
            binary(*op, l, r)
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
            let inside = v.compare(&lo) != Ordering::Less && v.compare(&hi) != Ordering::Greater;
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
            let mut saw_null = false;
            for item in list {
                let candidate = eval(item, ctx)?;
                if candidate.is_null() {
                    saw_null = true;
                    continue;
                }
                if v.eq_value(&candidate) {
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
            let matched = like(&v, &p, e.as_ref());
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
                return Err(Error::new(
                    ResultCode::Error,
                    format!("misuse of aggregate function {name}()"),
                ));
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
            call(name, &vals)
        }
        Expr::Case {
            operand,
            whens,
            otherwise,
        } => {
            let base = match operand {
                Some(e) => Some(eval(e, ctx)?),
                None => None,
            };
            for (cond, result) in whens {
                let cv = eval(cond, ctx)?;
                let hit = match &base {
                    // The searched form evaluates the arm's condition directly.
                    None => truthy(cv),
                    Some(b) => !b.is_null() && !cv.is_null() && b.eq_value(&cv),
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
pub fn truthy(v: Value) -> bool {
    match v {
        Value::Integer(i) => i != 0,
        Value::Real(r) => r != 0.0 && !r.is_nan(),
        Value::Null => false,
        other => match text_to_number(other) {
            Some(n) => n != 0.0,
            None => false,
        },
    }
}

fn text_to_number(v: Value) -> Option<f64> {
    match v {
        Value::Integer(i) => Some(i as f64),
        Value::Real(r) => Some(r),
        Value::Text(s) => s.trim().parse::<f64>().ok(),
        _ => None,
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
            Value::Integer(i) => Value::Integer(i.wrapping_neg()),
            Value::Real(r) => Value::real(-r),
            // Negating text is not a conversion SQLite performs; the whole
            // expression is a datatype mismatch rather than a silent zero.
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
        // Unary plus is a no-op that still converts text, as SQLite does.
        UnaryOp::Plus => match v {
            Value::Null => Value::Null,
            Value::Integer(_) | Value::Real(_) => v,
            Value::Text(s) => match s.trim().parse::<f64>() {
                Ok(f) if f.fract() == 0.0 => Value::Integer(f as i64),
                Ok(f) => Value::real(f),
                Err(_) => Value::Integer(0),
            },
            Value::Blob(_) => Value::Integer(0),
        },
        UnaryOp::BitwiseNot => match v {
            Value::Null => Value::Null,
            Value::Integer(i) => Value::Integer(!i),
            other => {
                return Err(Error::new(
                    ResultCode::Error,
                    format!(
                        "datatype mismatch: ~ requires an integer, got {}",
                        type_name(&other)
                    ),
                ))
            }
        },
    })
}

fn binary(op: BinOp, l: Value, r: Value) -> Result<Value> {
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
            let ord = l.compare(&r);
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
            // IS compares NULL as equal to NULL, and two reals with the same
            // bits as equal, which is why it is not the same as `=`.
            let same = match (&l, &r) {
                (Value::Null, Value::Null) => true,
                (Value::Null, _) | (_, Value::Null) => false,
                (Value::Integer(a), Value::Integer(b)) => a == b,
                (Value::Real(a), Value::Real(b)) => a.to_bits() == b.to_bits(),
                (Value::Text(a), Value::Text(b)) => a == b,
                (Value::Blob(a), Value::Blob(b)) => a == b,
                _ => false,
            };
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
            Ok(Value::Text(format!(
                "{}{}",
                as_text_for_concat(&l),
                as_text_for_concat(&r)
            )))
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
        Value::Blob(b) => String::from_utf8_lossy(b).into_owned(),
        other => other.to_string(),
    }
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
            if both_int {
                let (x, y) = (ai.unwrap(), bi.unwrap());
                if y == 0 {
                    return Ok(Value::Null);
                }
                Ok(Value::Integer(x.wrapping_rem(y)))
            } else if bf == 0.0 {
                Ok(Value::Null)
            } else {
                Ok(Value::real(af % bf))
            }
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

fn bitwise(op: BinOp, l: Value, r: Value) -> Result<Value> {
    // Bitwise operators take integers, converting text first.
    let to_int = |v: &Value| -> Option<i64> {
        match v {
            Value::Null => None,
            Value::Integer(i) => Some(*i),
            Value::Real(r) if r.fract() == 0.0 && r.is_finite() => Some(*r as i64),
            Value::Text(s) => s.trim().parse::<i64>().ok(),
            // A real with a fraction is not an integer, and a blob is not a
            // number at all, so both make the whole expression NULL.
            _ => None,
        }
    };
    let (Some(a), Some(b)) = (to_int(&l), to_int(&r)) else {
        return Ok(Value::Null);
    };
    Ok(Value::Integer(match op {
        BinOp::BitwiseOr => a | b,
        BinOp::BitwiseAnd => a & b,
        BinOp::LeftShift => {
            // A shift of 64 or more is undefined in SQLite and yields zero.
            if !(0..64).contains(&b) {
                0
            } else {
                a.wrapping_shl(b as u32)
            }
        }
        BinOp::RightShift => {
            if !(0..64).contains(&b) {
                if a < 0 {
                    -1
                } else {
                    0
                }
            } else {
                a.wrapping_shr(b as u32)
            }
        }
        _ => unreachable!("only bitwise operators reach here"),
    }))
}

/// A value as a number, for arithmetic and comparison.
/// The value as a number, for arithmetic and comparison.
///
/// A real is never narrowed, because a real anywhere in an expression makes the
/// result a real: 7/2.0 is 3.5, not 3. Text that parses as a whole number
/// becomes an integer, and text that does not parse becomes zero.
fn numeric_of(v: &Value) -> Value {
    match v {
        Value::Integer(_) | Value::Real(_) => v.clone(),
        Value::Text(s) => match s.trim().parse::<f64>() {
            Ok(f) if f.fract() == 0.0 && f.is_finite() => Value::Integer(f as i64),
            Ok(f) => Value::real(f),
            Err(_) => Value::Integer(0),
        },
        Value::Blob(_) => Value::Integer(0),
        Value::Null => Value::Null,
    }
}

/// SQLite's CAST, which is a conversion rather than an affinity.
pub fn cast(v: Value, ty: &str) -> Result<Value> {
    use crate::affinity::{affinity_of, Affinity};
    let aff = affinity_of(ty);
    if v.is_null() {
        return Ok(Value::Null);
    }
    Ok(match aff {
        Affinity::Blob => v,
        Affinity::Text => match v {
            Value::Integer(i) => Value::Text(i.to_string()),
            Value::Real(r) => Value::Text(crate::value::Value::real(r).to_string()),
            other => other,
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
            Value::Blob(b) => Value::Integer(text_as_int(&String::from_utf8_lossy(&b))),
        },
        Affinity::Numeric | Affinity::Real => match v {
            Value::Null => Value::Null,
            Value::Real(_) => v,
            // A REAL cast produces a real even for a whole number, which is
            // what typeof then reports.
            Value::Integer(i) if aff == Affinity::Real => Value::real(i as f64),
            Value::Integer(i) => Value::Integer(i),
            Value::Text(s) => match s.trim().parse::<f64>() {
                Ok(f) if aff == Affinity::Real => Value::real(f),
                Ok(f) if f.fract() == 0.0 => Value::Integer(f as i64),
                Ok(f) => Value::real(f),
                // Text that is not a number casts to zero, rather than failing.
                Err(_) => Value::Integer(0),
            },
            Value::Blob(b) => Value::Integer(text_as_int(&String::from_utf8_lossy(&b))),
        },
    })
}

/// CAST's text-to-integer rule: take the leading numeric prefix, or zero.
fn text_as_int(s: &str) -> i64 {
    let t = s.trim();
    let mut end = 0;
    let bytes = t.as_bytes();
    if end < bytes.len() && (bytes[end] == b'-' || bytes[end] == b'+') {
        end += 1;
    }
    while end < bytes.len() && bytes[end].is_ascii_digit() {
        end += 1;
    }
    if end == 0 || (end == 1 && !bytes[0].is_ascii_digit()) {
        return 0;
    }
    t[..end].parse::<i64>().unwrap_or(0)
}

/// SQLite's LIKE, which is case-insensitive for ASCII and has no escape by
/// default.
///
/// The wildcard is `%` for any run and `_` for one character. The comparison is
/// over characters, and for a non-ASCII pattern the case folding stops, so a
/// LIKE against text outside ASCII is case-sensitive.
pub fn like(value: &Value, pattern: &Value, escape: Option<&Value>) -> bool {
    let (Value::Text(v), Value::Text(p)) = (value, pattern) else {
        return false;
    };
    let esc = match escape {
        Some(Value::Text(e)) => e.as_bytes().first().copied(),
        _ => None,
    };
    like_bytes(v.as_bytes(), p.as_bytes(), esc)
}

fn like_bytes(text: &[u8], pattern: &[u8], escape: Option<u8>) -> bool {
    // A backtracking matcher: the pattern is anchored at both ends, and `%`
    // consumes any run, so a greedy attempt has to be able to give characters
    // back. The recursion is bounded by the pattern length, not the text.
    fn go(t: &[u8], p: &[u8], esc: Option<u8>) -> bool {
        if p.is_empty() {
            return t.is_empty();
        }
        let mut pi = 0;
        if let Some(e) = esc {
            if p[pi] == e && pi + 1 < p.len() {
                let want = p[pi + 1];
                return (!t.is_empty()
                    && ascii_lower(t[0]) == ascii_lower(want)
                    && go(&t[1..], &p[pi + 2..], esc))
                    || (p[pi] == b'%' && go(t, &p[pi + 1..], esc));
            }
        }
        match p[pi] {
            b'%' => {
                // Try every split, longest run first, which is what makes the
                // common case linear.
                for skip in 0..=t.len() {
                    if go(&t[skip..], &p[pi + 1..], esc) {
                        return true;
                    }
                }
                false
            }
            b'_' => !t.is_empty() && go(&t[1..], &p[pi + 1..], esc),
            c => {
                !t.is_empty()
                    && ascii_lower(t[0]) == ascii_lower(c)
                    && go(&t[1..], &p[pi + 1..], esc)
            }
        }
    }
    go(text, pattern, escape)
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
pub fn call(name: &str, args: &[Value]) -> Result<Value> {
    let arity_error = |want: usize| {
        Error::new(
            ResultCode::Error,
            format!("wrong number of arguments to function {name}()"),
        )
        .with_extended(0)
    };
    let _ = arity_error;
    let lname = name.to_ascii_lowercase();
    match lname.as_str() {
        "abs" => {
            expect_arity(name, args, 1)?;
            Ok(match &args[0] {
                Value::Null => Value::Null,
                Value::Integer(i) => Value::Integer(i.wrapping_abs()),
                Value::Real(r) => Value::real(r.abs()),
                other => numeric_of(other),
            })
        }
        "coalesce" => {
            if args.is_empty() {
                return Err(Error::new(
                    ResultCode::Error,
                    "wrong number of arguments to function coalesce()",
                ));
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
                Value::Text(s) => Value::Integer(s.chars().count() as i64),
                Value::Blob(b) => Value::Integer(b.len() as i64),
                other => Value::Integer(absolute(other).to_string().chars().count() as i64),
            })
        }
        "lower" => {
            expect_arity(name, args, 1)?;
            Ok(match &args[0] {
                Value::Null => Value::Null,
                Value::Text(s) => Value::Text(s.to_lowercase()),
                other => other.clone(),
            })
        }
        "upper" => {
            expect_arity(name, args, 1)?;
            Ok(match &args[0] {
                Value::Null => Value::Null,
                Value::Text(s) => Value::Text(s.to_uppercase()),
                other => other.clone(),
            })
        }
        "trim" => {
            expect_range(name, args, 1, 2)?;
            if args[0].is_null() {
                return Ok(Value::Null);
            }
            let s = match &args[0] {
                Value::Text(s) => s.clone(),
                other => other.to_string(),
            };
            // With one argument the default is spaces; with two, the character.
            let cut: Option<char> = if args.len() == 2 {
                if args[1].is_null() {
                    return Ok(Value::Null);
                }
                match &args[1] {
                    Value::Text(t) => t.chars().next(),
                    other => other.to_string().chars().next(),
                }
            } else {
                Some(' ')
            };
            Ok(Value::Text(match cut {
                Some(c) => s.trim_matches(c).to_string(),
                None => s,
            }))
        }
        "ltrim" | "rtrim" => {
            expect_range(name, args, 1, 2)?;
            if args[0].is_null() {
                return Ok(Value::Null);
            }
            let s = match &args[0] {
                Value::Text(s) => s.clone(),
                other => other.to_string(),
            };
            let c = if args.len() == 2 {
                if args[1].is_null() {
                    return Ok(Value::Null);
                }
                match &args[1] {
                    Value::Text(t) => t.chars().next().unwrap_or(' '),
                    other => other.to_string().chars().next().unwrap_or(' '),
                }
            } else {
                ' '
            };
            let out = if lname == "ltrim" {
                s.trim_start_matches(c)
            } else {
                s.trim_end_matches(c)
            };
            Ok(Value::Text(out.to_string()))
        }
        "substr" | "substring" => {
            expect_range(name, args, 2, 3)?;
            if args.iter().any(|a| a.is_null()) {
                return Ok(Value::Null);
            }
            let s = match &args[0] {
                Value::Text(s) => s.clone(),
                Value::Blob(b) => String::from_utf8_lossy(b).into_owned(),
                other => other.to_string(),
            };
            let chars: Vec<char> = s.chars().collect();
            // A negative start counts from the end, and a start of 0 behaves
            // like 1, which is the off-by-one the documentation calls out.
            let start_raw = as_index(&args[1])?;
            // A negative start counts back from the end, so -3 of "hello" is
            // the third character from the end. A start of zero behaves as one.
            let start = if start_raw < 0 {
                (chars.len() as i64 + start_raw).max(0) as usize
            } else {
                start_raw.max(1) as usize - 1
            };
            let n = if args.len() == 3 {
                as_index(&args[2])?
            } else {
                -1
            };
            let (from, to) = if n < 0 {
                // No length, or a negative one, runs to the end.
                (start, chars.len())
            } else {
                (start, (start + n as usize).min(chars.len()))
            };
            let out = chars
                .get(from.min(chars.len())..to.max(from).min(chars.len()))
                .map(|c| c.iter().collect::<String>())
                .unwrap_or_default();
            Ok(Value::Text(out))
        }
        "replace" => {
            expect_arity(name, args, 3)?;
            if args.iter().any(|a| a.is_null()) {
                return Ok(Value::Null);
            }
            let s = string_arg(&args[0]);
            let from = string_arg(&args[1]);
            let to = string_arg(&args[2]);
            if from.is_empty() {
                return Ok(Value::Text(s));
            }
            Ok(Value::Text(s.replace(&from, &to)))
        }
        "instr" => {
            expect_arity(name, args, 2)?;
            if args.iter().any(|a| a.is_null()) {
                return Ok(Value::Null);
            }
            let hay = string_arg(&args[0]);
            let needle = string_arg(&args[1]);
            // The position is in characters and one-based, and zero means the
            // needle was not found rather than that it matched at the start.
            let idx = if needle.is_empty() {
                1
            } else {
                match hay.find(&needle) {
                    Some(byte_at) => hay[..byte_at].chars().count() as i64 + 1,
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
            Ok(Value::Text(quote(&args[0])))
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
            Ok(Value::real(r))
        }
        "min" | "max" => {
            if args.is_empty() {
                return Err(Error::new(
                    ResultCode::Error,
                    format!("wrong number of arguments to function {lname}()"),
                ));
            }
            let mut best: Option<Value> = None;
            for a in args {
                // A NULL is ignored rather than poisoning the result, and an
                // all-NULL call yields NULL.
                if a.is_null() {
                    continue;
                }
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
        _ => Err(Error::new(
            ResultCode::Error,
            format!("no such function: {lname}"),
        )),
    }
}

fn expect_arity(name: &str, args: &[Value], want: usize) -> Result<()> {
    if args.len() == want {
        Ok(())
    } else {
        Err(Error::new(
            ResultCode::Error,
            format!("wrong number of arguments to function {name}()"),
        ))
    }
}

fn expect_range(name: &str, args: &[Value], lo: usize, hi: usize) -> Result<()> {
    if args.len() >= lo && args.len() <= hi {
        Ok(())
    } else {
        Err(Error::new(
            ResultCode::Error,
            format!("wrong number of arguments to function {name}()"),
        ))
    }
}

fn as_index(v: &Value) -> Result<i64> {
    match numeric_of(v) {
        Value::Integer(i) => Ok(i),
        Value::Real(r) => Ok(r.trunc() as i64),
        _ => Ok(0),
    }
}

fn string_arg(v: &Value) -> String {
    match v {
        Value::Text(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
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
fn quote(v: &Value) -> String {
    match v {
        Value::Null => "NULL".into(),
        Value::Integer(i) => i.to_string(),
        Value::Real(r) => Value::real(*r).to_string(),
        Value::Text(s) => format!("'{}'", s.replace('\'', "''")),
        Value::Blob(b) => format!("X'{}'", hex(&Value::Blob(b.clone()))),
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
        // Two reals with the same bits are IS-equal even if one is a negative
        // zero, which comparison would call equal anyway.
        assert_eq!(ok("0.0 IS 0"), Value::Integer(0));
        assert_eq!(ok("1 IS 1.0"), Value::Integer(0));
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
        // A start of zero behaves as one.
        assert_eq!(ok("substr('hello', 0, 2)"), Value::Text("he".into()));
    }

    #[test]
    fn min_max_ignore_nulls() {
        assert_eq!(ok("min(3, NULL, 1)"), Value::Integer(1));
        assert_eq!(ok("max(3, NULL, 1)"), Value::Integer(3));
        assert_eq!(ok("min(NULL, NULL)"), Value::Null);
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
    fn concatenation_treats_null_as_empty() {
        assert_eq!(ok("'a' || 'b'"), Value::Text("ab".into()));
        assert_eq!(ok("'a' || NULL"), Value::Text("a".into()));
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
    fn an_unknown_function_says_so() {
        let e = is_err("nosuchfunction(1)");
        assert!(e.message.contains("no such function"), "got: {}", e.message);
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
}
