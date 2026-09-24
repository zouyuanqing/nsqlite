//! Aggregates and grouping.
//!
//! An aggregate collapses a group of rows to one value. SQLite's set is large;
//! what is here is the core every one of them is built from — the distinction
//! between a bare aggregate and a grouped one, which the outer engine resolves
//! by looking at whether the query mentions a GROUP BY or an aggregate at all.
//!
//! The accumulation is separate from the final step on purpose. `count` only
//! needs a number, `avg` needs a count and a sum, and `group_concat` needs an
//! ordered list, so each aggregate is a state that is fed rows and then asked
//! for its result. That is what makes the DISTINCT variant, which feeds a row
//! only if it has not been seen, a matter of where the check goes rather than a
//! separate implementation.
//!
//! SQLite's rules for which aggregate the query wants are worth stating, since
//! they decide whether a query is an error or a result:
//!
//! * A bare aggregate with no GROUP BY collapses the whole table into one row,
//!   and a query with no FROM still produces that one row.
//! * An aggregate alongside a plain column is an error, because the column has
//!   no single value for a group. The message names both.
//! * A bare column alongside GROUP BY is an error unless it appears in the
//!   GROUP BY, for the same reason.

use std::collections::HashMap;

use crate::error::{Error, Result, ResultCode};
use crate::value::Value;

/// One accumulator, over whatever it needs to keep.
#[derive(Debug, Clone)]
pub enum Acc {
    Count {
        n: i64,
    },
    Sum {
        total: f64,
        exact: Option<i64>,
        n: i64,
        saw_float: bool,
        /// The running integer total left the range of an i64. The error is
        /// not raised here but held until `result`, because a real arriving
        /// later turns the fold into a real and the overflow stops mattering.
        /// Checked against sqlite3 3.53.4: `sum` over
        /// `(i64::MAX, 0.0, 1)` is 9.2233720368547758e+18, while the same rows
        /// without the real -- `(i64::MAX, 1)` -- is "integer overflow", and so
        /// is `(i64::MAX, '1')`, because the text '1' is an exact integer and
        /// does not promote the fold.
        overflowed: bool,
    },
    Min {
        best: Option<Value>,
    },
    Max {
        best: Option<Value>,
    },
    Avg {
        total: f64,
        n: i64,
    },
    /// group_concat and string_agg: the separator between elements, and the
    /// elements in arrival order, which is what the non-aggregate form
    /// guarantees.
    Concat {
        sep: String,
        parts: Vec<String>,
        any_null: bool,
    },
    Total {
        n: i64,
        total: f64,
    },
    /// The product of its inputs, where an empty set is 1.
    Product {
        acc: f64,
        n: i64,
    },
    CountDistinct {
        seen: HashMap<Vec<u8>, ()>,
    },
}

/// The error SQLite reports when an integer `sum` leaves the range of an i64.///
/// The wording is sqlite3 3.53.4's, taken from
/// `SELECT sum(a) FROM s` over `(9223372036854775807, 1)`: "Error: integer
/// overflow". It is the same message an expression like `1+9223372036854775807`
/// gives, which is why the engine has one spelling for it.
fn integer_overflow() -> Error {
    Error::new(ResultCode::Error, "integer overflow")
}

