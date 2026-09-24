//! GROUP BY and aggregates, as the executor sees them.
//!
//! The accumulation itself lives in [`crate::aggregate`]. What is here is the
//! part that needs the statement: recognising an aggregate, deciding whether
//! the query groups at all, and turning each row into its group's
//! contribution.
//!
//! # The rules, as SQLite applies them
//!
//! Five facts decide what a query means, and all five were checked against
//! `sqlite3` 3.53.4 rather than assumed:
//!
//! * A bare aggregate with no GROUP BY collapses everything into one row, and a
//!   query with no FROM still produces it — `SELECT count(*)` is 1, not an
//!   error. A grouped query over no rows produces no rows, which is the other
//!   half of the rule.
//! * Groups come out **sorted by their key**, not in the order the rows
//!   arrived. `GROUP BY k` over rows inserted as `m, z, a, m` yields `a, m, z`.
//!   The ordering is [`Value::compare`]'s, so NULL sorts first, then numbers,
//!   then text, then blobs.
//! * The output columns are in the order they were **written**. SQLite does not
//!   hoist the GROUP BY columns to the front: `SELECT sum(a), b FROM t GROUP BY
//!   b` has `sum(a)` first.
//! * A bare column alongside an aggregate is *not* an error. It takes the value
//!   from the first row of its group in scan order: over the rows
//!   `('a',5),('a',1),('a',9)`, `SELECT v, count(*) FROM f GROUP BY k` reports
//!   5 — not the minimum 1, and not an error. (An earlier draft of this
//!   engine's `Group::note_plain` anticipated a "misuse of non-aggregated
//!   column" error; that string appears nowhere in the SQLite sources and
//!   3.53.4 accepts the query, so the error path is unreachable and is not
//!   implemented.)
//! * An aggregate may be folded wherever a *group* is in scope: the result
//!   columns, HAVING, and the ORDER BY of a grouped query. It may not be folded
//!   in WHERE, and SQLite says so with two different messages — "misuse of
//!   aggregate function count()" when no aggregate is in scope at all, and
//!   "misuse of aggregate: count()" when one is. The difference is a flag in
//!   the name resolver, not a difference in the query.
//!
//! The one thing a bare column cannot do is produce a value where there is no
//! row to take it from: `SELECT a, count(*) FROM t` over an empty table gives
//! `(NULL, 0)`.
//!
//! # What this module does not do
//!
//! An aggregate nested inside another aggregate is refused by SQLite
//! ("misuse of aggregate function count()" for `sum(count(a))`), and so is one
//! in the GROUP BY. Both are reported here. Window functions and `FILTER` are
//! parsed and ignored, as they are elsewhere in the engine.

use std::collections::HashMap;

use crate::aggregate::{Acc, Counting};
use crate::error::{Error, Result, ResultCode};
use crate::eval::{eval, truthy, EvalCtx};
use crate::join::{Bound, From, JoinedRow};
use crate::parser::{Expr, Select, SelectBody};
use crate::value::Value;

/// The function names SQLite folds when they are written as aggregates.
///
/// `min` and `max` are the awkward ones: SQLite registers each as both a scalar
/// and an aggregate and chooses by arity. `min(a)` folds a group, but
/// `min(a,b)` is a scalar call on one row. Checked with `sqlite3`, where
/// `SELECT b, min(a,b), min(a) FROM v GROUP BY b` gives 10 for the scalar and 5
/// for the aggregate over the rows `(10,'9'),(5,'9')`. `product` is listed by
/// [`crate::aggregate::Acc`] but SQLite 3.53.4 has no such function —
/// `SELECT product(2,3)` is "no such function: product" — so it is
/// deliberately absent here.
const AGGREGATES: &[&str] = &[
    "count",
    "sum",
    "total",
    "avg",
    "min",
    "max",
    "group_concat",
    "string_agg",
];

