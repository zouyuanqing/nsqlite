//! The comparison rule checked against a table of cases measured on sqlite3
//! 3.53.4.
//!
//! Every expectation here is the answer the real program gave for the SQL
//! written beside it, so a rule that is right for the wrong reason still passes
//! and a rule that is wrong for the right reason still fails. The commands that
//! produced them are in `docs/affinity-grid.md` and in the module docs; the
//! point of this file is that the *table* is the oracle, not a recollection.

use super::*;
use crate::value::Value;

/// One measured case: the two operands, their column affinities, and what
/// sqlite3 answered for `left OP right`.
///
/// `op` is the operator the case was measured with, so a case that is only
/// meaningful for `=` is written with it and a case that is meaningful for any
/// comparison is measured three ways in the table below.
struct Case {
    left: Value,
    left_aff: Option<Affinity>,
    right: Value,
    right_aff: Option<Affinity>,
    eq: bool,
    less: bool,
    greater: bool,
    /// What the case is, so a failure names the rule rather than a pair of
    /// values.
    what: &'static str,
}

/// Builds a case from what the oracle answered.
fn case(
    what: &'static str,
    left: Value,
    left_aff: Option<Affinity>,
    right: Value,
    right_aff: Option<Affinity>,
    eq: bool,
    less: bool,
    greater: bool,
) -> Case {
    Case {
        left,
        left_aff,
        right,
        right_aff,
        eq,
        less,
        greater,
        what,
    }
}

fn i(v: i64) -> Value {
    Value::Integer(v)
}
fn r(v: f64) -> Value {
    Value::real(v)
}
fn t(s: &str) -> Value {
    Value::Text(s.into())
}

const TEXT: Option<Affinity> = Some(Affinity::Text);
const INT: Option<Affinity> = Some(Affinity::Integer);
const REAL: Option<Affinity> = Some(Affinity::Real);
const NUM: Option<Affinity> = Some(Affinity::Numeric);
const BLOB: Option<Affinity> = Some(Affinity::Blob);

/// The case the spec names, and the one the suite's `types.test` leans on.
///
/// Measured on sqlite3 3.53.4:
/// ```text
/// CREATE TABLE t(a TEXT, b INTEGER); INSERT INTO t VALUES('5', 5);
/// SELECT a = b;   -- 1
/// ```
fn the_specified_asymmetry() -> Case {
    case(
        "TEXT column against an INTEGER column",
        t("5"),
        TEXT,
        i(5),
        INT,
        true,
        false,
        false,
    )
}

/// The same comparison written the other way, which answers the same because
/// the rule is written in terms of which side is numeric rather than which came
/// first.
fn the_specified_asymmetry_reversed() -> Case {
    case(
        "INTEGER column against a TEXT column",
        i(5),
        INT,
        t("5"),
        TEXT,
        true,
        false,
        false,
    )
}

/// A column against a literal, both orders. Measured:
/// ```text
/// CREATE TABLE t(a TEXT, b INTEGER); INSERT INTO t VALUES('5', 5);
/// SELECT a = 5;   -- 1     SELECT 5 = a;   -- 1
/// SELECT b = '5'; -- 1     SELECT '5' = b; -- 1
/// ```
fn columns_against_literals() -> Vec<Case> {
    vec![
        case(
            "TEXT column against the integer 5",
            t("5"),
            TEXT,
            i(5),
            None,
            true,
            false,
            false,
        ),
        case(
            "the integer 5 against a TEXT column",
            i(5),
            None,
            t("5"),
            TEXT,
            true,
            false,
            false,
        ),
        case(
            "INTEGER column against the text '5'",
            i(5),
            INT,
            t("5"),
            None,
            true,
            false,
            false,
        ),
        case(
            "the text '5' against an INTEGER column",
            t("5"),
            None,
            i(5),
            INT,
            true,
            false,
            false,
        ),
    ]
}

/// The two literals, which have no affinity at all. Measured: `1 = 1.0` is 1
/// and `'1' = 1.0` is 0, so nothing converts either side.
///
/// The two `false` cases also pin the *direction*, which is the other half of
/// "compared as written": a text sorts after every number because SQLite's
/// storage-class order is NULL, numeric, text, blob, so `'1' > 1` rather than
/// `'1' < 1`. An implementation that compared the two by value rather than by
/// class would get the equality right and the ordering wrong.
fn two_literals_are_compared_as_written() -> Vec<Case> {
    vec![
        case(
            "integer 1 against real 1.0",
            i(1),
            None,
            r(1.0),
            None,
            true,
            false,
            false,
        ),
        case(
            "text '1' against real 1.0",
            t("1"),
            None,
            r(1.0),
            None,
            false,
            false,
            true,
        ),
        case(
            "text '1' against integer 1",
            t("1"),
            None,
            i(1),
            None,
            false,
            false,
            true,
        ),
    ]
}