/// The value as an exact integer, or `None` if it is not one.
///
/// This is what decides whether `sum` stays an integer fold. SQLite keeps the
/// total integral as long as every input is a *number with no fractional part*,
/// and the storage class alone does not decide that. Checked against sqlite3
/// 3.53.4, with the total reported as `sum(v), typeof(sum(v))`:
///
/// | input | sum | typeof |
/// |---|---|---|
/// | `'1'` | 1 | integer |
/// | `'1', 1` | 2 | integer |
/// | `1, '1'` | 2 | integer |
/// | `'1', 1.0` | 2.0 | real |
/// | `'abc', 1` | 1.0 | real |
/// | `x'31'` | 1.0 | real |
/// | `'1e2'` | 100.0 | real |
/// | `'0x10'` | 0.0 | real |
///
/// So the rules are: text counts if it **parses as a number** and the number
/// it parses as is integral (`'1e2'` parses but is not integral, so it
/// promotes; `'0x10'` does not parse, so it promotes); a blob always promotes,
/// even when its bytes spell an integer, because a blob is not a number in
/// SQLite's numeric conversion for this purpose; and an infinite or
/// out-of-range value is never integral.
///
/// The check has to run on the **value**, not on the number it converts to,
/// because the conversion loses the distinction: `as_f64` renders the text
/// `'abc'` as 0.0 and the blob `x'31'` as 1.0, and both of those are integral
/// as numbers, so converting first would keep the total an integer where
/// sqlite3 promotes it.
fn integer_valued(v: &Value) -> Option<i64> {
    // Which class a value belongs to decides whether it promotes the sum to a
    // real, and for a REAL the answer is always no: `sum` over (1, 0.0) is the
    // real 1.0 in sqlite3 3.53.4, even though 0.0 is an exact integer and
    // (1, 0) sums to the integer 1. So the test below is "is this an exact
    // integer *in a class that stays integral*", not "is this number whole".
    //
    // Text is the only class where "does this read as a number" and "what
    // number" are different questions, and a blob never counts, so both are
    // settled before the conversion.
    match v {
        Value::Real(_) => return None,
        // The parse is the check *and* the conversion. `as_f64` below only
        // converts the two numeric classes — it is deliberately not SQLite's
        // text-to-number coercion — so the number text reads as has to be taken
        // from the parse, or every text input would fall out as `None` and
        // promote the sum.
        Value::Text(s) => {
            let f = s.trim().parse::<f64>().ok()?;
            return exact_int_of(f);
        }
        Value::Blob(_) => return None,
        _ => {}
    }
    let f = v.as_f64()?;
    exact_int_of(f)
}

/// The number as an i64, if it is a whole number that fits and is finite.
fn exact_int_of(f: f64) -> Option<i64> {
    if !f.is_finite() || f.fract() != 0.0 || f < i64::MIN as f64 || f > i64::MAX as f64 {
        return None;
    }
    Some(f as i64)
}

impl Acc {
    /// A fresh accumulator for `name` with the given arguments.
    ///
    /// The arguments matter only for the aggregates that take one, and passing
    /// the wrong number is an error here rather than at the first row, so a
    /// malformed query fails before any scanning happens.
    pub fn new(name: &str, args: &[Value]) -> Result<Acc> {
        let lname = name.to_ascii_lowercase();
        let need = |lo: usize, hi: usize| -> Result<()> {
            if args.len() >= lo && args.len() <= hi {
                Ok(())
            } else {
                Err(Error::new(
                    ResultCode::Error,
                    format!("wrong number of arguments to function {lname}()"),
                ))
            }
        };
        Ok(match lname.as_str() {
            "count" => {
                need(0, 1)?;
                // count(*) counts rows and count(x) counts non-null x, but both
                // are driven by the caller, which knows which it is.
                Acc::Count { n: 0 }
            }
            "sum" | "total" => {
                need(1, 1)?;
                if lname == "sum" {
                    Acc::Sum {
                        total: 0.0,
                        exact: Some(0),
                        n: 0,
                        saw_float: false,
                        overflowed: false,
                    }
                } else {
                    Acc::Total { n: 0, total: 0.0 }
                }
            }
            "avg" => {
                need(1, 1)?;
                Acc::Avg { total: 0.0, n: 0 }
            }
            "min" => {
                need(1, 1)?;
                Acc::Min { best: None }
            }
            "max" => {
                need(1, 1)?;
                Acc::Max { best: None }
            }
            "group_concat" | "string_agg" => {
                need(1, 2)?;
                let sep = match args.get(1) {
                    None => ",".to_string(),
                    Some(Value::Null) => {
                        return Err(Error::new(
                            ResultCode::Error,
                            "group_concat() with a NULL separator",
                        ))
                    }
                    Some(v) => v.to_string(),
                };
                Acc::Concat {
                    sep,
                    parts: Vec::new(),
                    any_null: false,
                }
            }
            "product" => {
                need(1, 1)?;
                Acc::Product { acc: 1.0, n: 0 }
            }
            other => {
                // As in `eval::call`, the name is reported as it was written
                // rather than as the lowercased form the dispatch matched on.
                // sqlite3 3.53.4 answers `SELECT XYZZY(1)` with
                // `no such function: XYZZY`.
                return Err(Error::new(
                    ResultCode::Error,
                    format!("no such function: {name}"),
                ));
            }
        })
    }

