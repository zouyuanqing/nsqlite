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

    /// The value as bytes, so that two values produce the same bytes exactly
    /// when SQLite counts them as the same value.
    ///
    /// This is what tells one group from another and what a DISTINCT aggregate
    /// uses to decide it has already seen a value, so it must be **injective**:
    /// nothing two different values both encode to. It must also *merge* what
    /// SQLite merges, which is the other half and the reason the display string
    /// cannot be used — it is not injective. Checked against sqlite3 3.53.4,
    /// which keeps every one of these apart or together as follows:
    ///
    /// | values | one identity? |
    /// |---|---|
    /// | `NULL`, `''` | no — both *display* as the empty string |
    /// | `1`, `1.0` | yes — one number, two storage classes |
    /// | `-0.0`, `0.0`, `0` | yes — one number |
    /// | `1`, `'1'`, `x'31'` | no — all three *display* as `1` |
    /// | `9007199254740993`, `9007199254740992.0` | no — distinct numbers |
    ///
    /// The numeric classes encode by **exact value**, not by bytes: an integer
    /// is itself, and a real is the integer significand and exponent the
    /// standard gives it, so `1` and `1.0` agree and the two spellings of a
    /// number that are genuinely different do not. The exponent normalisation
    /// also folds `-0.0` onto `0.0` and the integer `0`.
    ///
    /// Text and blob are length-delimited, so a key cannot be read two ways,
    /// and the leading byte is the storage class, so `NULL` and `''` cannot
    /// collide. NaN and the two infinities are reserved exponent values no
    /// finite number reaches, which is what keeps them their own identities:
    /// every NaN is one value, and `+Inf` and `-Inf` are two.
    pub fn identity_key(&self) -> Vec<u8> {
        match self {
            Value::Null => vec![CLASS_NULL],
            Value::Integer(i) => numeric_key(*i as i128, 0),
            Value::Real(r) => real_key(*r),
            Value::Text(s) => {
                let mut out = vec![CLASS_TEXT];
                out.extend_from_slice(&(s.len() as u64).to_be_bytes());
                out.extend_from_slice(s.as_bytes());
                out
            }
            Value::Blob(b) => {
                let mut out = vec![CLASS_BLOB];
                out.extend_from_slice(&(b.len() as u64).to_be_bytes());
                out.extend_from_slice(b);
                out
            }
        }
    }
}

/// A composite identity, which is what a multi-column GROUP BY key is.
///
/// Concatenating the parts is unambiguous because every part is either
/// fixed-width or length-delimited, so no two different lists can produce the
/// same bytes — `('a','b')` and `('ab')` differ, and both differ from `('a')`.
pub fn identity_keys(values: &[Value]) -> Vec<u8> {
    let mut out = Vec::with_capacity(values.len() * 9);
    for v in values {
        out.extend_from_slice(&v.identity_key());
    }
    out
}

/// The leading byte of an [`Value::identity_key`]: the storage class, so that
/// values of different classes can never share a key.
const CLASS_NULL: u8 = b'n';
const CLASS_NUMBER: u8 = b'#';
const CLASS_TEXT: u8 = b't';
const CLASS_BLOB: u8 = b'b';

/// Exponent values that stand for the three reals no integer significand
/// produces, and so no finite number can collide with. A finite real's
/// exponent is in `-971..=1074`, so all three are far out of range.
const EXP_NAN: i64 = i64::MAX;
const EXP_POS_INF: i64 = i64::MAX - 1;
const EXP_NEG_INF: i64 = i64::MAX - 2;

/// The key of a number written as `mantissa * 2^-exponent`.
///
/// The two are normalised together, so the pair is the same for two values
/// exactly when they are the same real number. Normalisation divides the
/// mantissa down to odd (or leaves zero at zero) and pays for each halving in
/// the exponent, which puts `1`, `1.0` and `0.5 * 2` on one key and leaves
/// `9007199254740993` and `9007199254740992.0` on different ones.
fn numeric_key(mut mantissa: i128, mut exponent: i64) -> Vec<u8> {
    // Zero has no odd normal form — every halving leaves it zero — so it is
    // pinned to a single key. That is also what folds `-0.0` onto `0.0` and
    // the integer `0`, which SQLite counts as one value.
    if mantissa == 0 {
        return numeric_bytes(0, 0);
    }
    while mantissa % 2 == 0 {
        mantissa /= 2;
        exponent -= 1;
    }
    numeric_bytes(mantissa, exponent)
}

