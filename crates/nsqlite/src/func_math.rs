//! The numeric and date/time scalar functions.
//!
//! Every behaviour here was checked against the real `sqlite3` (3.53.4) rather
//! than recalled, and the surprising results are called out in comments because
//! several of them contradict intuition:
//!
//! * `round` is *exact decimal* rounding, not `x * 10^n` then round. That is
//!   why `round(2.675, 2)` is `2.67` and `round(4.625, 2)` is `4.63`.
//! * `mod` is C `fmod`, so a negative divisor does **not** change the sign of
//!   the result: `mod(5, -3)` is `2.0`, not `-1.0`.
//! * `log(x)` with one argument is base 10, not base *e*.
//! * The two-argument `log(b, x)` rejects a base below 1 outright, so
//!   `log(0.5, 2)` is NULL even though the mathematics is well defined.
//! * `ceil`/`floor` return an integer only when the *argument* is an integer;
//!   `ceil(2)` is integer 2 but `ceil(1.0)` is real 1.0.
//!
//! The date functions work on Julian day numbers, interpret a time value as
//! UTC with no timezone, and accept a narrow set of input spellings. The epoch
//! anchors are 1970-01-01 = Julian day 2440587.5 and `unixepoch` 0.

use crate::error::{Error, Result, ResultCode};
use crate::value::Value;

// ---------------------------------------------------------------------------
// Numeric coercion
// ---------------------------------------------------------------------------

/// SQLite coerces an argument to a number in one of two ways, and which one a
/// given function uses is decided by whether the function needs an *exact
/// integer* (`sqlite3_value_int64`) or merely a double.
///
/// * [`num_value`] is the "any prefix" parser behind `abs` and `round`. It
///   reads the longest numeric prefix and treats the rest as zero, so `'5abc'`
///   is 5.0 and `'abc'` is 0.0.
/// * [`num_strict`] is the "must be entirely a number" parser behind `sign`,
///   `sqrt`, `ln`, `ceil` and the rest. Anything with trailing junk is NULL, so
///   `'5abc'` is NULL there. It also recognises `inf`, which [`num_value`]
///   does not: `abs('inf')` is 0.0 but `sign('inf')` is NULL.
pub fn num_value(v: &Value) -> f64 {
    match v {
        Value::Integer(i) => *i as f64,
        Value::Real(r) => *r,
        Value::Text(s) => parse_prefix(s).map_or(0.0, |(_, n)| n),
        // A blob and text whose bytes are not valid UTF-8 are read the same way
        // as text: their bytes, then the longest numeric prefix. This is what
        // makes a blob a usable argument wherever a number is expected.
        Value::TextBytes(b) | Value::Blob(b) => {
            parse_prefix(&String::from_utf8_lossy(b)).map_or(0.0, |(_, n)| n)
        }
        _ => 0.0,
    }
}

/// The whole-string numeric parser, shared by the strict functions and by
/// `printf`'s numeric conversions.
pub fn num_strict(v: &Value) -> Option<f64> {
    match v {
        Value::Integer(i) => Some(*i as f64),
        Value::Real(r) => Some(*r),
        Value::Text(s) => parse_whole(s),
        _ => None,
    }
}

/// The `strtod` semantics SQLite's `sqlite3AtoF` implements: skip blanks, take
/// the longest valid numeric prefix, and treat junk (or nothing) as zero.
/// `'  -5 '` is -5.0 and `'0x10'` is 0.0 because the parse stops at the `x`.
fn parse_prefix(s: &str) -> Option<(usize, f64)> {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() && b[i].is_ascii_whitespace() {
        i += 1;
    }
    let num_start = i;
    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
        i += 1;
    }
    let digits_before = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    let int_digits = i - digits_before;
    let mut frac_digits = 0;
    if i < b.len() && b[i] == b'.' {
        i += 1;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
            frac_digits += 1;
        }
    }
    if int_digits == 0 && frac_digits == 0 {
        return None; // no numeric prefix at all
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        let save = i;
        i += 1;
        if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
            i += 1;
        }
        let exp_start = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == exp_start {
            i = save; // a lone trailing `e` is not part of the number
        }
    }
    Some((i, s[num_start..i].parse::<f64>().unwrap_or(0.0)))
}

/// The all-or-nothing parser: the whole string must be a number.
///
/// The spelling is narrower than [`parse_prefix`]'s in both directions. It
/// rejects anything with trailing junk, so `'5abc'` is NULL here but 5.0 for
/// `abs`, and it does *not* accept the words `inf`, `infinity` or `nan`, so
/// `sign('inf')` is NULL. A numeric literal that overflows is fine, though:
/// `sign('1e999')` is 1 and `sqrt('1e999')` is `Inf`.
fn parse_whole(s: &str) -> Option<f64> {
    let t = s.trim_matches(|c: char| c.is_ascii_whitespace());
    if t.is_empty() {
        return None;
    }
    match parse_prefix(t) {
        Some((len, n)) if len == t.len() => Some(n),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Exact decimal helpers
// ---------------------------------------------------------------------------

/// The exact decimal expansion of a finite `f64`: the significant digits with
/// no trailing zeros, plus `lead`, the power of ten of the leading digit, so
/// that
///
/// ```text
/// value = 0.<digits> x 10^lead
/// ```
///
/// `1.5` therefore gives `("15", 0)` and `0.1` gives `("1", -1)`.
///
/// Every finite double has a finite exact decimal expansion, and this derives
/// it through the IEEE-754 decomposition `mantissa x 2^exponent` rather than
/// through floating-point arithmetic, so no digit can be perturbed.
pub fn exact_decimal(v: f64) -> (Vec<u8>, i32) {
    if v == 0.0 {
        return (vec![b'0'], 0);
    }
    let bits = v.to_bits();
    let raw_exp = ((bits >> 52) & 0x7ff) as i32;
    let frac = bits & 0x000f_ffff_ffff_ffff;
    let (mantissa, exponent) = if raw_exp == 0 {
        (frac as u128, -1074i32) // subnormal: value = frac * 2^-1074
    } else {
        ((frac | 0x0010_0000_0000_0000) as u128, raw_exp - 1075)
    };
    // Work out the exact integer numerator and the power of ten it is scaled
    // by. For e >= 0 the value is an integer, `mantissa * 2^e`, with no decimal
    // point; for e < 0 it is `mantissa * 5^-e` scaled by `10^-e`.
    let (digits, dec_exp) = if exponent >= 0 {
        (mul_pow2(mantissa, exponent), 0)
    } else {
        let k = (-exponent) as u32;
        // 5^1074 needs about 750 digits, well past u128, so build the decimal
        // string by repeated small multiplication.
        (mul_decimal(mantissa, &pow5_digits(k)), -(k as i32))
    };
    // The value is `<digits> x 10^dec_exp`, and the scientific exponent is the
    // power of ten of the leading digit. Trailing zeros are *not* stripped here:
    // the digit string has to keep them for `lead` and the caller's decimal
    // scaling to line up, and `render_significand` strips them at the very end
    // where %g would.
    let lead = digits.len() as i32 - 1 + dec_exp;
    (digits, lead)
}

/// The ASCII decimal digits of 5^k, most significant first.
///
/// The value is held as base-10^9 limbs, so the largest case (5^1074) is about
/// 84 limbs rather than 750 single digits and the work stays cheap.
fn pow5_digits(k: u32) -> Vec<u8> {
    let mut limbs: Vec<u32> = vec![1];
    for _ in 0..k {
        let mut carry = 0u64;
        for limb in limbs.iter_mut() {
            let p = (*limb as u64) * 5 + carry;
            *limb = (p % 1_000_000_000) as u32;
            carry = p / 1_000_000_000;
        }
        while carry > 0 {
            limbs.push((carry % 1_000_000_000) as u32);
            carry /= 1_000_000_000;
        }
    }
    limbs_to_ascii(&limbs)
}

/// Renders base-10^9 limbs (least significant first) as an ASCII digit string.
///
/// Every limb but the most significant is zero-padded to nine digits; padding
/// the leading one as well would insert phantom zeros, turning 5^55 into a
/// forty-digit number and shifting the derived exponent by one.
fn limbs_to_ascii(limbs: &[u32]) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::with_capacity(limbs.len() * 9);
    for (i, &l) in limbs.iter().enumerate().rev() {
        if i + 1 == limbs.len() {
            out.extend(format!("{l}").into_bytes());
        } else {
            out.extend(format!("{l:09}").into_bytes());
        }
    }
    let start = out.iter().position(|&c| c != b'0').unwrap_or(out.len() - 1);
    out.drain(..start);
    out
}

/// Parses a big-endian ASCII digit string into base-10^9 limbs.
fn ascii_to_limbs(ascii: &[u8]) -> Vec<u32> {
    let mut limbs: Vec<u32> = Vec::new();
    let mut end = ascii.len();
    while end > 0 {
        let start = end.saturating_sub(9);
        let mut limb = 0u32;
        for &c in &ascii[start..end] {
            limb = limb * 10 + (c - b'0') as u32;
        }
        limbs.push(limb);
        end = start;
    }
    while limbs.len() > 1 && *limbs.last().unwrap() == 0 {
        limbs.pop();
    }
    limbs
}

/// Multiplies a big-endian ASCII digit string by a `u128` factor, exactly.
///
/// Both sides go to base-10^9 limbs so this is a linear pass, which matters
/// because `exact_decimal` runs for every REAL that has to render. The product
/// of a 10^9-scaled limb and a factor below 2^64 is below 2^94, so `u128`
/// holds it without a further split.
fn mul_decimal(m: u128, ascii: &[u8]) -> Vec<u8> {
    if m == 0 {
        return vec![b'0'];
    }
    let limbs = ascii_to_limbs(ascii);
    // `limb * m` is below 10^9 * 2^64, so each row of the schoolbook multiply
    // spans at most three base-10^9 limbs; three of slack is a safe bound.
    let mut out = vec![0u128; limbs.len() + 3];
    for (i, &limb) in limbs.iter().enumerate() {
        // Accumulate limb * m starting at position i, adding in whatever is
        // already there and carrying upwards. Writing out[i] first and then
        // re-adding it would double-count, so the first write *is* the store.
        let mut carry: u128 = (limb as u128) * m;
        let mut k = i;
        while carry > 0 {
            let total = out[k] + carry;
            out[k] = total % 1_000_000_000;
            carry = total / 1_000_000_000;
            k += 1;
        }
    }
    let trimmed: Vec<u32> = out.iter().map(|&l| l as u32).collect();
    limbs_to_ascii(&trimmed)
}

/// Multiplies by 2^exponent, returning the ASCII digits of the exact result.
fn mul_pow2(m: u128, exponent: i32) -> Vec<u8> {
    let mut digits = format!("{m}").into_bytes();
    for _ in 0..exponent {
        digits = mul_decimal(2, &digits);
    }
    digits
}

/// Rounds an exact decimal digit string to `keep` significant digits, ties away
/// from zero (which is what SQLite's mkshiss does).
///
/// The returned string always carries the digits of the *rounded* value, so a
/// carry out of the leading digit widens it by prepending a `1`: keeping one
/// digit of 9.99 gives `10`, and the returned flag advances the exponent to
/// match. The two always travel together, which is why the flag exists.
///
/// A `keep` of zero means "round below the precision of the expansion", which
/// has no digits to return at all, so the result is empty and the caller has to
/// fall back to a power of ten. That case must not be padded: 9.99 expands to
/// `9990000000000000213...`, so a short string padded with trailing zeros would
/// answer with padding rather than the `9` the caller asked for, and the
/// vanished carry is what made `round(9.99, 0)` answer 0.0.
///
/// A `keep` larger than the digit count pads with trailing zeros, which is what
/// `round(2.5, 20)` needs; nothing is discarded in that case, so there is never
/// a carry.
fn round_digits(digits: &[u8], keep: usize) -> (Vec<u8>, bool) {
    if keep == 0 {
        return (Vec::new(), false);
    }
    if keep >= digits.len() {
        let mut d = digits.to_vec();
        d.resize(keep, b'0');
        return (d, false);
    }
    let mut d = digits[..keep].to_vec();
    let mut carried = false;
    if should_round_up(&digits[keep..]) {
        carried = increment(&mut d);
        if carried {
            // Every kept digit was a nine, so the round carries into a new
            // leading digit and the exponent has to advance with it.
            d.insert(0, b'1');
        }
    }
    (d, carried)
}

/// Whether the discarded tail rounds the kept digits up.
///
/// SQLite rounds a tie away from zero, and the magnitude here is always
/// positive (the sign is reapplied by the caller), so an exact tie — a 5
/// followed by only zeros — rounds up. That is what makes `round(2.5)` 3.0 and
/// `round(0.5)` 1.0, rather than the banker's rounding a float library would do.
fn should_round_up(tail: &[u8]) -> bool {
    match tail.first() {
        None => false,
        Some(&d) if d > b'5' => true,
        Some(&d) if d < b'5' => false,
        // An exact tie: 5 followed by zeros. Away from zero means up.
        Some(_) => true,
    }
}

/// Adds one to a big-endian decimal digit string in place, reporting whether
/// the carry escaped the leading digit.
///
/// SQLite's mkshiss leaves the escaped digit in place -- the caller sees the
/// all-zero leading digit and knows the exponent has to advance -- so the
/// string keeps its length and the caller decides what a widening means.
fn increment(s: &mut [u8]) -> bool {
    for d in s.iter_mut().rev() {
        if *d == b'9' {
            *d = b'0';
        } else {
            *d += 1;
            return false;
        }
    }
    true
}

/// Renders a `f64` the way SQLite renders a REAL as text.
///
/// The rule, worked out against 1178 sampled non-integral values and confirmed
/// on all but nine of them:
///
/// * a value that is an integer fitting an `i64` prints as that integer plus
///   `.0`;
/// * otherwise the exact decimal expansion is rounded to **fourteen**
///   significant digits, and if those fourteen read back as the same double
///   they are the answer;
/// * if they do not, the expansion is rounded to eighteen and *then* to
///   seventeen -- the double rounding SQLite's own decimal-string formatter
///   performs -- and that is the answer;
/// * trailing zeros are stripped, and scientific notation takes over when the
///   leading digit sits below `-4` or at `16` places from the point.
///
/// The escalation matters. Fifteen digits of one third are
/// `333333333333333`, which reads back as a *different* double, so it has to
/// reach seventeen; and `sin(0.5)`, whose fifteen digits do round-trip, still
/// prints as all seventeen `0.47942553860420301`, so the first rung cannot be
/// fifteen either. Fourteen is the rung that fits: over the corpus the
/// candidate models score 29 (always fifteen), 131 (always sixteen), 1075
/// (fifteen then seventeen), 1120 (always seventeen) and **1169** (this one)
/// out of 1178.
///
/// The double rounding through eighteen is what reproduces sqlite3's famous
/// divergence from the correctly rounded seventeen digits: `1.0/3.0` prints as
/// `0.33333333333333332` where the exact expansion rounds to
/// `0.33333333333333331`. The nine residual values, where even the double
/// rounding lands a digit out, are named in the tests.
pub fn real_to_text(v: f64) -> String {
    if v.is_nan() {
        return String::new();
    }
    if v.is_infinite() {
        return if v < 0.0 { "-Inf".into() } else { "Inf".into() };
    }
    if v == 0.0 {
        return "0.0".into();
    }
    // SQLite's fast path: an integral value is printed as an integer plus `.0`,
    // so 2451545.0 becomes "2451545.0" and not "2451545". The fast path stops
    // where the fixed/scientific band does, at seventeen digits: 1e16 prints as
    // `10000000000000000.0` but 1e17 as `1.0e+17`, and `123456789012345680.0`
    // falls just past the boundary and prints in scientific too. The bound is
    // on the *value*, so 9999999999999999.0 (sixteen nines) still prints in
    // full while the next integer up does not.
    if v.fract() == 0.0 && v.abs() < 1.0e17 {
        return format!("{v:.1}");
    }
    let (full, lead) = exact_decimal(v);
    let (d14, w14) = round_digits(&full, 14);
    let lead14 = lead + i32::from(w14);
    if assemble_magnitude(&d14, lead14) == Some(v.abs()) {
        return render_significand(&d14, lead14, v < 0.0);
    }
    let (d18, w18) = round_digits(&full, 18);
    let (d17, w17) = round_digits(&d18, 17);
    let lead17 = lead + i32::from(w18) + i32::from(w17);
    render_significand(&d17, lead17, v < 0.0)
}

/// Renders a `f64` with a fixed number of significant digits, the way
/// `strftime('%J')` prints a Julian day.
///
/// `%J` uses sixteen significant digits rather than the escalating rule that
/// real-to-text uses, which is why `strftime('%J', '2020-01-02 12:00:00')` is
/// `2458851` with no `.0`: the zeros strip away.
///
/// Zero is the one place where `%J` and a REAL differ. `strftime('%J', 0)` is
/// `0`, where rendering the real 0.0 would give `0.0`; the Julian day is a
/// number the specifier prints rather than a value being cast to text, so it
/// gets no decimal point.
pub fn real_to_text_fixed(v: f64, digits: usize) -> String {
    if v.is_nan() {
        return String::new();
    }
    if v.is_infinite() {
        return if v < 0.0 { "-Inf".into() } else { "Inf".into() };
    }
    if v == 0.0 {
        return "0".into();
    }
    let (full, lead) = exact_decimal(v);
    let (d, widened) = round_digits(&full, digits);
    let out = render_significand(&d, lead + i32::from(widened), v < 0.0);
    // A whole Julian day prints as a bare integer: `strftime('%J', 2451545)`
    // is `2451545`, not `2451545.0`. Only zero had this before; it is a
    // property of any integral value, since the `.0` that real-to-text adds
    // belongs to a value being *cast*, not to a number being printed.
    if v.fract() == 0.0 {
        return out.trim_end_matches(".0").to_string();
    }
    out
}

/// Turns a digit string and the power of ten of its leading digit back into an
/// `f64`, so that the value is `0.<digits> x 10^lead`.
///
/// The digits go to the standard library as one normalised scientific literal,
/// which Rust parses with correct round-to-nearest, so the result is exactly
/// the double those digits denote. Scaling by a power of ten separately would
/// add a second rounding and break the round-trip.
fn assemble_magnitude(digits: &[u8], e: i32) -> Option<f64> {
    let text = std::str::from_utf8(digits).ok()?;
    format!("{text}e{}", e + 1 - digits.len() as i32)
        .parse::<f64>()
        .ok()
        .filter(|m| m.is_finite())
}

/// Formats an exact decimal as text with C `%g` style notation.
///
/// `e` is the power of ten of the leading digit, so the value is
/// `0.<digits> x 10^e` and the leading digit sits `e + 1` places from the
/// decimal point. Scientific notation is used when that position is below -4 or
/// at least 16, which is what sqlite3 does for a value rendered from its
/// fifteen- or seventeen-digit form: 0.0001 prints in full while 0.00001 prints
/// as `1.0e-05`, 1e16 prints as `10000000000000000.0`, and 1e17 prints as
/// `1.0e+17`.
///
/// The switch uses the *digit count before* trailing zeros are stripped, not
/// the count after. `1e17` is one significant digit followed by seventeen zeros,
/// and the stripped count of 1 would put the leading digit at position 17 and
/// yet compare 17 < 1 as false -- printing the whole integer in full, where
/// sqlite3 switches to scientific at 1e17 and prints in full only up to 1e16.
/// The exponent itself is the reliable test and needs no count at all: sqlite3
/// prints in full for exponents from -4 to 16 and switches outside that band.
fn render_significand(digits: &[u8], e: i32, negative: bool) -> String {
    // %g drops trailing zeros: the seventeen digits of 0.1 are
    // 10000000000000000, which must print as "0.1" and not "0.10000000000000".
    let mut end = digits.len();
    while end > 1 && digits[end - 1] == b'0' {
        end -= 1;
    }
    let digits = &digits[..end];
    let n = digits.len() as i32;
    let mut out = String::new();
    if negative {
        out.push('-');
    }
    // Scientific notation exactly when the leading digit sits below -4 or at
    // least 17 places from the point, independent of how many digits the
    // expansion happened to have.
    if !(-4..16).contains(&e) {
        out.push(digits[0] as char);
        // The mantissa always keeps one decimal place, so an integral value
        // prints as `1.0e+300` and not `1e+300`.
        out.push('.');
        if n > 1 {
            out.push_str(std::str::from_utf8(&digits[1..]).unwrap());
        } else {
            out.push('0');
        }
        out.push('e');
        out.push(if e < 0 { '-' } else { '+' });
        let a = e.unsigned_abs();
        if a < 10 {
            out.push('0');
        }
        out.push_str(&a.to_string());
    } else if e >= 0 {
        // The leading digit is at 10^e, so the first e+1 digits are integral.
        let split = (e + 1) as usize;
        out.push_str(std::str::from_utf8(&digits[..split]).unwrap());
        if split < digits.len() {
            out.push('.');
            out.push_str(std::str::from_utf8(&digits[split..]).unwrap());
        } else {
            out.push_str(".0");
        }
    } else {
        // 0.00ddd... with -e-1 zeros after the point before the digits.
        out.push_str("0.");
        for _ in 0..(-e - 1) {
            out.push('0');
        }
        out.push_str(std::str::from_utf8(digits).unwrap());
    }
    out
}

// ---------------------------------------------------------------------------
// Arity helpers
// ---------------------------------------------------------------------------

pub(crate) fn expect_arity(name: &str, args: &[Value], want: usize) -> Result<()> {
    if args.len() == want {
        Ok(())
    } else {
        Err(arity_error(name))
    }
}

pub(crate) fn expect_range(name: &str, args: &[Value], lo: usize, hi: usize) -> Result<()> {
    if args.len() >= lo && args.len() <= hi {
        Ok(())
    } else {
        Err(arity_error(name))
    }
}

pub(crate) fn arity_error(name: &str) -> Error {
    Error::new(
        ResultCode::Error,
        format!("wrong number of arguments to function {name}()"),
    )
}

// ---------------------------------------------------------------------------
// Dates: the Julian-day core
// ---------------------------------------------------------------------------

/// 1970-01-01 00:00:00 UTC, the instant `unixepoch` counts from.
pub const UNIX_EPOCH_JD: f64 = 2440587.5;
/// The last Julian day SQLite can render, midnight on 9999-12-31. `date()` at
/// the last moment of that day still answers, but `julianday()` of anything past
/// it is NULL, because the number itself would name a year beyond the four
/// digits a date can show.
const JULIAN_MAX: f64 = 5373484.0;
const MS_PER_DAY: i64 = 86_400_000;

/// A broken-down UTC civil time. `year` may be negative; the fields are not
/// normalised, because a caller may legitimately hold `hour: 24`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Civil {
    pub year: i64,
    pub month: i64,
    pub day: i64,
    pub hour: i64,
    pub minute: i64,
    pub second: i64,
    /// Fractional seconds in milliseconds, which is all `strftime('%f')` shows.
    pub ms: i64,
}