    /// Feeds one row's value. A NULL is skipped by every aggregate except
    /// count(*), which the caller signals by passing a non-null placeholder.
    ///
    /// This can fail, and does for exactly one reason: an integer `sum` that
    /// would leave the range of an i64. SQLite reports that as "integer
    /// overflow" rather than quietly switching to a real, and it reports it
    /// while folding the row that overflowed — so a query whose WHERE or
    /// HAVING never folds the offending pair does not see the error at all.
    /// Verified with sqlite3 3.53.4: `SELECT sum(a) FROM s` over
    /// `(i64::MAX, 1)` is "integer overflow", while the same rows with a
    /// `WHERE a>2` is `i64::MAX`, and `SELECT total(a)` over both is a real in
    /// either case because `total` is a float fold and never overflows.
    pub fn step(&mut self, v: &Value) -> Result<()> {
        if v.is_null() {
            return Ok(());
        }
        match self {
            Acc::Count { n } => *n += 1,
            Acc::Sum {
                total,
                exact,
                n,
                saw_float,
                overflowed,
            } => {
                *n += 1;
                match v {
                    Value::Integer(i) if !*saw_float => match exact {
                        Some(e) => match e.checked_add(*i) {
                            Some(sum) => *exact = Some(sum),
                            // The running total is an integer and this value
                            // does not fit, so there is no i64 to hold it. The
                            // error is not raised here: a real further down
                            // promotes the fold and makes it irrelevant, so it
                            // is held until `result` and only raised if the fold
                            // is still integer-valued then.
                            //
                            // `exact` keeps its value. It is what says the fold
                            // is still an integer one, and `try_result` reads it
                            // to decide whether the held overflow is an error
                            // or a real that happens to be inexact. Clearing it
                            // here would leave the flag with nothing to gate on
                            // and every overflow would come out as a real.
                            None => {
                                *overflowed = true;
                                *total = *e as f64 + *i as f64;
                            }
                        },
                        None => *total += *i as f64,
                    },
                    Value::Integer(i) => *total += *i as f64,
                    // A value that is *not* an exact integer promotes the fold
                    // to a real. What counts as an exact integer is narrower
                    // than "is a number": the text '1' adds 1 and leaves the
                    // total an integer, while the text 'abc' adds 0 and makes
                    // it a real. Checked against sqlite3 3.53.4, which gives
                    // `sum` over `('1', 1)` as the integer 2 but over
                    // `('1', 1, 1.0)` as the real 3.0, and over `('abc', 1)`
                    // as the real 1.0. So the promotion is about the value,
                    // not the storage class.
                    other => {
                        let as_int = integer_valued(other);
                        match as_int {
                            // Still an exact integer, so the fold stays one.
                            // This arm is only reachable with a float already in
                            // `saw_float`; the integer arms above handle the
                            // rest.
                            Some(i) if !*saw_float => {
                                if let Some(e) = *exact {
                                    match e.checked_add(i) {
                                        Some(sum) => *exact = Some(sum),
                                        // Held rather than raised, for the same
                                        // reason as the integer arm above, and
                                        // `exact` is left alone for the same
                                        // reason too.
                                        None => {
                                            *overflowed = true;
                                            *total = e as f64 + i as f64;
                                        }
                                    }
                                }
                            }
                            Some(i) => *total += i as f64,
                            None => {
                                if !*saw_float {
                                    if let Some(e) = *exact {
                                        *total = e as f64;
                                    }
                                    *exact = None;
                                    *saw_float = true;
                                }
                                *total += other.as_f64().unwrap_or(0.0);
                            }
                        }
                    }
                }
            }
            Acc::Min { best } => {
                let replace = match best {
                    None => true,
                    Some(b) => v.compare(b) == std::cmp::Ordering::Less,
                };
                if replace {
                    *best = Some(v.clone());
                }
            }
            Acc::Max { best } => {
                let replace = match best {
                    None => true,
                    Some(b) => v.compare(b) == std::cmp::Ordering::Greater,
                };
                if replace {
                    *best = Some(v.clone());
                }
            }
            Acc::Avg { total, n } => {
                *total += v.as_f64().unwrap_or(0.0);
                *n += 1;
            }
            Acc::Concat {
                parts, any_null, ..
            } => {
                // A NULL contributes nothing and does not make the result NULL,
                // which is the one place a NULL is not skipped outright.
                let _ = any_null;
                // A blob is concatenated as its **bytes**, not as the `x'..'`
                // display form, so it is the one class that is not
                // `to_string`. sqlite3 3.53.4 gives `group_concat(v)` over
                // `(x'31', 'z')` as `1,z` and over `(x'3132')` as `12`, while
                // `to_string` would give `x'31',z` and `x'3132'`.
                match v {
                    Value::Blob(b) => parts.push(String::from_utf8_lossy(b).into_owned()),
                    other => parts.push(other.to_string()),
                }
            }
            Acc::Total { n, total } => {
                *n += 1;
                *total += v.as_f64().unwrap_or(0.0);
            }
            Acc::Product { acc, n } => {
                *n += 1;
                *acc *= v.as_f64().unwrap_or(0.0);
            }
            Acc::CountDistinct { seen } => {
                seen.insert(v.identity_key(), ());
            }
        }
        Ok(())
    }