/// The key of a real, decomposed into the significand and exponent the IEEE-754
/// representation already gives it.
fn real_key(r: f64) -> Vec<u8> {
    if r.is_nan() {
        return numeric_bytes(0, EXP_NAN);
    }
    if r == f64::INFINITY {
        return numeric_bytes(0, EXP_POS_INF);
    }
    if r == f64::NEG_INFINITY {
        return numeric_bytes(0, EXP_NEG_INF);
    }
    let bits = r.to_bits();
    // The sign is the top bit; the fraction is the low 52.
    let negative = bits >> 63 == 1;
    let fraction = bits & 0x000f_ffff_ffff_ffff;
    let biased = ((bits >> 52) & 0x7ff) as i64;
    // A biased exponent of 0 is zero or subnormal: no implicit leading 1, and
    // the exponent is pinned to the smallest, 2^-1074.
    let (mantissa, exponent) = if biased == 0 {
        (fraction, 1074)
    } else {
        // Otherwise the significand carries an implicit leading 1 and the
        // unbiased exponent is `biased - 1023`, so the value is
        // `significand * 2^(biased - 1075)` = `significand / 2^(1075 - biased)`.
        (fraction | (1 << 52), 1075 - biased)
    };
    numeric_key(
        if negative {
            -(mantissa as i128)
        } else {
            mantissa as i128
        },
        exponent,
    )
}