/// The `2^53` case, which is the only value in the table that can tell NUMERIC
/// from REAL. Measured:
/// ```text
/// CREATE TABLE t(x TEXT);  INSERT INTO t VALUES('9007199254740993');
/// CREATE TABLE r(x REAL);  INSERT INTO r VALUES(9007199254740992.0);
/// SELECT t.x = r.x;   -- 0
/// ```
fn the_2_pow_53_divergence() -> Vec<Case> {
    vec![
        case(
            "text 2^53+1 against a REAL column holding 2^53",
            t("9007199254740993"),
            TEXT,
            r(9007199254740992.0),
            REAL,
            false,
            false,
            true,
        ),
        case(
            "a REAL column holding 2^53 against the text 2^53+1",
            r(9007199254740992.0),
            REAL,
            t("9007199254740993"),
            TEXT,
            false,
            true,
            false,
        ),
    ]
}

/// A BLOB column meeting a TEXT column converts neither side. Measured: a BLOB
/// column holding the integer 5 against a TEXT column holding '5' answers 0,
/// so rule 2 did not fire between two columns.
fn a_blob_column_against_a_text_column() -> Vec<Case> {
    vec![
        case(
            "BLOB column holding 5 against a TEXT column holding '5'",
            i(5),
            BLOB,
            t("5"),
            TEXT,
            false,
            true,
            false,
        ),
        case(
            "TEXT column holding '5' against a BLOB column holding 5",
            t("5"),
            TEXT,
            i(5),
            BLOB,
            false,
            false,
            true,
        ),
    ]
}

/// A BLOB column against a numeric one: the numeric side is what gets NUMERIC,
/// and a blob is never converted, so the pair is a blob against a number and
/// is never equal.
///
/// Measured on sqlite3 3.53.4:
/// ```text
/// CREATE TABLE t(z BLOB, i INTEGER); INSERT INTO t VALUES(x'01', 1);
/// SELECT z = i;   -- 0
/// SELECT z < i;   -- 0
/// SELECT z > i;   -- 1
/// ```
/// The `less`/`greater` pair is not decoration: a blob sorts *after* every
/// number, because SQLite's storage-class order is NULL, numeric, text, blob.
/// So this case also pins the direction, and a rule that converted the blob
/// into a number would make it equal rather than greater.
fn a_blob_column_against_a_numeric_column() -> Case {
    case(
        "BLOB column holding a blob against an INTEGER column holding 1",
        Value::Blob(vec![1]),
        BLOB,
        i(1),
        INT,
        false,
        false,
        true,
    )
}

/// A NUMERIC column holding a whole real is already an integer, because NUMERIC
/// narrows on the way in -- so this is the same case as the INTEGER one, and it
/// is here to check that the two agree.
fn a_numeric_column_holds_an_integer() -> Case {
    case(
        "NUMERIC column holding 5 against a TEXT column holding '5'",
        i(5),
        NUM,
        t("5"),
        TEXT,
        true,
        false,
        false,
    )
}

/// The case that loses information, in the direction the rule is easiest to get
/// wrong: a text that is not entirely a number stays text, and a column's
/// affinity is applied to the *other* side, so a NUMERIC column does not turn
/// '12abc' into 12 on either side of a comparison.
///
/// Measured:
/// ```text
/// CREATE TABLE t(a INTEGER, b TEXT); INSERT INTO t VALUES(12, '12abc');
/// SELECT a = b;   -- 0
/// ```
fn information_losing_values_stay_unconverted() -> Vec<Case> {
    vec![
        // The column that holds the number comes first here, and 12 < '12abc'
        // is false the other way round: a text sorts after every number.
        case(
            "an INTEGER column holding 12 against a TEXT column holding '12abc'",
            i(12),
            INT,
            t("12abc"),
            TEXT,
            false,
            true,
            false,
        ),
        case(
            "a TEXT column holding '12abc' against the integer 12",
            t("12abc"),
            TEXT,
            i(12),
            None,
            false,
            false,
            true,
        ),
        case(
            "a TEXT column holding '12abc' against the real 12.0",
            t("12abc"),
            TEXT,
            r(12.0),
            None,
            false,
            false,
            true,
        ),
    ]
}