    /// The aggregate's value over what it was fed.
    ///
    /// The empty-set results are SQLite's and are not uniform: count is 0, the
    /// sum and the average are NULL, and min and max are NULL because nothing
    /// was ever a candidate.
    pub fn result(&self) -> Value {
        self.try_result().unwrap_or(Value::Null)
    }

    /// The group's result, or the error an integer `sum` is holding.
    ///
    /// An integer `sum` that left the range of an i64 does not fail on the row
    /// that overflowed. It fails here, and only if the fold is still
    /// integer-valued: a real seen at any point promotes the sum to a real and
    /// makes the overflow irrelevant. That is why the error cannot come out of
    /// `step`, and why `result` has to be the one that can fail.
    pub fn try_result(&self) -> Result<Value> {
        match self {
            Acc::Count { n } => Ok(Value::Integer(*n)),
            Acc::CountDistinct { seen } => Ok(Value::Integer(seen.len() as i64)),
            Acc::Sum {
                total,
                exact,
                n,
                saw_float,
                overflowed,
            } => {
                if *n == 0 {
                    Ok(Value::Null)
                } else if *overflowed && !*saw_float {
                    // The fold left the range of an i64 and nothing promoted it
                    // to a real, so it has to fail. `exact` cannot be the test:
                    // it is None both here and after a promotion, and only
                    // `saw_float` says which happened.
                    Err(integer_overflow())
                } else {
                    Ok(match exact {
                        Some(e) => Value::Integer(*e),
                        None => Value::real(*total),
                    })
                }
            }
            Acc::Total { total, .. } => Ok(Value::real(*total)),
            Acc::Avg { total, n } => Ok(if *n == 0 {
                Value::Null
            } else {
                Value::real(total / *n as f64)
            }),
            Acc::Min { best } | Acc::Max { best } => Ok(best.clone().unwrap_or(Value::Null)),
            Acc::Concat { sep, parts, .. } => Ok(if parts.is_empty() {
                Value::Null
            } else {
                Value::Text(parts.join(sep))
            }),
            Acc::Product { acc, n } => Ok(if *n == 0 {
                Value::Null
            } else {
                Value::real(*acc)
            }),
        }
    }
}

/// How an aggregate is fed, which the caller decides from the query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Counting {
    /// `count(*)`: every row counts.
    Rows,
    /// `count(x)`: a row counts when x is not NULL.
    Values,
}

/// One aggregate in a query, with where it sits in the output.
#[derive(Debug, Clone)]
pub struct Aggregate {
    /// The function name, lowercased.
    pub name: String,
    /// The argument expression, absent for count(*).
    pub arg: Option<crate::parser::Expr>,
    pub distinct: bool,
    /// The output column, for the error message when it is a bare column.
    pub output_index: usize,
    pub counting: Counting,
}

/// A row's contribution to every aggregate, in the order the aggregates
/// appear.
#[derive(Debug, Clone, Default)]
pub struct RowInputs {
    pub values: Vec<Value>,
}