/// Whether a call to `name` with `argc` arguments is an aggregate.
fn is_aggregate_call(name: &str, argc: usize) -> bool {
    let lname = name.to_ascii_lowercase();
    if lname == "min" || lname == "max" {
        // The scalar form takes any number of arguments; one is the aggregate.
        return argc == 1;
    }
    AGGREGATES.contains(&lname.as_str())
}

/// An aggregate call: a name from [`AGGREGATES`] with the arguments that make
/// it a fold rather than a scalar call.
#[derive(Debug, Clone)]
pub struct Aggregate {
    /// The lowercased name, which selects the accumulator.
    name: String,
    /// The argument expressions, empty for `count(*)`.
    args: Vec<Expr>,
    distinct: bool,
    /// Whether the call is `count(*)`, which counts rows rather than values.
    star: bool,
}

impl Aggregate {
    /// Recognises an aggregate call, or returns `None` for anything else.
    pub fn of(expr: &Expr) -> Option<Aggregate> {
        let Expr::Function {
            name,
            args,
            star,
            distinct,
        } = expr
        else {
            return None;
        };
        if *star {
            // A star aggregate is only spelled `count(*)` or `count(t.*)`.
            if !name.eq_ignore_ascii_case("count") {
                return None;
            }
            return Some(Aggregate {
                name: "count".into(),
                args: Vec::new(),
                distinct: *distinct,
                star: true,
            });
        }
        if !is_aggregate_call(name, args.len()) {
            return None;
        }
        Some(Aggregate {
            name: name.to_ascii_lowercase(),
            args: args.clone(),
            distinct: *distinct,
            star: false,
        })
    }

    /// How the call reads a row: `count(*)` and `count()` count the row,
    /// everything else counts its argument.
    fn reading(&self) -> Counting {
        if self.star || self.args.is_empty() {
            Counting::Rows
        } else {
            Counting::Values
        }
    }

    /// Whether two calls fold the same thing, which is how an aggregate in a
    /// HAVING is matched to the accumulator prepared for it.
    fn same_as(&self, other: &Aggregate) -> bool {
        self.name == other.name
            && self.star == other.star
            && self.distinct == other.distinct
            && self.args.len() == other.args.len()
    }

    /// A fresh accumulator for this call, with a `group_concat` separator taken
    /// from `sep` when the call has one.
    ///
    /// The separator is the one argument `group_concat` has that is not part of
    /// the set being concatenated, and SQLite evaluates it once, against the
    /// group's first row. A NULL separator is not an error — it means "no
    /// separator at all", so `group_concat(a, NULL)` joins with nothing.
    fn new_acc(&self, sep: Option<Value>) -> Result<Acc> {
        let args: Vec<Value> = match self.name.as_str() {
            "group_concat" | "string_agg" if self.args.len() == 2 => {
                let s = match sep {
                    None | Some(Value::Null) => String::new(),
                    Some(v) => v.to_string(),
                };
                vec![Value::Null, Value::Text(s)]
            }
            // `string_agg` requires the separator, which `group_concat` defaults
            // to a comma.
            "string_agg" => {
                return Err(Error::new(
                    ResultCode::Error,
                    "wrong number of arguments to function string_agg()",
                ))
            }
            // `count` with no argument is `count(*)`, which counts rows, so it
            // takes no argument here either. The other names need one, and it
            // has not been evaluated: a group is created before any row
            // arrives, so a placeholder of the right arity is all `Acc::new`
            // checks.
            "count" if self.args.is_empty() => Vec::new(),
            _ => self.args.iter().map(|_| Value::Null).collect(),
        };
        Acc::new(&self.name, &args)
    }
}

/// One aggregate in the query: its definition, and whether it fills an output
/// column.
#[derive(Debug, Clone)]
struct Slot {
    agg: Aggregate,
    /// The output column this aggregate fills, or `None` when the aggregate
    /// only appears in a HAVING.
    output: Option<usize>,
}