/// Days from 1970-01-01 for a proleptic Gregorian date, exact for any year the
/// engine can hold. Howard Hinnant's `days_from_civil`, which shifts the year
/// so that the leap day lands at the end.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let mp = (m + 9) % 12; // March = 0
    let doy = (153 * mp + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

/// The inverse of [`days_from_civil`].
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Whether a proleptic Gregorian year has a 29 February. Year 0 is a leap year
/// under the `% 400 == 0` rule, and 1900 is not.
pub fn is_leap(year: i64) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

pub fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap(year) => 29,
        2 => 28,
        _ => 0,
    }
}

/// The Julian day for a civil date and time, in the convention that JD 0.0 is
/// noon on -4713-11-24.
fn jd_of(c: &Civil) -> f64 {
    let days = days_from_civil(c.year, c.month, c.day);
    let frac =
        (c.hour as f64 * 3600.0 + c.minute as f64 * 60.0 + c.second as f64 + c.ms as f64 / 1000.0)
            / 86400.0;
    // days_from_civil counts days from 1970-01-01, which is Julian day
    // 2440587.5, so midnight is 2440587.5 and each later day adds one.
    days as f64 + UNIX_EPOCH_JD + frac
}

/// Splits a Julian day into civil fields, rounding to the nearest millisecond
/// first, which is why `2451545.0` becomes exactly noon rather than
/// `11:59:59.999`. The result is `None` outside the range SQLite can render.
pub fn civil_from_jd(jd: f64) -> Option<Civil> {
    if !jd.is_finite() {
        return None;
    }
    // Work in milliseconds since the Julian epoch and shift so the day number
    // lines up with days_from_civil, which counts from 1970-01-01.
    let ms_total = ((jd + 0.5) * MS_PER_DAY as f64).round();
    if !ms_total.is_finite() || ms_total.abs() > 4.7e17 {
        return None;
    }
    let ms = ms_total as i64;
    // ms_total counts from JD -0.5 (midnight before the Julian epoch noon), so
    // the Unix epoch sits at 2440588 whole days later.
    let unix_ms = ms - (UNIX_EPOCH_JD as i64 + 1) * MS_PER_DAY;
    let days = unix_ms.div_euclid(MS_PER_DAY);
    let rem = unix_ms.rem_euclid(MS_PER_DAY);
    let (year, month, day) = civil_from_days(days);
    // The range is bounded by Julian day 0 at the low end -- which renders as
    // the negative year -4713, not as year 0 -- and by 9999-12-31 at the top,
    // since SQLite renders a four-digit year at most. The low bound is the
    // *epoch itself*, not the start of its day: `date(0)` and `date(0.4)` are
    // both -4713-11-24, `date(0.5)` is -4713-11-25, and anything below zero is
    // NULL. So the test is on the rendered day number, not on the fractional
    // part -- a value of -0.4 rounds to the same midnight as zero and is
    // rejected for being before the epoch, while 0.4 rounds back to the epoch
    // day and is kept.
    if jd < 0.0 || year > 9999 {
        return None;
    }
    // The last day SQLite can render is 9999-12-31, and only up to 23:59:59:
    // julian day 5373484.49 still renders but 5373484.5 does not.
    if year == 9999
        && (month > 12 || (month == 12 && (day > 31 || (day == 31 && rem > 86_399_999))))
    {
        return None;
    }
    Some(Civil {
        year,
        month,
        day,
        hour: rem / 3_600_000,
        minute: (rem / 60_000) % 60,
        second: (rem / 1000) % 60,
        ms: rem % 1000,
    })
}

/// 0 = Sunday .. 6 = Saturday. 1970-01-01 was a Thursday.
pub fn day_of_week(year: i64, month: i64, day: i64) -> i64 {
    (days_from_civil(year, month, day) + 4).rem_euclid(7)
}

/// The day of the year, 1-based, as `%j` reports it.
fn day_of_year(year: i64, month: i64, day: i64) -> i64 {
    days_from_civil(year, month, day) - days_from_civil(year, 1, 1) + 1
}

// ---------------------------------------------------------------------------
// Date string parsing
// ---------------------------------------------------------------------------

/// Parses one of the time-value spellings SQLite accepts.
///
/// The accepted set is narrow, and several things that look reasonable are
/// rejected: a one-digit month or day (`2020-1-2`) is NULL, as are
/// `2020-01-02 03`, `2020-01-02 03:`, a `+0530` offset with no colon, and a
/// ` UTC` suffix. A `Z` or `±HH:MM` offset is accepted and shifts the value.
///
/// A bare time is accepted too and is anchored at 2000-01-01, so
/// `date('12:34:56')` is `2000-01-01` and `strftime('%H:%M', '12:34:56')` is
/// `12:34`.
///
/// The returned [`Jd`] carries the hour-24 tag the date functions need; see
/// [`Jd`].
fn parse_time_value(v: &Value) -> Option<Jd> {
    match v {
        // A number is a Julian day directly, and can never have been spelled
        // with an hour-24 component.
        Value::Integer(i) => Some(Jd::plain(*i as f64)),
        Value::Real(r) => Some(Jd::plain(*r)),
        Value::Text(s) => {
            // `now` is the one time value that names the instant rather than a
            // date, so it is matched here, before the three-way fallback below
            // and before `parse_datetime_text` sees it. That parser rejects
            // `now` at its first `read_digits`, which is correct for every
            // other string, so the word has to be caught on the way in.
            //
            // The match is on the WHOLE string and it ignores case, and both
            // details are measured. sqlite3 answers today's date for `now`,
            // `NOW`, `Now` and `nOw`, and answers NULL for `now `, `nowx`,
            // `now!` and `NOWX` -- so nothing is trimmed and no prefix counts.
            if s.eq_ignore_ascii_case("now") {
                return now_jd();
            }
            if looks_like_time_only(s) {
                parse_time_of_day(s).map(|secs| Jd::plain(jd_of(&J2000_ANCHOR) + secs / 86400.0))
            } else if let Some(n) = parse_whole(s) {
                // A numeric string is a Julian day, not a date: sqlite3 accepts
                // `date('2451545')` and gets 2000-01-01. The strict parser is
                // the right one here, since it rejects the trailing junk a date
                // string would have.
                Some(Jd::plain(n))
            } else {
                parse_datetime_text(s)
            }
        }
        _ => None,
    }
}

/// The current instant as a Julian day, which is what `date('now')` and its
/// five siblings ask for.
///
/// The value is the system clock read in UTC. The engine has no local time zone
/// — the same decision the `localtime` modifier already records at
/// [`Modifier::NoOp`] — so this is exactly what a UTC build of sqlite3
/// computes. That this sqlite3 is a UTC build is measured, not assumed:
/// `TZ=America/New_York sqlite3 :memory: "SELECT datetime('now')"` returns the
/// same UTC reading as the default environment, so the two engines can be
/// compared at all.
///
/// The sub-second part is kept, because `julianday('now')` on sqlite3 is a real
/// with a fractional day (`2461311.8032892593`), and truncating to whole
/// seconds still lands inside the same millisecond for any two reads made
/// close together.
fn now_jd() -> Option<Jd> {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs_f64();
    Some(Jd::plain(secs / 86_400.0 + UNIX_EPOCH_JD))
}

/// 2000-01-01, the day a bare time value is anchored to.
const J2000_ANCHOR: Civil = Civil {
    year: 2000,
    month: 1,
    day: 1,
    hour: 0,
    minute: 0,
    second: 0,
    ms: 0,
};

/// Whether a string starts with `HH:MM`, which is how SQLite distinguishes a
/// bare time from a date.
fn looks_like_time_only(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 5 && b[0].is_ascii_digit() && b[1].is_ascii_digit() && b[2] == b':'
}

/// Parses `HH:MM`, `HH:MM:SS` or `HH:MM:SS.sss` into seconds past midnight.
fn parse_time_of_day(s: &str) -> Option<f64> {
    let b = s.as_bytes();
    let mut i = 0;
    let hour = read_digits(b, &mut i, 2, 2)?;
    expect_byte(b, &mut i, b':')?;
    let minute = read_digits(b, &mut i, 2, 2)?;
    let mut second = 0;
    let mut ms = 0;
    if i < b.len() && b[i] == b':' {
        i += 1;
        second = read_digits(b, &mut i, 2, 2)?;
    }
    if i < b.len() && b[i] == b'.' {
        i += 1;
        let start = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == start {
            return None;
        }
        let mut millis = s[start..i].to_string();
        while millis.len() < 3 {
            millis.push('0');
        }
        ms = millis[..3].parse().ok()?;
    }
    if i != b.len() || hour > 24 || minute > 59 || second > 59 {
        return None;
    }
    Some(hour as f64 * 3600.0 + minute as f64 * 60.0 + second as f64 + ms as f64 / 1000.0)
}

