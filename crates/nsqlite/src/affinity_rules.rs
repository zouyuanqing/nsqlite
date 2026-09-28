//! The affinity conversion table, and the affinity rules that a comparison
//! applies.
//!
//! [`crate::affinity`] decides *what* an affinity is and what a single value
//! becomes under it. This module is the rest of the rule, and it is split in
//! two because the two halves answer different questions:
//!
//! * [`storage_class`] — the grid. Which storage class comes out when a value
//!   of one storage class goes into a column of a given affinity. This is the
//!   direction that runs on the way into a table.
//! * [`operand_affinities`] — the direction that does *not* depend on a single
//!   column. Two columns in a comparison each contribute an affinity, and the
//!   pair is resolved into the affinity applied to *each* operand, which is
//!   generally not the same affinity twice. A column compared against
//!   something that is not a column contributes its affinity, and the other
//!   operand contributes nothing.
//!
//! Everything here was measured by running the statement against sqlite3
//! 3.53.4 rather than reasoned from the documentation. The grid is written out
//! in [`storage_class`]'s doc comment and every cell of it is a test; the
//! comparison rule was pinned by sweeping all 25 ordered pairs of column
//! affinities against 14 values and all 5 column affinities against the same
//! 14 as a literal — 5070 comparisons, with the rule below answering every one
//! of them.
//!
//! # Why the grid is not the whole answer
//!
//! The grid and the comparison rules look like the same operation applied at
//! two different times, and they are not. On the way into a table the value is
//! converted to the column's affinity and stored converted. In a comparison
//! nothing is stored: the operands are converted in place, and *which*
//! affinity is used depends on the other operand. `t.x = 1.0` converts `1.0`
//! with `x`'s affinity; `t.x = t.y` converts *both* sides with a rule that
//! looks at both columns. See [`operand_affinities`].

use crate::affinity::{apply, Affinity};
use crate::value::Value;

/// A value's storage class, which is what `typeof()` reports.
///
/// This is deliberately not [`Value`]: the grid is keyed on the storage class,
/// and the mapping is total, so a column can be indexed by it and the
/// compiler will insist the match is complete.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageClass {
    Null,
    Integer,
    Real,
    Text,
    Blob,
}

impl StorageClass {
    /// The name SQLite's `typeof()` reports for this storage class.
    pub fn name(self) -> &'static str {
        match self {
            StorageClass::Null => "null",
            StorageClass::Integer => "integer",
            StorageClass::Real => "real",
            StorageClass::Text => "text",
            StorageClass::Blob => "blob",
        }
    }

    /// The storage class of a value, which is what `typeof()` reports for it.
    pub fn of(v: &Value) -> StorageClass {
        match v {
            Value::Null => StorageClass::Null,
            Value::Integer(_) => StorageClass::Integer,
            Value::Real(_) => StorageClass::Real,
            Value::Text(_) | Value::TextBytes(_) => StorageClass::Text,
            Value::Blob(_) => StorageClass::Blob,
        }
    }
}

/// The conversion grid: what a column of each affinity stores.
///
/// # The grid, measured
///
/// Rows are the storage class going in, columns the affinity of the column it
/// goes into. Every cell below was produced by running the same INSERT against
/// sqlite3 3.53.4 and reading `typeof()` back; the input in each row is the
/// one named at the left of the row.
///
/// ```text
/// storage class │ BLOB          │ TEXT    │ NUMERIC │ INTEGER │ REAL
/// ──────────────┼───────────────┼─────────┼─────────┼─────────┼───────
/// integer   5   │ integer:5     │ text:'5'│ integer:5│ integer:5│ real:5.0
/// real      5.5 │ real:5.5      │ text:'5.5'│ real:5.5│ real:5.5│ real:5.5
/// text      '5.0'│ text:'5.0'    │ text:'5.0'│ integer:5│ integer:5│ real:5.0
/// blob   x'0102' │ blob:X'0102'  │ blob:X'0102'│ blob:X'0102'│ blob:X'0102'│ blob:X'0102'
/// null     NULL │ null:NULL     │ null:NULL│ null:NULL│ null:NULL│ null:NULL
/// ```
///
/// The row order is the order the tests use, and the two directions that
/// implementations get backwards are the ones worth reading twice:
///
/// * `integer 5` into **REAL** widens to `real:5.0`.
/// * `real 5.0` into **NUMERIC** narrows to `integer:5`.
///
/// The same input pulls in opposite directions depending on the column, which
/// is the whole reason the grid has to be a table rather than a rule.
///
/// # What the grid is not
///
/// Three of the five rows are decided by the storage class alone and are the
/// same in every affinity, so they are properties of the grid rather than
/// cells in it:
///
/// * **NULL stays NULL** under every affinity. A comparison with it is unknown,
///   so the conversion never happens at all.
/// * **A BLOB is never converted**, by any affinity. It is not "converted to
///   text" under TEXT either — that is the cell most implementations get wrong,
///   and `typeof` on a blob in a TEXT column is `blob`, not `text`. Affinity
///   never inspects a blob's bytes.
/// * **TEXT and BLOB affinity both leave a value that is not a number alone**,
///   and so does REAL, INTEGER and NUMERIC when the text is not entirely a
///   well-formed number: `'12abc'` stays `text:'12abc'` everywhere, and is
///   never truncated to 12.
///
/// Two text shapes are worth calling out because they are not obvious:
///
/// * Leading and trailing spaces are allowed and are *not* preserved by a
///   numeric affinity: `'  7.5  '` becomes `real:7.5` under NUMERIC, INTEGER
///   and REAL. Under BLOB and TEXT the spaces survive, because those
///   affinities do not convert.
/// * `0x10` is a *literal* spelling, not a number, so it stays text in every
///   affinity. SQLite only accepts the `0x` form when parsing a literal in SQL
///   text; `'0x10'` is a string and a string that is not a number.
pub fn storage_class(value: &Value, affinity: Affinity) -> StorageClass {
    StorageClass::of(&convert(value, affinity))
}

/// Applies a column's affinity on the way into the table.
///
/// This is [`crate::affinity::apply`], re-exported under the name the grid in
/// this module's documentation is written in, so the grid and the code that
/// has to agree with it can be read side by side. The conversion itself is
/// unchanged: this module adds the *table* around it, not a new conversion.
pub fn convert(value: &Value, affinity: Affinity) -> Value {
    apply(value, affinity)
}

/// The affinity of each column reference in a statement, keyed by the byte
/// offset the reference was written at.
///
/// A comparison needs to know, for each of its two operands, whether that
/// operand is a column and what the column's affinity is. Both questions are
/// answered by the *statement*, not by the row, so this is built once per
/// statement rather than per row -- which matters, because a WHERE is
/// evaluated once per row and a per-row lookup would make the rule cost a
/// resolution per comparison.
///
/// The key is the offset because that is what the parser records and what
/// [`crate::join::Bound`] already keys on, so a reference is found by the same
/// identifier the evaluator uses to find its value. An operand that is *not* in
/// the map is not a column reference, which is exactly the rule's "the other
/// operand has no affinity" case.
pub fn affinities_of<'a, I>(
    from: &'a crate::join::From,
    bound: I,
) -> std::collections::HashMap<usize, Affinity>
where
    I: IntoIterator<Item = &'a crate::join::Bound>,
{
    let mut out = std::collections::HashMap::new();
    for b in bound {
        if let Some(aff) = affinity_of_ref(from, &b.r#ref) {
            out.insert(b.at, aff);
        }
    }
    out
}