/// One group's result: everything the executor needs to build its output row.
pub struct GroupOutput {
    /// The grouping key, in the order the GROUP BY expressions were written.
    pub keys: Vec<Value>,
    /// The aggregate results, in the order the aggregates were found.
    pub aggs: Vec<Value>,
    /// The values of the group's first row, which is where a bare column
    /// reads. Empty for the group a bare aggregate over no rows produces.
    pub first: Vec<Value>,
    /// The first row bound by name, which the HAVING and the bare expressions
    /// evaluate against.
    pub named: Vec<(String, Value)>,
    /// The references resolved before the statement ran, read by byte offset.
    pub resolved: Vec<(usize, Value)>,
}

/// A query's grouping: which aggregates it mentions, and how rows split.
pub struct Plan {
    /// The result columns, each a bare expression or the index of an aggregate.
    output: Vec<Output>,
    /// Every aggregate the query folds, output ones first and HAVING's after.
    slots: Vec<Slot>,
    /// The HAVING, whose aggregates are already among the slots.
    having: Option<Expr>,
    /// The grouping expressions, empty when the query does not group.
    group_by: Vec<Expr>,
}

/// One output column: either a bare expression or an aggregate.
enum Output {
    /// A bare expression, evaluated against the group's first row.
    Plain(Expr),
    /// An aggregate, whose index into the plan's slots is where it is.
    Aggregate(usize),
}

impl Plan {
    /// Builds the plan, or reports the error SQLite reports for a query it will
    /// not run.
    ///
    /// The checks run before any row is read, which is what makes them errors
    /// rather than results.
    pub fn build(sel: &Select) -> Result<Plan> {
        let SelectBody::Simple {
            columns,
            group_by,
            having,
            ..
        } = &sel.body
        else {
            // A compound or nested arm never reaches here: the executor
            // refuses those before it asks for a plan.
            return Err(Error::new(
                ResultCode::Error,
                "a compound select is not supported yet",
            ));
        };
        let mut slots: Vec<Slot> = Vec::new();
        let mut output = Vec::with_capacity(columns.len());
        for (i, rc) in columns.iter().enumerate() {
            match Aggregate::of(&rc.expr) {
                Some(agg) => {
                    let at = slots.len();
                    slots.push(Slot {
                        agg,
                        output: Some(i),
                    });
                    output.push(Output::Aggregate(at));
                }
                None => {
                    // An aggregate nested in a result expression still folds
                    // once per group: `SELECT count(*) + 1 FROM t` is 6 over
                    // five rows, not 5. The column stays a bare expression and
                    // its aggregate is substituted in before it is evaluated,
                    // which is what [`Plan::project`] does.
                    collect_aggregates(&rc.expr, &mut slots, false, Some(i))?;
                    output.push(Output::Plain(rc.expr.clone()));
                }
            }
        }

        // A HAVING folds its own aggregates, appended after the output ones so
        // that their results are available to the predicate. One it shares
        // with a result column reuses that column's accumulator, which is why
        // `SELECT count(*) ... HAVING count(*) > 2` folds once.
        if let Some(h) = having {
            collect_aggregates(h, &mut slots, false, None)?;
        }

        if having.is_some() && slots.is_empty() && group_by.is_empty() {
            return Err(Error::new(
                ResultCode::Error,
                "HAVING clause on a non-aggregate query",
            ));
        }
        // An aggregate in the GROUP BY is refused by name, and the message
        // quotes the function it found.
        for e in group_by {
            if let Some(agg) = Aggregate::of(e) {
                return Err(Error::new(
                    ResultCode::Error,
                    format!(
                        "aggregate functions are not allowed in the GROUP BY clause: {}()",
                        agg.name
                    ),
                ));
            }
        }

        Ok(Plan {
            output,
            slots,
            having: having.clone(),
            group_by: group_by.clone(),
        })
    }

    /// Whether the query mentions an aggregate, which is what makes a bare
    /// aggregate over no rows still produce a row.
    pub fn has_aggregate(&self) -> bool {
        !self.slots.is_empty()
    }

