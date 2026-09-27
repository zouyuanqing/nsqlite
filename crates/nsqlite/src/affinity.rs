//! Type affinity, which decides how a declared type is interpreted.
//!
//! SQLite's five affinities are derived from the declared type's text rather
//! than matched exactly, and the rules are ordered: the first that applies
//! wins. A type name may also carry a length, as `VARCHAR(10)`, and the
//! parentheses do not change the affinity.
//!
//! This matters beyond storage. Affinity converts a value on its way into a
//! table, which is why `INSERT INTO t VALUES('123')` into an INTEGER column
//! stores the integer 123, and why a column declared `TEXT` turns the number
//! 123 into the string '123' when a comparison has to compare them.

use std::fmt;

/// A column's affinity, as derived from its declared type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Affinity {
    /// No conversion: a value keeps whatever type it has.
    Blob,
    /// Values are converted to text.
    Text,
    /// Values that look numeric are converted to a number, and numbers that do
    /// not look like integers keep a real part.
    Numeric,
    /// Values are converted to an integer where that is lossless.
    Integer,
    /// Values are converted to a real number.
    Real,
}

impl Affinity {
    /// The name SQLite uses in `PRAGMA table_info`, which is the type name
    /// with its length removed.
    pub fn name(self) -> &'static str {
        match self {
            Affinity::Blob => "BLOB",
            Affinity::Text => "TEXT",
            Affinity::Numeric => "NUMERIC",
            Affinity::Integer => "INTEGER",
            Affinity::Real => "REAL",
        }
    }
}

impl fmt::Display for Affinity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Derives the affinity of a declared type.
///
/// The order is the one in SQLite's documentation, and it is not the order the
/// names suggest: `INT` is checked before `CHAR` only in the sense that
/// `VARCHAR(20)` contains no `INT`, while `CHARINT` would, because the rules
/// test for substrings rather than words.
pub fn affinity_of(declared: &str) -> Affinity {
    // The declared type is case-insensitive, and a length in parentheses is
    // not part of the name for this purpose.
    let ty = declared.to_ascii_uppercase();
    let ty = strip_length(&ty);

    // Rule 5, checked last as the fallback but first in SQLite's own ordering
    // for the name it matches. The documentation numbers the rules in the
    // opposite order from the code, so the order below is the one the code
    // uses.
    if ty.contains("INT") {
        Affinity::Integer
    } else if ty.contains("CHAR") || ty.contains("CLOB") || ty.contains("TEXT") {
        Affinity::Text
    } else if ty.is_empty() || ty.contains("BLOB") {
        // An empty declared type means BLOB, which is the "no affinity" case:
        // nothing is converted.
        Affinity::Blob
    } else if ty.contains("REAL") || ty.contains("FLOA") || ty.contains("DOUB") {
        Affinity::Real
    } else {
        // Everything else is NUMERIC.
        Affinity::Numeric
    }
}

/// Removes a trailing parenthesised length, keeping any text before it.
fn strip_length(ty: &str) -> String {
    match ty.find('(') {
        Some(at) => ty[..at].trim_end().to_string(),
        None => ty.to_string(),
    }
}

/// Applies a column's affinity to a value, as SQLite does on insert.
///
/// The conversion is deliberately conservative: it never loses information, so
/// a text that does not look entirely like a number is left as text rather than
/// becoming a truncated one. That is what makes `INSERT INTO t VALUES('12abc')`
/// into an INTEGER column store the string, not the number 12.
pub fn apply(value: &crate::value::Value, affinity: Affinity) -> crate::value::Value {
    use crate::value::Value;
    match affinity {
        Affinity::Blob => value.clone(),
        Affinity::Text => match value {
            Value::Integer(i) => Value::Text(i.to_string()),
            Value::Real(r) => Value::Text(format_real(*r)),
            other => other.clone(),
        },
        Affinity::Integer | Affinity::Numeric | Affinity::Real => match value {
            Value::Text(s) => match text_to_number(s) {
                Some(n) => match affinity {
                    Affinity::Real => Value::real(n.as_f64()),
                    _ => n.value(),
                },
                _ => value.clone(),
            },
            Value::Real(r) if affinity == Affinity::Integer => {
                // A real converts to an integer only when nothing is lost.
                match lossless_int(*r) {
                    Some(i) => Value::Integer(i),
                    None => value.clone(),
                }
            }
            Value::Real(r) if affinity == Affinity::Numeric => {
                // NUMERIC keeps the real part when one is present and otherwise
                // narrows to an integer, which is what makes a NUMERIC column
                // hold integers for whole values.
                match lossless_int(*r) {
                    Some(i) => Value::Integer(i),
                    None => value.clone(),
                }
            }
            // A REAL column holds a real even for a whole number: verified
            // against sqlite3, where typeof is 'real' for an integer inserted
            // into a REAL column.
            Value::Integer(i) if affinity == Affinity::Real => Value::real(*i as f64),
            other => other.clone(),
        },
    }
}