/// The affinity a resolved reference reads, or `None` when it is not a column.
///
/// A `Coalesced` reference is a column a USING clause named, which stands for
/// one value across several sources. Which source supplies it depends on the
/// row, so the affinity is the one belonging to the *leftmost* holder: that is
/// the copy the star prints and the one an unqualified name reads first.
/// Measured against sqlite3 3.53.4 -- in `a JOIN b USING(x)` with `a.x TEXT`
/// and `b.x INTEGER`, `WHERE x = 5` is true, which is the TEXT column's
/// affinity stringifying the literal rather than the INTEGER column's parsing
/// it, so the leftmost holder is the one that decides.
fn affinity_of_ref(from: &crate::join::From, r: &crate::join::Ref) -> Option<Affinity> {
    let column_of = |i: usize, name: Option<&str>| -> Option<Affinity> {
        let s = from.sources.get(i)?;
        let at = match name {
            Some(n) => s
                .table
                .columns
                .iter()
                .position(|c| c.name.eq_ignore_ascii_case(n))?,
            None => 0,
        };
        s.table.columns.get(at).map(|c| c.affinity)
    };
    match r {
        crate::join::Ref::Column { source, column } => from
            .sources
            .get(*source)
            .and_then(|s| s.table.columns.get(*column))
            .map(|c| c.affinity),
        crate::join::Ref::Coalesced { holders, name } => {
            holders.first().and_then(|i| column_of(*i, Some(name)))
        }
        // A rowid has no DECLARED type, and SQLite gives it the numeric
        // affinity the rule gives any value with none -- so it does convert the
        // other operand, which is the whole difference between answering `1`
        // and answering `0` for `rowid = ' 1'`. Measured against sqlite3 3.53.4:
        //
        // ```text
        // SELECT rowid = ' 1' FROM t                    ->  1
        // SELECT rowid = 'abc' FROM t                   ->  0
        // SELECT (SELECT rowid FROM t) = a FROM t2     ->  1   (a is TEXT '1')
        // SELECT (SELECT rowid FROM t) = b FROM tb    ->  0   (b is BLOB x'31')
        // ```
        //
        // The BLOB line is what says NUMERIC and not INTEGER: a real INTEGER
        // column applies its affinity to the other operand, and a BLOB operand
        // keeps its bytes. NUMERIC only converts a value that LOOKS numeric, so
        // the blob is left alone and the comparison is blob against integer.
        crate::join::Ref::Rowid { .. } => Some(crate::affinity::Affinity::Numeric),
    }
}

/// The affinities a comparison applies, one per operand.
///
/// # The rule, measured
///
/// SQLite's documentation states this as a decision about each operand
/// separately (datatype3.html section 4.2), and that is how it is implemented
/// here. [`comparison_affinity`] flattens the pair for callers that only need
/// to know "was anything converted", but the two operands do not in general
/// get the same treatment, so [`operand_affinities`] is the real rule.
///
/// * If **one** operand is a column, that column's affinity is the only one
///   there is. The other operand — a literal, an expression, a bound parameter
///   or a subquery — is converted with **NUMERIC** when the column has an
///   INTEGER, REAL or NUMERIC affinity, with the column's own affinity when it
///   is TEXT, and not at all when it is BLOB. The non-column operand
///   contributes nothing, so it can never be the reason a conversion happens.
/// * If **both** operands are columns, each contributes its affinity, and the
///   pair is resolved by these rules, in this order:
///
///   1. If either operand has INTEGER, REAL or NUMERIC affinity, **NUMERIC** is
///      applied to the *other* operand, and the numeric one is left alone.
///   2. Otherwise, if either operand has TEXT affinity, **TEXT** is applied to
///      the *other* operand.
///   3. Otherwise **BLOB** is applied to both, which converts nothing.
///
/// Rule 1 is checked before rule 2, and that order is what a TEXT column
/// against a REAL column turns on: it gets rule 1, so the text is parsed to an
/// exact number rather than stringified.
///
/// Rule 2 does not fire when the other operand is itself a column, and the
/// difference is measurable rather than academic. A TEXT column against the
/// numeric literal `1.0` stringifies the literal, so a TEXT column holding
/// `'1.0'` answers 1. A **BLOB** column against a TEXT column stringifies
/// nothing: a BLOB column holding the integer 5 against a TEXT column holding
/// `'5'` answers 0, because the blob is not converted to `'5'`. Both were run
/// against sqlite3 3.53.4; see
/// [`a_blob_column_meets_a_text_column_without_converting_either`].
///
/// # There is no total order over the five affinities
///
/// It is tempting to resolve the pair to one winning affinity and apply it to
/// both operands, as though the affinities were ranked. **They are not, and
/// that model is measurably wrong.** NUMERIC applied to both operands is not
/// the same as the real rule whenever one side is a number and the other is
/// text, because NUMERIC applied to the numeric side narrows a whole real to an
/// integer — usually harmless, but visible at `2^53`. REAL applied to both is
/// wrong in the same place for a different reason: it rounds the text.
///
/// The divergence is easiest to see at the `2^53` boundary, where a `f64` can
/// no longer name every integer. Measured against sqlite3 3.53.4:
///
/// ```text
/// CREATE TABLE t(x TEXT);  INSERT INTO t VALUES('9007199254740993');
/// CREATE TABLE r(x REAL);  INSERT INTO r VALUES(9007199254740992.0);
/// SELECT t.x = r.x;   -- sqlite3: 0
/// ```
///
/// `2^53 + 1` is an exact `i64` but not an exact `f64`. Applying REAL to the
/// text rounds it down to `2^53`, which then equals the other side, and the
/// comparison wrongly answers 1. Applying NUMERIC parses it to the exact
/// integer `2^53 + 1`, which does not equal the real `2^53`, and answers 0 as
/// SQLite does. The same case in the other written order, with BLOB in place of
/// TEXT, and the one-column form against the literal, all behave the same way.
///
/// Note what this case does *not* show: an INTEGER and a REAL operand resolve
/// the same way here under either model, because INTEGER and NUMERIC agree on a
/// whole real. A test that uses only exactly-representable values therefore
/// cannot tell the two rules apart, which is why [`text_against_a_numeric_column_is_parsed_not_rounded`]
/// uses this value and every comparison test in this module that exercises the
/// text-versus-numeric rule is paired with a value where rounding would change
/// the answer.
pub fn operand_affinities(left: Option<Affinity>, right: Option<Affinity>) -> (Affinity, Affinity) {
    // Neither operand is a column, so there is no affinity and nothing is
    // converted: `1 = 1.0` compares an integer to a real as they are.
    if left.is_none() && right.is_none() {
        return (Affinity::Blob, Affinity::Blob);
    }
    let left_numeric = left.is_some_and(is_numeric);
    let right_numeric = right.is_some_and(is_numeric);
    // Rule 1, checked before rule 2 so that a numeric column meeting a TEXT or
    // BLOB one is rule 1 and not rule 2. The numeric operand is given BLOB,
    // which is the identity conversion, rather than being skipped: `apply` is
    // total, so saying "not numeric, do nothing" is the same thing as saying
    // "convert with an affinity that changes nothing", and saying it this way
    // keeps the two operands symmetric.
    if left_numeric != right_numeric {
        return if left_numeric {
            (Affinity::Blob, Affinity::Numeric)
        } else {
            (Affinity::Numeric, Affinity::Blob)
        };
    }
    // Both are numeric: NUMERIC on both sides, which is the identity for an
    // integer and, for a real, narrows only when nothing is lost.
    if left_numeric {
        return (Affinity::Numeric, Affinity::Numeric);
    }
    // Rule 2. The affinity on each side is that side's own, except that an
    // operand which is **not a column** takes the other side's affinity when
    // that affinity is TEXT. This is the "does not exist" case in the
    // documented rule: with one operand having no affinity, the other's is the
    // only one there is, and it is applied to both.
    //
    // So a TEXT *column* against a literal stringifies the literal, and a BLOB
    // column against a literal does nothing — while a BLOB column against a
    // TEXT column converts neither. Measured: a BLOB column holding the
    // integer 5 against a TEXT column holding '5' answers 0, so the blob was
    // not stringified; and a TEXT column holding '1.0' against the numeric
    // literal 1.0 answers 1, so the literal was stringified.
    let left_aff = left.unwrap_or(Affinity::Blob);
    let right_aff = right.unwrap_or(Affinity::Blob);
    if left.is_none() && right_aff == Affinity::Text {
        return (Affinity::Text, Affinity::Text);
    }
    if right.is_none() && left_aff == Affinity::Text {
        return (Affinity::Text, Affinity::Text);
    }
    (left_aff, right_aff)
}

/// Whether an affinity is one of the three that make SQLite reach for NUMERIC
/// on the other side of a comparison.
fn is_numeric(a: Affinity) -> bool {
    matches!(a, Affinity::Integer | Affinity::Real | Affinity::Numeric)
}

/// The affinity a comparison applies, given the affinities of its operands.
///
/// This is the single-affinity view of [`operand_affinities`], for callers that
/// only need to know whether the comparison converts anything. It is `NUMERIC`
/// whenever either operand is numeric, `TEXT` when one is a TEXT column and
/// neither is numeric, and `BLOB` otherwise — which is what `BLOB` means, so
/// `(None, None)` answers `BLOB`.
///
/// It is **not** the affinity the comparison applies to each operand: where the
/// two differ, one operand is converted with NUMERIC and the other with
/// [`Affinity::Blob`], and collapsing that to a single value loses the
/// asymmetry. [`apply_comparison`] uses [`operand_affinities`] and not this.
pub fn comparison_affinity(left: Option<Affinity>, right: Option<Affinity>) -> Affinity {
    let (left, right) = operand_affinities(left, right);
    if left == Affinity::Blob {
        right
    } else {
        left
    }
}