    /// Whether the query has a GROUP BY.
    pub fn is_grouped(&self) -> bool {
        !self.group_by.is_empty()
    }

    /// The value an aggregate call has in this group.
    ///
    /// The accumulator is shared by every identical call, so a query that
    /// repeats `count(*)` folds it once and reads it twice — which is what
    /// `SELECT count(*), count(*) FROM t` means, and what makes
    /// `count(*) + 1` work without folding the same call twice.
    pub fn result_of(&self, agg: &Aggregate, group: &GroupOutput) -> Result<Value> {
        let at = self
            .slots
            .iter()
            .position(|s| s.agg.same_as(agg))
            .ok_or_else(|| Error::new(ResultCode::Error, "misuse of aggregate in HAVING"))?;
        group
            .aggs
            .get(at)
            .cloned()
            .ok_or_else(|| Error::new(ResultCode::Error, "misuse of aggregate in HAVING"))
    }

    /// The group a query with no FROM produces, whose accumulators are fresh.
    ///
    /// `SELECT count(*)` with nothing to select from still folds the one empty
    /// row a SELECT with no FROM evaluates against, so the result is 1 rather
    /// than an error. `count(1)` reads that row, which is the constant, and
    /// everything else is the empty-set result.
    pub fn empty_group(&self) -> Result<GroupOutput> {
        let none = empty_row();
        let accs = self.new_accs(&none)?;
        Ok(GroupOutput {
            keys: Vec::new(),
            aggs: accs.iter().map(|a| a.result()).collect(),
            first: Vec::new(),
            named: Vec::new(),
            resolved: Vec::new(),
        })
    }

    /// Builds one group's output row.
    ///
    /// A result column that *is* an aggregate takes the group's value for it. A
    /// bare column or expression is evaluated against the group's first row,
    /// which is what SQLite does for `SELECT v, count(*) ... GROUP BY k`; an
    /// aggregate nested in such an expression is substituted for its result
    /// first, so `count(*) + 1` reads the folded count rather than the row's.
    pub fn project(&self, group: &GroupOutput, star: Option<&[Value]>) -> Result<Vec<Value>> {
        let Some(star) = star else {
            let ctx = EvalCtx {
                params: &[],
                row: group.named.clone(),
                columns: &[],
                resolved: group.resolved.clone(),
                context: None,
            };
            let mut values = Vec::with_capacity(self.output.len());
            for slot in &self.output {
                values.push(match slot {
                    Output::Aggregate(at) => group.aggs[*at].clone(),
                    Output::Plain(e) => {
                        let mut rewritten = e.clone();
                        self.substitute(&mut rewritten, group)?;
                        eval(&rewritten, &ctx)?
                    }
                });
            }
            return Ok(values);
        };
        Ok(star.to_vec())
    }
}

/// Walks an expression for aggregate calls, appending each to `slots`.
///
/// A HAVING nests its aggregates in an ordinary expression, so the whole tree
/// is walked. An aggregate found *inside* another one is the "misuse of
/// aggregate function" case SQLite refuses.
fn collect_aggregates(
    expr: &Expr,
    slots: &mut Vec<Slot>,
    inside: bool,
    output: Option<usize>,
) -> Result<()> {
    for child in children(expr) {
        if let Some(agg) = Aggregate::of(child) {
            if inside {
                return Err(Error::new(
                    ResultCode::Error,
                    format!("misuse of aggregate function {}()", agg.name),
                ));
            }
            let Expr::Function { args, .. } = child else {
                unreachable!("a recognised aggregate is a function")
            };
            slots.push(Slot {
                agg: Aggregate {
                    name: agg.name,
                    args: args.clone(),
                    distinct: agg.distinct,
                    star: agg.star,
                },
                output,
            });
            // The arguments of a fold may not themselves fold.
            for a in args {
                collect_aggregates(a, slots, true, None)?;
            }
            continue;
        }
        collect_aggregates(child, slots, inside, output)?;
    }
    Ok(())
}