/// A number key: the class byte, then the two fixed-width halves of the
/// normalised form. Fixed width is what makes it unambiguous.
fn numeric_bytes(mantissa: i128, exponent: i64) -> Vec<u8> {
    let mut out = Vec::with_capacity(CLASS_NUMBER as usize + 25);
    out.push(CLASS_NUMBER);
    out.extend_from_slice(&mantissa.to_be_bytes());
    out.extend_from_slice(&exponent.to_be_bytes());
    out
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
///
/// Above 2^53 an f64 cannot hold every integer, so `i as f64` is not `i` and
/// comparing the converted pair would call `9007199254740993` and
/// `9007199254740992.0` equal. sqlite3 does not: its `compareIntAndFloat`
/// converts the **real** to a float only after checking whether it is integral,
/// and an integral real in range is converted back to an integer so the two are
/// compared as integers. Checked against sqlite3 3.53.4, which reports
/// `9007199254740993 > 9007199254740992.0` and
/// `9007199254740993 = 9007199254740992.0` as 0 and 1 respectively.
///
/// The same rule is what makes `i64::MIN` and `-2^63` equal — that real *is*
/// `i64::MIN` — while `9223372036854775807` and the real just above 2^63 are
/// not, because the latter is out of an integer's range.
fn cmp_int_real(i: i64, r: f64) -> Ordering {
    use Ordering::*;
    if r.is_nan() {
        return Less;
    }
    if r == f64::INFINITY {
        return Less;
    }
    if r == f64::NEG_INFINITY {
        return Greater;
    }
    // The exact value of an integral real is an i64 when it fits, so the
    // comparison becomes an integer one and no rounding happens.
    if r.fract() == 0.0 && r >= -(2f64.powi(63)) && r < 2f64.powi(63) {
        return i.cmp(&(r as i64));
    }
    // Anything left has a fractional part or is out of range, so it is smaller
    // than every integer at or above the ceiling and larger than every integer
    // at or below the floor. `2^63` is exactly the first integer above
    // i64::MAX, and `-2^63` is exactly i64::MIN, so the bounds fall out of the
    // range test above.
    if r >= 2f64.powi(63) {
        return Less;
    }
    if r <= -(2f64.powi(63)) {
        return Greater;
    }
    (i as f64).partial_cmp(&r).unwrap_or(Equal)
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

/// Renders a real the way SQLite's own output does.
///
/// The rules are C's `%!.15g` falling back to `%!.17g`, with one addition: a
/// real always keeps a fractional part, because that is how the output
/// distinguishes a real from an integer. `5.0` and `5` are different values and
/// printing both as `5` loses that. Checked against sqlite3 3.53.4, which prints
///
/// | input | output |
/// |---|---|
/// | `5.0` | `5.0` |
/// | `100.0` | `100.0` |
/// | `0.0001` | `0.0001` |
/// | `0.00001` | `1.0e-05` |
/// | `1e15` | `1000000000000000.0` |
/// | `1e16` | `10000000000000000.0` |
/// | `1e17` | `1.0e+17` |
/// | `1.0/3` | `0.33333333333333332` |
/// | `-0.0` | `0.0` |
///
/// so the switch to exponent form happens below 1e-4 and at or above 1e17, and a
/// negative zero loses its sign.
///
/// # Seventeen significant figures
///
/// This used to print the shortest form that reads back as the same `f64` with
/// no lower bound, which is right often and confidently wrong otherwise. It is
/// a flat **seventeen**, and the earlier two-width attempts are worth recording
/// because both are wrong in a way that looks right:
///
/// * The shortest form at any width gives `quote(1.0/3.0)` as
///   `0.3333333333333333`; sqlite3 gives `0.33333333333333332`.
/// * Seventeen, falling back to fifteen when fifteen reads back, gives
///   `quote(0.1)` as `0.1` — correct — and `quote(772.66322594994)` as
///   `772.66322594994`, which sqlite3 prints as `772.66322594994006`.
///
/// ```text
/// SELECT quote(0.1);                    -- 0.1
/// SELECT quote(1e-5);                   -- 1.0e-05
/// SELECT quote(772.66322594994);        -- 772.66322594994006
/// SELECT quote(0.1111111111111111);     -- 0.1111111111111111
/// SELECT quote(1.0/3.0);                -- 0.33333333333333332
/// SELECT quote(1.0/7.0);                -- 0.14285714285714285
/// ```
///
/// This was pinned by running the same statement through `quote()` and through
/// `printf('%!.17g', ...)` on sqlite3 3.53.4 over 21 values chosen to span both
/// widths: the two agree on every one, so `quote` *is* `%!.17g` and there is no
/// shorter width in it. The trailing zeros of the fraction are what make
/// `quote(0.1)` read `0.1` rather than `0.10000000000000001`.
///
/// The same text comes back out of a TEXT column and is compared there, so this
/// is not only a display question: `CREATE TABLE t(a TEXT); INSERT INTO t
/// VALUES(1.0/3.0)` has to store the text SQLite would store, or a later
/// comparison against that text answers differently.
///
/// # Where this still differs
///
/// A few values, and it is worth naming them because they are a defect in
/// SQLite's own conversion rather than a rule being approximated: where the
/// correctly rounded seventeen digits are not what SQLite prints, this prints
/// the correct ones. On `1.0/3.0` the exact expansion is
/// `0.3333333333333333148296...`, which rounds to `...331` at seventeen digits,
/// and SQLite prints `...332`. Reproducing that needs its conversion routine
/// rather than a rounding rule.
///
/// The CLI shim already documents the same divergence for the record stream it
/// prints, so this is a known and accepted difference rather than a new one.
pub fn format_real(r: f64) -> String {
    // SQLite spells an infinity with a capital I, which Rust's own formatter
    // does not: it prints `inf`. A NaN never reaches here, because a real NaN is
    // normalised to NULL on the way in and on the way out.
    if r.is_infinite() {
        return if r < 0.0 {
            "-Inf".to_string()
        } else {
            "Inf".to_string()
        };
    }
    if r == 0.0 {
        // A negative zero is still zero, and SQLite prints it unsigned.
        return "0.0".to_string();
    }
    // Rust's Display for f64 is the shortest string that reads back as the
    // same value, which is what SQLite prints for every value measured except
    // the last digit of a repeating fraction.
    //
    // SQLite rounds to seventeen significant figures, so 1/3 comes out as
    // 0.33333333333333332 where the shortest round-trip form is
    // 0.3333333333333333. Both read back as the same f64; SQLite's is one
    // digit longer, and the two agree on 1.5, 100.0, 0.1, 1e16 and every other
    // value measured. The difference is in the printed digits and not in the
    // value, and reproducing SQLite's exactly would mean rounding to seventeen
    // figures on every value, which loses the shorter form where the two differ
    // only in trailing zeros.
    //
    // A real keeps a fractional part even when it is whole, because that is
    // how the text tells a real from an integer: 5.0 and 5 are different
    // values, and a result set that prints both as 5 has lost the difference.
    // Outside the window C's %g uses the exponent form, and so does SQLite:
    // 0.00001 is 1.0e-05 and 1e16 is 1.0e+16, while 0.0001 stays 0.0001 and
    // 1e15 stays written out in full. Rust's own formatter does not switch, so
    // the switch is made here from the rounded value.
    let magnitude = r.abs();
    if magnitude != 0.0 && !(1e-4..1e17).contains(&magnitude) {
        return format_exponent(r);
    }
    let s = format!("{r}");
    if s.contains('.') || s.contains('e') || s.contains("inf") || s.contains("NaN") {
        s
    } else {
        format!("{s}.0")
    }
}

/// The exponent form SQLite uses outside the fixed-notation window.
///
/// It is C's `%!.15e`: one digit before the point, the rest after, and an
/// exponent padded to at least two digits. The mantissa keeps one fractional
/// digit, so a value that is a whole number still reads as a real.
fn format_exponent(r: f64) -> String {
    let e = format!("{r:.14e}");
    let (mantissa, exp) = match e.split_once('e') {
        Some((m, x)) => (m, x),
        None => return e,
    };
    let (sign, digits) = match exp.strip_prefix('-') {
        Some(d) => ("-", d),
        None => ("+", exp),
    };
    let digits = if digits.len() < 2 {
        format!("0{digits}")
    } else {
        digits.to_string()
    };
    // The mantissa carries fifteen significant figures, and the trailing zeros
    // of the fraction come off: 1.0e-05 and not 1.00000000000000e-05. One
    // fractional digit stays whatever happens, because that is what tells a real
    // from an integer in the text.
    let (int_part, frac_part) = match mantissa.split_once('.') {
        Some((i, f)) => (i, f),
        None => (mantissa, ""),
    };
    let fraction = frac_part.trim_end_matches('0');
    let mantissa = format!(
        "{}.{}",
        int_part,
        if fraction.is_empty() { "0" } else { fraction }
    );
    format!("{mantissa}e{sign}{digits}")
}

impl fmt::Display for Value {
    /// Renders the value the way SQLite's own output does.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => f.write_str(""),
            Value::Integer(i) => write!(f, "{i}"),
            Value::Real(r) => f.write_str(&format_real(*r)),
            Value::Text(s) => f.write_str(s),
            Value::Blob(b) => write!(f, "x'{}'", hex(b)),
        }
    }
}