/// The integer a real converts to, or `None` when that would lose something.
///
/// SQLite does not test `r.fract() == 0.0`; it narrows and asks whether
/// converting back reproduces the real. That is a looser test than "has no
/// fractional part", and the difference is visible: every `f64` at or above
/// 2^53 is a whole number with a fraction below the ulp, so `fract() == 0.0`
/// is true for all of them -- but not all of them fit an `i64`, and the ones
/// that do not must stay real. Verified against sqlite3 3.53.4:
///
/// ```text
/// INSERT INTO t VALUES('9223372036854774784.0');  -- integer
/// INSERT INTO t VALUES('9223372036854775807.0');  -- real 9.2233720368547758e+18
/// ```
///
/// The `2^53` boundary is where the round trip starts to matter: below it an
/// `f64` that looks whole always is one, and above it a real may be whole in
/// appearance and still not name an `i64`.
fn lossless_int(r: f64) -> Option<i64> {
    if !r.is_finite() || r < i64::MIN as f64 || r >= -(i64::MIN as f64) {
        return None;
    }
    let i = r as i64;
    (i as f64 == r).then_some(i)
}

/// A number parsed from text, remembering whether it named an integer exactly,
/// so a REAL-affinity column can render `5.0` where an INTEGER would render `5`.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Number {
    integer: Option<i64>,
    real: f64,
}

/// Parses text as a number, or returns `None` if it is not entirely numeric.
///
/// SQLite requires the whole string to be a well-formed number; a leading or
/// trailing non-numeric character leaves the value as text.
fn text_to_number(s: &str) -> Option<Number> {
    let t = s.trim();
    if t.is_empty() {
        return None;
    }
    // A hex literal is not a number in this context, and affinity agrees:
    // sqlite3 leaves the text '0x10' alone in an INTEGER column, so the branch
    // that used to parse it as 16 contradicts the comment above it and the
    // engine. Verified against sqlite3 3.53.4 -- `typeof` is 'text' for every
    // affinity when the value is the string '0x10'. The 0x form is a *literal*
    // spellings rule, applied by the parser, not an affinity rule.
    // Reject anything Rust would accept but SQLite would not, so the two
    // parsers agree.
    if t.chars()
        .any(|c| matches!(c, 'n' | 'N' | 'i' | 'I' | 'x' | 'X' | '_'))
    {
        return None;
    }
    if let Ok(i) = t.parse::<i64>() {
        return Some(Number {
            integer: Some(i),
            real: i as f64,
        });
    }
    // A spelling with a fraction or an exponent that lands on a whole number is
    // still an integer where the round trip is exact: sqlite3 stores '5.0',
    // '-5.0' and '1e3' as integers in a NUMERIC column. This is the text
    // counterpart of `lossless_int`, and it has to be tried after the `i64`
    // parse so a plain '5' keeps its exact integer.
    let f = t.parse::<f64>().ok()?;
    if !f.is_finite() {
        // 'inf' and 'nan' are text to sqlite3, not numbers. The character
        // filter above already rejects the spellings that reach here as whole
        // words, so this only catches a numeric overflow such as '1e400'.
        return None;
    }
    Some(Number {
        integer: lossless_int(f),
        real: f,
    })
}

impl Number {
    /// The value, as an integer when it parsed as one and a real otherwise.
    fn value(self) -> crate::value::Value {
        match self.integer {
            Some(i) => crate::value::Value::Integer(i),
            None => crate::value::Value::real(self.real),
        }
    }

    /// The value as a real, which is what a REAL column stores even for a whole
    /// number.
    fn as_f64(self) -> f64 {
        self.real
    }
}