/// The sub-expressions of an expression, in evaluation order.
fn children(expr: &Expr) -> Vec<&Expr> {
    use Expr::*;
    match expr {
        Unary { expr, .. } | IsNull { expr, .. } | Collate { expr, .. } | Cast { expr, .. } => {
            vec![expr]
        }
        Binary { left, right, .. } => vec![left, right],
        Between { expr, low, high, .. } => vec![expr, low, high],
        InList { expr, list, .. } => {
            let mut v = vec![expr.as_ref()];
            v.extend(list.iter());
            v
        }
        Like {
            expr, pattern, escape, ..
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
            for (c, r) in whens {
                v.push(c);
                v.push(r);
            }
            v.extend(otherwise.iter().map(|b| b.as_ref()));
            v
        }
        _ => Vec::new(),
    }
}

/// The mutable sub-expressions of an expression, in evaluation order.
pub fn children_mut(expr: &mut Expr) -> Vec<&mut Expr> {
    use Expr::*;
    match expr {
        Unary { expr, .. } | IsNull { expr, .. } | Collate { expr, .. } | Cast { expr, .. } => {
            vec![expr]
        }
        Binary { left, right, .. } => vec![left, right],
        Between { expr, low, high, .. } => vec![expr, low, high],
        InList { expr, list, .. } => {
            let mut v: Vec<&mut Expr> = vec![expr];
            v.extend(list.iter_mut());
            v
        }
        Like {
            expr, pattern, escape, ..
        } => {
            let mut v: Vec<&mut Expr> = vec![expr, pattern];
            v.extend(escape.iter_mut().map(|b| b.as_mut()));
            v
        }
        Function { args, .. } => args.iter_mut().collect(),
        Case {
            operand,
            whens,
            otherwise,
        } => {
            let mut v: Vec<&mut Expr> = operand.iter_mut().map(|b| b.as_mut()).collect();
            for (c, r) in whens.iter_mut() {
                v.push(c);
                v.push(r);
            }
            v.extend(otherwise.iter_mut().map(|b| b.as_mut()));
            v
        }
        _ => Vec::new(),
    }
}

/// One group's state while it is being filled.
struct Group {
    /// The grouping key, which is also the group's identity.
    keys: Vec<Value>,
    /// One accumulator per slot, in slot order.
    accs: Vec<Acc>,
    /// The row that created the group, which is where the bare columns read.
    /// The group a bare aggregate over no rows produces has no row at all.
    first: Option<Row>,
    /// The values each DISTINCT aggregate has already seen, per slot.
    seen: Vec<Vec<String>>,
}

/// One row as the grouping sees it.
#[derive(Clone)]
pub struct Row {
    /// Every column of every table, in source order, which is the whole row a
    /// bare `*` reads.
    pub values: Vec<Value>,
    /// The same values by column name, which is what the ordinary evaluator
    /// falls back to.
    pub named: Vec<(String, Value)>,
    /// The references resolved before the statement ran, read by byte offset.
    pub resolved: Vec<(usize, Value)>,
}

impl Row {
    /// Builds a row from a joined row and the references bound for it.
    pub fn new(jr: &JoinedRow, from: &From, bound: &[Bound]) -> Row {
        let resolved = crate::join::resolved_values(jr, from, bound);
        Row {
            values: jr.values.clone(),
            named: crate::join::named_row(from, jr),
            resolved,
        }
    }

    /// The context a GROUP BY key, a bare expression or a HAVING evaluates
    /// against.
    fn ctx(&self) -> EvalCtx<'_> {
        EvalCtx {
            params: &[],
            row: self.named.clone(),
            columns: &[],
            resolved: self.resolved.clone(),
            context: None,
        }
    }
}