/// Leading and trailing spaces are dropped by a numeric affinity, so a padded
/// text is a number to an INTEGER column. Measured:
/// ```text
/// CREATE TABLE t(a TEXT, b INTEGER); INSERT INTO t VALUES('  7.5  ', 7.5);
/// SELECT a = b;   -- 1
/// ```
fn padded_text_meets_a_numeric_column() -> Case {
    case(
        "a TEXT column holding '  7.5  ' against an INTEGER column holding 7.5",
        t("  7.5  "),
        TEXT,
        r(7.5),
        INT,
        true,
        false,
        false,
    )
}

/// A TEXT column stringifies a *literal* it is compared against, which is the
/// one place rule 2 reaches the other side, and the stringification is the
/// same conversion a TEXT column applies on the way in.
///
/// Measured on sqlite3 3.53.4:
/// ```text
/// CREATE TABLE t(a TEXT);  INSERT INTO t VALUES(1.5);
/// SELECT a = 1.5;   -- 1
/// ```
/// A fractional real is the case that bites, because a whole real is the case
/// that depends on the formatter: a TEXT column stores the real `1.0` as the
/// text `'1.0'`, so a literal `1.0` stringifies to `'1.0'` and matches, but
/// only because the formatter writes the `.0`. `1.5` is used here for the same
/// reason the module's own test uses it -- it does not depend on whether the
/// formatter keeps a fractional part on a whole number.
///
/// The `false` half is the other measured case and it is the reason the pair is
/// spelled out rather than asserted as one: `CREATE TABLE t(a TEXT); INSERT
/// INTO t VALUES(5.0); SELECT a = 1.0` answers 0, because the column holds
/// `'5.0'` and the literal stringifies to `'1.0'`.
fn a_text_column_stringifies_a_literal() -> Vec<Case> {
    vec![
        case(
            "a TEXT column holding '1.5' against the literal 1.5",
            t("1.5"),
            TEXT,
            r(1.5),
            None,
            true,
            false,
            false,
        ),
        case(
            "a TEXT column holding '5.0' against the literal 1.0",
            t("5.0"),
            TEXT,
            r(1.0),
            None,
            false,
            false,
            true,
        ),
    ]
}

/// A BLOB column converts nothing, so the literal keeps its own type. Measured:
/// a BLOB column holding the integer 1 against the literal 1 answers 1, because
/// neither side is converted and they are the same value.
fn a_blob_column_converts_nothing_for_a_literal() -> Case {
    case(
        "a BLOB column holding the integer 1 against the literal 1",
        i(1),
        BLOB,
        i(1),
        None,
        true,
        false,
        false,
    )
}

/// A REAL column against a text that names a whole number parses it exactly,
/// so a value where the two spellings differ is what the rule is for.
fn a_real_column_parses_its_text_exactly() -> Vec<Case> {
    vec![
        case(
            "a REAL column holding 5.0 against the text '5'",
            r(5.0),
            REAL,
            t("5"),
            None,
            true,
            false,
            false,
        ),
        case(
            "a REAL column holding 7.5 against the text '7.5'",
            r(7.5),
            REAL,
            t("7.5"),
            None,
            true,
            false,
            false,
        ),
    ]
}

/// Every measured case, so the table above is a set of named cases and this is
/// the list they are checked from.
fn all_cases() -> Vec<Case> {
    let mut v = vec![
        the_specified_asymmetry(),
        the_specified_asymmetry_reversed(),
    ];
    v.push(a_numeric_column_holds_an_integer());
    v.push(a_blob_column_against_a_numeric_column());
    v.push(padded_text_meets_a_numeric_column());
    v.extend(a_text_column_stringifies_a_literal());
    v.push(a_blob_column_converts_nothing_for_a_literal());
    v.extend(columns_against_literals());
    v.extend(two_literals_are_compared_as_written());
    v.extend(the_2_pow_53_divergence());
    v.extend(a_blob_column_against_a_text_column());
    v.extend(information_losing_values_stay_unconverted());
    v.extend(a_real_column_parses_its_text_exactly());
    v
}