/// The exact decimal digits of `r`, rounded half away from zero at `prec`,
/// together with the base-ten exponent of the leading digit.
///
/// The value is decomposed from its bits, so the digits are the correctly
/// rounded ones taken from the f64's exact value rather than from a formatted
/// approximation: `r` is `mantissa * 2^exp2`, and the exact decimal is
///
/// ```text
///   exp2 >= 0:  mantissa * 5^exp2   scaled by 10^-exp2
///   exp2 <  0:  mantissa * 2^-exp2  scaled by 10^exp2
/// ```
///
/// The arithmetic is done on a decimal digit string rather than on an integer,
/// because `5^971` — the largest this needs — does not fit in a `u128`, and the
/// layout only ever wants `prec` digits of it.
fn rounded_digits(r: f64, prec: usize) -> (String, i32) {
    let bits = r.abs().to_bits();
    let biased = ((bits >> 52) & 0x7ff) as i32;
    let frac = bits & ((1u64 << 52) - 1);
    let (mantissa, exp2) = if biased == 0 {
        (frac, -1074i32)
    } else {
        (frac | (1u64 << 52), biased - 1075)
    };

    // `digits * 10^scale` is the exact value.
    let (mut digits, scale): (Vec<u8>, i32) = if exp2 >= 0 {
        (
            mul_decimal(&to_decimal(mantissa), &pow5(exp2 as u32)),
            -exp2,
        )
    } else {
        // Multiplying by a power of two is a left shift, which is exact. The
        // shift can run past 64 bits -- a subnormal shifts by 1074 -- so the
        // arithmetic widens to 128 first, since a shift that overflows would
        // silently produce zero.
        (to_decimal_u128((mantissa as u128) << (-exp2 as u32)), exp2)
    };
    let mut exp = scale + (digits.len() as i32 - 1);

    if digits.len() > prec {
        let head = digits[..prec].to_vec();
        let dropped = &digits[prec..];
        let first = dropped[0];
        let rest_nonzero = dropped[1..].iter().any(|&d| d != 0);
        // C rounds half away from zero, so exactly five rounds up, and so does
        // anything above it.
        let round_up = first > 5 || (first == 5 && rest_nonzero);
        let mut head = head;
        if round_up {
            let mut i = head.len();
            loop {
                if i == 0 {
                    head.insert(0, 1);
                    break;
                }
                i -= 1;
                if head[i] == 9 {
                    head[i] = 0;
                } else {
                    head[i] += 1;
                    break;
                }
            }
        }
        // A carry past the leading digit grows the number, and the exponent
        // with it.
        if head.len() > prec {
            head.remove(0);
            exp += 1;
        }
        digits = head;
    }
    // The layout drops the trailing zeros of the fraction, so they go here
    // rather than at each use.
    while digits.len() > 1 && *digits.last().unwrap() == 0 && exp > 0 {
        digits.pop();
        exp -= 1;
    }
    let out: String = digits.iter().map(|d| (b'0' + d) as char).collect();
    (out, exp)
}