/// Renders a real the way SQLite renders it as text, which is what a TEXT
/// affinity conversion produces.
///
/// This is [`crate::value::format_real`], which is SQLite's own rendering and
/// was measured against it; the copy that used to live here formatted to
/// fifteen places and trimmed, which is a different function and disagreed with
/// sqlite3 on every real outside the narrow range where the two agree. The
/// disagreement was not only cosmetic. `CREATE TABLE t(a TEXT); INSERT INTO t
/// VALUES(5.0)` has to store the text `'5.0'` and not `'5'`, because that text
/// is what a comparison converts back: `SELECT a = 5.0 FROM t` is 0 in sqlite3
/// and 1 if the stored text is `'5'`, since 5.0 converts with the column's TEXT
/// affinity to `'5.0'` and `'5'` is not `'5.0'`. A wrong rendering here is a
/// wrong answer to a comparison, not only a wrong display.
///
/// Measured against sqlite3 3.53.4, which is the source of every case:
///
/// | input | sqlite3 | this |
/// |---|---|---|
/// | `5.0` | `'5.0'` | `'5.0'` |
/// | `2.0` | `'2.0'` | `'2.0'` |
/// | `100.0` | `'100.0'` | `'100.0'` |
/// | `-0.0` | `'0.0'` | `'0.0'` |
/// | `1e20` | `'1.0e+20'` | `'1.0e+20'` |
/// | `1e-20` | `'1.0e-20'` | `'1.0e-20'` |
/// | `1e15` | `'1000000000000000.0'` | `'1000000000000000.0'` |
fn format_real(r: f64) -> String {
    crate::value::format_real(r)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::Value;

    #[test]
    fn affinity_follows_substrings_not_words() {
        // Each of these is what the documentation gives as an example, including
        // the ones that only match because of a substring.
        assert_eq!(affinity_of("INTEGER"), Affinity::Integer);
        assert_eq!(affinity_of("INT"), Affinity::Integer);
        assert_eq!(affinity_of("BIGINT"), Affinity::Integer);
        assert_eq!(affinity_of("UNSIGNED BIG INT"), Affinity::Integer);
        assert_eq!(affinity_of("CHARACTER"), Affinity::Text);
        assert_eq!(affinity_of("VARCHAR(255)"), Affinity::Text);
        assert_eq!(affinity_of("VARYING CHARACTER(255)"), Affinity::Text);
        assert_eq!(affinity_of("NCHAR"), Affinity::Text);
        assert_eq!(affinity_of("NATIVE CHARACTER(70)"), Affinity::Text);
        assert_eq!(affinity_of("NVARCHAR(100)"), Affinity::Text);
        assert_eq!(affinity_of("TEXT"), Affinity::Text);
        assert_eq!(affinity_of("CLOB"), Affinity::Text);
        assert_eq!(affinity_of("BLOB"), Affinity::Blob);
        assert_eq!(affinity_of("REAL"), Affinity::Real);
        assert_eq!(affinity_of("DOUBLE"), Affinity::Real);
        assert_eq!(affinity_of("DOUBLE PRECISION"), Affinity::Real);
        assert_eq!(affinity_of("FLOAT"), Affinity::Real);
        assert_eq!(affinity_of("NUMERIC"), Affinity::Numeric);
        assert_eq!(affinity_of("DECIMAL(10,5)"), Affinity::Numeric);
        assert_eq!(affinity_of("BOOLEAN"), Affinity::Numeric);
        // An undeclared type is BLOB, which converts nothing.
        assert_eq!(affinity_of(""), Affinity::Blob);
        // Anything else is NUMERIC.
        assert_eq!(affinity_of("MONEY"), Affinity::Numeric);
        assert_eq!(affinity_of("DATE"), Affinity::Numeric);
    }

    #[test]
    fn affinity_is_case_insensitive() {
        assert_eq!(affinity_of("integer"), affinity_of("INTEGER"));
        assert_eq!(affinity_of("vArChAr(10)"), Affinity::Text);
    }

    #[test]
    fn text_affinity_turns_numbers_into_text() {
        assert_eq!(
            apply(&Value::Integer(123), Affinity::Text),
            Value::Text("123".into())
        );
        assert_eq!(
            apply(&Value::real(1.5), Affinity::Text),
            Value::Text("1.5".into())
        );
        assert_eq!(
            apply(&Value::real(2.0), Affinity::Text),
            Value::Text("2.0".into())
        );
        // Text and blobs are already what they are.
        assert_eq!(
            apply(&Value::Text("x".into()), Affinity::Text),
            Value::Text("x".into())
        );
        assert_eq!(
            apply(&Value::Blob(vec![1]), Affinity::Text),
            Value::Blob(vec![1])
        );
        assert_eq!(apply(&Value::Null, Affinity::Text), Value::Null);
    }

    #[test]
    fn integer_affinity_converts_only_whole_numeric_text() {
        assert_eq!(
            apply(&Value::Text("123".into()), Affinity::Integer),
            Value::Integer(123)
        );
        assert_eq!(
            apply(&Value::Text("-5".into()), Affinity::Integer),
            Value::Integer(-5)
        );
        // A fractional value is kept, as a real: sqlite3 stores 1.5 in an
        // INTEGER column as a real rather than rounding or leaving text.
        assert_eq!(
            apply(&Value::Text("1.5".into()), Affinity::Integer),
            Value::real(1.5)
        );
        // So does anything that is not entirely a number.
        assert_eq!(
            apply(&Value::Text("12abc".into()), Affinity::Integer),
            Value::Text("12abc".into())
        );
        assert_eq!(
            apply(&Value::Text("".into()), Affinity::Integer),
            Value::Text("".into())
        );
        assert_eq!(
            apply(&Value::Text(" 42 ".into()), Affinity::Integer),
            Value::Integer(42)
        );
    }

    #[test]
    fn integer_affinity_narrows_a_whole_real() {
        assert_eq!(
            apply(&Value::real(3.0), Affinity::Integer),
            Value::Integer(3)
        );
        assert_eq!(
            apply(&Value::real(3.5), Affinity::Integer),
            Value::real(3.5)
        );
    }

    #[test]
    fn numeric_affinity_keeps_a_fractional_part() {
        assert_eq!(
            apply(&Value::Text("1".into()), Affinity::Numeric),
            Value::Integer(1)
        );
        assert_eq!(
            apply(&Value::Text("1.5".into()), Affinity::Numeric),
            Value::real(1.5)
        );
        assert_eq!(
            apply(&Value::Text("1e3".into()), Affinity::Numeric),
            Value::Integer(1000)
        );
        // NUMERIC narrows a whole real, which INTEGER also does.
        assert_eq!(
            apply(&Value::real(4.0), Affinity::Numeric),
            Value::Integer(4)
        );
        assert_eq!(
            apply(&Value::real(4.5), Affinity::Numeric),
            Value::real(4.5)
        );
    }

    /// A whole real narrows to an integer when the `f64` -> `i64` -> `f64`
    /// round trip is exact, which is a different test from "has no fractional
    /// part" once the real is large enough that its ulp exceeds 1.
    ///
    /// The spellings are taken from sqlite3 3.53.4, where each of these was
    /// inserted into a NUMERIC column and `typeof` read back.
    #[test]
    fn numeric_affinity_narrows_on_exactness_not_on_frac() {
        // The ones that round trip: every spelling of a whole number becomes an
        // integer, fraction, exponent, sign, padding or not. The expected value
        // is spelled out rather than parsed, so the test states the answer
        // instead of re-running the parser it is checking.
        for (s, want) in [
            ("5.0", 5i64),
            ("-5.0", -5),
            ("5.00", 5),
            ("5.", 5),
            (".0", 0),
            ("0.0", 0),
            ("5e0", 5),
            ("1e3", 1000),
            (" 5.0 ", 5),
            ("+5.0", 5),
            ("0", 0),
        ] {
            assert_eq!(
                apply(&Value::Text(s.into()), Affinity::Numeric),
                Value::Integer(want),
                "{s:?}"
            );
            assert_eq!(
                apply(&Value::Text(s.into()), Affinity::Integer),
                Value::Integer(want),
                "{s:?}"
            );
        }
        // A real that does not fit an i64 stays real even though it is whole in
        // appearance: 9223372036854775807 as an f64 is 2^63, one past i64::MAX.
        let past_max = 9223372036854775807.0f64;
        assert_eq!(
            apply(&Value::real(past_max), Affinity::Numeric),
            Value::real(past_max)
        );
        assert_eq!(
            apply(&Value::real(past_max), Affinity::Integer),
            Value::real(past_max)
        );
        // The largest double that does name an i64 does narrow.
        let fits = 9223372036854774784.0f64;
        assert_eq!(
            apply(
                &Value::Text("9223372036854774784.0".into()),
                Affinity::Numeric
            ),
            Value::Integer(9223372036854774784)
        );
        assert_eq!(
            apply(&Value::real(fits), Affinity::Numeric),
            Value::Integer(9223372036854774784)
        );
        // A fraction stays a fraction.
        for a in [Affinity::Numeric, Affinity::Integer] {
            assert_eq!(apply(&Value::Text("0.5".into()), a), Value::real(0.5));
            assert_eq!(apply(&Value::Text("-0.5".into()), a), Value::real(-0.5));
            assert_eq!(apply(&Value::real(0.5), a), Value::real(0.5));
        }
        // An infinite real is not an integer. sqlite3 renders an out-of-range
        // exponent as Inf and keeps it real.
        assert_eq!(
            apply(&Value::real(f64::INFINITY), Affinity::Numeric),
            Value::real(f64::INFINITY)
        );
    }

    /// The `0x` form is a literal spelling, not an affinity rule: sqlite3
    /// leaves the *string* `0x10` as text in an INTEGER column. Verified
    /// against sqlite3 3.53.4, where `typeof` is `text` for all four
    /// affinities.
    #[test]
    fn affinity_leaves_a_hex_spelled_string_as_text() {
        for a in [
            Affinity::Integer,
            Affinity::Numeric,
            Affinity::Real,
            Affinity::Text,
            Affinity::Blob,
        ] {
            assert_eq!(
                apply(&Value::Text("0x10".into()), a),
                Value::Text("0x10".into()),
                "{a}"
            );
        }
    }

    /// Text that overflows a real is text, not `Inf`: sqlite3 keeps `1e400` as
    /// text in a NUMERIC column rather than converting it to an infinity.
    #[test]
    fn affinity_leaves_an_unrepresentable_real_as_text() {
        assert_eq!(
            apply(&Value::Text("1e400".into()), Affinity::Numeric),
            Value::Text("1e400".into())
        );
    }

    #[test]
    fn real_affinity_holds_a_real_even_for_a_whole_number() {
        assert_eq!(
            apply(&Value::Text("1".into()), Affinity::Real),
            Value::real(1.0)
        );
        assert_eq!(
            apply(&Value::Text("1.5".into()), Affinity::Real),
            Value::real(1.5)
        );
        // An integer inserted into a REAL column comes back as a real, which
        // is what typeof reports in sqlite3.
        assert_eq!(apply(&Value::Integer(3), Affinity::Real), Value::real(3.0));
        // Non-numeric text is not a real, and a blob is never converted.
        assert_eq!(
            apply(&Value::Text("12abc".into()), Affinity::Real),
            Value::Text("12abc".into())
        );
        assert_eq!(
            apply(&Value::Blob(vec![1]), Affinity::Real),
            Value::Blob(vec![1])
        );
    }

    #[test]
    fn blob_affinity_converts_nothing() {
        for v in [
            Value::Integer(1),
            Value::real(1.5),
            Value::Text("1".into()),
            Value::Blob(vec![1]),
            Value::Null,
        ] {
            assert_eq!(apply(&v, Affinity::Blob), v);
        }
    }

    #[test]
    fn a_non_numeric_string_is_never_truncated_to_a_number() {
        // The failure this prevents: '12abc' becoming 12.
        for aff in [Affinity::Integer, Affinity::Numeric, Affinity::Real] {
            assert_eq!(
                apply(&Value::Text("12abc".into()), aff),
                Value::Text("12abc".into()),
                "{aff} truncated a non-numeric string"
            );
        }
    }

    #[test]
    fn a_null_stays_null_under_every_affinity() {
        for aff in [
            Affinity::Blob,
            Affinity::Text,
            Affinity::Numeric,
            Affinity::Integer,
            Affinity::Real,
        ] {
            assert_eq!(apply(&Value::Null, aff), Value::Null);
        }
    }
}