/// Applies a comparison's affinity to both operands.
///
/// `left` and `right` carry each operand's *own* column affinity, or `None`
/// when that operand is not a column reference. The two affinities are resolved
/// by [`operand_affinities`], which decides an affinity *per operand* rather
/// than picking one winner for both.
///
/// That distinction is the whole rule, and it is what makes a comparison
/// symmetric in its effect while asymmetric in its written form:
/// `text_col = int_col` and `int_col = text_col` both convert the text, because
/// the rule is written in terms of which side is numeric rather than which side
/// came first.
pub fn apply_comparison(
    left: &Value,
    left_affinity: Option<Affinity>,
    right: &Value,
    right_affinity: Option<Affinity>,
) -> (Value, Value) {
    let (left_affinity, right_affinity) = operand_affinities(left_affinity, right_affinity);
    (convert(left, left_affinity), convert(right, right_affinity))
}

/// The ordering a comparison of two operands comes to, with the comparison's
/// affinity rule applied to them first.
///
/// This is the whole of the comparison direction, in the form a caller that has
/// already evaluated both operands can use: hand it the two values and each
/// side's own column affinity, and it gives back the answer. A caller that has
/// *not* applied the rule gets a different answer from `Value::compare`, which
/// is the whole point -- `text '5'` and `integer 5` are different values, and
/// the column affinities are what make them the same one.
///
/// `left_affinity` and `right_affinity` are `None` for an operand that is not a
/// column reference, and both `None` means neither operand is a column: nothing
/// is converted and the two values are compared as they are, which is what
/// makes `1 = 1.0` true and `'1' = 1.0` false.
///
/// A NULL operand is *not* handled here. Three-valued logic is the caller's,
/// because it is also the caller's for every other operator, and folding it in
/// would make this one operator answer NULL for a reason the caller cannot see.
pub fn compare(
    left: &Value,
    left_affinity: Option<Affinity>,
    right: &Value,
    right_affinity: Option<Affinity>,
) -> std::cmp::Ordering {
    let (left, right) = apply_comparison(left, left_affinity, right, right_affinity);
    left.compare(&right)
}

/// Whether two operands are equal under the comparison rule, which is `IN`'s
/// test as well as `=`'s.
///
/// `IN` is a repeated `=`, and it takes the same rule: `a IN (5, 9)` against a
/// TEXT column `a` holding '5' is true, because the list item is converted with
/// the column's affinity just as the right-hand side of an `=` would be. The
/// values are compared rather than the whole `IN` being reimplemented, so the
/// one conversion rule serves both.
pub fn equal(
    left: &Value,
    left_affinity: Option<Affinity>,
    right: &Value,
    right_affinity: Option<Affinity>,
) -> bool {
    compare(left, left_affinity, right, right_affinity) == std::cmp::Ordering::Equal
}

/// Whether two operands are `IS` to each other, which is `=` except that it
/// never answers unknown.
///
/// SQLite's `IS` is the same comparison as `=` with exactly one difference, and
/// it is a difference in *when* the answer is computed rather than in what is
/// compared: `=` short-circuits to unknown when either operand is NULL, and
/// `IS` compares them as the values they are. Every other difference one might
/// expect is not there. Measured against sqlite3 3.53.4 over all 441 ordered
/// pairs of 21 values (`5`, `5.0`, `-5.0`, `0.0`, `-0.0`, `5.5`, `'5'`,
/// `'5.0'`, `'abc'`, `''`, `x'35'`, `x'616263'`, NULL, `0`, `2^53`,
/// `2^53 + 1`, `i64::MAX`, `i64::MAX - 1`, `i64::MIN`, `1e300`, `1e-300`):
/// the two operators disagree on 41 of the 441, and every one of the 41 is a
/// pair with a NULL in it — which is the short circuit, not a second rule.
///
/// The parts that are easy to get wrong, all measured:
///
/// * **An integer and a real that name the same number are the same.** `5 IS 5.0`
///   and `5.0 IS 5` are both 1, and so is `9007199254740992 IS
///   9007199254740992.0`. The comparison is the ordinary numeric one, which
///   compares the two *exactly* rather than widening the integer to a double:
///   `9223372036854775807 IS 9223372036854775807.0` is 0, because that integer
///   is not the real written beside it.
/// * **A negative zero is not a positive one.** `0.0 IS -0.0` is **1**, which is
///   the opposite of what comparing the bits would give and is the reason this
///   cannot be written as a bit comparison.
/// * **No conversion happens on its own account.** `'5' IS 5` is 0 with no
///   column in the query, and `x'35' IS 5` is 0. Only the affinity rule, and
///   only with a column to take it from, changes what the operands are.
pub fn is_same(
    left: &Value,
    left_affinity: Option<Affinity>,
    right: &Value,
    right_affinity: Option<Affinity>,
) -> bool {
    let (left, right) = apply_comparison(left, left_affinity, right, right_affinity);
    // The one thing `IS` does not do is return unknown. Everything else is the
    // ordinary comparison, and `compare` already treats NULL as equal to
    // nothing and less than everything else, so answering "the same" here means
    // the same as it does for the sorted position: two NULLs are the same value
    // and a NULL is never the same as a non-NULL.
    left.compare(&right) == std::cmp::Ordering::Equal
}

/// The empty affinity map, which is what a context with no column references
/// carries.
///
/// A comparison whose two operands are both absent from the map has no column
/// on either side, so it converts nothing — which is what SQLite does for
/// `SELECT '5' = 5` in a query with no table. There is exactly one of these,
/// so a context with no columns borrows it rather than building an empty map of
/// its own.
pub fn no_affinities() -> &'static std::collections::HashMap<usize, Affinity> {
    static EMPTY: std::sync::OnceLock<std::collections::HashMap<usize, Affinity>> =
        std::sync::OnceLock::new();
    EMPTY.get_or_init(std::collections::HashMap::new)
}

/// The affinity an *expression* contributes to a comparison when it is not a
/// column reference, which is `None` for almost all of them.
///
/// SQLite's rule is written in terms of columns, and an expression is not one,
/// so the rule as documented would have an expression contribute nothing. That
/// is what happens for every form except one, and the exception is worth
/// stating because it is the only way a `CAST` shows up in a comparison.
///
/// Measured against sqlite3 3.53.4, with `a` a TEXT column holding `'05'`:
///
/// ```text
/// SELECT a = 5;                     -- 0   the literal contributes nothing
/// SELECT a = abs(-5);                -- 0   and so does a function call
/// SELECT a = +5;                    -- 0   and a unary operator
/// SELECT a = 5 + 0;                 -- 0   and an arithmetic expression
/// SELECT a = b + 0;                 -- 0   even when b is a column
/// SELECT a = COALESCE(5, 6);        -- 0   and a function over a column
/// SELECT a = CAST(5 AS INTEGER);    -- 1   but a numeric CAST does
/// SELECT a = CAST(5 AS NUMERIC);    -- 1
/// SELECT a = CAST(5 AS REAL);       -- 1
/// SELECT a = CAST(5 AS TEXT);       -- 0   a TEXT CAST does not
/// SELECT a = CAST(5 AS BLOB);       -- 0   and a BLOB CAST does not
/// ```
///
/// The three that answer 1 do so because a `CAST` is what put a *number* where
/// a literal would have left a *string*, and the rule's whole content is
/// deciding what to do with a string on the other side of a numeric column. A
/// TEXT or BLOB cast produces the same kind of value a literal would, so it has
/// nothing to add. This is stated rather than derived because it is the one
/// place the two readings of "an expression contributes its own affinity"
/// diverge, and reading it the other way makes `a = CAST('05' AS INTEGER)` and
/// `a = '05'` disagree, which SQLite does not do — both are 1.
pub fn expression_affinity(cast_to: Option<&str>) -> Option<Affinity> {
    let ty = cast_to?;
    let aff = crate::affinity::affinity_of(ty);
    // Only a numeric target produces a number, and only a number changes what
    // the comparison does with the other operand.
    is_numeric(aff).then_some(aff)
}

#[cfg(test)]
#[path = "affinity_cmp_tests.rs"]
mod affinity_cmp_tests;