/// A row with no columns, which is what a bare aggregate over no FROM has.
fn empty_row() -> Row {
    Row {
        values: Vec::new(),
        named: Vec::new(),
        resolved: Vec::new(),
    }
}

/// Runs a grouped query over `rows`, which are the rows that passed the WHERE.
///
/// One [`GroupOutput`] comes back per group, sorted by the group key. The
/// executor assembles the output row from it, because a bare column reads
/// `first` and an aggregate reads `aggs`, and the two are not in one list.
pub fn run(plan: &Plan, rows: &[Row]) -> Result<Vec<GroupOutput>> {
    let mut groups: Vec<Group> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    let grouped = plan.is_grouped();

    for row in rows {
        // The key is each GROUP BY expression evaluated against this row. A
        // GROUP BY names an expression, not a column, so `GROUP BY a%2` splits
        // on the value of that expression rather than on the leading columns.
        let ctx = row.ctx();
        let keys: Vec<Value> = plan
            .group_by
            .iter()
            .map(|e| eval(e, &ctx))
            .collect::<Result<_>>()?;
        // The key is the values' text, joined by a byte that cannot occur in
        // one, which is what tells two groups apart.
        let key_text = keys
            .iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join("\u{1}");
        // A group's payload is the value of every aggregate's argument, so an
        // aggregate folds its argument rather than the whole result column.
        let payload = plan.evaluate_payload(row)?;
        let pos = match index.get(&key_text) {
            Some(&p) => p,
            None => {
                let accs = plan.new_accs(row)?;
                groups.push(Group {
                    keys,
                    accs,
                    first: Some(row.clone()),
                    seen: vec![Vec::new(); plan.slots.len()],
                });
                let p = groups.len() - 1;
                index.insert(key_text, p);
                p
            }
        };
        fold(plan, &mut groups[pos], &payload);
    }

    // A bare aggregate over no rows still has a value: 0 for count, NULL for
    // the rest. A grouped query over no rows has no groups and so no rows,
    // which is the asymmetry SQLite keeps.
    if groups.is_empty() && !grouped && !plan.slots.is_empty() {
        let accs = plan.new_accs(&empty_row())?;
        groups.push(Group {
            keys: Vec::new(),
            accs,
            first: None,
            seen: vec![Vec::new(); plan.slots.len()],
        });
    }

    // Groups come out sorted by their key. Without a GROUP BY there is at most
    // one group, so the sort is the identity.
    let mut order: Vec<usize> = (0..groups.len()).collect();
    if grouped {
        order.sort_by(|&a, &b| compare_keys(&groups[a].keys, &groups[b].keys));
    }

    let none = empty_row();
    let mut out = Vec::with_capacity(order.len());
    for g in order {
        let group = &groups[g];
        // HAVING runs after folding and before the row is built, and sees the
        // finished aggregates, so a group can filter on count(*) the way any
        // other predicate would.
        if let Some(h) = &plan.having {
            let first = group.first.as_ref().unwrap_or(&none);
            let ctx = first.ctx();
            if !truthy(plan.eval_having(h, group, &ctx)?) {
                continue;
            }
        }
        let first = group.first.as_ref().unwrap_or(&none);
        out.push(GroupOutput {
            keys: group.keys.clone(),
            aggs: group.accs.iter().map(|a| a.result()).collect(),
            first: first.values.clone(),
            named: first.named.clone(),
            resolved: first.resolved.clone(),
        });
    }
    Ok(out)
}

/// Orders two grouping keys, position by position.
fn compare_keys(a: &[Value], b: &[Value]) -> std::cmp::Ordering {
    for (x, y) in a.iter().zip(b.iter()) {
        let ord = x.compare(y);
        if ord != std::cmp::Ordering::Equal {
            return ord;
        }
    }
    a.len().cmp(&b.len())
}