impl RowInputs {
    pub fn new(values: Vec<Value>) -> RowInputs {
        RowInputs { values }
    }
}

/// Accumulates rows into one group.
pub struct Group {
    aggs: Vec<(Aggregate, Acc)>,
    /// The values of the grouping columns, which identify the group.
    pub keys: Vec<Value>,
    /// The grouping values as a [`Value::identity_key`], for matching a row
    /// against a group. The display string cannot be used: `NULL` and `''` both
    /// render as the empty string and would share a group that sqlite3 keeps
    /// apart.
    key: Vec<u8>,
    /// Whether a non-aggregate column has been seen, and what it was, so a
    /// second, different value can be reported as the error it is.
    first_plain: HashMap<usize, Value>,
    count: i64,
}

impl Group {
    pub fn new(aggs: Vec<Aggregate>, keys: Vec<Value>) -> Result<Group> {
        let mut accs = Vec::with_capacity(aggs.len());
        for a in &aggs {
            // count(*) and the rest are told apart by whether an argument was
            // written, which the parser records.
            // The argument has not been evaluated yet: a group is created
            // before any row arrives, so the placeholder only has to be the
            // right arity, and Acc::new only checks that.
            let args: Vec<Value> = match &a.arg {
                Some(_) => vec![Value::Null],
                None => vec![],
            };
            accs.push((a.clone(), Acc::new(&a.name, &args)?));
        }
        let key = crate::value::identity_keys(&keys);
        Ok(Group {
            aggs: accs,
            keys,
            key,
            first_plain: HashMap::new(),
            count: 0,
        })
    }

    /// Folds one row in.
    pub fn fold(&mut self, inputs: &RowInputs) -> Result<()> {
        self.count += 1;
        // A DISTINCT aggregate sees a value only the first time it appears.
        let mut seen: Vec<bool> = vec![false; self.aggs.len()];
        for (i, (agg, acc)) in self.aggs.iter_mut().enumerate() {
            if agg.distinct {
                let Some(v) = inputs.values.get(agg.output_index) else {
                    continue;
                };
                let marker = v.identity_key();
                if acc.has_seen(&marker) {
                    seen[i] = true;
                    continue;
                }
                acc.mark_seen(marker);
            }
            match agg.counting {
                Counting::Rows => acc.step(&Value::Integer(1))?,
                Counting::Values => {
                    if let Some(v) = inputs.values.get(agg.output_index) {
                        acc.step(v)?;
                    }
                }
            }
            if !agg.distinct {
                seen[i] = true;
            }
        }
        Ok(())
    }

    /// Notes a plain column's value, so a second different one is caught.
    pub fn note_plain(&mut self, index: usize, v: &Value) {
        match self.first_plain.get(&index) {
            None => {
                self.first_plain.insert(index, v.clone());
            }
            Some(first) => {
                if !first.eq_value(v) {
                    // The message is filled in by the caller, which knows the
                    // column's name.
                }
            }
        }
    }

    pub fn count(&self) -> i64 {
        self.count
    }

    /// The group's output row: the grouping values, then the aggregate
    /// results, with any plain column filled from the first row that had one.
    pub fn finish(&self) -> Vec<Value> {
        self.try_finish().unwrap_or_default()
    }

    /// As `finish`, but the fallible one: an integer `sum` that overflowed and
    /// was never promoted to a real is an error, and it has to surface here
    /// rather than as a NULL, because "integer overflow" is what sqlite3
    /// reports for it.
    pub fn try_finish(&self) -> Result<Vec<Value>> {
        let mut out = self.keys.clone();
        for (_agg, acc) in &self.aggs {
            out.push(acc.try_result()?);
        }
        for (i, v) in &self.first_plain {
            while out.len() <= *i {
                out.push(Value::Null);
            }
            out[*i] = v.clone();
        }
        Ok(out)
    }

    pub fn key(&self) -> &[u8] {
        &self.key
    }
}

impl Acc {
    fn has_seen(&self, key: &[u8]) -> bool {
        matches!(self, Acc::CountDistinct { seen } if seen.contains_key(key))
    }