/// Checks one case against all three comparison answers, which is what makes the
/// table say `less` and `greater` rather than just "not equal".
fn check(c: &Case) {
    use std::cmp::Ordering;
    let ord = compare(&c.left, c.left_aff, &c.right, c.right_aff);
    assert_eq!(
        ord == Ordering::Equal,
        c.eq,
        "{}: expected equal={}, got {:?} on {:?} vs {:?}",
        c.what,
        c.eq,
        ord,
        c.left,
        c.right
    );
    assert_eq!(
        ord == Ordering::Less,
        c.less,
        "{}: expected less={}",
        c.what,
        c.less
    );
    assert_eq!(
        ord == Ordering::Greater,
        c.greater,
        "{}: expected greater={}",
        c.what,
        c.greater
    );
    // The same answer through `equal`, which is what `IN` uses, so the two
    // entry points cannot disagree about equality.
    assert_eq!(
        equal(&c.left, c.left_aff, &c.right, c.right_aff),
        c.eq,
        "{}: `equal` disagrees with `compare`",
        c.what
    );
}

/// Every measured case, checked. This is the test that would have caught the
/// whole gap: before the rule was applied, every case with a column on either
/// side answered as the two raw values compared.
#[test]
fn every_measured_comparison_case() {
    let cases = all_cases();
    assert!(cases.len() >= 23, "the table has {} cases", cases.len());
    for c in &cases {
        check(c);
    }
}

/// The six operators agree, which is what a caller gets from routing all of them
/// through one comparison: the answer to `<` is the mirror of the answer to
/// `>`, because the rule is symmetric in its effect even where it is asymmetric
/// in its written form.
#[test]
fn the_rule_is_symmetric_in_effect_however_it_is_written() {
    use std::cmp::Ordering;
    for c in all_cases() {
        let forward = compare(&c.left, c.left_aff, &c.right, c.right_aff);
        let backward = compare(&c.right, c.right_aff, &c.left, c.left_aff);
        assert_eq!(
            forward,
            backward.reverse(),
            "{}: the two written orders disagree",
            c.what
        );
        // And the equality of the two orders is the same, which is what `IN`
        // and `=` need.
        assert_eq!(forward == Ordering::Equal, backward == Ordering::Equal);
    }
}

/// The affinities of a statement's column references, resolved from the
/// references the join already bound.
///
/// The map is keyed by the offset the reference was written at, which is the
/// key the evaluator finds a value by, so the two agree on what "this operand"
/// is without a second resolution. A statement with no columns in it produces
/// an empty map, and every comparison in it then converts nothing.
#[test]
fn a_statement_with_no_columns_contributes_no_affinity() {
    let from = crate::join::resolve(Vec::new()).expect("an empty FROM is legal");
    let map = affinities_of(&from, std::iter::empty());
    assert!(map.is_empty());
    // Which is the same as the rule's own statement about two literals.
    assert_eq!(
        compare(&i(1), None, &r(1.0), None),
        std::cmp::Ordering::Equal
    );
}

/// `affinities_of` and `apply_comparison` agree about what a column
/// contributes: a column the map knows about has the affinity the map says, and
/// one it does not know about contributes nothing.
///
/// The map is built from resolved references rather than from names, so the two
/// are keyed the same way the evaluator keys a value -- and this is the check
/// that the key really is the offset, since the two halves have to meet there.
#[test]
fn the_affinity_map_and_the_comparison_rule_use_the_same_key() {
    // A FROM with one table of one INTEGER column, built the way the executor
    // builds it, and a reference resolved against it.
    let table = crate::catalog::Table {
        name: "t".into(),
        columns: vec![crate::catalog::Column {
            name: "x".into(),
            declared_type: "INTEGER".into(),
            affinity: Affinity::Integer,
            not_null: false,
            default: None,
            rowid_alias: false,
        }],
        rowid_alias: None,
        without_rowid: false,
        root_page: 0,
    };
    let source = crate::join::Source {
        name: "t".into(),
        table,
        join: None,
        on: None,
        using: Vec::new(),
    };
    let from = crate::join::resolve(vec![source]).expect("one source is legal");
    let r#ref = crate::join::resolve_ref(&from, Some("t"), "x", false).expect("x is a column of t");
    let bound = vec![crate::join::Bound { at: 42, r#ref }];
    let map = affinities_of(&from, &bound);
    assert_eq!(map.get(&42), Some(&Affinity::Integer));
    assert_eq!(
        map.get(&7),
        None,
        "an offset with no reference has no affinity"
    );
    // The value the map says is the one the rule then applies to the other
    // operand: with the column on the left and a text on the right, the text is
    // parsed, which is rule 1.
    let (l, rr) = apply_comparison(&i(5), map.get(&42).copied(), &t("5"), None);
    assert_eq!((l, rr), (i(5), i(5)));
}