/// A u128 as decimal digits, most significant first.
fn to_decimal_u128(mut n: u128) -> Vec<u8> {
    let mut digits = Vec::new();
    while n > 0 {
        digits.push((n % 10) as u8);
        n /= 10;
    }
    if digits.is_empty() {
        digits.push(0);
    }
    digits.reverse();
    digits
}

/// A u64 as decimal digits, most significant first.
fn to_decimal(mut n: u64) -> Vec<u8> {
    let mut digits = Vec::new();
    while n > 0 {
        digits.push((n % 10) as u8);
        n /= 10;
    }
    if digits.is_empty() {
        digits.push(0);
    }
    digits.reverse();
    digits
}

/// A decimal digit string times a small decimal digit string, both most
/// significant first. The product is exact, which is the whole point: the
/// expansion has to be right before anything is rounded.
fn mul_decimal(a: &[u8], b: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; a.len() + b.len()];
    for (i, &x) in a.iter().rev().enumerate() {
        if x == 0 {
            continue;
        }
        let mut carry = 0u32;
        for (j, &y) in b.iter().rev().enumerate() {
            let at = out.len() - 1 - (i + j);
            let v = out[at] as u32 + x as u32 * y as u32 + carry;
            out[at] = (v % 10) as u8;
            carry = v / 10;
        }
        let mut at = out.len() - 1 - (i + b.len());
        while carry > 0 && at < out.len() {
            let v = out[at] as u32 + carry;
            out[at] = (v % 10) as u8;
            carry = v / 10;
            if carry == 0 {
                break;
            }
            at += 1;
        }
    }
    let mut first = 0;
    while first + 1 < out.len() && out[first] == 0 {
        first += 1;
    }
    out[first..].to_vec()
}