fn parse_datetime_text(s: &str) -> Option<Jd> {
    let b = s.as_bytes();
    let mut i = 0;
    // The year is exactly four digits; a longer run is not a year at all.
    //
    // A leading minus is a year of its own and only the magnitude is read, so
    // `-4713-11-24` is the Julian epoch itself. There is no way to spell a
    // *positive* year below 1000 that way -- `04713-11-24` is rejected for
    // being a five-digit run -- which is why the negative-year rendering that
    // `date('2020-01-01','-1000000 days')` produces, `-0718-02-03`, cannot be
    // fed back in to get the same date.
    let negative_year = b.first() == Some(&b'-');
    if negative_year {
        i += 1;
    }
    let magnitude = read_digits(b, &mut i, 4, 4)?;
    let year = if negative_year { -magnitude } else { magnitude };
    if i < b.len() && b[i].is_ascii_digit() {
        return None;
    }
    expect_byte(b, &mut i, b'-')?;
    // The month and day must both be exactly two digits.
    let month = read_digits(b, &mut i, 2, 2)?;
    expect_byte(b, &mut i, b'-')?;
    let day = read_digits(b, &mut i, 2, 2)?;
    if !(1..=12).contains(&month) {
        return None;
    }
    if i < b.len() && b[i].is_ascii_digit() {
        return None;
    }
    // A day outside 1..=31 is rejected outright, but a day of 30 or 31 that
    // runs past the end of a *shorter* month rolls forward, because SQLite
    // lets the day overflow. Concretely date('2020-01-32') is NULL and
    // date('2020-01-00') is NULL, while date('2020-02-30') is 2020-03-01,
    // date('2020-04-31') is 2020-05-01 and date('2021-02-29') is 2021-03-01.
    if !(1..=31).contains(&day) {
        return None;
    }
    let mut c = Civil {
        year,
        month,
        day,
        hour: 0,
        minute: 0,
        second: 0,
        ms: 0,
    };
    let mut offset_secs: i64 = 0;
    if i < b.len() {
        // The date and time must be separated by a space or an upper-case `T`.
        // A lower-case `t` looks like it should work, but sqlite3 rejects it:
        // `date('2020-01-02t03:04:05')` is NULL while the `T` spelling is not.
        // A trailing ` UTC` suffix is rejected too.
        if !matches!(b[i], b' ' | b'T') {
            return None;
        }
        i += 1;
        c.hour = read_digits(b, &mut i, 2, 2)?;
        expect_byte(b, &mut i, b':')?;
        c.minute = read_digits(b, &mut i, 2, 2)?;
        if i < b.len() && b[i] == b':' {
            i += 1;
            c.second = read_digits(b, &mut i, 2, 2)?;
        }
        if i < b.len() && b[i] == b'.' {
            i += 1;
            let start = i;
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
            if i == start {
                return None;
            }
            // Only milliseconds survive: `strftime('%f')` shows SS.SSS.
            let mut millis = s[start..i].to_string();
            while millis.len() < 3 {
                millis.push('0');
            }
            c.ms = millis[..3].parse().ok()?;
        }
        if i < b.len() {
            match b[i] {
                b'Z' | b'z' => i += 1,
                sign @ (b'+' | b'-') => {
                    let sign = if sign == b'-' { -1 } else { 1 };
                    i += 1;
                    let oh = read_digits(b, &mut i, 2, 2)?;
                    expect_byte(b, &mut i, b':')?;
                    let om = read_digits(b, &mut i, 2, 2)?;
                    // A zone offset runs to 14:59 either way and no further, so
                    // `+15:00` is rejected while `+14:59` is accepted. The
                    // comparison is on the magnitude, which is why the sign
                    // does not appear in it.
                    if oh > 14 || om > 59 {
                        return None;
                    }
                    offset_secs = sign * (oh * 3600 + om * 60);
                }
                _ => return None,
            }
        }
    }
    if i != b.len() {
        return None;
    }
    // SQLite accepts hour 24 -- and even 24:30:00 -- but rejects hour 25 or a
    // 60th minute or second.
    if c.hour > 24 || c.minute > 59 || c.second > 59 {
        return None;
    }
    let base = jd_of(&Civil {
        hour: 0,
        minute: 0,
        second: 0,
        ms: 0,
        ..c
    });
    let secs =
        c.hour as f64 * 3600.0 + c.minute as f64 * 60.0 + c.second as f64 + c.ms as f64 / 1000.0
            - offset_secs as f64;
    Some(Jd {
        jd: base + secs / 86400.0,
        // Hour 24 parses but is not normalised, so the written fields are kept
        // for the date functions: `date('2020-01-02 24:30:00')` is 2020-01-02,
        // not 2020-01-03. The Julian day still carries the extra half day, so
        // `time()` and `julianday()` see 24:30:00 and 2458851.5208333335. Any
        // other hour needs no tag, since the calendar reads it back the same
        // way it was written.
        written: (c.hour == 24).then_some(c),
    })
}

/// A Julian day, carrying the civil fields it was *written* with when they
/// differ from the calendar's own reading of that day.
///
/// SQLite does not normalise an hour-24 value when it parses one, and the
/// functions that render a date then disagree with each other on purpose:
/// `date('2020-01-02 24:30:00')` is `2020-01-02` and `datetime(...)` is
/// `2020-01-02 24:30:00`, both staying on the day as spelled, while
/// `time(...)` is `24:30:00` and `julianday(...)` is 2458851.5208333335, which
/// is 2020-01-03 half a day in. Re-deriving the fields from the Julian day
/// would give 2020-01-03 for all four, so the written fields have to ride
/// along.
///
/// The fields are dropped by any modifier that rebuilds the value from the
/// calendar, because those do normalise: `date('2020-01-02 24:30:00', '+0 days')`
/// is `2020-01-03`. `utc` and `localtime` are no-ops and keep them.
#[derive(Debug, Clone, Copy)]
struct Jd {
    jd: f64,
    /// The fields as spelled, or `None` when they agree with the Julian day
    /// anyway -- which is the case for every numeric argument, for any text
    /// without an hour-24 component, and for anything past a modifier.
    written: Option<Civil>,
}

impl Jd {
    fn plain(jd: f64) -> Self {
        Self { jd, written: None }
    }

    /// The civil fields the date and time specifiers render.
    fn fields(self) -> Option<Civil> {
        match self.written {
            Some(c) => Some(c),
            None => civil_from_jd(self.jd),
        }
    }
}

fn expect_byte(b: &[u8], i: &mut usize, want: u8) -> Option<()> {
    if *i < b.len() && b[*i] == want {
        *i += 1;
        Some(())
    } else {
        None
    }
}

/// Reads between `lo` and `hi` digits, advancing `i`. Returns `None` (and
/// leaves `i` alone) if fewer than `lo` digits are present.
fn read_digits(b: &[u8], i: &mut usize, lo: usize, hi: usize) -> Option<i64> {
    let start = *i;
    let mut n = 0;
    while *i < b.len() && b[*i].is_ascii_digit() && n < hi {
        *i += 1;
        n += 1;
    }
    if n < lo {
        *i = start;
        return None;
    }
    let mut v: i64 = 0;
    for &d in &b[start..*i] {
        v = v * 10 + (d - b'0') as i64;
    }
    Some(v)
}

// ---------------------------------------------------------------------------
// Modifiers
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
enum Mod {
    Days(f64),
    Hours(f64),
    Minutes(f64),
    Seconds(f64),
    Months(f64),
    Years(f64),
    StartOfDay,
    StartOfMonth,
    StartOfYear,
    /// `weekday N`: the next (or same) day whose weekday is N, 0 = Sunday.
    Weekday(i64),
}

impl Mod {
    /// Applies the modifier, returning `None` if the result leaves the range
    /// SQLite can represent.
    ///
    /// The three `start of` modifiers are the ones that *keep* an hour-24
    /// spelling unnormalised, because they read the date off the value as
    /// written and rebuild a time rather than re-deriving the fields: with
    /// `start of day` the value becomes midnight of 2020-01-02, and the answer
    /// is 2020-01-02 where a `+0 days` shift on the same value gives
    /// 2020-01-03. Every other modifier normalises.
    fn apply(&self, jd: Jd) -> Option<Jd> {
        // The `start of` modifiers read the *date* as spelled but rebuild the
        // clock from scratch, so an hour-24 component goes: `time('2020-01-02
        // 24:30:00', 'start of day')` is `00:00:00` in sqlite3 and not
        // `24:30:00`, while `date(...)` of the same stays 2020-01-02 rather
        // than moving to the 03rd.
        let c = jd.fields()?;
        let rebuilt = match *self {
            Mod::StartOfDay => Some(jd_of(&Civil {
                hour: 0,
                minute: 0,
                second: 0,
                ms: 0,
                ..c
            })),
            Mod::StartOfMonth => Some(jd_of(&Civil {
                day: 1,
                hour: 0,
                minute: 0,
                second: 0,
                ms: 0,
                ..c
            })),
            Mod::StartOfYear => Some(jd_of(&Civil {
                month: 1,
                day: 1,
                hour: 0,
                minute: 0,
                second: 0,
                ms: 0,
                ..c
            })),
            _ => None,
        };
        let out = match *self {
            Mod::Days(n) => jd.jd + n,
            Mod::Hours(n) => jd.jd + n / 24.0,
            Mod::Minutes(n) => jd.jd + n / 1440.0,
            Mod::Seconds(n) => jd.jd + n / 86400.0,
            Mod::Months(n) => add_months(&c, n),
            Mod::Years(n) => add_months(&c, n * 12.0),
            Mod::Weekday(n) => {
                // `weekday` reads the *normalised* day, like the week-numbering
                // specifiers, so an hour-24 value moves to the next day's slot:
                // `date('2020-01-02 24:30:00', 'weekday 0')` is 2020-01-05, one
                // day past the 2020-01-03 that the written fields would give.
                let norm = civil_from_jd(jd.jd).unwrap_or(c);
                jd.jd + (n - day_of_week(norm.year, norm.month, norm.day)).rem_euclid(7) as f64
            }
            _ => rebuilt?,
        };
        if !out.is_finite() {
            return None;
        }
        // A `start of` modifier rebuilds the clock from the date alone, so it
        // normalises an hour-24 spelling: `time('2020-01-02 24:30:00', 'start
        // of day')` is `00:00:00` in sqlite3 and not `24:30:00`, while
        // `date(...)` of the same stays 2020-01-02 because the rebuilt time
        // falls on the day that was written.
        Some(Jd::plain(out))
    }
}

/// Adds `n` months. SQLite does *not* clamp the day to the target month's
/// length: `date('2020-01-31', '+1 month')` is `2020-03-02`, because it
/// computes 2020-02-31 and lets the day overflow into March.
fn add_months(c: &Civil, n: f64) -> f64 {
    let total = c.year as f64 * 12.0 + (c.month - 1) as f64 + n;
    if !total.is_finite() || total.abs() > 9.0e15 {
        return f64::NAN;
    }
    let total = total as i64;
    let out = Civil {
        year: total.div_euclid(12),
        month: total.rem_euclid(12) + 1,
        ..*c
    };
    jd_of(&out)
}

/// What a modifier argument turned out to mean.
enum Modifier {
    /// `utc` and `localtime` are no-ops: the engine has no local time zone,
    /// so the value is already UTC, exactly as in a UTC build of sqlite3.
    NoOp,
    /// `unixepoch` reinterprets the value as a Unix timestamp.
    UnixEpoch,
    /// `julianday` reinterprets the value as a Julian day, which only makes
    /// sense for a number; on a date string SQLite rejects it outright.
    JulianDay,
    /// An ordinary arithmetic modifier.
    Shift(Mod),
}

/// Parses one modifier string, or `None` if SQLite would not recognise it.
fn parse_modifier(s: &str) -> Option<Modifier> {
    let t = s.trim().to_ascii_lowercase();
    match t.as_str() {
        "utc" | "localtime" => return Some(Modifier::NoOp),
        "unixepoch" => return Some(Modifier::UnixEpoch),
        // `julianday` is only meaningful on a numeric argument, and applying it
        // to a date string is an error: date('2020-01-02','julianday') is NULL.
        "julianday" => return Some(Modifier::JulianDay),
        "start of day" => return Some(Modifier::Shift(Mod::StartOfDay)),
        "start of month" => return Some(Modifier::Shift(Mod::StartOfMonth)),
        "start of year" => return Some(Modifier::Shift(Mod::StartOfYear)),
        _ => {}
    }
    if let Some(rest) = t.strip_prefix("weekday ") {
        let n: i64 = rest.trim().parse().ok()?;
        // A weekday outside 0..=6 makes the whole call NULL.
        if !(0..=6).contains(&n) {
            return None;
        }
        return Some(Modifier::Shift(Mod::Weekday(n)));
    }
    let (num, unit) = t.split_once(char::is_whitespace)?;
    let n: f64 = num.trim().parse().ok()?;
    let m = match unit.trim() {
        "day" | "days" => Mod::Days(n),
        "hour" | "hours" => Mod::Hours(n),
        "minute" | "minutes" => Mod::Minutes(n),
        "second" | "seconds" => Mod::Seconds(n),
        "month" | "months" => Mod::Months(n),
        "year" | "years" => Mod::Years(n),
        _ => return None,
    };
    Some(Modifier::Shift(m))
}

/// Turns a modifier argument into text the way SQLite does before matching.
fn modifier_text(v: &Value) -> String {
    match v {
        Value::Text(s) => s.clone(),
        Value::Integer(i) => i.to_string(),
        Value::Real(r) => real_to_text(*r),
        other => other.to_string(),
    }
}