impl Plan {
    /// A group's payload: the value of every aggregate's argument, in slot
    /// order. `count(*)` has no argument and contributes a NULL nothing reads.
    fn evaluate_payload(&self, row: &Row) -> Result<Vec<Value>> {
        let ctx = row.ctx();
        let mut out = Vec::with_capacity(self.slots.len());
        for slot in &self.slots {
            out.push(match slot.agg.args.first() {
                None => Value::Null,
                Some(arg) => eval(arg, &ctx)?,
            });
        }
        Ok(out)
    }

    /// One accumulator per slot, built against the row that starts a group.
    fn new_accs(&self, row: &Row) -> Result<Vec<Acc>> {
        let ctx = row.ctx();
        let mut accs = Vec::with_capacity(self.slots.len());
        for slot in &self.slots {
            // A group_concat separator is evaluated once per group, against the
            // group's first row, as SQLite does.
            let sep = if matches!(slot.agg.name.as_str(), "group_concat" | "string_agg") {
                slot.agg
                    .args
                    .get(1)
                    .map(|e| eval(e, &ctx))
                    .transpose()?
            } else {
                None
            };
            accs.push(slot.agg.new_acc(sep)?);
        }
        Ok(accs)
    }

    /// Evaluates a HAVING with the group's aggregate results substituted in.
    ///
    /// The substitution is what lets a predicate mention an aggregate at all:
    /// the ordinary evaluator would read it per row, or refuse it as misuse.
    fn eval_having(&self, expr: &Expr, group: &Group, ctx: &EvalCtx<'_>) -> Result<Value> {
        let mut rewritten = expr.clone();
        self.substitute(&mut rewritten, group)?;
        eval(&rewritten, ctx)
    }

    /// Replaces every aggregate call in `expr` with a literal of its result.
    fn substitute(&self, expr: &mut Expr, group: &Group) -> Result<()> {
        if let Some(agg) = Aggregate::of(expr) {
            let at = self
                .slots
                .iter()
                .position(|s| s.agg.same_as(&agg))
                .ok_or_else(|| Error::new(ResultCode::Error, "misuse of aggregate in HAVING"))?;
            *expr = literal_of(group.accs[at].result());
            return Ok(());
        }
        for child in children_mut(expr) {
            self.substitute(child, group)?;
        }
        Ok(())
    }
}

/// Folds one row's payload into its group.
fn fold(plan: &Plan, group: &mut Group, payload: &[Value]) {
    for (i, slot) in plan.slots.iter().enumerate() {
        if slot.agg.distinct {
            // A DISTINCT aggregate sees a value once. The identity is the
            // value's text, which puts the integer 1 and the real 1.0 together
            // and keeps the text '1' separate, as SQLite does.
            let Some(v) = payload.get(i) else { continue };
            if v.is_null() {
                continue;
            }
            let key = v.to_string();
            let seen = &mut group.seen[i];
            if seen.contains(&key) {
                continue;
            }
            seen.push(key);
        }
        let acc = match group.accs.get_mut(i) {
            Some(a) => a,
            None => continue,
        };
        // count(*) has no argument to read and counts the row itself; every
        // other aggregate folds the value its argument evaluated to.
        match slot.agg.reading() {
            Counting::Rows => acc.step(&Value::Integer(1)),
            Counting::Values => {
                if let Some(v) = payload.get(i) {
                    acc.step(v);
                }
            }
        }
    }
}

/// A value as a literal, which is how a substituted aggregate reaches the
/// ordinary evaluator.
pub fn literal_of(v: Value) -> Expr {
    use crate::parser::Literal;
    match v {
        Value::Null => Expr::Literal(Literal::Null),
        Value::Integer(i) => Expr::Literal(Literal::Integer(i)),
        Value::Real(r) => Expr::Literal(Literal::Real(r)),
        Value::Text(s) => Expr::Literal(Literal::Text(s)),
        Value::Blob(b) => Expr::Literal(Literal::Blob(b)),
    }
}

#[cfg(test)]
#[path = "grouping_tests.rs"]
mod grouping_tests;