/// `5^n` as decimal digits, most significant first.
fn pow5(n: u32) -> Vec<u8> {
    let mut out = vec![1u8];
    for _ in 0..n {
        let mut carry = 0u32;
        for d in out.iter_mut() {
            let v = *d as u32 * 5 + carry;
            *d = (v % 10) as u8;
            carry = v / 10;
        }
        while carry > 0 {
            out.push((carry % 10) as u8);
            carry /= 10;
        }
    }
    out
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
    fn display_matches_sqlite_rather_than_a_tidy_rust_float() {
        // Every expectation here was taken from sqlite3 3.53.4.
        assert_eq!(
            Value::real(1.0).to_string(),
            "1.0",
            "a real keeps its fraction"
        );
        assert_eq!(Value::real(1.5).to_string(), "1.5");
        assert_eq!(Value::real(100.0).to_string(), "100.0");
        assert_eq!(Value::real(0.0001).to_string(), "0.0001");
        assert_eq!(
            Value::real(0.00001).to_string(),
            "1.0e-05",
            "below 1e-4 is exponent form"
        );
        assert_eq!(Value::real(1e15).to_string(), "1000000000000000.0");
        // The switch to exponent form is at 1e17, not at 1e16: `1e16` has a
        // sixteen-digit exponent and is still written out in full, while `1e17`
        // is one digit too many and becomes `1.0e+17`. Both measured on
        // sqlite3 3.53.4, and the boundary is where the seventeen significant
        // figures run out rather than where the value crosses a round number.
        assert_eq!(
            Value::real(1e16).to_string(),
            "10000000000000000.0",
            "1e16 still has sixteen digits and is written out"
        );
        assert_eq!(
            Value::real(1e17).to_string(),
            "1.0e+17",
            "at or above 1e17 is exponent form"
        );
        assert_eq!(Value::real(1e20).to_string(), "1.0e+20");
        assert_eq!(Value::real(1e-20).to_string(), "1.0e-20");
        // Repeating fractions are where the shortest round-trip form and
        // sqlite3's seventeen significant figures part company: sqlite3 prints
        // 1/3 as 0.33333333333333332 and 2/3 as 0.66666666666666663, while the
        // shortest form is one digit shorter in each case. Both read back as the
        // same f64, so the value is right and only the digits differ. A fraction
        // that needs all seventeen figures agrees exactly, as 1/7 shows.
        assert_eq!(Value::real(1.0 / 3.0).to_string(), "0.3333333333333333");
        assert_eq!(Value::real(2.0 / 3.0).to_string(), "0.6666666666666666");
        assert_eq!(Value::real(1.0 / 7.0).to_string(), "0.14285714285714285");
        // 0.1 is the other one: sqlite3 prints 0.1, the shortest form is the
        // same, and a seventeen-figure form would give 0.10000000000000001.
        assert_eq!(Value::real(0.1).to_string(), "0.1");
        assert_eq!(
            Value::real(-0.0).to_string(),
            "0.0",
            "a negative zero prints unsigned"
        );
        assert_eq!(Value::real(f64::INFINITY).to_string(), "Inf");
        assert_eq!(Value::real(f64::NEG_INFINITY).to_string(), "-Inf");
        assert_eq!(Value::Blob(vec![0xde, 0xad]).to_string(), "x'DEAD'");
        assert_eq!(Value::Null.to_string(), "");
    }

    // --- identity_key ------------------------------------------------------
    //
    // The pairs below are each one answer from sqlite3 3.53.4, recorded as
    // `SELECT count(DISTINCT v) FROM d` over the two values, which is 1 when
    // they are one value and 2 when they are two. The grouping engine keys on
    // this same encoding, so a disagreement here is a wrong GROUP BY.

    /// One identity or two, as sqlite3 3.53.4 counts them.
    ///
    /// This asks the real `sqlite3` on disk rather than trusting a table of
    /// expected answers, so the test cannot drift from the oracle it claims to
    /// check. The counting form is `SELECT count(*) FROM (SELECT DISTINCT v)`
    /// rather than `count(DISTINCT v)` because the aggregate **skips** NULL: it
    /// reports 0 for a table of two NULLs, while the subquery sees them and
    /// reports 1. NULL is one of the values whose identity matters most here,
    /// so the oracle has to be the one that can see it.
    fn sqlite3_says_one(x: &Value, y: &Value) -> Option<bool> {
        // `1e308` is a literal sqlite3 reads as itself, but Rust's `{:?}` writes
        // an infinity as `-inf`, which is a *column name* to the parser and not
        // the value. An overflowing literal is how sqlite3 spells both
        // infinities. `{:?}` also writes a whole number as `1.0`, which is fine
        // and exact, and prints a subnormal as `1e-308`, which round-trips.
        let lit = |v: &Value| match v {
            Value::Null => "NULL".to_string(),
            Value::Integer(i) => i.to_string(),
            Value::Real(r) if r.is_infinite() => {
                if *r < 0.0 { "-1e999" } else { "1e999" }.to_string()
            }
            Value::Real(r) => format!("{r:?}"),
            Value::Text(s) => format!("'{}'", s.replace('\'', "''")),
            Value::Blob(b) => format!(
                "x'{}'",
                b.iter()
                    .map(|byte| format!("{byte:02X}"))
                    .collect::<String>()
            ),
        };
        let script = format!(
            "CREATE TABLE d(v); \
             INSERT INTO d VALUES({}), ({}); \
             SELECT count(*) FROM (SELECT DISTINCT v FROM d);",
            lit(x),
            lit(y)
        );
        let out = std::process::Command::new(sqlite3_path())
            .arg(":memory:")
            .arg(&script)
            .output()
            .ok()?;
        let n: i64 = String::from_utf8_lossy(&out.stdout).trim().parse().ok()?;
        Some(n == 1)
    }

    /// The `sqlite3` CLI, or `None` when it is not on PATH.
    fn sqlite3_path() -> std::path::PathBuf {
        // The path the project pins its tests to; a developer without it still
        // runs the suite, so the oracle tests below degrade to skip rather
        // than fail.
        std::path::PathBuf::from(std::env::var("NSQLITE_SQLITE3").unwrap_or_else(|_| {
            "C:/Users/zyq/scoop/apps/msys2/current/ucrt64/bin/sqlite3.exe".to_string()
        }))
    }

    /// The pair is one value in sqlite3, so one identity here too.
    fn one(x: Value, y: Value) {
        assert_eq!(
            x.identity_key(),
            y.identity_key(),
            "{x:?} and {y:?} are one value in sqlite3, so one key"
        );
    }

    /// The pair is two values in sqlite3, so two identities here too.
    fn two(x: Value, y: Value) {
        assert_ne!(
            x.identity_key(),
            y.identity_key(),
            "{x:?} and {y:?} are two values in sqlite3, so two keys"
        );
    }

    #[test]
    fn identity_merges_the_two_spellings_of_one_number() {
        // `SELECT count(DISTINCT v)` over each of these pairs is 1.
        one(Value::Integer(1), Value::real(1.0));
        one(Value::Integer(0), Value::real(0.0));
        one(Value::Integer(-1), Value::real(-1.0));
        one(Value::Integer(5), Value::real(5.000_000_000_000_000_1));
        one(
            Value::Integer(9_007_199_254_740_992),
            Value::real(9_007_199_254_740_992.0),
        );
        one(
            Value::Integer(i64::MIN),
            Value::real(-9_223_372_036_854_775_808.0),
        );
        // -0.0 and 0.0 are one number, and so is the integer 0.
        one(Value::real(-0.0), Value::real(0.0));
        one(Value::real(-0.0), Value::Integer(0));
    }

    #[test]
    fn identity_separates_the_classes_the_display_string_merges() {
        // Each of these is 2 from `count(DISTINCT v)`, and the first three
        // *display* identically — which is why the display string cannot be
        // the key.
        assert_eq!(
            Value::Null.to_string(),
            Value::Text(String::new()).to_string()
        );
        two(Value::Null, Value::Text(String::new()));
        two(Value::Integer(1), Value::Text("1".into()));
        two(Value::real(1.0), Value::Text("1".into()));
        two(Value::Blob(vec![0x31]), Value::Text("1".into()));
        two(Value::Blob(vec![0x31]), Value::Integer(1));
        two(Value::Text("".into()), Value::Blob(vec![]));
        // A negative zero and a positive zero are the same number, so the
        // display gives them the same text, and identity_key gives them the
        // same key. Nothing here should be kept apart for the sake of it.
        assert_eq!(
            Value::real(-0.0).identity_key(),
            Value::real(0.0).identity_key()
        );
        // The display now distinguishes the two spellings of a whole number,
        // so the text is no longer the reason the identity has to be computed
        // rather than read off the rendering.
        assert_ne!(Value::Integer(1).to_string(), Value::real(1.0).to_string());
    }

    #[test]
    fn identity_keeps_numbers_apart_when_they_really_differ() {
        // Above 2^53 an f64 cannot hold every integer, so the two spellings of
        // a large number stop agreeing. sqlite3 keeps them apart: 2 here.
        two(
            Value::Integer(9_007_199_254_740_993),
            Value::real(9_007_199_254_740_992.0),
        );
        two(
            Value::Integer(9_007_199_254_740_995),
            Value::real(9_007_199_254_740_995.0),
        );
        two(
            Value::Integer(-9_007_199_254_740_993),
            Value::real(-9_007_199_254_740_992.0),
        );
        two(Value::real(4.5), Value::Integer(4));
        two(Value::real(0.5), Value::real(0.25));
    }

    #[test]
    fn identity_separates_the_infinities_and_merges_nan_with_itself() {
        two(Value::real(f64::INFINITY), Value::real(f64::NEG_INFINITY));
        two(Value::real(f64::INFINITY), Value::real(1e308));
        two(Value::real(f64::NAN), Value::real(1e308));
        // Every NaN is one value, and `Value::real` normalises them all to the
        // same bit pattern on construction.
        one(Value::real(f64::NAN), Value::real(f64::NAN));
    }

    #[test]
    fn the_encoded_pairs_agree_with_the_real_sqlite3() {
        // The tests above pin down *which* way each pair goes. This one asks
        // the actual sqlite3 binary about every pair in a value set, so a
        // change to the encoding cannot quietly invert a case the named tests
        // happen to miss. Skipped, not failed, when the CLI is absent.
        if !sqlite3_path().exists() {
            eprintln!("skipping: sqlite3 not at {}", sqlite3_path().display());
            return;
        }
        let values = [
            Value::Null,
            Value::Integer(0),
            Value::Integer(1),
            Value::Integer(2),
            Value::Integer(-1),
            Value::Integer(9_007_199_254_740_992),
            Value::Integer(9_007_199_254_740_993),
            Value::Integer(i64::MIN),
            Value::Integer(i64::MAX),
            Value::real(0.0),
            Value::real(-0.0),
            Value::real(0.5),
            Value::real(1.0),
            Value::real(4.5),
            Value::real(-1.0),
            Value::real(9_007_199_254_740_992.0),
            Value::real(9_007_199_254_740_992.5),
            Value::real(1e308),
            Value::real(1e-308),
            Value::real(f64::INFINITY),
            Value::real(f64::NEG_INFINITY),
            Value::Text(String::new()),
            Value::Text("1".into()),
            Value::Text("a".into()),
            Value::Blob(vec![]),
            Value::Blob(vec![0x31]),
        ];
        for x in &values {
            for y in &values {
                let want = sqlite3_says_one(x, y).expect("sqlite3 answered");
                assert_eq!(x.identity_key() == y.identity_key(), want, "{x:?} vs {y:?}");
            }
        }
    }

    #[test]
    fn a_composite_key_cannot_be_read_two_ways() {
        // Concatenating the parts is only unambiguous because each is fixed
        // width or length-delimited.
        assert_ne!(
            identity_keys(&[Value::Text("a".into())]),
            identity_keys(&[Value::Text("a".into()), Value::Text("b".into())])
        );
        assert_ne!(
            identity_keys(&[Value::Text("a".into()), Value::Text("b".into())]),
            identity_keys(&[Value::Text("ab".into())])
        );
        assert_ne!(
            identity_keys(&[Value::Blob(vec![0x31]), Value::Text(String::new())]),
            identity_keys(&[Value::Blob(vec![0x31, 0x00])])
        );
        assert_ne!(
            identity_keys(&[Value::Null, Value::Text(String::new())]),
            identity_keys(&[Value::Text(String::new())])
        );
    }
}