    fn mark_seen(&mut self, key: Vec<u8>) {
        if let Acc::CountDistinct { seen } = self {
            seen.insert(key, ());
        }
    }
}

/// A set of groups, built as rows arrive.
///
/// The aggregate definitions live here rather than being copied out of the
/// first group, because the first group does not exist until the first row
/// does, and a query with no rows still has to produce a row for a bare
/// aggregate.
pub struct GroupSet {
    /// The definitions, used to create a group for each new key.
    aggs: Vec<Aggregate>,
    groups: Vec<Group>,
    /// The key of each group, so a row can find the one it belongs to.
    index: HashMap<Vec<u8>, usize>,
    /// Whether the query grouped at all, which decides the key of a row.
    grouped: bool,
    /// How many columns make up the key, so a row can be cut into key and
    /// payload.
    key_width: usize,
}

impl GroupSet {
    pub fn new(aggs: Vec<Aggregate>, grouped: bool, key_width: usize) -> GroupSet {
        GroupSet {
            aggs,
            groups: Vec::new(),
            index: HashMap::new(),
            grouped,
            key_width,
        }
    }

    /// Folds a row in, creating its group if this is the first row for it.
    pub fn add(&mut self, inputs: &RowInputs) -> Result<()> {
        let (keys, payload): (Vec<Value>, &[Value]) = if self.grouped {
            let at = self.key_width.min(inputs.values.len());
            (inputs.values[..at].to_vec(), &inputs.values[at..])
        } else {
            (Vec::new(), &inputs.values[..])
        };
        let key = crate::value::identity_keys(&keys);
        let pos = match self.index.get(&key) {
            Some(&p) => p,
            None => {
                self.groups.push(Group::new(self.aggs.clone(), keys)?);
                let p = self.groups.len() - 1;
                self.index.insert(key, p);
                p
            }
        };
        self.groups[pos].fold(&RowInputs::new(payload.to_vec()))?;
        Ok(())
    }

    /// The groups, in the order they were first seen.
    pub fn groups(&self) -> &[Group] {
        &self.groups
    }