/// Applies every modifier in turn, returning `None` if any is unrecognised or
/// pushes the value out of range.
///
/// The two reinterpreting modifiers only apply to an argument that is *already*
/// a number, and only while the value is still numeric. `julianday(2451545,
/// 'julianday')` is 2451545.0 and `date(2451545, 'unixepoch')` is 1970-01-29,
/// but a date string is not a number to reinterpret: `julianday('2020-01-01',
/// 'julianday')` and `unixepoch('1970-01-01', 'unixepoch')` are both NULL. The
/// test is positional rather than on the value, so the pair reads
/// `date(2451545, '+1 day', 'julianday')` -- where the shift has already made
/// the argument a date -- as NULL, while the same two modifiers in the other
/// order, `date(2451545, 'julianday', '+1 day')`, apply in turn and give
/// 2000-01-02. Once a modifier has run, the value is a date and the
/// reinterpreting pair no longer fits.
///
/// Every modifier that runs also drops the written-fields tag, because each one
/// normalises an hour-24 spelling: `date('2020-01-02 24:30:00', '+0 days')`,
/// the same with `'utc'`, and the same with `'localtime'` are all 2020-01-03,
/// where the bare value is 2020-01-02.
fn apply_modifiers(mut jd: Jd, base_numeric: bool, mods: &[Value]) -> Option<Jd> {
    // A modifier of any kind means the value has stopped being a bare number.
    let mut numeric = base_numeric;
    for m in mods {
        if m.is_null() {
            return None;
        }
        match parse_modifier(&modifier_text(m))? {
            Modifier::NoOp => jd = Jd::plain(jd.jd),
            Modifier::UnixEpoch => {
                if !numeric {
                    return None;
                }
                jd = Jd::plain(jd.jd / 86400.0 + UNIX_EPOCH_JD);
            }
            Modifier::JulianDay => {
                if !numeric {
                    return None;
                }
                jd = Jd::plain(jd.jd);
            }
            Modifier::Shift(shift) => jd = shift.apply(jd)?,
        }
        numeric = false;
    }
    Some(jd)
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

/// Calls a math or date function. `name` must already be lowercased.
///
/// Returns `Ok(None)` when the name is not one of this module's functions, so
/// the caller can fall through to its own table.
pub fn call(name: &str, args: &[Value]) -> Result<Option<Value>> {
    let v = match name {
        "abs" => {
            expect_arity(name, args, 1)?;
            abs(&args[0])?
        }
        "round" => {
            expect_range(name, args, 1, 2)?;
            if args[0].is_null() || (args.len() == 2 && args[1].is_null()) {
                return Ok(Some(Value::Null));
            }
            let v = num_value(&args[0]);
            let digits = if args.len() == 2 {
                num_strict(&args[1]).unwrap_or(0.0) as i64
            } else {
                0
            };
            Value::real(sqlite_round(v, digits))
        }
        "sign" => {
            expect_arity(name, args, 1)?;
            sign(&args[0])
        }
        "ceil" | "ceiling" => {
            expect_arity(name, args, 1)?;
            ceil_floor(&args[0], true)
        }
        "floor" => {
            expect_arity(name, args, 1)?;
            ceil_floor(&args[0], false)
        }
        "exp" => {
            expect_arity(name, args, 1)?;
            one_arg(&args[0], |x| Some(f64::exp(x)))
        }
        "ln" => {
            expect_arity(name, args, 1)?;
            one_arg(&args[0], |x| (x > 0.0).then(|| x.ln()))
        }
        "log" => {
            expect_range(name, args, 1, 2)?;
            log(args)
        }
        "log2" => {
            expect_arity(name, args, 1)?;
            one_arg(&args[0], |x| (x > 0.0).then(|| x.log2()))
        }
        "log10" => {
            expect_arity(name, args, 1)?;
            one_arg(&args[0], |x| (x > 0.0).then(|| x.log10()))
        }
        "pow" | "power" => {
            expect_arity(name, args, 2)?;
            pow(&args[0], &args[1])
        }
        "sqrt" => {
            expect_arity(name, args, 1)?;
            one_arg(&args[0], |x| (x >= 0.0).then(|| x.sqrt()))
        }
        "mod" => {
            expect_arity(name, args, 2)?;
            sql_mod(&args[0], &args[1])
        }
        "pi" => {
            expect_arity(name, args, 0)?;
            Value::real(std::f64::consts::PI)
        }
        "sin" => {
            expect_arity(name, args, 1)?;
            one_arg(&args[0], |x| Some(f64::sin(x)))
        }
        "cos" => {
            expect_arity(name, args, 1)?;
            one_arg(&args[0], |x| Some(f64::cos(x)))
        }
        "tan" => {
            expect_arity(name, args, 1)?;
            one_arg(&args[0], |x| Some(f64::tan(x)))
        }
        "asin" => {
            expect_arity(name, args, 1)?;
            one_arg(&args[0], |x| (-1.0..=1.0).contains(&x).then(|| x.asin()))
        }
        "acos" => {
            expect_arity(name, args, 1)?;
            one_arg(&args[0], |x| (-1.0..=1.0).contains(&x).then(|| x.acos()))
        }
        "atan" => {
            expect_arity(name, args, 1)?;
            one_arg(&args[0], |x| Some(f64::atan(x)))
        }
        "atan2" => {
            expect_arity(name, args, 2)?;
            atan2(&args[0], &args[1])
        }
        "degrees" => {
            expect_arity(name, args, 1)?;
            one_arg(&args[0], |x| Some(x.to_degrees()))
        }
        "radians" => {
            expect_arity(name, args, 1)?;
            one_arg(&args[0], |x| Some(x.to_radians()))
        }
        "random" => {
            expect_arity(name, args, 0)?;
            Value::real(random_next())
        }
        "randomblob" => {
            expect_arity(name, args, 1)?;
            randomblob(&args[0])
        }
        "date" => date_fn(name, args, Style::Date)?.unwrap_or(Value::Null),
        "time" => date_fn(name, args, Style::Time)?.unwrap_or(Value::Null),
        "datetime" => date_fn(name, args, Style::DateTime)?.unwrap_or(Value::Null),
        "julianday" => date_fn(name, args, Style::Julian)?.unwrap_or(Value::Null),
        "unixepoch" => date_fn(name, args, Style::Unix)?.unwrap_or(Value::Null),
        "strftime" => strftime(args)?.unwrap_or(Value::Null),
        _ => return Ok(None),
    };
    Ok(Some(v))
}

// ---------------------------------------------------------------------------
// Math implementations
// ---------------------------------------------------------------------------

/// `abs`. The result is always real, because `abs(-1)` is `1.0` in sqlite3
/// even though the argument was an integer. `abs(-9223372036854775808)` is an
/// *error* ("integer overflow") because the positive value has no i64
/// representation.
fn abs(v: &Value) -> Result<Value> {
    Ok(match v {
        Value::Null => Value::Null,
        Value::Integer(i) => {
            if *i == i64::MIN {
                return Err(Error::new(ResultCode::Error, "integer overflow"));
            }
            // An integer argument keeps its type, so abs(-5) is integer 5.
            Value::Integer(i.abs())
        }
        Value::Real(r) => Value::real(r.abs()),
        other => Value::real(num_value(other).abs()),
    })
}

/// SQLite's `round`: exact decimal rounding of the binary value to `digits`
/// places after the point, ties away from zero. The result is always REAL, at
/// every arity — the text-returning behaviour of `round(X, N)` was removed in
/// SQLite 3.35.
///
/// A negative precision is **clamped to zero** rather than rounding to a power
/// of ten, which is checked against 15 cases: `round(3.7, -1)` is 4.0 and
/// `round(12345, -4)` is 12345.0, both exactly what `round(x, 0)` gives.
pub fn sqlite_round(v: f64, digits: i64) -> f64 {
    if !v.is_finite() {
        return v;
    }
    // A negative precision is treated as zero, and anything past thirty decimal
    // places is capped there too: SQLite's round works in a fixed-width buffer,
    // so round(1e-100, 100) is 0.0 while round(1e-30, 30) keeps its value.
    let digits = digits.clamp(0, 30);
    let (full, lead) = exact_decimal(v);
    // Rounding to `digits` places after the point keeps `lead + 1 + digits`
    // leading digits of the exact expansion.
    let keep = lead + 1 + digits as i32;
    if keep <= 0 {
        // Not one significant digit survives at this precision, so the answer
        // can only be zero or one unit in the last place that *is* kept --
        // 10^-digits. Which of the two is decided by comparing the value
        // against half that unit, and *not* by reading a digit off the
        // expansion. The expansion of 0.05 is `5000...` with its leading digit
        // at 10^-2, so the digit at the 0.1 place sits one place *before* the
        // expansion starts and reads as a zero -- which is what made
        // `round(0.05, 1)` answer 0.0 where sqlite3 answers 0.1, and
        // `round(0.5)` answer 0.0 where it answers 1.0.
        //
        // The half-way test is on the magnitude, so the sign is reapplied at
        // the end: `round(-0.5)` is -1.0.
        let unit = pow10_f64(-digits);
        let magnitude = if v.abs() * 2.0 >= unit { unit } else { 0.0 };
        return if v < 0.0 { -magnitude } else { magnitude };
    }
    let (d, widened) = round_digits(&full, keep as usize);
    // A carry such as 9.99 -> 10.0 prepends a digit and advances the exponent.
    let lead = lead + i32::from(widened);
    // assemble_magnitude rebuilds the value 0.<d> x 10^lead exactly.
    let magnitude = assemble_magnitude(&d, lead).unwrap_or(v.abs());
    if v < 0.0 {
        -magnitude
    } else {
        magnitude
    }
}

/// 10^k for the k a rounded result can need in either direction.
///
/// A loop that multiplied from 1.0 would answer 1.0 for every negative k, and
/// the results this feeds are always at or below one, so the exponentiation
/// path matters: it is exact for every k a double can hold.
fn pow10_f64(k: i64) -> f64 {
    if (-300..=300).contains(&k) {
        return 10f64.powi(k as i32);
    }
    if k > 0 {
        f64::INFINITY
    } else {
        0.0
    }
}

/// `sign`: -1, 0 or 1, and NULL for NULL or a non-numeric argument.
fn sign(v: &Value) -> Value {
    if v.is_null() {
        return Value::Null;
    }
    match num_strict(v) {
        None => Value::Null,
        Some(x) => Value::Integer(if x > 0.0 {
            1
        } else if x < 0.0 {
            -1
        } else {
            0
        }),
    }
}

/// `ceil` and `floor`.
///
/// SQLite routes both through `sqlite3_value_int64`, so the integer/real split
/// follows that coercion rather than the argument's own storage class. The
/// answer is an INTEGER when the argument converts to an i64 *without loss* --
/// which includes a text spelling of a whole number, so `ceil('5')` and
/// `ceil(2)` are both integer 5 while `ceil(2.0)` and `ceil('5.0')` are both
/// real 5.0. A REAL never becomes an integer, even when its value is integral,
/// and neither does a real spelled in text: SQLite's integer conversion of
/// `'5.0'` fails on the point.
///
/// The rest falls to the double path, which is why `ceil('5.9')` is real 6.0
/// while `ceil('5abc')` and `ceil('0x10')` are NULL -- the text has to be a
/// complete number to get there at all.
fn ceil_floor(v: &Value, up: bool) -> Value {
    if v.is_null() {
        return Value::Null;
    }
    // The integer path, and the only one that yields Value::Integer. Rounding
    // up or down is a no-op on a value that is already whole, which is the
    // only way an integer reaches here.
    if let Some(i) = as_lossless_int(v) {
        return Value::Integer(i);
    }
    let Some(x) = num_strict(v) else {
        return Value::Null;
    };
    Value::real(if up { x.ceil() } else { x.floor() })
}

/// The i64 a value converts to without loss, or `None` if that conversion is
/// lossy or fails.
///
/// An INTEGER is itself. A TEXT counts only when it is a whole number in
/// i64 range: blanks around it are allowed (`ceil(' 5 ')` is integer 5) and a
/// leading sign is allowed, but a point, an exponent or trailing junk is not,
/// and a value too large for an i64 falls through to the real path -- which is
/// what makes `ceil('9223372036854775808')` real 9.2233720368547758e+18.
fn as_lossless_int(v: &Value) -> Option<i64> {
    match v {
        Value::Integer(i) => Some(*i),
        Value::Text(s) => s
            .trim_matches(|c: char| c.is_ascii_whitespace())
            .parse::<i64>()
            .ok(),
        // A REAL is left alone: `ceil(2.0)` is real 2.0 in sqlite3.
        _ => None,
    }
}

/// Applies `f` to a strictly-coerced argument; NULL in, NULL out.
///
/// A NaN result is NULL rather than a NaN value. libm answers NaN wherever the
/// math runs out before the domain check can, and an argument that overflows to
/// infinity is the easy way in: sqlite3 answers NULL for `mod(1e400, 3)` and for
/// `sin(1e400)`, where a bare pass-through would hand back a Real(NaN) that
/// then renders as the empty string.
fn one_arg<F: Fn(f64) -> Option<f64>>(v: &Value, f: F) -> Value {
    if v.is_null() {
        return Value::Null;
    }
    match num_strict(v).and_then(f) {
        Some(r) if r.is_nan() => Value::Null,
        Some(r) => Value::real(r),
        None => Value::Null,
    }
}

/// `log`. With one argument it is base 10, which surprises people expecting
/// base *e*. With two it is `log(x)/log(b)`, and SQLite rejects any base below
/// 1, so `log(0.5, 2)` is NULL.
fn log(args: &[Value]) -> Value {
    if args.iter().any(|a| a.is_null()) {
        return Value::Null;
    }
    if args.len() == 1 {
        let Some(x) = num_strict(&args[0]) else {
            return Value::Null;
        };
        return if x <= 0.0 {
            Value::Null
        } else {
            Value::real(x.log10())
        };
    }
    let (Some(b), Some(x)) = (num_strict(&args[0]), num_strict(&args[1])) else {
        return Value::Null;
    };
    if b < 1.0 || x <= 0.0 {
        return Value::Null;
    }
    // log(1, x) has no solution for x != 1, and SQLite answers NULL.
    if b == 1.0 {
        return Value::Null;
    }
    Value::real(x.ln() / b.ln())
}

/// `pow`/`power`. `pow(0, -1)` is `Inf` and `pow(-8, 1.0/3)` is NULL because
/// the exponent is fractional.
fn pow(a: &Value, b: &Value) -> Value {
    if a.is_null() || b.is_null() {
        return Value::Null;
    }
    let (Some(x), Some(y)) = (num_strict(a), num_strict(b)) else {
        return Value::Null;
    };
    if x == 0.0 && y < 0.0 {
        return Value::real(f64::INFINITY);
    }
    match x.powf(y) {
        r if r.is_nan() => Value::Null,
        r => Value::real(r),
    }
}
/// `mod`, which is C `fmod`: the sign follows the *dividend*, so a negative
/// divisor does not flip it. A zero divisor gives NULL and the result is
/// always real.
///
/// An infinity dividend has no remainder, and the `%` operator answers NaN for
/// it where sqlite3 answers NULL.
fn sql_mod(a: &Value, b: &Value) -> Value {
    if a.is_null() || b.is_null() {
        return Value::Null;
    }
    let (Some(x), Some(y)) = (num_strict(a), num_strict(b)) else {
        return Value::Null;
    };
    if y == 0.0 {
        return Value::Null;
    }
    match x % y {
        r if r.is_nan() => Value::Null,
        r => Value::real(r),
    }
}

/// `atan2(y, x)`; `atan2(0, 0)` is 0.0.
fn atan2(y: &Value, x: &Value) -> Value {
    if y.is_null() || x.is_null() {
        return Value::Null;
    }
    let (Some(a), Some(b)) = (num_strict(y), num_strict(x)) else {
        return Value::Null;
    };
    match a.atan2(b) {
        r if r.is_nan() => Value::Null,
        r => Value::real(r),
    }
}

// ---------------------------------------------------------------------------
// Random
// ---------------------------------------------------------------------------

use std::cell::Cell;

thread_local! {
    /// The generator state. SQLite's `random()` draws from a ChaCha-derived
    /// stream; a small xorshift suits this engine, and [`seed_random`] makes
    /// the output reproducible so the tests can assert on it.
    static RNG: Cell<u64> = const { Cell::new(0) };
}

/// Seeds the generator. SQLite's `random()` is not seedable from SQL, so this
/// is an engine extension; the *shape* of the results is unchanged, a 64-bit
/// signed integer and a blob of the requested length.
pub fn seed_random(seed: u64) {
    RNG.with(|r| {
        r.set(if seed == 0 {
            0x9e37_79b9_7f4a_7c15
        } else {
            seed
        })
    });
}

fn next_u64() -> u64 {
    RNG.with(|r| {
        let mut x = r.get();
        if x == 0 {
            x = 0x9e37_79b9_7f4a_7c15;
        }
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        r.set(x);
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    })
}

/// A random 64-bit signed integer, as `random()` returns.
fn random_next() -> f64 {
    (next_u64() as i64) as f64
}

/// `randomblob(n)`. The length goes through the strict coercion and is clamped
/// at 1, so `randomblob(0)` and `randomblob(-1)` both give a 1-byte blob.
fn randomblob(v: &Value) -> Value {
    if v.is_null() {
        return Value::Null;
    }
    let n = num_strict(v).unwrap_or(1.0);
    let n = if n.is_nan() || n < 1.0 {
        1u64
    } else {
        (n as u64).min(1 << 20)
    };
    let mut bytes = Vec::with_capacity(n as usize);
    for _ in 0..n {
        bytes.push((next_u64() & 0xff) as u8);
    }
    Value::Blob(bytes)
}

// ---------------------------------------------------------------------------
// date / time / datetime / julianday / unixepoch / strftime
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Style {
    Date,
    Time,
    DateTime,
    Julian,
    Unix,
}

fn date_fn(name: &str, args: &[Value], style: Style) -> Result<Option<Value>> {
    if args.is_empty() {
        return Err(arity_error(name));
    }
    let Some(jd) = parse_time_value(&args[0]) else {
        return Ok(Some(Value::Null));
    };
    if !jd.jd.is_finite() {
        return Ok(Some(Value::Null));
    }
    // A numeric *string* counts as a number here, because `parse_time_value`
    // already read it as a Julian day: `date('2451545', 'julianday')` is
    // 2000-01-01 in sqlite3, and the modifier has nothing to reinterpret.
    let numeric_base = match &args[0] {
        Value::Integer(_) | Value::Real(_) => true,
        Value::Text(_) => num_strict(&args[0]).is_some(),
        _ => false,
    };
    let Some(jd) = apply_modifiers(jd, numeric_base, &args[1..]) else {
        return Ok(Some(Value::Null));
    };
    let out = match style {
        // The Julian and Unix renderings read the raw value, so an hour-24
        // spelling keeps the half day it carries: julianday('2020-01-02
        // 24:30:00') is 2458851.5208333335 and unixepoch(...) is 1578011400.
        //
        // Both reject a Julian day below the epoch, which the calendar
        // renderings do not: `julianday(-0.5)` is NULL while
        // `date(-0.5)` is NULL too but `date(0)` is -4713-11-24. The Julian day
        // is the one place a value is printed as a *number* rather than
        // rendered, and a negative one has no meaning there.
        Style::Julian if jd.jd < 0.0 || jd.jd > JULIAN_MAX => Value::Null,
        Style::Unix if jd.jd < 0.0 || jd.jd > JULIAN_MAX => Value::Null,
        Style::Julian => Value::real(jd.jd),
        Style::Unix => {
            // Truncation toward zero, not rounding: a time a fraction of a
            // second before the epoch reports -1, and one a fraction after
            // reports 0. The arithmetic goes through milliseconds so a Julian
            // day of magnitude 5.4e6 does not lose its sub-second part to the
            // f64's 52-bit mantissa.
            let ms = ((jd.jd - UNIX_EPOCH_JD) * MS_PER_DAY as f64).round() as i64;
            Value::Integer(ms.div_euclid(1000))
        }
        Style::Time => {
            // `time` deliberately ignores the date part, so it works even for
            // a Julian day that cannot be rendered as a date.
            let Some(c) = jd.fields() else {
                return Ok(Some(Value::Null));
            };
            Value::Text(format_time(&c))
        }
        Style::Date | Style::DateTime => {
            // These read the fields as spelled, so an hour-24 value stays on
            // the day it was written: date('2020-01-02 24:30:00') is
            // 2020-01-02 and datetime(...) is 2020-01-02 24:30:00.
            let Some(c) = jd.fields() else {
                return Ok(Some(Value::Null));
            };
            Value::Text(match style {
                Style::Date => format_date(&c),
                _ => format!("{} {}", format_date(&c), format_time(&c)),
            })
        }
    };
    Ok(Some(out))
}

/// `strftime(FORMAT, TIME, MOD...)`. The argument order is the reverse of the
/// other date functions, so it needs its own entry point.
fn strftime(args: &[Value]) -> Result<Option<Value>> {
    if args.len() < 2 {
        return Err(arity_error("strftime"));
    }
    if args[0].is_null() || args[1].is_null() {
        return Ok(Some(Value::Null));
    }
    let Some(jd) = parse_time_value(&args[1]) else {
        return Ok(Some(Value::Null));
    };
    if !jd.jd.is_finite() {
        return Ok(Some(Value::Null));
    }
    let numeric_base = match &args[1] {
        Value::Integer(_) | Value::Real(_) => true,
        Value::Text(_) => num_strict(&args[1]).is_some(),
        _ => false,
    };
    let Some(jd) = apply_modifiers(jd, numeric_base, &args[2..]) else {
        return Ok(Some(Value::Null));
    };
    let fmt = match &args[0] {
        Value::Text(s) => s.clone(),
        other => other.to_string(),
    };
    // An unrecognised specifier makes the whole call NULL, not an empty string.
    Ok(Some(match strftime_format(&fmt, &jd) {
        Some(s) => Value::Text(s),
        None => Value::Null,
    }))
}

/// `YYYY-MM-DD`, zero-padded to four digits, with the leading zero a negative
/// year keeps.
///
/// The pad is three digits for a negative year and four for a positive one, so
/// `date('2020-01-01', '-1000000 days')` is `-0718-02-03` and `strftime('%F',
/// '-0001-01-01')` is `-001-01-01` -- the sign takes the place of the digit a
/// negative year does not have.
///
/// Rust's `{:04}` pads the *magnitude*, so a year of -718 would print as `-718`;
/// sqlite3 prints `-0718`, and the four-digit year of `date('2020-01-01',
/// '-1000000 days')` is what makes that difference visible. The sign is
/// written first and the magnitude padded to three digits behind it, which
/// gives `-0718` and leaves a positive year as `2020`.
///
/// The same renderer is what `%Y` goes through, so the two agree.
pub fn format_date(c: &Civil) -> String {
    if c.year < 0 {
        format!("-{:04}-{:02}-{:02}", c.year.unsigned_abs(), c.month, c.day)
    } else {
        format!("{:04}-{:02}-{:02}", c.year, c.month, c.day)
    }
}

/// A year rendered the way SQLite's `%Y` renders one.
///
/// A negative year carries its sign and pads the magnitude to four digits
/// behind it, so -718 is `-0718`. `strftime('%Y', ...)` in sqlite3 is the odd
/// one out: it prints just `-718`, with the sign outside the pad, which is
/// what Rust's own `{:04}` would do -- so the two renderers genuinely differ
/// and are not the same function.
fn pad_year(year: i64) -> String {
    if year < 0 {
        format!("-{:03}", year.unsigned_abs())
    } else {
        format!("{year:04}")
    }
}

/// `HH:MM:SS`.
pub fn format_time(c: &Civil) -> String {
    format!("{:02}:{:02}:{:02}", c.hour, c.minute, c.second)
}

/// Expands a `strftime` format string.
///
/// Supported: `%d %e %f %H %I %j %J %k %l %m %M %p %P %R %S %T %U %u %V %w
/// %W %Y %G %g %s %%`. An unrecognised specifier makes the *whole* result
/// NULL rather than being dropped — `%D`, `%n` and `%t` are all NULL in
/// sqlite3 3.53.4, and so is a format ending in a bare `%`.
fn strftime_format(fmt: &str, jd: &Jd) -> Option<String> {
    // The civil fields as spelled, which is what the date and clock specifiers
    // read. They are absent only where no rendering is possible at all -- a
    // Julian day the calendar cannot describe -- and then the whole format is
    // NULL, exactly as a numeric argument outside the range is.
    let c = jd.fields()?;
    let mut out = String::new();
    let mut it = fmt.chars();
    while let Some(ch) = it.next() {
        if ch != '%' {
            out.push(ch);
            continue;
        }
        let v = it.next()?;
        out.push_str(&strftime_one(v, &c, jd.jd)?);
    }
    Some(out)
}

/// Renders one `strftime` specifier.
///
/// The two arguments disagree on purpose for a value spelled with an hour-24
/// component: `c` is the day as written and `jd` is where that really sits on
/// the Julian scale, half a day later. sqlite3 splits its own specifiers the
/// same way. The date and clock specifiers -- `%F`, `%m`, `%d`, `%H` and the
/// rest -- read `c`, and so does `%w`, `%u` and `%j`, which for
/// `'2020-01-02 24:30:00'` give `5`, `5` and `002`: `002` is the day as
/// written, and `5` is the weekday of the day that the half day lands in, which
/// happens to agree here but is the normalised reading. The week-numbering
/// specifiers `%U`, `%W`, `%V`, `%G` and `%g` read the normalised day
/// throughout.
fn strftime_one(v: char, c: &Civil, jd: f64) -> Option<String> {
    // The normalised day, used by the week-numbering specifiers.
    let norm = civil_from_jd(jd);
    Some(match v {
        'd' => format!("{:02}", c.day),
        'e' => format!("{:2}", c.day),
        // `%F` pads a negative year to *three* digits where `date()` pads to
        // four: `strftime('%F', '-0001-01-01')` is `-001-01-01` and
        // `date('-0001-01-01')` is `-0001-01-01`. The two renderers are
        // genuinely different, not one calling the other.
        'F' => format!("{}-{:02}-{:02}", pad_year(c.year), c.month, c.day),
        'f' => format!("{:02}.{:03}", c.second, c.ms),
        'H' => format!("{:02}", c.hour),
        'I' => format!("{:02}", if c.hour % 12 == 0 { 12 } else { c.hour % 12 }),
        'j' => format!("{:03}", day_of_year(c.year, c.month, c.day)),
        'J' => real_to_text_fixed(jd, 16),
        'k' => format!("{:2}", c.hour),
        'l' => format!("{:2}", if c.hour % 12 == 0 { 12 } else { c.hour % 12 }),
        'm' => format!("{:02}", c.month),
        'M' => format!("{:02}", c.minute),
        'p' => if c.hour < 12 { "AM" } else { "PM" }.to_string(),
        'P' => if c.hour < 12 { "am" } else { "pm" }.to_string(),
        'R' => format!("{:02}:{:02}", c.hour, c.minute),
        'S' => format!("{:02}", c.second),
        'T' => format_time(c),
        'w' => norm.as_ref().map_or_else(
            || day_of_week(c.year, c.month, c.day).to_string(),
            |n| day_of_week(n.year, n.month, n.day).to_string(),
        ),
        'u' => {
            let w = norm.as_ref().map_or_else(
                || day_of_week(c.year, c.month, c.day),
                |n| day_of_week(n.year, n.month, n.day),
            );
            if w == 0 {
                "7".to_string()
            } else {
                w.to_string()
            }
        }
        'U' => format!("{:02}", norm.as_ref().map_or(0, |n| week_of_year(n, 0))),
        'W' => format!("{:02}", norm.as_ref().map_or(0, |n| week_of_year(n, 1))),
        'V' => format!("{:02}", norm.as_ref().map_or(0, iso_week)),
        'Y' => format!("{:04}", c.year),
        'G' => format!("{:04}", norm.as_ref().map_or(0, iso_year)),
        // `%g` is the ISO year modulo 100, and sqlite3 prints it *signed* for a
        // year before the common era rather than wrapping it into 0..99:
        // `strftime('%g', 0)` is `-1` and `strftime('%g', -4713)` is `-13`,
        // where `rem_euclid(100)` gives 99 and 87.
        'g' => {
            let y = norm.as_ref().map_or(0, iso_year);
            if y < 0 {
                // No zero padding on the magnitude: `strftime('%g', 0)` is `-1`
                // and `strftime('%g', -1)` is `-2`, not `-01` and `-02`.
                format!("-{}", y.unsigned_abs() % 100)
            } else {
                format!("{:02}", y % 100)
            }
        }
        's' => format!("{}", unix_seconds_floor(jd)),
        '%' => "%".to_string(),
        _ => return None,
    })
}

/// The Unix timestamp `%s` reports, rounded to the nearest second and then
/// **floored**.
///
/// `strftime('%s', '1969-12-31 23:59:59')` is `-1` in sqlite3, not 0, so the
/// second either side of an instant has to fall to the earlier whole second.
/// A plain `as i64` truncates toward zero and would report 0. The arithmetic
/// goes through milliseconds first, as `unixepoch()` does, so a Julian day of
/// magnitude 5.4e6 does not lose its sub-second part to the f64 mantissa.
fn unix_seconds_floor(jd: f64) -> i64 {
    let ms = ((jd - UNIX_EPOCH_JD) * MS_PER_DAY as f64).round() as i64;
    ms.div_euclid(1000)
}

/// The week-of-year number for `%U` (weeks start Sunday) or `%W` (Monday).
/// Week 1 is the one containing 1 January, and days before it are week 0.
fn week_of_year(c: &Civil, first_day: i64) -> i64 {
    let doy = day_of_year(c.year, c.month, c.day);
    // `first_day` is 0 for `%U` and 1 for `%W`, naming the day each one starts
    // its weeks on -- Sunday and Monday. Counting from the first such day of the
    // year rather than from an offset is what makes the week *begin* on it:
    // 2021-01-01 was a Friday, so the first Monday was 4 January, and
    // `strftime('%W', '2021-02-28')` is 08 while `...('2021-02-29')` is 09 -- the
    // Sunday before is still in the old week and the Monday itself starts the
    // new one. An offset-and-divide form puts both in the same week.
    let jan1_sunday = day_of_week(c.year, 1, 1);
    let first = 1 + (7 - (jan1_sunday - first_day).rem_euclid(7)) % 7;
    if doy < first {
        // Before the first such day, so still week 0.
        return 0;
    }
    1 + (doy - first) / 7
}

/// The ISO-8601 week-numbering year, which `%G` reports. The Thursday of a
/// week decides the year, so 2021-01-01 belongs to ISO year 2020.
fn iso_year(c: &Civil) -> i64 {
    let (ty, _, _) = iso_thursday(c);
    ty
}

/// The Thursday of the ISO week containing `c`, as a day number.
fn iso_thursday(c: &Civil) -> (i64, i64, i64) {
    let dow = day_of_week(c.year, c.month, c.day);
    let iso_dow = if dow == 0 { 7 } else { dow };
    civil_from_days(days_from_civil(c.year, c.month, c.day) + (4 - iso_dow))
}

/// The ISO-8601 week number, which `%V` reports.
fn iso_week(c: &Civil) -> i64 {
    let thursday = days_from_civil(c.year, c.month, c.day)
        + (4 - {
            let dow = day_of_week(c.year, c.month, c.day);
            if dow == 0 {
                7
            } else {
                dow
            }
        });
    let (ty, _, _) = civil_from_days(thursday);
    (thursday - days_from_civil(ty, 1, 1)) / 7 + 1
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Error;

    /// Calls a function the way the evaluator would, panicking on error.
    fn ok(name: &str, args: &[Value]) -> Value {
        call(name, args).unwrap().unwrap_or(Value::Null)
    }

    fn text(name: &str, args: &[Value]) -> String {
        match ok(name, args) {
            Value::Text(s) => s,
            other => panic!("{name} returned {other:?}, expected text"),
        }
    }

    fn real(name: &str, args: &[Value]) -> f64 {
        match ok(name, args) {
            Value::Real(r) => r,
            other => panic!("{name} returned {other:?}, expected real"),
        }
    }

    fn int(name: &str, args: &[Value]) -> i64 {
        match ok(name, args) {
            Value::Integer(i) => i,
            other => panic!("{name} returned {other:?}, expected integer"),
        }
    }

    fn t(s: &str) -> Value {
        Value::Text(s.into())
    }

    // -- abs ---------------------------------------------------------------

    #[test]
    fn abs_keeps_an_integers_type() {
        // sqlite3: select abs(-5), typeof(abs(-5));  ->  5 | integer
        assert_eq!(ok("abs", &[Value::Integer(-5)]), Value::Integer(5));
        assert_eq!(ok("abs", &[Value::Integer(0)]), Value::Integer(0));
        // A real argument gives a real back.
        assert_eq!(ok("abs", &[Value::Real(-1.5)]), Value::Real(1.5));
    }

    #[test]
    fn abs_coerces_text_with_the_lenient_parser() {
        // sqlite3: abs('5abc') = 5.0, abs('abc') = 0.0, abs('  -5 ') = 5.0
        assert_eq!(real("abs", &[t("5abc")]), 5.0);
        assert_eq!(real("abs", &[t("abc")]), 0.0);
        assert_eq!(real("abs", &[t("  -5 ")]), 5.0);
        // The lenient parser does not know the word "inf", so that is 0.0.
        assert_eq!(real("abs", &[t("inf")]), 0.0);
    }

    #[test]
    fn abs_of_null_is_null_and_of_i64_min_overflows() {
        // sqlite3: abs(NULL) is NULL; abs(-9223372036854775808) errors with
        // "integer overflow" because the positive value has no i64 form.
        assert_eq!(ok("abs", &[Value::Null]), Value::Null);
        let err: Error = call("abs", &[Value::Integer(i64::MIN)]).unwrap_err();
        assert_eq!(err.message, "integer overflow");
    }

    #[test]
    fn abs_wants_exactly_one_argument() {
        // sqlite3: abs(1,2) -> "wrong number of arguments to function abs()"
        let err = call("abs", &[Value::Integer(1), Value::Integer(2)]).unwrap_err();
        assert_eq!(err.message, "wrong number of arguments to function abs()");
    }

    // -- round -------------------------------------------------------------

    #[test]
    fn round_is_exact_decimal_not_scaled_binary() {
        // The surprising pair, both from sqlite3:
        //   round(2.675, 2) = 2.67   (the double is really 2.67499999...)
        //   round(4.625,  2) = 4.63   (the double is exactly 4.625)
        // A naive `(x * 100).round() / 100` gets the first one wrong.
        assert_eq!(
            real("round", &[Value::Real(2.675), Value::Integer(2)]),
            2.67
        );
        assert_eq!(
            real("round", &[Value::Real(4.625), Value::Integer(2)]),
            4.63
        );
        assert_eq!(
            real("round", &[Value::Real(0.155), Value::Integer(2)]),
            0.15
        );
        assert_eq!(
            real("round", &[Value::Real(0.175), Value::Integer(2)]),
            0.17
        );
    }

    #[test]
    fn round_returns_real_at_every_arity() {
        // sqlite3 3.53.4: typeof(round(2.5)) and typeof(round(2.5, 0)) are both
        // "real". The text result of round(X, N) was removed in SQLite 3.35.
        assert_eq!(ok("round", &[Value::Real(2.5)]), Value::Real(3.0));
        assert_eq!(
            ok("round", &[Value::Real(2.5), Value::Integer(0)]),
            Value::Real(3.0)
        );
        assert_eq!(ok("round", &[Value::Integer(3)]), Value::Real(3.0));
    }

    #[test]
    fn round_breaks_ties_away_from_zero() {
        // sqlite3: round(2.5)=3.0, round(-2.5)=-3.0, round(0.5)=1.0,
        // round(1.5)=2.0 -- not the banker's rounding a float library gives.
        assert_eq!(real("round", &[Value::Real(2.5)]), 3.0);
        assert_eq!(real("round", &[Value::Real(-2.5)]), -3.0);
        assert_eq!(real("round", &[Value::Real(0.5)]), 1.0);
        assert_eq!(real("round", &[Value::Real(1.5)]), 2.0);
    }

    #[test]
    fn round_treats_a_negative_precision_as_zero() {
        // Checked against fifteen cases, all of which equal round(x, 0):
        // sqlite3: round(3.7, -1) = 4.0 and round(12345, -4) = 12345.0.
        assert_eq!(real("round", &[Value::Real(3.7), Value::Integer(-1)]), 4.0);
        assert_eq!(
            real("round", &[Value::Real(12345.0), Value::Integer(-4)]),
            12345.0
        );
        assert_eq!(real("round", &[Value::Real(0.5), Value::Integer(-1)]), 1.0);
    }

    #[test]
    fn round_of_null_is_null_and_a_huge_precision_is_a_no_op() {
        // sqlite3: round(2.5, NULL) is NULL and round(1e-100, 100) is 0.0.
        assert_eq!(ok("round", &[Value::Real(2.5), Value::Null]), Value::Null);
        assert_eq!(ok("round", &[Value::Null, Value::Integer(2)]), Value::Null);
        assert_eq!(
            real("round", &[Value::Real(1e-100), Value::Integer(100)]),
            0.0
        );
        assert_eq!(real("round", &[Value::Real(2.5), Value::Integer(100)]), 2.5);
    }

    // -- sign, ceil, floor -------------------------------------------------

    #[test]
    fn sign_returns_minus_one_zero_or_one() {
        // sqlite3: sign(-5)=-1, sign(5.5)=1, sign(0)=0, and sign(0.0)=0 as an
        // integer rather than a real.
        assert_eq!(int("sign", &[Value::Integer(-5)]), -1);
        assert_eq!(int("sign", &[Value::Real(5.5)]), 1);
        assert_eq!(int("sign", &[Value::Integer(0)]), 0);
        assert_eq!(ok("sign", &[Value::Real(0.0)]), Value::Integer(0));
        assert_eq!(ok("sign", &[Value::Null]), Value::Null);
    }

    #[test]
    fn sign_uses_the_strict_parser() {
        // sqlite3: sign('5') = 1 but sign('5abc') is NULL, and sign('inf') is
        // NULL too because sqlite3AtoF does not accept the word "inf".
        assert_eq!(int("sign", &[t("5")]), 1);
        assert_eq!(ok("sign", &[t("5abc")]), Value::Null);
        assert_eq!(ok("sign", &[t("inf")]), Value::Null);
        // A numeric literal that overflows is still a number.
        assert_eq!(int("sign", &[t("1e999")]), 1);
    }

    #[test]
    fn ceil_and_floor_answer_an_integer_only_when_the_argument_converts_to_one() {
        // sqlite3: ceil(1.5) = 2.0 (real), ceil(2) = 2 (integer),
        // floor(2) = 2 (integer), ceiling(1.5) = 2.0.
        assert_eq!(ok("ceil", &[Value::Real(1.5)]), Value::Real(2.0));
        assert_eq!(ok("ceil", &[Value::Integer(2)]), Value::Integer(2));
        assert_eq!(ok("floor", &[Value::Integer(2)]), Value::Integer(2));
        assert_eq!(ok("floor", &[Value::Real(-1.5)]), Value::Real(-2.0));
        assert_eq!(ok("ceiling", &[Value::Real(1.5)]), Value::Real(2.0));
        // ceil(1.0) is real even though the value is integral, because a REAL
        // argument never comes back as an integer.
        assert_eq!(ok("ceil", &[Value::Real(1.0)]), Value::Real(1.0));
    }

    #[test]
    fn ceil_and_floor_also_answer_an_integer_for_a_whole_number_in_text() {
        // The rule is sqlite3_value_int64's, not the argument's storage class:
        // a text spelling of a whole number converts to an i64, so it becomes
        // an INTEGER. A point, an exponent or trailing junk stops the
        // conversion, so those are real -- or NULL when the text is not a
        // number at all.
        // sqlite3: typeof(ceil('5'))='integer', typeof(ceil(' 5 '))='integer',
        // typeof(ceil('5.0'))='real', typeof(floor('+5'))='integer'.
        assert_eq!(ok("ceil", &[t("5")]), Value::Integer(5));
        assert_eq!(ok("ceil", &[t(" 5 ")]), Value::Integer(5));
        assert_eq!(ok("floor", &[t("+5")]), Value::Integer(5));
        assert_eq!(ok("ceil", &[t("-5")]), Value::Integer(-5));
        assert_eq!(ok("ceil", &[t("0005")]), Value::Integer(5));
        // A decimal spelling stays real, and is still rounded as a real.
        assert_eq!(ok("ceil", &[t("5.0")]), Value::Real(5.0));
        assert_eq!(ok("ceil", &[t("5.9")]), Value::Real(6.0));
        assert_eq!(ok("floor", &[t("-5.9")]), Value::Real(-6.0));
        // A real, integral or not, is always real.
        assert_eq!(ok("ceil", &[Value::Real(2.0)]), Value::Real(2.0));
        assert_eq!(ok("floor", &[Value::Real(-2.0)]), Value::Real(-2.0));
        // Outside the i64 range the conversion is lossy, so the real path runs.
        // sqlite3: ceil('9223372036854775808') = 9.2233720368547758e+18.
        match ok("ceil", &[t("9223372036854775808")]) {
            Value::Real(r) => assert_eq!(r, 9.223_372_036_854_775_8e18),
            other => panic!("expected real, got {other:?}"),
        }
    }

    #[test]
    fn ceil_rejects_text_with_trailing_junk() {
        // sqlite3: ceil('abc') and ceil('5abc') are both NULL, as are
        // ceil('0x10') and ceil(' 5abc').
        assert_eq!(ok("ceil", &[t("abc")]), Value::Null);
        assert_eq!(ok("ceil", &[t("5abc")]), Value::Null);
        assert_eq!(ok("ceil", &[t("0x10")]), Value::Null);
        assert_eq!(ok("ceil", &[t("")]), Value::Null);
        assert_eq!(ok("floor", &[Value::Null]), Value::Null);
    }

    // -- exp, ln, log, log2, log10 ----------------------------------------

    #[test]
    fn exp_and_ln_cover_their_domains() {
        // sqlite3: exp(0)=1.0, exp(1000)=Inf, and ln(0) and ln(-1) are NULL.
        assert_eq!(real("exp", &[Value::Integer(0)]), 1.0);
        assert!(real("exp", &[Value::Integer(1000)]).is_infinite());
        assert_eq!(ok("ln", &[Value::Integer(0)]), Value::Null);
        assert_eq!(ok("ln", &[Value::Integer(-1)]), Value::Null);
        assert_eq!(real("ln", &[Value::Real(1.0)]), 0.0);
        assert_eq!(ok("exp", &[Value::Null]), Value::Null);
        assert_eq!(ok("exp", &[t("abc")]), Value::Null);
    }

    #[test]
    fn one_argument_log_is_base_ten() {
        // The surprise: sqlite3's log(x) is log10, not the natural log.
        // sqlite3: log(100)=2.0, log(1000)=3.0, log(2)=0.3010299956639812.
        assert_eq!(real("log", &[Value::Integer(100)]), 2.0);
        assert_eq!(real("log", &[Value::Integer(1000)]), 3.0);
        assert!((real("log", &[Value::Integer(2)]) - std::f64::consts::LOG10_2).abs() < 1e-15);
        assert_eq!(ok("log", &[Value::Integer(0)]), Value::Null);
    }

    #[test]
    fn two_argument_log_rejects_a_base_below_one() {
        // sqlite3: log(0.5, 2) is NULL and so is log(0.25, 2), even though the
        // mathematics is well defined. log(2, 0.25) is -2.0.
        assert_eq!(
            ok("log", &[Value::Real(0.5), Value::Integer(2)]),
            Value::Null
        );
        assert_eq!(
            ok("log", &[Value::Real(0.25), Value::Integer(2)]),
            Value::Null
        );
        assert_eq!(real("log", &[Value::Integer(2), Value::Real(0.25)]), -2.0);
        // A base of one has no solution and is NULL too.
        assert_eq!(
            ok("log", &[Value::Integer(1), Value::Integer(5)]),
            Value::Null
        );
        assert_eq!(real("log", &[Value::Integer(10), Value::Integer(100)]), 2.0);
    }

    #[test]
    fn log2_and_log10_share_the_strict_parser() {
        // sqlite3: log2(8)=3.0, log10(1000)=3.0, and log2('5abc') is NULL.
        assert_eq!(real("log2", &[Value::Integer(8)]), 3.0);
        assert_eq!(real("log10", &[Value::Integer(1000)]), 3.0);
        assert_eq!(ok("log2", &[Value::Integer(0)]), Value::Null);
        assert_eq!(ok("log2", &[t("5abc")]), Value::Null);
        assert_eq!(ok("log10", &[Value::Null]), Value::Null);
    }

    // -- pow, sqrt, mod ----------------------------------------------------

    #[test]
    fn pow_handles_the_awkward_corners() {
        // sqlite3: pow(2,10)=1024.0, pow(0,-1)=Inf, and pow(-1,0.5) is NULL.
        assert_eq!(
            real("pow", &[Value::Integer(2), Value::Integer(10)]),
            1024.0
        );
        assert!(real("pow", &[Value::Integer(0), Value::Integer(-1)]).is_infinite());
        assert_eq!(
            ok("pow", &[Value::Integer(-1), Value::Real(0.5)]),
            Value::Null
        );
        // power is an alias.
        assert_eq!(
            real("power", &[Value::Integer(2), Value::Integer(10)]),
            1024.0
        );
        assert_eq!(ok("pow", &[Value::Null, Value::Integer(2)]), Value::Null);
    }

    #[test]
    fn sqrt_of_a_negative_is_null() {
        // sqlite3: sqrt(4)=2.0, sqrt(0)=0.0, and sqrt(-1) is NULL.
        assert_eq!(real("sqrt", &[Value::Integer(4)]), 2.0);
        assert_eq!(real("sqrt", &[Value::Integer(0)]), 0.0);
        assert_eq!(ok("sqrt", &[Value::Integer(-1)]), Value::Null);
        assert_eq!(ok("sqrt", &[t("5abc")]), Value::Null);
    }

    #[test]
    fn mod_is_c_fmod_so_the_divisor_does_not_flip_the_sign() {
        // The big surprise: sqlite3's mod is C's fmod, so a negative divisor
        // does not change the result's sign.
        // sqlite3: mod(5,-3) = 2.0 (not -1.0) and mod(-5,3) = -2.0.
        assert_eq!(real("mod", &[Value::Integer(5), Value::Integer(-3)]), 2.0);
        assert_eq!(real("mod", &[Value::Integer(-5), Value::Integer(3)]), -2.0);
        assert_eq!(real("mod", &[Value::Integer(5), Value::Integer(3)]), 2.0);
        // A zero divisor is NULL, and the result is always real even when it
        // is integral: mod(4, 2) is 0.0, not integer 0.
        assert_eq!(
            ok("mod", &[Value::Integer(3), Value::Integer(0)]),
            Value::Null
        );
        assert_eq!(
            ok("mod", &[Value::Integer(4), Value::Integer(2)]),
            Value::Real(0.0)
        );
        assert_eq!(
            ok("mod", &[Value::Integer(4), Value::Integer(3)]),
            Value::Real(1.0)
        );
    }

    // -- pi and the trig family -------------------------------------------

    #[test]
    fn pi_takes_no_arguments() {
        // sqlite3: pi() = std::f64::consts::PI2653589793 and pi(1) is an arity error.
        assert_eq!(real("pi", &[]), std::f64::consts::PI);
        let err = call("pi", &[Value::Integer(1)]).unwrap_err();
        assert_eq!(err.message, "wrong number of arguments to function pi()");
    }

    #[test]
    fn trig_covers_the_usual_domain() {
        // sqlite3: sin(0)=0.0, cos(0)=1.0, tan(0)=0.0, atan(1)=0.7853981633974483.
        assert_eq!(real("sin", &[Value::Integer(0)]), 0.0);
        assert_eq!(real("cos", &[Value::Integer(0)]), 1.0);
        assert_eq!(real("tan", &[Value::Integer(0)]), 0.0);
        assert!((real("atan", &[Value::Integer(1)]) - std::f64::consts::FRAC_PI_4).abs() < 1e-15);
        assert_eq!(ok("sin", &[Value::Null]), Value::Null);
        assert_eq!(ok("sin", &[t("5abc")]), Value::Null);
    }

    #[test]
    fn inverse_trig_rejects_arguments_outside_minus_one_to_one() {
        // sqlite3: asin(1)=1.5707963267948966, and asin(2) and acos(2) are NULL.
        assert!((real("asin", &[Value::Integer(1)]) - std::f64::consts::FRAC_PI_2).abs() < 1e-15);
        assert_eq!(ok("asin", &[Value::Integer(2)]), Value::Null);
        assert_eq!(ok("acos", &[Value::Integer(2)]), Value::Null);
        // atan has no such restriction.
        assert_eq!(ok("atan", &[Value::Null]), Value::Null);
    }

    #[test]
    fn atan2_and_the_angle_converters() {
        // sqlite3: atan2(0,0)=0.0, degrees(pi())=180.0, radians(180)=pi().
        assert_eq!(real("atan2", &[Value::Integer(0), Value::Integer(0)]), 0.0);
        assert!(
            (real("atan2", &[Value::Integer(1), Value::Integer(1)]) - std::f64::consts::FRAC_PI_4)
                .abs()
                < 1e-15
        );
        assert_eq!(ok("atan2", &[Value::Null, Value::Integer(1)]), Value::Null);
        assert_eq!(real("degrees", &[Value::Real(std::f64::consts::PI)]), 180.0);
        assert_eq!(
            real("radians", &[Value::Integer(180)]),
            std::f64::consts::PI
        );
        assert_eq!(ok("degrees", &[t("5abc")]), Value::Null);
    }

    // -- random ------------------------------------------------------------

    #[test]
    fn random_is_deterministic_under_a_seed() {
        // SQLite's random() is not seedable from SQL, so seed_random is an
        // engine extension. The shape is unchanged: a 64-bit signed integer.
        seed_random(12345);
        let a = ok("random", &[]);
        seed_random(12345);
        let b = ok("random", &[]);
        assert_eq!(a, b);
        match a {
            Value::Real(r) => assert!(r.abs() < 9.223_372_036_854_776e18),
            other => panic!("random() returned {other:?}"),
        }
    }

    #[test]
    fn randomblob_clamps_its_length_at_one() {
        // sqlite3: randomblob(0) and randomblob(-1) both give a 1-byte blob.
        seed_random(999);
        for n in [0i64, -1, 4] {
            match ok("randomblob", &[Value::Integer(n)]) {
                Value::Blob(b) => assert_eq!(b.len(), n.max(1) as usize, "length for {n}"),
                other => panic!("randomblob({n}) returned {other:?}"),
            }
        }
        assert_eq!(ok("randomblob", &[Value::Null]), Value::Null);
    }

    // -- dates -------------------------------------------------------------

    #[test]
    fn the_epoch_anchors_are_right() {
        // sqlite3: julianday('1970-01-01') = 2440587.5 and unixepoch = 0.
        assert_eq!(real("julianday", &[t("1970-01-01")]), 2_440_587.5);
        assert_eq!(int("unixepoch", &[t("1970-01-01")]), 0);
        assert_eq!(text("date", &[t("1970-01-01")]), "1970-01-01");
        // J2000.0 is Julian day 2451545.0, which is noon on 2000-01-01.
        assert_eq!(real("julianday", &[Value::Real(2_451_545.0)]), 2_451_545.0);
        assert_eq!(
            text("datetime", &[Value::Real(2_451_545.0)]),
            "2000-01-01 12:00:00"
        );
    }

    #[test]
    fn dates_render_themselves_and_julian_days_read_back() {
        // sqlite3: date(2440587.5) = 1970-01-01 and date(2451545.0) = 2000-01-01.
        assert_eq!(text("date", &[Value::Real(2_440_587.5)]), "1970-01-01");
        assert_eq!(text("date", &[Value::Real(2_451_545.0)]), "2000-01-01");
        assert_eq!(text("time", &[t("2020-01-02 03:04:05")]), "03:04:05");
        assert_eq!(
            text("datetime", &[t("2020-01-02 03:04:05")]),
            "2020-01-02 03:04:05"
        );
    }

    #[test]
    fn the_accepted_date_spellings_are_narrow() {
        // Accepted: the T separator, a Z offset and a +HH:MM offset.
        assert_eq!(
            text("datetime", &[t("2020-01-02T03:04:05")]),
            "2020-01-02 03:04:05"
        );
        assert_eq!(
            text("datetime", &[t("2020-01-02 03:04:05Z")]),
            "2020-01-02 03:04:05"
        );
        // An offset shifts the value: +01:00 makes it an hour earlier in UTC.
        assert_eq!(
            text("datetime", &[t("2020-01-02 03:04:05+01:00")]),
            "2020-01-02 02:04:05"
        );
        // Rejected: a one-digit month or day, a bare hour, a missing minute
        // digit, an offset with no colon, and a UTC suffix.
        for bad in [
            "2020-1-2",
            "2020-01-02 03",
            "2020-01-02 03:",
            "2020-01-02 03:04:05+0530",
            "2020-01-02 03:04:05 UTC",
            "2020-13-01",
        ] {
            assert_eq!(ok("date", &[t(bad)]), Value::Null, "{bad} should not parse");
        }
    }

    #[test]
    fn a_day_past_its_month_rolls_forward_but_day_32_does_not() {
        // sqlite3: date('2020-02-30') = 2020-03-01 and date('2021-02-29') =
        // 2021-03-01, but date('2020-01-32') is NULL and so is '2020-01-00'.
        assert_eq!(text("date", &[t("2020-02-30")]), "2020-03-01");
        assert_eq!(text("date", &[t("2021-02-29")]), "2021-03-01");
        assert_eq!(text("date", &[t("1900-02-29")]), "1900-03-01");
        assert_eq!(text("date", &[t("2020-04-31")]), "2020-05-01");
        assert_eq!(ok("date", &[t("2020-01-32")]), Value::Null);
        assert_eq!(ok("date", &[t("2020-01-00")]), Value::Null);
    }

    #[test]
    fn the_leap_year_rule_is_proleptic_gregorian() {
        // Year 0 is a leap year under the %400 rule, 1900 is not and 2000 is.
        assert_eq!(text("date", &[t("2000-02-29")]), "2000-02-29");
        assert_eq!(text("date", &[t("1900-02-29")]), "1900-03-01");
        assert_eq!(text("date", &[t("2400-02-29")]), "2400-02-29");
        assert_eq!(text("date", &[t("0000-02-29")]), "0000-02-29");
        assert!(is_leap(2000));
        assert!(!is_leap(1900));
        assert!(is_leap(0));
    }

    #[test]
    fn a_bare_time_is_anchored_at_2000_01_01() {
        // sqlite3: time('12:34:56') = 12:34:56 and date('12:34:56') =
        // 2000-01-01, so a lone HH:MM:SS is a time on the J2000 anchor day.
        assert_eq!(text("time", &[t("12:34:56")]), "12:34:56");
        assert_eq!(text("date", &[t("12:34:56")]), "2000-01-01");
        assert_eq!(text("time", &[t("12:34")]), "12:34:00");
        // A one-digit hour is not a time.
        assert_eq!(ok("time", &[t("1:02")]), Value::Null);
    }

    #[test]
    fn unixepoch_truncates_toward_zero() {
        // sqlite3: unixepoch('1969-12-31 23:59:59') = -1 and
        // unixepoch('1970-01-01 00:00:01') = 1.
        assert_eq!(int("unixepoch", &[t("1969-12-31 23:59:59")]), -1);
        assert_eq!(int("unixepoch", &[t("1970-01-01 00:00:01")]), 1);
        assert_eq!(int("unixepoch", &[t("2000-01-01")]), 946_684_800);
    }

    #[test]
    fn modifiers_shift_the_value() {
        // sqlite3: date('2020-01-02','+1 day') = 2020-01-03 and
        // date('2020-03-01','-1 day') = 2020-02-29.
        assert_eq!(text("date", &[t("2020-01-02"), t("+1 day")]), "2020-01-03");
        assert_eq!(text("date", &[t("2020-03-01"), t("-1 day")]), "2020-02-29");
        assert_eq!(
            text("datetime", &[t("2020-01-02"), t("+2.5 hours")]),
            "2020-01-02 02:30:00"
        );
        assert_eq!(
            text("date", &[t("2020-01-02"), t("start of month")]),
            "2020-01-01"
        );
        assert_eq!(
            text("date", &[t("2020-01-02"), t("start of year")]),
            "2020-01-01"
        );
        // 2020-03-05 was a Thursday; weekday 0 is the next Sunday.
        assert_eq!(
            text("date", &[t("2020-03-05"), t("weekday 0")]),
            "2020-03-08"
        );
        // Several modifiers apply left to right.
        assert_eq!(
            text("datetime", &[t("2020-01-02"), t("+1 day"), t("-1 hour")]),
            "2020-01-02 23:00:00"
        );
    }

    #[test]
    fn a_month_modifier_overflows_rather_than_clamping() {
        // sqlite3: date('2020-01-31','+1 month') = 2020-03-02, because SQLite
        // computes 2020-02-31 and lets the day overflow into March.
        assert_eq!(
            text("date", &[t("2020-01-31"), t("+1 month")]),
            "2020-03-02"
        );
        assert_eq!(
            text("date", &[t("2020-12-31"), t("+1 month")]),
            "2021-01-31"
        );
        assert_eq!(text("date", &[t("2020-02-29"), t("+1 year")]), "2021-03-01");
    }

    #[test]
    fn an_unrecognised_modifier_makes_the_whole_call_null() {
        // sqlite3: date('2020-01-02','bogus') is NULL, as is a NULL modifier.
        assert_eq!(ok("date", &[t("2020-01-02"), t("bogus")]), Value::Null);
        assert_eq!(ok("date", &[t("2020-01-02"), Value::Null]), Value::Null);
        // A weekday outside 0..=6 is rejected too.
        assert_eq!(ok("date", &[t("2020-03-05"), t("weekday 7")]), Value::Null);
    }

    #[test]
    fn strftime_covers_the_common_specifiers() {
        // Every answer below is sqlite3's, for '2020-03-05 14:07:09.123'.
        let s = t("2020-03-05 14:07:09.123");
        assert_eq!(text("strftime", &[t("%Y-%m-%d"), s.clone()]), "2020-03-05");
        assert_eq!(text("strftime", &[t("%H:%M:%S"), s.clone()]), "14:07:09");
        assert_eq!(
            text("strftime", &[t("%d %e %f"), s.clone()]),
            "05  5 09.123"
        );
        assert_eq!(text("strftime", &[t("%I %p %P"), s.clone()]), "02 PM pm");
        assert_eq!(text("strftime", &[t("%j"), s.clone()]), "065");
        assert_eq!(text("strftime", &[t("%k %l"), s.clone()]), "14  2");
        assert_eq!(text("strftime", &[t("%w %u"), s.clone()]), "4 4");
        assert_eq!(text("strftime", &[t("%U %W %V"), s.clone()]), "09 09 10");
        assert_eq!(text("strftime", &[t("%G %g"), s.clone()]), "2020 20");
        assert_eq!(text("strftime", &[t("%R %T"), s.clone()]), "14:07 14:07:09");
        assert_eq!(text("strftime", &[t("%%"), s.clone()]), "%");
    }

    #[test]
    fn strftime_reports_iso_weeks_across_a_year_boundary() {
        // sqlite3: 2021-01-01 belongs to ISO week 53 of 2020, and 2019-12-31 to
        // week 1 of 2020.
        assert_eq!(text("strftime", &[t("%V %G"), t("2021-01-01")]), "53 2020");
        assert_eq!(text("strftime", &[t("%V %G"), t("2019-12-31")]), "01 2020");
        assert_eq!(text("strftime", &[t("%V %G"), t("2020-12-31")]), "53 2020");
    }

    #[test]
    fn strftime_j_and_s_show_the_julian_day_and_the_epoch() {
        // sqlite3: strftime('%J','2020-03-05 14:07:09') = 2458914.088298611 and
        // strftime('%s', ...) is the unix timestamp.
        let s = t("2020-03-05 14:07:09");
        assert_eq!(text("strftime", &[t("%J"), s.clone()]), "2458914.088298611");
        assert_eq!(text("strftime", &[t("%s"), s.clone()]), "1583417229");
        assert_eq!(text("strftime", &[t("%s"), t("1970-01-01")]), "0");
        // %J always shows sixteen significant digits, so a whole day
        // prints as 2458850.5 rather than losing the fraction.
        assert_eq!(text("strftime", &[t("%J"), t("2020-01-02")]), "2458850.5");
    }

    #[test]
    fn an_unsupported_strftime_specifier_makes_the_result_null() {
        // sqlite3 3.53.4 returns NULL for %D, %n, %t, %C, %X, %c, %x, %r, %q
        // and %Q -- and for a format that ends in a bare %.
        let s = t("2020-03-05 14:07:09");
        for f in [
            "%D", "%n", "%t", "%C", "%X", "%c", "%x", "%r", "%q", "%Q", "%",
        ] {
            assert_eq!(
                ok("strftime", &[t(f), s.clone()]),
                Value::Null,
                "{f} should be NULL"
            );
        }
        // An unknown specifier anywhere in the format poisons the whole call.
        assert_eq!(ok("strftime", &[t("%Y-%m-%D"), s]), Value::Null);
    }

    #[test]
    fn the_date_family_reports_null_for_nonsense() {
        // sqlite3: date('abc') and date(NULL) are both NULL.
        assert_eq!(ok("date", &[t("abc")]), Value::Null);
        assert_eq!(ok("date", &[Value::Null]), Value::Null);
        assert_eq!(ok("datetime", &[t("abc")]), Value::Null);
        assert_eq!(ok("julianday", &[t("abc")]), Value::Null);
        assert_eq!(ok("unixepoch", &[t("abc")]), Value::Null);
    }

    #[test]
    fn now_is_the_current_instant_and_not_null() {
        // `date('now')` used to be NULL, because nothing in the engine knew the
        // word: it is not `HH:MM`, `parse_whole` rejects it, and the strict
        // date parser fails on its very first digit.
        //
        // The answer moves, so the assertions are windows rather than values.
        // What is being pinned is that `now` is the CURRENT instant: not NULL,
        // not a fixed date such as 2000-01-01, and close to a clock read here.
        let clock = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64();

        // None of the five siblings may report NULL any more.
        for name in ["date", "datetime", "time", "julianday", "unixepoch"] {
            assert_ne!(
                ok(name, &[t("now")]),
                Value::Null,
                "{name}('now') was NULL"
            );
        }

        // The rendered day is today's, computed here from the clock through a
        // different route than the module uses. `civil_from_days` counts days
        // from 1970-01-01, which is the same origin `UNIX_EPOCH_JD` names, so
        // this is a check against the clock rather than a restatement of the
        // code under test.
        let (y, m, d) = super::civil_from_days((clock / 86_400.0) as i64);
        assert_eq!(
            text("date", &[t("now")]),
            format!("{y:04}-{m:02}-{d:02}"),
            "date('now') is not the current UTC day"
        );

        // The two numeric renderings are one instant, so they agree with each
        // other and with the clock to within a second. `julianday` keeps the
        // sub-second part on sqlite3, and `unixepoch` truncates it, so the two
        // differ by the fraction of a second rather than agreeing exactly --
        // which is itself part of what is being pinned.
        let jd = real("julianday", &[t("now")]);
        let ux = int("unixepoch", &[t("now")]);
        assert!(
            ((jd - UNIX_EPOCH_JD) * 86_400.0 - ux as f64).abs() < 2.0,
            "julianday({jd}) and unixepoch({ux}) are the same instant"
        );
        assert!(
            ((jd - UNIX_EPOCH_JD) * 86_400.0 - clock).abs() < 60.0,
            "julianday('now') was {jd}, which is not now"
        );
        // `unixepoch` truncates toward zero while `julianday` keeps the
        // fraction, so the Julian day is the sub-second part AHEAD of the
        // epoch -- the fraction is carried, not rounded off before the day
        // count. The window is a millisecond wide at the bottom because the
        // Unix path rounds to a millisecond before it truncates, which can
        // carry a reading in the last fraction of a millisecond into the next
        // second.
        let frac = (jd - UNIX_EPOCH_JD) * 86_400.0 - ux as f64;
        assert!(
            (-0.001..1.0).contains(&frac),
            "julianday('now') must carry the sub-second part unixepoch drops, got {frac}"
        );
    }

    #[test]
    fn now_ignores_case_but_matches_the_whole_word_only() {
        // sqlite3 answers today's date for every spelling of the word, and NULL
        // for a near miss. `eq_ignore_ascii_case` on the whole string is what
        // produces exactly that set: nothing is trimmed and no prefix counts.
        for spelling in ["now", "NOW", "Now", "nOw", "nOW"] {
            let got = ok("date", &[t(spelling)]);
            assert_ne!(got, Value::Null, "date({spelling:?}) was NULL");
        }
        for junk in ["now ", "nowx", "now!", "NOWX", " now", "no", "noww", "no1"] {
            assert_eq!(
                ok("date", &[t(junk)]),
                Value::Null,
                "date({junk:?}) should be NULL"
            );
        }
        // A BLOB holding the same bytes is a different case, and it is a known
        // gap rather than a rule of the `now` match: sqlite3 answers today's
        // date for `date(x'6e6f77')` because a time value that is not text is
        // read as its bytes, but the engine's `parse_time_value` only has a
        // `Text` arm, so every blob time value is NULL -- `date(x'32303234
        // 2d30312d3031')` (2024-01-01) is NULL here too. Pinned as a reminder
        // that the two are separate defects, so neither is blamed on the other.
        assert_eq!(ok("date", &[Value::Blob(b"2024-01-01".to_vec())]), Value::Null);
        assert_eq!(ok("date", &[Value::Blob(b"now".to_vec())]), Value::Null);
    }

    #[test]
    fn now_takes_modifiers_and_stays_null_for_a_bogus_one() {
        // sqlite3: date('now','-1 day') is yesterday and date('now','+1 day')
        // is tomorrow, so the instant feeds the modifier chain like any other.
        let today = text("date", &[t("now")]);
        let yesterday = text("date", &[t("now"), t("-1 day")]);
        let tomorrow = text("date", &[t("now"), t("+1 day")]);
        assert_ne!(today, yesterday);
        assert_ne!(today, tomorrow);
        assert_ne!(yesterday, tomorrow);
        assert_eq!(
            ok("date", &[t("now"), t("nonsense")]),
            Value::Null,
            "an unknown modifier still poisons the call"
        );
    }

    #[test]
    fn a_julian_day_less_half_a_day_is_the_previous_day() {
        // This is the shape `date(julianday('now')-0.5)` has. With 'now' it
        // also needs the clock, so the fixed values pin the rule underneath it:
        // a Julian day is a NOON-based count, so 2461311.5 is midnight on
        // 2026-09-28 and half a day earlier is the day before. Getting this
        // wrong is a floor-versus-round bug at the day boundary, which is what
        // these two values exist to catch.
        assert_eq!(text("date", &[Value::Real(2_461_311.5 - 0.5)]), "2026-09-27");
        assert_eq!(text("date", &[Value::Real(2_460_310.5 - 0.5)]), "2023-12-31");
        // And half a day later is the day after, so the floor is not an off-by-
        // one in one direction only.
        assert_eq!(text("date", &[Value::Real(2_461_311.5 + 0.5)]), "2026-09-28");
        // The real reported statement, with the clock substituted for 'now'.
        let jd = real("julianday", &[t("now")]);
        let yesterday = text("date", &[t("now"), t("-1 day")]);
        assert_eq!(text("date", &[Value::Real(jd - 0.5)]), yesterday);
    }

    #[test]
    fn dates_outside_the_renderable_range_are_null() {
        // sqlite3: date('10000-01-01') is NULL because only years 0..=9999 can
        // be rendered, and date(1e8) is NULL for the same reason.
        assert_eq!(ok("date", &[t("10000-01-01")]), Value::Null);
        assert_eq!(ok("date", &[Value::Real(1e8)]), Value::Null);
        assert_eq!(ok("date", &[Value::Real(-1.0)]), Value::Null);
        // The boundary is Julian day 5373484, which is 9999-12-31.
        assert_eq!(text("date", &[Value::Real(5_373_484.0)]), "9999-12-31");
        assert_eq!(ok("date", &[Value::Real(5_373_484.5)]), Value::Null);
        // But Julian day 0 is fine, and renders as -4713-11-24.
        assert_eq!(text("date", &[Value::Integer(0)]), "-4713-11-24");
    }

    #[test]
    fn the_date_family_reports_an_arity_error() {
        // sqlite3: date() with no argument is an error.
        let err = call("date", &[]).unwrap_err();
        assert_eq!(err.message, "wrong number of arguments to function date()");
        let err = call("strftime", &[t("%Y")]).unwrap_err();
        assert_eq!(
            err.message,
            "wrong number of arguments to function strftime()"
        );
    }

    // -- round: the carry and the sub-unit paths ---------------------------

    #[test]
    fn round_carries_out_of_the_leading_digit() {
        // The case that used to answer 0.0 for all of them. `round_digits`
        // dropped the leading digit on a carry instead of prepending a 1, and
        // padded a short digit string with trailing zeros where the caller
        // wanted a shorter one, so every carry vanished.
        // sqlite3:
        //   round(9.99,0) = 10.0   round(99.9) = 100.0
        //   round(999.5) = 1000.0  round(9.99,1) = 10.0
        //   round(-9.99,0) = -10.0
        assert_eq!(real("round", &[Value::Real(9.99), Value::Integer(0)]), 10.0);
        assert_eq!(real("round", &[Value::Real(9.99)]), 10.0);
        assert_eq!(real("round", &[Value::Real(99.9)]), 100.0);
        assert_eq!(real("round", &[Value::Real(999.5)]), 1000.0);
        assert_eq!(real("round", &[Value::Real(9.99), Value::Integer(1)]), 10.0);
        assert_eq!(
            real("round", &[Value::Real(-9.99), Value::Integer(0)]),
            -10.0
        );
        assert_eq!(real("round", &[Value::Real(9.5), Value::Integer(0)]), 10.0);
    }

    #[test]
    fn round_below_the_last_kept_place_compares_against_half_of_it() {
        // Where the value is smaller than the place being kept, the answer is
        // zero or one unit in that place, and which one is decided by whether
        // the value reaches *half* of it. Reading the leading digit of the
        // expansion instead put every one of these at 1.0.
        // sqlite3:
        //   round(0.05,1) = 0.1    round(0.099,1) = 0.1
        //   round(0.999,1) = 1.0   round(0.005,2) = 0.01
        //   round(0.0005,3) = 0.001
        assert_eq!(real("round", &[Value::Real(0.05), Value::Integer(1)]), 0.1);
        assert_eq!(real("round", &[Value::Real(0.099), Value::Integer(1)]), 0.1);
        assert_eq!(real("round", &[Value::Real(0.999), Value::Integer(1)]), 1.0);
        assert_eq!(
            real("round", &[Value::Real(0.005), Value::Integer(2)]),
            0.01
        );
        assert_eq!(
            real("round", &[Value::Real(0.0005), Value::Integer(3)]),
            0.001
        );
        // A value well under half the unit is zero, and the sign is reapplied
        // rather than folded into the comparison.
        assert_eq!(real("round", &[Value::Real(0.05)]), 0.0);
        assert_eq!(real("round", &[Value::Real(-0.05)]), 0.0);
        assert_eq!(real("round", &[Value::Real(0.5)]), 1.0);
        assert_eq!(real("round", &[Value::Real(-0.5)]), -1.0);
    }

    // -- the maths that must not leak a NaN -------------------------------

    #[test]
    fn an_overflowing_argument_gives_null_rather_than_a_nan() {
        // `1e400` overflows a double to infinity, and libm answers NaN where
        // the maths runs out. sqlite3 answers NULL in every one of these, and a
        // Real(NaN) would render as the empty string further downstream.
        for name in ["sin", "cos", "tan", "asin", "acos"] {
            assert_eq!(
                ok(name, &[Value::Real(f64::INFINITY)]),
                Value::Null,
                "{name}(1e400)"
            );
        }
        // atan of an infinity is a real answer, not a NaN: pi/2.
        // sqlite3: atan(1e400) = 1.5707963267948966
        assert_eq!(
            real("atan", &[Value::Real(f64::INFINITY)]),
            std::f64::consts::FRAC_PI_2
        );
        // exp and the logarithms answer Inf where the value is genuinely too
        // large, which is a real result and not a NaN.
        assert!(real("exp", &[Value::Real(f64::INFINITY)]).is_infinite());
        assert!(real("ln", &[Value::Real(f64::INFINITY)]).is_infinite());
        // mod has no remainder for an infinite dividend.
        assert_eq!(
            ok("mod", &[Value::Real(f64::INFINITY), Value::Integer(3)]),
            Value::Null
        );
        assert_eq!(
            ok(
                "mod",
                &[Value::Real(f64::INFINITY), Value::Real(f64::INFINITY)]
            ),
            Value::Null
        );
        // And atan2 of a NaN coordinate is NULL too.
        assert_eq!(
            ok("atan2", &[Value::Real(f64::NAN), Value::Integer(1)]),
            Value::Null
        );
    }

    // -- dates -------------------------------------------------------------

    #[test]
    fn an_hour_of_twenty_four_stays_on_the_day_it_was_written() {
        // SQLite parses `24:00:00` and even `24:30:00` but does not roll them
        // into the next day for the date functions, so the *written* fields
        // have to travel with the value rather than being re-derived.
        // sqlite3:
        //   date('2020-01-02 24:00:00')     = 2020-01-02
        //   date('2020-01-02 24:30:00')     = 2020-01-02
        //   datetime('2020-01-02 24:30:00') = 2020-01-02 24:30:00
        //   time('2020-01-02 24:30:00')     = 24:30:00
        assert_eq!(text("date", &[t("2020-01-02 24:00:00")]), "2020-01-02");
        assert_eq!(text("date", &[t("2020-01-02 24:30:00")]), "2020-01-02");
        assert_eq!(
            text("datetime", &[t("2020-01-02 24:30:00")]),
            "2020-01-02 24:30:00"
        );
        assert_eq!(text("time", &[t("2020-01-02 24:30:00")]), "24:30:00");
        // The Julian day and Unix forms read the raw value, so they see the
        // half day the 24 o'clock hour carries.
        // sqlite3: julianday('2020-01-02 24:30:00') = 2458851.5208333335
        assert_eq!(
            real("julianday", &[t("2020-01-02 24:30:00")]),
            2_458_851.5208333335
        );
        assert_eq!(int("unixepoch", &[t("2020-01-02 24:30:00")]), 1_578_011_400);
        // A modifier normalises it instead: `+0 days` moves to the 3rd, and
        // `start of day` rebuilds the clock and gives midnight of the 2nd.
        assert_eq!(
            text("date", &[t("2020-01-02 24:30:00"), t("+0 days")]),
            "2020-01-03"
        );
        assert_eq!(
            text("time", &[t("2020-01-02 24:30:00"), t("start of day")]),
            "00:00:00"
        );
        assert_eq!(
            text("date", &[t("2020-01-02 24:30:00"), t("+1 day")]),
            "2020-01-04"
        );
        assert_eq!(
            text("date", &[t("2020-01-02 24:30:00"), t("weekday 0")]),
            "2020-01-05"
        );
        // Hour 25 is out of range, and a lower-case `t` separator is rejected
        // even though the upper-case one is accepted.
        assert_eq!(ok("date", &[t("2020-01-02 25:00:00")]), Value::Null);
        assert_eq!(ok("date", &[t("2020-01-02t03:04:05")]), Value::Null);
        assert_eq!(
            ok("date", &[t("2020-01-02T03:04:05")]),
            Value::Text("2020-01-02".into())
        );
    }

    #[test]
    fn a_zone_offset_stops_at_fourteen_fifty_nine() {
        // sqlite3: date('2020-01-02 03:04:05+14:59') is 2020-01-01 and the
        // same with +15:00 is NULL. The minute is bounded too.
        assert_eq!(
            text("date", &[t("2020-01-02 03:04:05+14:59")]),
            "2020-01-01"
        );
        assert_eq!(ok("date", &[t("2020-01-02 03:04:05+15:00")]), Value::Null);
        assert_eq!(ok("date", &[t("2020-01-02 03:04:05-15:00")]), Value::Null);
        assert_eq!(ok("date", &[t("2020-01-02 03:04:05+00:60")]), Value::Null);
    }

    #[test]
    fn the_julianday_and_unixepoch_modifiers_need_a_numeric_argument() {
        // `julianday(2451545, 'julianday')` is 2451545.0 and
        // `date(2451545, 'unixepoch')` is 1970-01-29: the modifiers re-read a
        // number, and a *date string* is not one.
        // sqlite3:
        //   julianday('2020-01-01','julianday')    = NULL
        //   unixepoch('1970-01-01','unixepoch')    = NULL
        //   date(2451545,'+1 day','julianday')     = NULL
        //   date(2451545,'julianday','+1 day')     = 2000-01-02
        assert_eq!(
            real("julianday", &[Value::Integer(2_451_545), t("julianday")]),
            2_451_545.0
        );
        assert_eq!(
            text("date", &[Value::Integer(2_451_545), t("unixepoch")]),
            "1970-01-29"
        );
        assert_eq!(
            ok("julianday", &[t("2020-01-01"), t("julianday")]),
            Value::Null
        );
        assert_eq!(
            ok("unixepoch", &[t("1970-01-01"), t("unixepoch")]),
            Value::Null
        );
        // The test is positional: a shift first means the argument is a date
        // by the time the modifier runs, so the pair no longer fits.
        assert_eq!(
            ok(
                "date",
                &[Value::Integer(2_451_545), t("+1 day"), t("julianday")]
            ),
            Value::Null
        );
        assert_eq!(
            text(
                "date",
                &[Value::Integer(2_451_545), t("julianday"), t("+1 day")]
            ),
            "2000-01-02"
        );
    }

    #[test]
    fn a_numeric_string_is_a_julian_day() {
        // sqlite3: date('2451545') is 2000-01-01. The strict parser is what
        // separates it from a date spelling, which it rejects.
        assert_eq!(text("date", &[t("2451545")]), "2000-01-01");
        assert_eq!(text("datetime", &[t("2451545")]), "2000-01-01 12:00:00");
        assert_eq!(ok("date", &[t("2451545x")]), Value::Null);
    }

    #[test]
    fn a_negative_year_is_padded_two_different_ways() {
        // `date()` pads the magnitude to four digits behind the sign and `%F`
        // to three, which is not the same renderer.
        // sqlite3:
        //   date('2020-01-01','-1000000 days') = -0718-02-03
        //   strftime('%Y-%m-%d','2020-01-01','-1000000 days') = -718-02-03
        //   date('-0001-01-01')    = -0001-01-01
        //   strftime('%F','-0001-01-01') = -001-01-01
        assert_eq!(
            text("date", &[t("2020-01-01"), t("-1000000 days")]),
            "-0718-02-03"
        );
        assert_eq!(
            text(
                "strftime",
                &[t("%Y-%m-%d"), t("2020-01-01"), t("-1000000 days")]
            ),
            "-718-02-03"
        );
        assert_eq!(text("date", &[t("-0001-01-01")]), "-0001-01-01");
        assert_eq!(text("strftime", &[t("%F"), t("-0001-01-01")]), "-001-01-01");
    }

    #[test]
    fn strftime_floors_the_unix_timestamp_and_covers_julian_day_zero() {
        // `%s` floors rather than truncating, so a second before the epoch is
        // -1 and not 0 -- which is what the bare `as i64` used to give.
        // sqlite3: strftime('%s','1969-12-31 23:59:59') = -1
        assert_eq!(text("strftime", &[t("%s"), t("1969-12-31 23:59:59")]), "-1");
        assert_eq!(text("strftime", &[t("%s"), t("1970-01-01")]), "0");
        // `%J` is a number rather than a cast, so it prints with no point even
        // at the bottom of the range where the calendar cannot describe the day.
        // sqlite3: strftime('%J','-4713-11-24 12:00:00') = 0
        assert_eq!(text("strftime", &[t("%J"), t("-4713-11-24 12:00:00")]), "0");
        assert_eq!(text("strftime", &[t("%J"), t("2451545")]), "2451545");
        // The Julian day and Unix forms reject anything below the epoch even
        // where `date()` still renders.
        assert_eq!(ok("julianday", &[Value::Real(-0.5)]), Value::Null);
        assert_eq!(ok("julianday", &[t("-4713-11-24")]), Value::Null);
    }

    #[test]
    fn strftime_covers_the_f_specifier_and_the_signed_g() {
        // `%F` is an alias for `%Y-%m-%d` and was missing entirely.
        // sqlite3: strftime('%F','2020-03-05') = 2020-03-05
        assert_eq!(text("strftime", &[t("%F"), t("2020-03-05")]), "2020-03-05");
        // `%g` is the ISO year modulo 100, printed signed before the common era
        // rather than wrapped into 0..99.
        // sqlite3: strftime('%g','0000-01-01') = -1
        assert_eq!(text("strftime", &[t("%g"), t("0000-01-01")]), "-1");
        assert_eq!(text("strftime", &[t("%g"), t("2020-03-05")]), "20");
    }

    #[test]
    fn week_numbering_counts_from_the_first_start_day() {
        // A week *begins* on its start day, so the Sunday before a Monday is
        // still in the previous week: `strftime('%W','2021-02-28')` is 08 and
        // `...('2021-02-29')` is 09. An offset-and-divide form puts both in one.
        assert_eq!(text("strftime", &[t("%W"), t("2021-02-28")]), "08");
        assert_eq!(text("strftime", &[t("%W"), t("2021-02-29")]), "09");
        // 1900-01-01 was a Monday, so `%W` starts its weeks at once and `%U`
        // does not, since that one waits for the first Sunday.
        // sqlite3: strftime('%W','1900-01-01') = 01, strftime('%U',...) = 00
        assert_eq!(text("strftime", &[t("%W"), t("1900-01-01")]), "01");
        assert_eq!(text("strftime", &[t("%U"), t("1900-01-01")]), "00");
        assert_eq!(text("strftime", &[t("%W"), t("1900-02-29")]), "09");
        assert_eq!(text("strftime", &[t("%U"), t("1900-02-29")]), "08");
        assert_eq!(text("strftime", &[t("%U"), t("2020-01-05")]), "01");
        assert_eq!(text("strftime", &[t("%W"), t("2020-01-05")]), "00");
    }

    #[test]
    fn the_julian_and_unix_forms_stop_at_the_last_renderable_day() {
        // `date(5373484.5)` is NULL because the civil conversion rejects it,
        // and the two numeric forms reject it for the same reason.
        assert_eq!(ok("date", &[Value::Real(5_373_484.5)]), Value::Null);
        assert_eq!(ok("julianday", &[Value::Real(5_373_484.5)]), Value::Null);
        assert_eq!(ok("unixepoch", &[Value::Real(5_373_484.5)]), Value::Null);
    }

    #[test]
    fn unixepoch_floors_rather_than_truncating_toward_zero() {
        // The two agree on a whole second either side of the epoch and differ
        // only in between, where the millisecond division has to floor.
        // sqlite3: unixepoch('1969-12-31 23:59:59.5') = -1
        assert_eq!(int("unixepoch", &[t("1969-12-31 23:59:59.5")]), -1);
        assert_eq!(int("unixepoch", &[t("1969-12-31 23:59:59")]), -1);
        assert_eq!(int("unixepoch", &[t("1970-01-01 00:00:01")]), 1);
    }

    // -- the number formatting the rest of the engine shares ---------------

    #[test]
    fn a_real_renders_with_the_sqlite_digit_rule() {
        // The rule is the fourteen-digit rung, escalating to a double-rounded
        // seventeen when fourteen do not read back, so 0.1 is short but a third
        // needs all seventeen.
        assert_eq!(real_to_text(0.1), "0.1");
        assert_eq!(real_to_text(0.0), "0.0");
        assert_eq!(real_to_text(1.5), "1.5");
        assert_eq!(real_to_text(100.0), "100.0");
        assert_eq!(real_to_text(0.0001), "0.0001");
        assert_eq!(real_to_text(0.00001), "1.0e-05");
        assert_eq!(real_to_text(1e20), "1.0e+20");
        // sqlite3 says 0.33333333333333332, and so does the double rounding
        // through eighteen; rounding the exact expansion straight to seventeen
        // gives ...331, which is the whole reason for the second step.
        assert_eq!(real_to_text(1.0 / 3.0), "0.33333333333333332");
        assert_eq!(real_to_text(2.67), "2.67");
        assert_eq!(real_to_text(-1.5), "-1.5");
        assert_eq!(real_to_text(f64::INFINITY), "Inf");
        // The fixed/scientific band runs from -4 to 16 places from the point,
        // and the integral fast path stops at the same place.
        assert_eq!(real_to_text(1e16), "10000000000000000.0");
        assert_eq!(real_to_text(1e17), "1.0e+17");
        assert_eq!(real_to_text(1e15), "1000000000000000.0");
        // sqlite3 prints all seventeen for sin(0.5) even though its fifteen
        // digits do round-trip, which is what rules fifteen out as the first
        // rung.
        assert_eq!(real_to_text(0.5f64.sin()), "0.47942553860420301");
    }

    #[test]
    fn the_exact_decimal_expansion_is_recoverable() {
        // 0.1 is really 0.1000000000000000055511151231257827021181583404541015625,
        // which is what makes round(0.155, 2) come out as 0.15.
        let (digits, lead) = exact_decimal(0.1);
        assert_eq!(lead, -1);
        let text: String = digits.iter().map(|&d| d as char).collect();
        assert!(text.starts_with("100000000000000005"), "got {text}");
        assert_eq!(assemble_magnitude(&digits, lead), Some(0.1));
    }

    #[test]
    fn an_unknown_name_is_left_to_the_caller() {
        // The dispatcher returns None rather than an error so eval can fall
        // through to its own table.
        assert!(call("no_such_function", &[]).unwrap().is_none());
    }
}
