//! Runtime values: the storage classes SQLite records can hold.

use std::cmp::Ordering;
use std::fmt;

/// SQLite's storage classes, in the order their type codes sort.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Datatype {
    Null = 0,
    Integer = 1,
    Real = 2,
    Text = 3,
    Blob = 4,
}

impl Datatype {
    pub fn name(self) -> &'static str {
        match self {
            Datatype::Null => "null",
            Datatype::Integer => "integer",
            Datatype::Real => "real",
            Datatype::Text => "text",
            Datatype::Blob => "blob",
        }
    }
}

/// A value as stored in a record.
///
/// Floating-point NaN is normalised on construction: the file format reserves
/// one NaN bit pattern as the canonical null-like NaN, and comparisons treat
/// every NaN as larger than every other value, matching SQLite.
#[derive(Debug, Clone)]
pub enum Value {
    Null,
    Integer(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

impl Value {
    /// Builds a `Real`, mapping any NaN to the canonical NaN.
    pub fn real(v: f64) -> Value {
        if v.is_nan() {
            Value::Real(f64::NAN)
        } else {
            Value::Real(v)
        }
    }

    pub fn datatype(&self) -> Datatype {
        match self {
            Value::Null => Datatype::Null,
            Value::Integer(_) => Datatype::Integer,
            Value::Real(_) => Datatype::Real,
            Value::Text(_) => Datatype::Text,
            Value::Blob(_) => Datatype::Blob,
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Integer(i) => Some(*i),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Integer(i) => Some(*i as f64),
            Value::Real(r) => Some(*r),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Text(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_blob(&self) -> Option<&[u8]> {
        match self {
            Value::Blob(b) => Some(b),
            _ => None,
        }
    }

    /// SQLite's SQL comparison semantics, as `sqlite3MemCompare` implements
    /// them. Storage classes order NULL < numbers < text < blob, but two
    /// numbers of different classes compare *numerically*, so the integer 1
    /// equals the real 1.0. NaN is larger than every other number, and text
    /// compares as its UTF-8 bytes.
    pub fn compare(&self, other: &Value) -> Ordering {
        use Ordering::*;
        use Value::*;
        // Integers and reals share a sort group because SQLite compares them
        // numerically; only a genuine group difference orders by class.
        if self.sort_group() != other.sort_group() {
            return self.sort_group().cmp(&other.sort_group());
        }
        match (self, other) {
            (Null, Null) => Equal,
            (Integer(x), Integer(y)) => x.cmp(y),
            (Real(x), Real(y)) => cmp_real(*x, *y),
            (Integer(x), Real(y)) => cmp_int_real(*x, *y),
            (Real(x), Integer(y)) => cmp_int_real(*y, *x).reverse(),
            (Text(x), Text(y)) => x.as_bytes().cmp(y.as_bytes()),
            (Blob(x), Blob(y)) => x.cmp(y),
            (Null, _) => Less,
            (_, Null) => Greater,
            // Cross-class pairs the group guard already rejected.
            _ => self.sort_group().cmp(&other.sort_group()),
        }
    }

    /// The class group used for ordering: NULL, then the two numeric classes,
    /// then text, then blobs. Integers and reals are deliberately one group.
    fn sort_group(&self) -> u8 {
        use Datatype::*;
        match self.datatype() {
            Null => 0,
            Integer | Real => 1,
            Text => 2,
            Blob => 3,
        }
    }

    /// The ordering b-tree index keys use. It agrees with [`Value::compare`]
    /// except that the integer 1 and the real 1.0 occupy distinct cells, with
    /// the integer first. That is what lets an index hold both spellings of one
    /// number while a range scan still finds either.
    pub fn sort_key_cmp(&self, other: &Value) -> Ordering {
        use Ordering::*;
        use Value::*;
        match (self, other) {
            (Integer(_), Real(_)) => match cmp_int_real(
                self.as_i64().unwrap_or(0),
                other.as_f64().unwrap_or(f64::NAN),
            ) {
                Equal => Less,
                other => other,
            },
            (Real(_), Integer(_)) => {
                match cmp_int_real(
                    other.as_i64().unwrap_or(0),
                    self.as_f64().unwrap_or(f64::NAN),
                ) {
                    Equal => Greater,
                    other => other.reverse(),
                }
            }
            _ => self.compare(other),
        }
    }

    /// Equality under SQLite's comparison rules, where numeric values compare
    /// across the integer/real boundary and text is compared as bytes.
    pub fn eq_value(&self, other: &Value) -> bool {
        self.compare(other) == Ordering::Equal
    }
}

/// Compares two reals with NaN treated as larger than everything, and as equal
/// to itself.
fn cmp_real(x: f64, y: f64) -> Ordering {
    use Ordering::*;
    match (x.is_nan(), y.is_nan()) {
        (true, true) => Equal,
        (true, false) => Greater,
        (false, true) => Less,
        (false, false) => x.partial_cmp(&y).unwrap_or(Equal),
    }
}

/// Compares an integer against a real by value, without letting the f64
/// conversion round two distinct large integers onto the same real.
fn cmp_int_real(i: i64, r: f64) -> Ordering {
    use Ordering::*;
    if r.is_nan() {
        return Less;
    }
    // 2^63 as an f64 bound: at or above it the real is outside i64's range, and
    // -2^63 is exactly i64::MIN, so both ends need explicit handling.
    const TWO_POW_63: f64 = 9_223_372_036_854_775_808.0;
    if r == -TWO_POW_63 {
        return i.cmp(&i64::MIN);
    }
    if r > -TWO_POW_63 && r < TWO_POW_63 {
        (i as f64).partial_cmp(&r).unwrap_or(Equal)
    } else if r >= TWO_POW_63 {
        Less
    } else {
        Greater
    }
}

impl PartialEq for Value {
    fn eq(&self, other: &Value) -> bool {
        self.eq_value(other)
    }
}

impl Eq for Value {}

impl From<i64> for Value {
    fn from(v: i64) -> Value {
        Value::Integer(v)
    }
}

impl From<i32> for Value {
    fn from(v: i32) -> Value {
        Value::Integer(v as i64)
    }
}

impl From<bool> for Value {
    fn from(v: bool) -> Value {
        Value::Integer(v as i64)
    }
}

impl From<f64> for Value {
    fn from(v: f64) -> Value {
        Value::real(v)
    }
}

impl From<&str> for Value {
    fn from(v: &str) -> Value {
        Value::Text(v.to_owned())
    }
}

impl From<String> for Value {
    fn from(v: String) -> Value {
        Value::Text(v)
    }
}

impl From<Vec<u8>> for Value {
    fn from(v: Vec<u8>) -> Value {
        Value::Blob(v)
    }
}

impl fmt::Display for Value {
    /// Renders the value the way the CLI prints it.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => f.write_str(""),
            Value::Integer(i) => write!(f, "{i}"),
            Value::Real(r) => {
                if r.is_infinite() {
                    f.write_str(if *r < 0.0 { "-Inf" } else { "Inf" })
                } else {
                    // 15 significant digits round-trips an f64 while reading
                    // like the CLI's default output.
                    let s = format!("{r:.15}");
                    let s = s.trim_end_matches('0').trim_end_matches('.');
                    f.write_str(if s.is_empty() || s == "-" { "0" } else { s })
                }
            }
            Value::Text(s) => f.write_str(s),
            Value::Blob(b) => write!(f, "x'{}'", hex(b)),
        }
    }
}

fn hex(b: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789ABCDEF";
    let mut s = String::with_capacity(b.len() * 2);
    for &byte in b {
        s.push(DIGITS[(byte >> 4) as usize] as char);
        s.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cmp::Ordering::*;

    #[test]
    fn storage_classes_sort_in_declaration_order() {
        let mut vals = vec![
            Value::Blob(vec![0]),
            Value::Text("a".into()),
            Value::Real(1.0),
            Value::Integer(1),
            Value::Null,
        ];
        vals.sort_by(|a, b| a.sort_key_cmp(b));
        assert_eq!(
            vals,
            vec![
                Value::Null,
                Value::Integer(1),
                Value::Real(1.0),
                Value::Text("a".into()),
                Value::Blob(vec![0]),
            ]
        );
    }

    #[test]
    fn sort_keys_separate_int_one_from_real_one() {
        // They compare equal as SQL values but need distinct index cells, with
        // the integer spelling first.
        assert_eq!(Value::Integer(1).compare(&Value::Real(1.0)), Equal);
        assert_eq!(Value::Integer(1).sort_key_cmp(&Value::Real(1.0)), Less);
        assert_eq!(Value::Real(1.0).sort_key_cmp(&Value::Integer(1)), Greater);
        assert_eq!(Value::Integer(2).sort_key_cmp(&Value::Real(1.0)), Greater);
    }

    #[test]
    fn sort_keys_keep_classes_apart() {
        assert_eq!(
            Value::Text("".into()).sort_key_cmp(&Value::Blob(vec![])),
            Less
        );
        assert_eq!(Value::Null.sort_key_cmp(&Value::Integer(i64::MIN)), Less);
        assert_eq!(
            Value::Real(1.5).sort_key_cmp(&Value::Text("1.5".into())),
            Less
        );
    }

    #[test]
    fn nan_is_above_every_real_below_any_blob() {
        assert_eq!(Value::Real(f64::NAN).compare(&Value::Real(1e300)), Greater);
        assert_eq!(Value::Real(f64::NAN).compare(&Value::Real(f64::NAN)), Equal);
        assert_eq!(Value::Real(f64::NAN).compare(&Value::Blob(vec![])), Less);
        assert_eq!(
            Value::Real(f64::NAN).sort_key_cmp(&Value::Integer(i64::MAX)),
            Greater
        );
    }

    #[test]
    fn integers_and_reals_compare_across_the_boundary() {
        assert_eq!(Value::Integer(1).compare(&Value::Real(1.0)), Equal);
        assert_eq!(Value::Integer(1).compare(&Value::Real(1.5)), Less);
        assert_eq!(Value::Real(0.5).compare(&Value::Integer(1)), Less);
        // 2^63 is not representable as i64, so the comparison must not round.
        assert_eq!(
            Value::Real(9_223_372_036_854_775_808.0).compare(&Value::Integer(i64::MAX)),
            Greater
        );
        assert_eq!(
            Value::Integer(i64::MIN).compare(&Value::Real(-9_223_372_036_854_775_808.0)),
            Equal
        );
    }

    #[test]
    fn nan_sorts_above_every_real() {
        assert_eq!(Value::Real(f64::NAN).compare(&Value::Real(1e300)), Greater);
        assert_eq!(Value::Real(1e300).compare(&Value::Real(f64::NAN)), Less);
        assert_eq!(Value::Real(f64::NAN).compare(&Value::Real(f64::NAN)), Equal);
        assert_eq!(Value::Integer(1).compare(&Value::Real(f64::NAN)), Less);
    }

    #[test]
    fn text_compares_as_bytes_not_as_unicode_scalars() {
        // U+00E9 encodes as 0xC3 0xA9, so it sorts after ASCII 'z' (0x7A).
        assert_eq!(
            Value::Text("z".into()).compare(&Value::Text("é".into())),
            Less
        );
    }

    #[test]
    fn display_matches_cli_conventions() {
        assert_eq!(Value::Real(1.0).to_string(), "1");
        assert_eq!(Value::Real(1.5).to_string(), "1.5");
        assert_eq!(Value::Real(f64::INFINITY).to_string(), "Inf");
        assert_eq!(Value::Blob(vec![0xde, 0xad]).to_string(), "x'DEAD'");
        assert_eq!(Value::Null.to_string(), "");
    }
}