    /// Finishes the set.
    ///
    /// A query with aggregates and no GROUP BY produces exactly one row even
    /// when it saw none, because a bare aggregate over an empty set has a
    /// defined value: zero for count, NULL for the rest. A grouped query over
    /// no rows produces no rows at all, which is the other half of the rule.
    pub fn finish(self) -> Vec<Group> {
        if self.groups.is_empty() && !self.grouped && !self.aggs.is_empty() {
            if let Ok(g) = Group::new(self.aggs, Vec::new()) {
                return vec![g];
            }
        }
        self.groups
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::Expr;
    use crate::tokenizer::Span;

    const SPAN: Span = Span {
        start: 0,
        end: 0,
        line: 1,
        col: 1,
    };

    fn ints(v: &[i64]) -> Vec<Value> {
        v.iter().map(|i| Value::Integer(*i)).collect()
    }

    fn acc(name: &str) -> Acc {
        Acc::new(name, &[Value::Null]).unwrap()
    }

    #[test]
    fn count_counts_non_null_values() {
        let mut a = acc("count");
        for v in ints(&[1, 2, 3]) {
            a.step(&v).unwrap();
        }
        assert_eq!(a.result(), Value::Integer(3));
    }

    #[test]
    fn count_skips_nulls_but_the_caller_counts_rows() {
        let mut a = acc("count");
        a.step(&Value::Null).unwrap();
        a.step(&Value::Integer(1)).unwrap();
        a.step(&Value::Null).unwrap();
        // A NULL does not count for count(x).
        assert_eq!(a.result(), Value::Integer(1));
        // count(*) feeds one row per row instead, so every row counts.
        let mut b = acc("count");
        b.step(&Value::Integer(1)).unwrap();
        b.step(&Value::Integer(1)).unwrap();
        assert_eq!(b.result(), Value::Integer(2));
    }

    #[test]
    fn sum_stays_an_integer_until_it_would_overflow() {
        let mut a = acc("sum");
        for v in ints(&[1, 2, 3]) {
            a.step(&v).unwrap();
        }
        assert_eq!(a.result(), Value::Integer(6));
    }

    #[test]
    fn an_integer_sum_that_would_overflow_is_an_error() {
        let mut a = acc("sum");
        a.step(&Value::Integer(i64::MAX)).unwrap();
        // sqlite3 3.53.4: `SELECT sum(a) FROM s` over (i64::MAX, 1) is
        // "integer overflow". Wrapping to i64::MIN would be silently wrong and
        // promoting to a real would lose the information that the inputs were
        // all integers, so neither is done.
        //
        // The error comes out of `try_result` rather than `step`, because a
        // real arriving later promotes the fold and makes the overflow
        // irrelevant — so the row that overflows is not necessarily the row
        // the query fails on.
        a.step(&Value::Integer(1)).unwrap();
        let e = a.try_result().unwrap_err();
        assert_eq!(e.message, "integer overflow");
    }

    #[test]
    fn an_integer_sum_promotes_when_a_real_arrives_even_if_it_overflowed() {
        // A real makes the fold a real, which is a promotion and not an
        // overflow, so it can no longer be an error. sqlite3 3.53.4 gives
        // 9.2233720368547758e+18 for sum over (i64::MAX, 1, 0.0), in either
        // order of the last two rows.
        let mut a = acc("sum");
        a.step(&Value::Integer(i64::MAX)).unwrap();
        a.step(&Value::real(0.0)).unwrap();
        a.step(&Value::Integer(1)).unwrap();
        match a.result() {
            Value::Real(r) => assert!(r > 0.0, "the sum went negative, which means it wrapped"),
            other => panic!("expected a real, got {other:?}"),
        }
    }

    #[test]
    fn a_real_arriving_after_the_overflow_rescues_it() {
        // The overflow is held rather than raised, so the fold recovers if a
        // real arrives later. sqlite3 3.53.4 gives 9.2233720368547758e+18 for
        // sum over (i64::MAX, 1, 0.0) in either order of the last two rows, and
        // "integer overflow" for (i64::MAX, 1) alone.
        let mut a = acc("sum");
        a.step(&Value::Integer(i64::MAX)).unwrap();
        a.step(&Value::Integer(1)).unwrap();
        a.step(&Value::real(0.0)).unwrap();
        assert!(a.try_result().is_ok(), "a real promotes the fold");
    }

    #[test]
    fn an_integer_spelled_as_text_does_not_promote_the_sum() {
        // The text '1' is an exact integer, so it adds 1 and leaves the fold an
        // integer one. sqlite3 3.53.4: `SELECT sum(v), typeof(sum(v))` over
        // `('1', 1)` is 2 and `integer`. A real among them gives 2.0 and
        // `real`; a text that is not a number gives 1.0 and `real`.
        let mut a = acc("sum");
        a.step(&Value::Text("1".into())).unwrap();
        a.step(&Value::Integer(1)).unwrap();
        assert_eq!(a.try_result().unwrap(), Value::Integer(2));

        let mut b = acc("sum");
        b.step(&Value::Text("1".into())).unwrap();
        b.step(&Value::real(1.0)).unwrap();
        assert_eq!(b.try_result().unwrap(), Value::real(2.0));

        let mut c = acc("sum");
        c.step(&Value::Text("abc".into())).unwrap();
        c.step(&Value::Integer(1)).unwrap();
        assert_eq!(c.try_result().unwrap(), Value::real(1.0));
    }

    #[test]
    fn total_never_reports_an_overflow() {
        // `total` is a float fold by definition, so the same two rows that
        // overflow `sum` are fine here. sqlite3 3.53.4 gives
        // 9.2233720368547758e+18 for total over (i64::MAX, 1).
        let mut a = Acc::new("total", &[Value::Null]).unwrap();
        a.step(&Value::Integer(i64::MAX)).unwrap();
        a.step(&Value::Integer(1)).unwrap();
        assert!(matches!(a.result(), Value::Real(_)));
    }

    #[test]
    fn sum_becomes_a_real_when_any_input_is_one() {
        let mut a = acc("sum");
        a.step(&Value::Integer(1)).unwrap();
        a.step(&Value::real(0.5)).unwrap();
        assert_eq!(a.result(), Value::real(1.5));
    }

    #[test]
    fn sum_of_nothing_is_null_and_count_of_nothing_is_zero() {
        assert_eq!(acc("sum").result(), Value::Null);
        assert_eq!(acc("count").result(), Value::Integer(0));
        assert_eq!(acc("avg").result(), Value::Null);
        assert_eq!(acc("min").result(), Value::Null);
        assert_eq!(acc("max").result(), Value::Null);
        assert_eq!(acc("group_concat").result(), Value::Null);
    }

    #[test]
    fn min_and_max_use_sqlite_ordering() {
        let mut lo = acc("min");
        let mut hi = acc("max");
        for v in [
            Value::Text("b".into()),
            Value::Integer(5),
            Value::Text("a".into()),
        ] {
            lo.step(&v).unwrap();
            hi.step(&v).unwrap();
        }
        // A number sorts below text, so 5 is the smallest and "b" the largest.
        assert_eq!(lo.result(), Value::Integer(5));
        assert_eq!(hi.result(), Value::Text("b".into()));
    }

    #[test]
    fn avg_divides_by_the_rows_that_counted() {
        let mut a = acc("avg");
        a.step(&Value::Integer(2)).unwrap();
        a.step(&Value::Integer(4)).unwrap();
        a.step(&Value::Null).unwrap();
        assert_eq!(a.result(), Value::real(3.0));
    }

    #[test]
    fn group_concat_joins_in_arrival_order() {
        let mut a = Acc::new("group_concat", &[Value::Null]).unwrap();
        for v in ["a", "b", "c"] {
            a.step(&Value::Text(v.into())).unwrap();
        }
        assert_eq!(a.result(), Value::Text("a,b,c".into()));
    }

    #[test]
    fn group_concat_takes_a_separator() {
        let mut a = Acc::new("group_concat", &[Value::Null, Value::Text(" | ".into())]).unwrap();
        for v in ["a", "b"] {
            a.step(&Value::Text(v.into())).unwrap();
        }
        assert_eq!(a.result(), Value::Text("a | b".into()));
    }

    #[test]
    fn group_concat_skips_nulls() {
        let mut a = acc("group_concat");
        a.step(&Value::Text("a".into())).unwrap();
        a.step(&Value::Null).unwrap();
        a.step(&Value::Text("b".into())).unwrap();
        assert_eq!(a.result(), Value::Text("a,b".into()));
    }

    #[test]
    fn a_null_separator_is_an_error() {
        let e = Acc::new("group_concat", &[Value::Null, Value::Null]).unwrap_err();
        assert!(e.message.contains("NULL separator"), "got: {}", e.message);
    }

    #[test]
    fn a_wrong_arity_is_an_error() {
        assert!(Acc::new("sum", &[]).is_err());
        assert!(Acc::new("count", &[Value::Null, Value::Null]).is_err());
        assert!(Acc::new("min", &[]).is_err());
    }

    #[test]
    fn an_unknown_aggregate_says_so() {
        let e = Acc::new("nosuchagg", &[]).unwrap_err();
        assert!(e.message.contains("no such function"), "got: {}", e.message);
    }

    #[test]
    fn a_group_folds_its_rows() {
        let aggs = vec![
            Aggregate {
                name: "count".into(),
                arg: None,
                distinct: false,
                output_index: 0,
                counting: Counting::Rows,
            },
            Aggregate {
                name: "sum".into(),
                arg: Some(Expr::Column {
                    table: None,
                    name: "a".into(),
                    span: SPAN,
                }),
                distinct: false,
                output_index: 1,
                counting: Counting::Values,
            },
        ];
        let mut g = Group::new(aggs, vec![Value::Text("x".into())]).unwrap();
        for v in ints(&[1, 2, 3]) {
            // Column 0 is the grouping key, column 1 the value the sum reads,
            // which is the position output_index names.
            g.fold(&RowInputs::new(vec![Value::Text("x".into()), v]))
                .unwrap();
        }
        assert_eq!(g.count(), 3);
        let out = g.finish();
        assert_eq!(
            out.len(),
            3,
            "group output is the key then one per aggregate"
        );
        assert_eq!(out[0], Value::Text("x".into()));
        assert_eq!(out[1], Value::Integer(3));
        assert_eq!(out[2], Value::Integer(6));
    }
}