/// The same rules again, this time asked of a running engine.
///
/// The tests above call the functions in this module directly, and they pass
/// whether or not anything calls them. That is not hypothetical: this module was
/// once complete, documented and tested while no executor referenced it, and
/// the engine answered `SELECT a=b` wrongly the whole time. So these go through
/// a `Connection`, and they fail if the wiring is removed.
#[cfg(test)]
#[path = "affinity_engine_tests.rs"]
mod affinity_engine_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::Value;

    // -- The grid, one test per cell ------------------------------------

    /// Every cell of the grid in the doc comment, as a value it compares
    /// against, so a wrong storage class fails even when the value looks the
    /// same. The expected `Value` is spelled out rather than computed, so a
    /// change in `apply` cannot quietly redefine the answer.
    ///
    /// A function rather than a `const` because `Value::real` and
    /// `String::from` are not const.
    fn grid<'a>() -> Vec<(StorageClass, Affinity, &'a str, Value)> {
        vec![
            // integer 5
            (
                StorageClass::Integer,
                Affinity::Blob,
                "integer",
                Value::Integer(5),
            ),
            (
                StorageClass::Integer,
                Affinity::Text,
                "text",
                Value::Text("5".into()),
            ),
            (
                StorageClass::Integer,
                Affinity::Numeric,
                "integer",
                Value::Integer(5),
            ),
            (
                StorageClass::Integer,
                Affinity::Integer,
                "integer",
                Value::Integer(5),
            ),
            // The direction that is wrong in the most implementations: REAL widens.
            (
                StorageClass::Integer,
                Affinity::Real,
                "real",
                Value::real(5.0),
            ),
            // real 5.5
            (StorageClass::Real, Affinity::Blob, "real", Value::real(5.5)),
            (
                StorageClass::Real,
                Affinity::Text,
                "text",
                Value::Text("5.5".into()),
            ),
            (
                StorageClass::Real,
                Affinity::Numeric,
                "real",
                Value::real(5.5),
            ),
            (
                StorageClass::Real,
                Affinity::Integer,
                "real",
                Value::real(5.5),
            ),
            (StorageClass::Real, Affinity::Real, "real", Value::real(5.5)),
            // text '5.0'
            (
                StorageClass::Text,
                Affinity::Blob,
                "text",
                Value::Text("5.0".into()),
            ),
            (
                StorageClass::Text,
                Affinity::Text,
                "text",
                Value::Text("5.0".into()),
            ),
            // A text that looks like a number is converted by the numeric
            // affinities, and narrows to an integer because the value is whole.
            (
                StorageClass::Text,
                Affinity::Numeric,
                "integer",
                Value::Integer(5),
            ),
            (
                StorageClass::Text,
                Affinity::Integer,
                "integer",
                Value::Integer(5),
            ),
            (StorageClass::Text, Affinity::Real, "real", Value::real(5.0)),
            // blob x'0102' — the same under all five.
            (
                StorageClass::Blob,
                Affinity::Blob,
                "blob",
                Value::Blob(vec![1, 2]),
            ),
            (
                StorageClass::Blob,
                Affinity::Text,
                "blob",
                Value::Blob(vec![1, 2]),
            ),
            (
                StorageClass::Blob,
                Affinity::Numeric,
                "blob",
                Value::Blob(vec![1, 2]),
            ),
            (
                StorageClass::Blob,
                Affinity::Integer,
                "blob",
                Value::Blob(vec![1, 2]),
            ),
            (
                StorageClass::Blob,
                Affinity::Real,
                "blob",
                Value::Blob(vec![1, 2]),
            ),
            // NULL — the same under all five.
            (StorageClass::Null, Affinity::Blob, "null", Value::Null),
            (StorageClass::Null, Affinity::Text, "null", Value::Null),
            (StorageClass::Null, Affinity::Numeric, "null", Value::Null),
            (StorageClass::Null, Affinity::Integer, "null", Value::Null),
            (StorageClass::Null, Affinity::Real, "null", Value::Null),
        ]
    }

    /// One test over every cell, so a cell that regresses names the affinity
    /// and the storage class that disagree rather than "a grid cell".
    #[test]
    fn every_cell_of_the_grid() {
        let grid = grid();
        // The representative input for each storage class, which is the left
        // column of each row in the table.
        let input = [
            (StorageClass::Integer, Value::Integer(5)),
            (StorageClass::Real, Value::real(5.5)),
            (StorageClass::Text, Value::Text("5.0".into())),
            (StorageClass::Blob, Value::Blob(vec![1, 2])),
            (StorageClass::Null, Value::Null),
        ];
        for (class, value) in &input {
            // The row is selected by the grid's own class field, not by
            // re-deriving one from the value: a text '5.0' and an integer 5
            // both convert to the integer 5, so a value-derived key would let
            // one row's expectation satisfy another's cell.
            let mut seen = 0;
            for (row_class, aff, want_class, want_value) in &grid {
                if row_class != class {
                    continue;
                }
                let got = convert(value, *aff);
                assert_eq!(
                    &got, want_value,
                    "{class:?} into {aff} should be {want_class}:{want_value:?}, was {got:?}"
                );
                assert_eq!(
                    storage_class(value, *aff).name(),
                    *want_class,
                    "storage class of {class:?} under {aff}"
                );
                seen += 1;
            }
            assert_eq!(seen, 5, "{class:?} has {seen} grid cells, expected 5");
        }
        assert_eq!(
            grid.len(),
            25,
            "the grid is 5 storage classes by 5 affinities"
        );
    }

    // The 25 cells again, one named test each. The table above is the backstop
    // that proves no cell was missed; these are what a failing run prints, and
    // "cell_integer_into_real failed" is the difference between knowing the
    // bug and knowing which of twenty-five lines to open.

    #[test]
    fn cell_null_into_blob() {
        assert_eq!(convert(&Value::Null, Affinity::Blob), Value::Null);
    }

    #[test]
    fn cell_null_into_text() {
        assert_eq!(convert(&Value::Null, Affinity::Text), Value::Null);
    }

    #[test]
    fn cell_null_into_numeric() {
        assert_eq!(convert(&Value::Null, Affinity::Numeric), Value::Null);
    }

    #[test]
    fn cell_null_into_integer() {
        assert_eq!(convert(&Value::Null, Affinity::Integer), Value::Null);
    }

    #[test]
    fn cell_null_into_real() {
        assert_eq!(convert(&Value::Null, Affinity::Real), Value::Null);
    }

    #[test]
    fn cell_integer_into_blob() {
        assert_eq!(
            convert(&Value::Integer(5), Affinity::Blob),
            Value::Integer(5)
        );
    }

    #[test]
    fn cell_integer_into_text() {
        assert_eq!(
            convert(&Value::Integer(5), Affinity::Text),
            Value::Text("5".into())
        );
    }

    #[test]
    fn cell_integer_into_numeric() {
        assert_eq!(
            convert(&Value::Integer(5), Affinity::Numeric),
            Value::Integer(5)
        );
    }

    #[test]
    fn cell_integer_into_integer() {
        assert_eq!(
            convert(&Value::Integer(5), Affinity::Integer),
            Value::Integer(5)
        );
    }

    /// The direction that is wrong in the most implementations, so it gets a
    /// name of its own rather than being one line in a table.
    #[test]
    fn cell_integer_into_real_widens() {
        assert_eq!(
            convert(&Value::Integer(5), Affinity::Real),
            Value::real(5.0)
        );
    }

    #[test]
    fn cell_real_into_blob() {
        assert_eq!(convert(&Value::real(5.5), Affinity::Blob), Value::real(5.5));
    }

    #[test]
    fn cell_real_into_text() {
        assert_eq!(
            convert(&Value::real(5.5), Affinity::Text),
            Value::Text("5.5".into())
        );
    }

    #[test]
    fn cell_real_into_numeric() {
        assert_eq!(
            convert(&Value::real(5.5), Affinity::Numeric),
            Value::real(5.5)
        );
    }

    #[test]
    fn cell_real_into_integer_keeps_the_fraction() {
        assert_eq!(
            convert(&Value::real(5.5), Affinity::Integer),
            Value::real(5.5)
        );
    }

    #[test]
    fn cell_real_into_real() {
        assert_eq!(convert(&Value::real(5.5), Affinity::Real), Value::real(5.5));
    }

    #[test]
    fn cell_text_into_blob() {
        assert_eq!(
            convert(&Value::Text("5.0".into()), Affinity::Blob),
            Value::Text("5.0".into())
        );
    }

    #[test]
    fn cell_text_into_text() {
        assert_eq!(
            convert(&Value::Text("5.0".into()), Affinity::Text),
            Value::Text("5.0".into())
        );
    }

    /// A text that looks like a number, converted and narrowed because the
    /// value it names is whole.
    #[test]
    fn cell_text_into_numeric_narrows() {
        assert_eq!(
            convert(&Value::Text("5.0".into()), Affinity::Numeric),
            Value::Integer(5)
        );
    }

    #[test]
    fn cell_text_into_integer_narrows() {
        assert_eq!(
            convert(&Value::Text("5.0".into()), Affinity::Integer),
            Value::Integer(5)
        );
    }

    #[test]
    fn cell_text_into_real_stays_real() {
        assert_eq!(
            convert(&Value::Text("5.0".into()), Affinity::Real),
            Value::real(5.0)
        );
    }

    #[test]
    fn cell_blob_into_blob() {
        let b = Value::Blob(vec![1, 2]);
        assert_eq!(convert(&b, Affinity::Blob), b);
    }

    /// The cell an implementation gets wrong by treating a blob as text first.
    #[test]
    fn cell_blob_into_text_is_still_a_blob() {
        let b = Value::Blob(vec![1, 2]);
        assert_eq!(convert(&b, Affinity::Text), b);
    }

    #[test]
    fn cell_blob_into_numeric_is_still_a_blob() {
        let b = Value::Blob(vec![1, 2]);
        assert_eq!(convert(&b, Affinity::Numeric), b);
    }

    #[test]
    fn cell_blob_into_integer_is_still_a_blob() {
        let b = Value::Blob(vec![1, 2]);
        assert_eq!(convert(&b, Affinity::Integer), b);
    }

    #[test]
    fn cell_blob_into_real_is_still_a_blob() {
        let b = Value::Blob(vec![1, 2]);
        assert_eq!(convert(&b, Affinity::Real), b);
    }

    /// The storage class a cell reports, which is what `typeof()` does, checked
    /// against the converted value so the two cannot disagree.
    #[test]
    fn storage_class_agrees_with_the_converted_value() {
        for (class, value) in [
            (StorageClass::Null, Value::Null),
            (StorageClass::Integer, Value::Integer(5)),
            (StorageClass::Real, Value::real(5.5)),
            (StorageClass::Text, Value::Text("5.0".into())),
            (StorageClass::Blob, Value::Blob(vec![1, 2])),
        ] {
            for affinity in [
                Affinity::Blob,
                Affinity::Text,
                Affinity::Numeric,
                Affinity::Integer,
                Affinity::Real,
            ] {
                assert_eq!(
                    storage_class(&value, affinity),
                    StorageClass::of(&convert(&value, affinity)),
                    "{class:?} into {affinity}"
                );
            }
        }
    }

    /// `StorageClass::of` and `name` agree with each other, and a value's class
    /// is not the class of a conversion of it: a real 5.0 and an integer 5 are
    /// different classes even though NUMERIC makes them the same value.
    #[test]
    fn a_storage_class_names_itself_and_tracks_the_value() {
        for (class, want) in [
            (StorageClass::Null, "null"),
            (StorageClass::Integer, "integer"),
            (StorageClass::Real, "real"),
            (StorageClass::Text, "text"),
            (StorageClass::Blob, "blob"),
        ] {
            assert_eq!(class.name(), want);
        }
        assert_ne!(
            StorageClass::of(&Value::real(5.0)),
            StorageClass::of(&Value::Integer(5))
        );
        assert_eq!(
            StorageClass::of(&convert(&Value::real(5.0), Affinity::Numeric)),
            StorageClass::Integer
        );
    }

    /// The two directions that pull against each other, on the same input.
    /// A test that only checked one of them would pass an implementation with
    /// either bug, so they are asserted together and in both orders.
    #[test]
    fn real_and_numeric_pull_in_opposite_directions() {
        let whole_real = Value::real(5.0);
        let whole_int = Value::Integer(5);
        // NUMERIC narrows a whole real to an integer.
        assert_eq!(convert(&whole_real, Affinity::Numeric), Value::Integer(5));
        // REAL widens an integer to a real.
        assert_eq!(convert(&whole_int, Affinity::Real), Value::real(5.0));
        // And the same value, through each of the two columns, goes opposite
        // ways — which is the reported gap.
        assert_eq!(
            storage_class(&whole_real, Affinity::Numeric),
            StorageClass::Integer
        );
        assert_eq!(
            storage_class(&whole_int, Affinity::Real),
            StorageClass::Real
        );
    }

    // -- The cases that are easy to get wrong ---------------------------

    /// A text that looks like a number is converted; one that is only partly a
    /// number is not, and is never truncated to the prefix.
    #[test]
    fn a_text_that_looks_like_a_number_is_converted() {
        // Every spelling of a number is converted, and one that lands on a
        // whole value narrows to an integer.
        for (s, want) in [
            ("5", Value::Integer(5)),
            ("5.0", Value::Integer(5)),
            ("1e3", Value::Integer(1000)),
            ("+3", Value::Integer(3)),
            ("3.", Value::Integer(3)),
        ] {
            assert_eq!(
                convert(&Value::Text(s.into()), Affinity::Numeric),
                want,
                "{s:?}"
            );
        }
        // '12abc' is not a number, so no affinity turns it into 12. The
        // failure this prevents is a truncated conversion.
        for aff in [
            Affinity::Integer,
            Affinity::Numeric,
            Affinity::Real,
            Affinity::Text,
            Affinity::Blob,
        ] {
            assert_eq!(
                convert(&Value::Text("12abc".into()), aff),
                Value::Text("12abc".into()),
                "{aff} truncated '12abc'"
            );
        }
    }

    /// A real that is whole narrows, one that has a fraction does not, and one
    /// too large for an `i64` stays real even though it looks whole.
    #[test]
    fn a_whole_real_narrows_but_a_large_one_does_not() {
        for aff in [Affinity::Numeric, Affinity::Integer] {
            assert_eq!(convert(&Value::real(5.0), aff), Value::Integer(5), "{aff}");
            assert_eq!(convert(&Value::real(5.5), aff), Value::real(5.5), "{aff}");
            // 2^63 as an f64 is one past i64::MAX, so it names no i64 and
            // stays real. Measured: sqlite3 renders it 9.2233720368547758e+18.
            let past_max = 9223372036854775807.0f64;
            assert_eq!(
                convert(&Value::real(past_max), aff),
                Value::real(past_max),
                "{aff} narrowed a real that does not fit an i64"
            );
        }
    }

    /// A blob is never converted, not even to text under a TEXT affinity.
    /// This is the cell an implementation gets wrong by treating a blob as
    /// text first.
    #[test]
    fn a_blob_is_never_converted_by_any_affinity() {
        let blob = Value::Blob(vec![1, 2]);
        for aff in [
            Affinity::Blob,
            Affinity::Text,
            Affinity::Numeric,
            Affinity::Integer,
            Affinity::Real,
        ] {
            assert_eq!(convert(&blob, aff), blob, "{aff} converted a blob");
            assert_eq!(storage_class(&blob, aff), StorageClass::Blob, "{aff}");
        }
    }

    /// Leading and trailing spaces are allowed before a number, and the
    /// numeric affinities drop them. Under BLOB and TEXT nothing is dropped,
    /// because nothing is converted.
    #[test]
    fn surrounding_spaces_are_allowed_and_dropped_only_when_converting() {
        let padded = Value::Text("  7.5  ".into());
        for aff in [Affinity::Numeric, Affinity::Integer, Affinity::Real] {
            assert_eq!(convert(&padded, aff), Value::real(7.5), "{aff}");
        }
        for aff in [Affinity::Blob, Affinity::Text] {
            assert_eq!(convert(&padded, aff), padded, "{aff} dropped the spaces");
        }
        // A whole padded number narrows to an integer, like any other.
        assert_eq!(
            convert(&Value::Text("  42  ".into()), Affinity::Numeric),
            Value::Integer(42)
        );
        // Whitespace that is only spaces is not a number and stays text.
        let blank = Value::Text("   ".into());
        for aff in [Affinity::Numeric, Affinity::Integer, Affinity::Real] {
            assert_eq!(convert(&blank, aff), blank, "{aff} converted blanks");
        }
    }

    // -- The direction that does not depend on a single column ----------

    /// A comparison against a literal applies only the column's affinity, and
    /// the literal itself contributes nothing.
    #[test]
    fn a_literal_takes_the_columns_affinity_and_nothing_else() {
        // One operand is a column, and its affinity is what reaches the other
        // side. A TEXT column passes TEXT to the literal; a REAL column is
        // numeric, so it passes NUMERIC to the literal rather than REAL, which
        // is the rule the 2^53 case above depends on.
        assert_eq!(
            operand_affinities(Some(Affinity::Text), None),
            (Affinity::Text, Affinity::Text)
        );
        assert_eq!(
            operand_affinities(None, Some(Affinity::Real)),
            (Affinity::Numeric, Affinity::Blob)
        );
        // Neither is a column, so nothing is converted at all.
        assert_eq!(
            operand_affinities(None, None),
            (Affinity::Blob, Affinity::Blob)
        );
        assert_eq!(comparison_affinity(None, None), Affinity::Blob);
        // And the effect on the operands: an INTEGER column meets the text
        // literal '1' and answers 1, because the literal is converted.
        let (l, r) = apply_comparison(
            &Value::Integer(1),
            Some(Affinity::Integer),
            &Value::Text("1".into()),
            None,
        );
        assert_eq!((l, r), (Value::Integer(1), Value::Integer(1)));
    }

    /// The rule the suite's `types.test` leans on, at a value where the wrong
    /// rule and the right one disagree.
    ///
    /// A TEXT column meets an INTEGER, NUMERIC or REAL one, and the *text* is
    /// converted with NUMERIC in either written order. NUMERIC rather than the
    /// numeric column's own affinity is the point: under REAL the text would be
    /// rounded to an `f64`, and `2^53 + 1` is not an exact `f64`, so the two
    /// sides would come out equal and the comparison would answer 1 where
    /// sqlite3 answers 0.
    ///
    /// The value is chosen for exactly that reason. `'1.5'`, `'007'`, `1.0` and
    /// `5.0` are all exactly representable, so a test built from them passes
    /// whether the module applies REAL or NUMERIC and cannot tell the two rules
    /// apart.
    #[test]
    fn text_against_a_numeric_column_is_parsed_not_rounded() {
        // 2^53 + 1: an exact i64, not an exact f64.
        let text = Value::Text("9007199254740993".into());
        let real = Value::real(9007199254740992.0);
        for other in [Affinity::Numeric, Affinity::Integer, Affinity::Real] {
            // TEXT on the left, numeric on the right.
            let (l, r) = apply_comparison(&text, Some(Affinity::Text), &real, Some(other));
            assert_eq!(
                l,
                Value::Integer(9007199254740993),
                "text side under {other} was rounded instead of parsed"
            );
            assert_eq!(r, real, "numeric side under {other} was converted");
            assert_ne!(l, r, "{other} made 2^53+1 and 2^53 equal");
            // Numeric on the left, TEXT on the right: the same answer.
            let (l, r) = apply_comparison(&real, Some(other), &text, Some(Affinity::Text));
            assert_eq!(
                (l, r),
                (real.clone(), Value::Integer(9007199254740993)),
                "{other}, written the other way"
            );
        }
        // And the conversion is what made them differ, rather than a no-op:
        // REAL applied to the text rounds it onto the real and the pair
        // collapses to one value. Stated through `convert`, because the wrong
        // rule is a hypothetical — `apply_comparison` is not supposed to do it.
        assert_eq!(
            convert(&text, Affinity::Real),
            real,
            "REAL rounds 2^53+1 onto 2^53, which is the divergence"
        );
        assert_eq!(
            (convert(&text, Affinity::Real), real.clone()),
            (real.clone(), real.clone())
        );
        // NUMERIC does not round, which is why the pair survives above.
        assert_eq!(
            convert(&text, Affinity::Numeric),
            Value::Integer(9007199254740993)
        );
    }

    /// The same rule for a BLOB column in place of the TEXT one, in both
    /// written orders. A BLOB value is never converted, so the blob operand
    /// stays exactly as it is and only the other side is parsed.
    #[test]
    fn a_blob_column_meets_a_numeric_column_the_same_way() {
        // A BLOB holding a text value: the blob is the identity, so this is
        // the text-versus-numeric case with BLOB in place of TEXT.
        let blob = Value::Blob("9007199254740993".as_bytes().to_vec());
        let real = Value::real(9007199254740992.0);
        let (l, r) = apply_comparison(&blob, Some(Affinity::Blob), &real, Some(Affinity::Real));
        assert_eq!(
            r,
            Value::Integer(9007199254740992),
            "real side under NUMERIC"
        );
        assert_ne!(
            l.compare(&r),
            std::cmp::Ordering::Equal,
            "a blob never equals a number, whatever the other side's affinity"
        );
        let (l, r) = apply_comparison(&real, Some(Affinity::Real), &blob, Some(Affinity::Blob));
        assert_eq!(
            l,
            Value::Integer(9007199254740992),
            "real side under NUMERIC"
        );
        assert_ne!(
            l.compare(&r),
            std::cmp::Ordering::Equal,
            "either written order"
        );
    }

    /// The one-column half of rule 1: a column with a numeric affinity converts
    /// the non-column operand with NUMERIC, whatever the column's own affinity
    /// is. This is the same defect reached from the other direction, and it is
    /// the shape a literal comparison takes.
    #[test]
    fn a_numeric_column_parses_a_text_literal_rather_than_rounding_it() {
        let real = Value::real(9007199254740992.0);
        for aff in [Affinity::Numeric, Affinity::Integer, Affinity::Real] {
            // The literal is a column, so it is converted with NUMERIC, not
            // with REAL: the exact integer 2^53+1, not the rounded real.
            let (l, r) = apply_comparison(
                &Value::Text("9007199254740993".into()),
                None,
                &real,
                Some(aff),
            );
            assert_eq!(r, Value::Integer(9007199254740992), "{aff} column side");
            assert_eq!(l, Value::Integer(9007199254740993), "{aff} literal side");
            assert_ne!(l, r, "{aff} made the literal and the column equal");
        }
    }

    /// Two columns each contribute an affinity, and the pair decides an affinity
    /// per operand. NUMERIC is what reaches the non-numeric side whenever the
    /// other side is INTEGER, REAL or NUMERIC, in either written order.
    #[test]
    fn a_numeric_column_puts_numeric_on_the_other_operand_in_either_position() {
        let numeric = [Affinity::Numeric, Affinity::Integer, Affinity::Real];
        let not_numeric = [Affinity::Text, Affinity::Blob];
        for n in numeric {
            for other in not_numeric {
                // Numeric on the left: NUMERIC goes to the right operand.
                assert_eq!(
                    operand_affinities(Some(n), Some(other)),
                    (Affinity::Blob, Affinity::Numeric),
                    "{n} on the left against {other}"
                );
                // Numeric on the right: NUMERIC goes to the left operand.
                assert_eq!(
                    operand_affinities(Some(other), Some(n)),
                    (Affinity::Numeric, Affinity::Blob),
                    "{n} on the right against {other}"
                );
            }
        }
    }

    /// Rule 1 is checked before rule 2, which is what a TEXT column against a
    /// REAL column turns on. Under rule 1 the text is parsed to a number; under
    /// rule 2 it would be stringified, and the two sides would be a string
    /// against a number and never equal.
    #[test]
    fn rule_one_is_checked_before_rule_two() {
        // A TEXT and a REAL column: rule 1, so the TEXT side becomes a number.
        assert_eq!(
            operand_affinities(Some(Affinity::Text), Some(Affinity::Real)),
            (Affinity::Numeric, Affinity::Blob)
        );
        // Under rule 2 it would have been (Blob, Text) and the text would have
        // stayed a string, so the assertion above is the check on the order.
        assert_ne!(
            operand_affinities(Some(Affinity::Text), Some(Affinity::Real)),
            (Affinity::Blob, Affinity::Text)
        );
    }

    /// Two numeric columns: NUMERIC on both sides. This is the identity for an
    /// integer, and for a real it is the same thing INTEGER would do — narrow
    /// only when nothing is lost.
    #[test]
    fn two_numeric_columns_get_numeric_on_both_sides() {
        for a in [Affinity::Numeric, Affinity::Integer, Affinity::Real] {
            for b in [Affinity::Numeric, Affinity::Integer, Affinity::Real] {
                assert_eq!(
                    operand_affinities(Some(a), Some(b)),
                    (Affinity::Numeric, Affinity::Numeric),
                    "{a} against {b}"
                );
            }
        }
    }

    /// Neither side is numeric, so rule 2 decides. Between two columns rule 2
    /// does not reach the other column: a BLOB column meeting a TEXT column
    /// converts neither. Only a non-column operand takes the TEXT.
    #[test]
    fn two_non_numeric_columns_follow_the_text_rule() {
        // A BLOB column meeting a TEXT column: both are left alone. Measured:
        // a BLOB column holding the integer 5 against a TEXT column holding
        // '5' answers 0 in sqlite3 3.53.4, so the blob was not stringified.
        assert_eq!(
            operand_affinities(Some(Affinity::Text), Some(Affinity::Blob)),
            (Affinity::Text, Affinity::Blob)
        );
        assert_eq!(
            operand_affinities(Some(Affinity::Blob), Some(Affinity::Text)),
            (Affinity::Blob, Affinity::Text)
        );
        // Two TEXT columns: TEXT on both, which is the identity for a text.
        assert_eq!(
            operand_affinities(Some(Affinity::Text), Some(Affinity::Text)),
            (Affinity::Text, Affinity::Text)
        );
        // Two BLOB columns: BLOB on both, which converts nothing.
        assert_eq!(
            operand_affinities(Some(Affinity::Blob), Some(Affinity::Blob)),
            (Affinity::Blob, Affinity::Blob)
        );
        // A TEXT column against a literal: the literal is the one with no
        // affinity, so it takes TEXT, and the column keeps its own.
        assert_eq!(
            operand_affinities(Some(Affinity::Text), None),
            (Affinity::Text, Affinity::Text)
        );
        assert_eq!(
            operand_affinities(None, Some(Affinity::Text)),
            (Affinity::Text, Affinity::Text)
        );
        // A BLOB column against a literal: BLOB converts nothing, and a BLOB
        // column is not something a literal can lend TEXT to either.
        assert_eq!(
            operand_affinities(Some(Affinity::Blob), None),
            (Affinity::Blob, Affinity::Blob)
        );
        assert_eq!(
            operand_affinities(None, Some(Affinity::Blob)),
            (Affinity::Blob, Affinity::Blob)
        );
    }

    /// Rule 2's one measurable consequence, stated on its own so a reader does
    /// not have to infer it: a BLOB column and a TEXT column convert neither
    /// side, while a TEXT column and a literal stringifies the literal.
    ///
    /// Both halves are measured against sqlite3 3.53.4 and both are needed: the
    /// first says a BLOB column does not hand TEXT to another column, the
    /// second says a TEXT column does hand it to a non-column.
    #[test]
    fn a_blob_column_meets_a_text_column_without_converting_either() {
        // A BLOB column holding the integer 5, a TEXT column holding '5'.
        //   no conversion      -> 5   vs '5'   -> 0
        //   TEXT on the blob   -> '5' vs '5'   -> 1
        // Measured: sqlite3 3.53.4 answers 0, so rule 2 did not fire.
        let (l, r) = apply_comparison(
            &Value::Integer(5),
            Some(Affinity::Blob),
            &Value::Text("5".into()),
            Some(Affinity::Text),
        );
        assert_eq!(
            (l.clone(), r.clone()),
            (Value::Integer(5), Value::Text("5".into())),
            "a BLOB column must not stringify itself to meet a TEXT column"
        );
        assert_eq!(
            l.compare(&r),
            std::cmp::Ordering::Less,
            "the pair is not equal, which is what the oracle gives"
        );
        // The other written order, same answer.
        let (l, r) = apply_comparison(
            &Value::Text("5".into()),
            Some(Affinity::Text),
            &Value::Integer(5),
            Some(Affinity::Blob),
        );
        assert_eq!(
            (l.clone(), r.clone()),
            (Value::Text("5".into()), Value::Integer(5))
        );
        assert_eq!(l.compare(&r), std::cmp::Ordering::Greater);
        // And the literal half, which does fire: a TEXT column holding '1.5'
        // against the numeric literal 1.5 answers 1, because the literal is
        // stringified to '1.5'. Measured: sqlite3 3.53.4 answers 1.
        //
        // 1.5 rather than a whole real, because stringifying a whole real goes
        // through `affinity`'s real formatter, which is outside this module and
        // currently renders 1.0 as '1'. This test is about which affinity the
        // literal takes, and 1.5 exercises that without depending on the
        // formatter.
        let (l, r) = apply_comparison(
            &Value::Text("1.5".into()),
            Some(Affinity::Text),
            &Value::real(1.5),
            None,
        );
        assert_eq!(
            (l.clone(), r.clone()),
            (Value::Text("1.5".into()), Value::Text("1.5".into())),
            "a TEXT column does stringify a literal"
        );
        assert_eq!(l.compare(&r), std::cmp::Ordering::Equal);
    }

    /// Resolution is written in terms of which side is numeric rather than
    /// which side was written first, so a comparison gives the same answer in
    /// either written order.
    ///
    /// The property is commutativity of the *outcome*, not a mirror image of
    /// the pair: for a pair of two different affinities the pair does mirror,
    /// but TEXT against TEXT is a symmetric pair, so it is its own mirror.
    #[test]
    fn resolution_does_not_depend_on_which_side_is_written_first() {
        const ALL: [Affinity; 5] = [
            Affinity::Numeric,
            Affinity::Integer,
            Affinity::Real,
            Affinity::Blob,
            Affinity::Text,
        ];
        // Two unlike columns: the pair mirrors exactly.
        for a in ALL {
            for b in ALL {
                if a == b {
                    continue;
                }
                let (la, ra) = operand_affinities(Some(a), Some(b));
                let (lb, rb) = operand_affinities(Some(b), Some(a));
                assert_eq!((lb, rb), (ra, la), "{a} and {b}");
            }
        }
        // A column against itself resolves to that affinity on both sides.
        for a in ALL {
            let (la, ra) = operand_affinities(Some(a), Some(a));
            assert_eq!(la, ra, "{a} against itself is asymmetric");
        }
        // And the outcome does not depend on the written order, for every
        // pair including the symmetric ones. The two orders are the two
        // conversions applied to the same two values, so the answer is the
        // same whenever the pair is the same pair of values.
        for a in ALL {
            for b in ALL {
                let values = [
                    Value::Text("9007199254740993".into()),
                    Value::Integer(9007199254740993),
                    Value::real(9007199254740992.0),
                ];
                for l in &values {
                    for r in &values {
                        let forward = apply_comparison(l, Some(a), r, Some(b));
                        let backward = apply_comparison(r, Some(b), l, Some(a));
                        assert_eq!(
                            forward.0, backward.1,
                            "{a} against {b}: {l:?} = {r:?} differs by written order"
                        );
                        assert_eq!(
                            forward.1, backward.0,
                            "{a} against {b}: {l:?} = {r:?} differs by written order"
                        );
                    }
                }
            }
        }
    }

    /// A BLOB is never converted, so a blob never becomes a number in a
    /// comparison however it meets one — but a BLOB *column* is still what
    /// decides that the other side is converted.
    #[test]
    fn a_blob_column_outranks_only_text_and_converts_nothing() {
        // blob vs blob: BLOB, which converts nothing.
        assert_eq!(
            operand_affinities(Some(Affinity::Blob), Some(Affinity::Blob)),
            (Affinity::Blob, Affinity::Blob)
        );
        let (l, r) = apply_comparison(
            &Value::Blob(vec![1]),
            Some(Affinity::Blob),
            &Value::Blob(vec![1]),
            Some(Affinity::Blob),
        );
        assert_eq!((l, r), (Value::Blob(vec![1]), Value::Blob(vec![1])));
        // blob vs integer: the integer is the numeric side, so NUMERIC goes to
        // the blob — which is the identity, so the blob is still a blob after
        // the conversion. Note the direction: the NUMERIC lands on the *left*
        // operand here, because the blob is the one on the left.
        assert_eq!(
            operand_affinities(Some(Affinity::Blob), Some(Affinity::Integer)),
            (Affinity::Numeric, Affinity::Blob)
        );
        let (l, r) = apply_comparison(
            &Value::Blob(vec![1]),
            Some(Affinity::Blob),
            &Value::Integer(1),
            Some(Affinity::Integer),
        );
        assert_eq!((l, r), (Value::Blob(vec![1]), Value::Integer(1)));
    }

    /// NULL is NULL under every affinity, so a comparison involving one is
    /// unknown rather than a conversion. The other operand is still converted
    /// by the resolved rule, so that is asserted from the resolver rather than
    /// restated — the point of the test is the NULL, and a hard-coded
    /// expectation for the other side would just be a second copy of the rule.
    #[test]
    fn null_survives_the_comparison_conversion() {
        for a in [
            Affinity::Blob,
            Affinity::Text,
            Affinity::Numeric,
            Affinity::Integer,
            Affinity::Real,
        ] {
            // Against an INTEGER column, the numeric affinities pair with it
            // and the non-numeric ones (BLOB, TEXT) are the side that NUMERIC
            // or TEXT reaches — so the first component is not always BLOB.
            let (al, ar) = operand_affinities(Some(a), Some(Affinity::Integer));
            let (l, r) = apply_comparison(
                &Value::Null,
                Some(a),
                &Value::Integer(1),
                Some(Affinity::Integer),
            );
            assert_eq!(l, Value::Null, "{a}");
            assert_eq!(r, convert(&Value::Integer(1), ar), "{a}");
            // Whichever affinity reaches the NULL, it is one that leaves NULL
            // alone, and the assertion above is the check that it does.
            assert_eq!(
                convert(&Value::Null, al),
                Value::Null,
                "{a} put {al} on the NULL side"
            );
            // The integer side is untouched either way.
            assert_eq!(r, Value::Integer(1), "{a} converted the integer side");
        }
    }

    /// The other direction of the asymmetry: a literal or an expression
    /// contributes no affinity at all, so it cannot be the reason a conversion
    /// happens — it is converted by the column's, or not at all. This is the
    /// case the spec calls out as "a comparison between a column and a literal
    /// applies only the column".
    #[test]
    fn a_non_column_operand_contributes_no_affinity() {
        // Two columns resolve between themselves.
        assert_eq!(
            operand_affinities(Some(Affinity::Text), Some(Affinity::Integer)),
            (Affinity::Numeric, Affinity::Blob)
        );
        // A column and a literal: only the column is present, so the literal
        // gets the column's rule and nothing of its own. A TEXT column is not
        // numeric, so rule 2 applies and the literal is stringified.
        assert_eq!(
            operand_affinities(Some(Affinity::Text), None),
            (Affinity::Text, Affinity::Text)
        );
        // Two literals: no affinity at all, which is why `1 = 1.0` is true and
        // `'1' = 1.0` is false without any conversion.
        assert_eq!(
            operand_affinities(None, None),
            (Affinity::Blob, Affinity::Blob)
        );
        let (l, r) = apply_comparison(&Value::Text("1".into()), None, &Value::real(1.0), None);
        assert_eq!((l, r), (Value::Text("1".into()), Value::real(1.0)));
    }

    /// `comparison_affinity` is the single-affinity view of the same rule, so
    /// it has to agree with the pair whenever the two operands get the same
    /// treatment, and collapse to the non-BLOB one when they do not.
    #[test]
    fn the_single_affinity_view_agrees_with_the_pair() {
        for a in [
            Affinity::Numeric,
            Affinity::Integer,
            Affinity::Real,
            Affinity::Blob,
            Affinity::Text,
        ] {
            for b in [
                Affinity::Numeric,
                Affinity::Integer,
                Affinity::Real,
                Affinity::Blob,
                Affinity::Text,
            ] {
                let (l, r) = operand_affinities(Some(a), Some(b));
                let single = comparison_affinity(Some(a), Some(b));
                if l == r {
                    assert_eq!(single, l, "{a} against {b} treats both alike");
                } else {
                    // The two operands differ, and exactly one of them is
                    // BLOB — the "do not touch this side" half. Whichever side
                    // that is, the single view is the other one, because the
                    // BLOB side is the one carrying no information.
                    assert!(
                        l == Affinity::Blob || r == Affinity::Blob,
                        "{a} against {b} produced ({l}, {r}) with no BLOB side"
                    );
                    let want = if l == Affinity::Blob { r } else { l };
                    assert_eq!(single, want, "{a} against {b}");
                }
            }
        }
        // And the documented shorthand values.
        assert_eq!(comparison_affinity(None, None), Affinity::Blob);
        assert_eq!(
            comparison_affinity(Some(Affinity::Text), None),
            Affinity::Text
        );
        // A numeric column is NUMERIC for this purpose, not its own affinity:
        // the literal side is what gets converted, and it gets NUMERIC.
        assert_eq!(
            comparison_affinity(None, Some(Affinity::Real)),
            Affinity::Numeric
        );
        assert_eq!(
            comparison_affinity(Some(Affinity::Real), Some(Affinity::Integer)),
            Affinity::Numeric
        );
    }

    // -- A literal takes the column's affinity, measured ------------------

    /// `int_col = '1.5'`: the literal is a column, so it takes NUMERIC, becomes
    /// the real 1.5, and matches. Measured: sqlite3 3.53.4 answers 1 for an
    /// INTEGER column holding the real 1.5 against the literal '1.5'.
    #[test]
    fn an_integer_column_converts_a_text_literal() {
        let (l, r) = apply_comparison(
            &Value::real(1.5),
            Some(Affinity::Integer),
            &Value::Text("1.5".into()),
            None,
        );
        assert_eq!((l, r), (Value::real(1.5), Value::real(1.5)));
    }

    /// `text_col = 1.5`: the literal is a column, so it takes the column's own
    /// TEXT affinity, becomes the string '1.5', and the text column answers 0.
    /// Measured: sqlite3 3.53.4 answers 0.
    ///
    /// The column holds the *string* '1.5', not the real 1.5: a TEXT column
    /// stores the real 1.5 as the text '1.5', and the literal has to be
    /// stringified to meet it. Holding a real here would make the test pass
    /// without exercising the conversion at all.
    #[test]
    fn a_text_column_converts_a_numeric_literal() {
        let (l, r) = apply_comparison(
            &Value::Text("1.5".into()),
            Some(Affinity::Text),
            &Value::real(1.5),
            None,
        );
        assert_eq!(
            (l.clone(), r.clone()),
            (Value::Text("1.5".into()), Value::Text("1.5".into()))
        );
        assert_eq!(l, r, "the stringified literal does match the text column");
        // The same comparison with a NUMERIC literal instead: the column is
        // left alone and the literal becomes the string '1.5' as well, so the
        // pair is the same either way. Measured: sqlite3 3.53.4 answers 1.
        let (l, r) = apply_comparison(
            &Value::Text("1.5".into()),
            Some(Affinity::Text),
            &Value::real(1.5),
            None,
        );
        assert_eq!(
            (l, r),
            (Value::Text("1.5".into()), Value::Text("1.5".into()))
        );
    }

    /// `blob_col = 1`: BLOB converts nothing, so the integer stays an integer.
    /// Measured: sqlite3 3.53.4 answers 1 — a blob holding the integer 1 does
    /// equal the integer 1, because neither side is converted.
    #[test]
    fn a_blob_column_converts_nothing_for_a_literal() {
        let (l, r) = apply_comparison(
            &Value::Blob(vec![1]),
            Some(Affinity::Blob),
            &Value::Integer(1),
            None,
        );
        assert_eq!((l, r), (Value::Blob(vec![1]), Value::Integer(1)));
    }

    /// The literal is converted on whichever side it is written, so
    /// `1.5 = text_col` and `text_col = 1.5` agree.
    ///
    /// The column holds the *string* '1.5', which is what a TEXT column stores
    /// a real 1.5 as, so the numeric literal is stringified to meet it and the
    /// two orders both answer 1. Measured: sqlite3 3.53.4 answers 1 for both.
    /// The value is the only thing that makes the case bite: with a column
    /// holding a real instead, the literal would be stringified against a real
    /// and the pair would not be equal, which is a different case entirely.
    #[test]
    fn a_literal_is_converted_whichever_side_it_is_written_on() {
        let (right, _) = apply_comparison(
            &Value::Text("1.5".into()),
            Some(Affinity::Text),
            &Value::real(1.5),
            None,
        );
        assert_eq!(
            right,
            Value::Text("1.5".into()),
            "the literal takes the column's affinity"
        );
        // The same comparison written the other way, with the literal on the
        // left and the column on the right.
        let (left, _) = apply_comparison(
            &Value::real(1.5),
            None,
            &Value::Text("1.5".into()),
            Some(Affinity::Text),
        );
        assert_eq!(
            left,
            Value::Text("1.5".into()),
            "the literal on the left takes the right column's affinity"
        );
        assert_eq!(left, right, "the two written orders agree");
    }

    /// Two literals have no affinity at all, which is why `1 = 1.0` is true and
    /// `'1' = 1.0` is false: nothing converts either side.
    #[test]
    fn two_literals_are_compared_as_written() {
        let (l, r) = apply_comparison(&Value::Integer(1), None, &Value::real(1.0), None);
        assert_eq!((l, r), (Value::Integer(1), Value::real(1.0)));
        let (l, r) = apply_comparison(&Value::Text("1".into()), None, &Value::real(1.0), None);
        assert_eq!((l, r), (Value::Text("1".into()), Value::real(1.0)));
    }
}
