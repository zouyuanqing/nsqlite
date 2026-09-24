//! The line-oriented protocol the TCL test shim speaks.
//!
//! The shim drives the engine one statement at a time over `exec`, so the
//! engine's own row format cannot be what it parses. The default print mode
//! separates values with a pipe and has no escaping, which loses an embedded
//! newline in a TEXT value and cannot tell NULL from the empty string; it
//! renders an empty result set as a Rust `Vec` debug, which is not a column
//! list. Floating point goes out through Rust's `{}`, which writes `1` where
//! SQLite writes `1.0`.
//!
//! So this mode emits a record stream instead. One record per line, a tag byte
//! for the record kind, and every value hex-encoded so nothing in a value can
//! be mistaken for a record boundary. The tags are:
//!
//! ```text
//! C <n> <hex>...   n column names, opening a statement
//! R <hex>...       one row of that statement
//! E <text>         the statement failed; the text is the engine's message
//! X                a statement that reported a row count
//! N                a statement that returned nothing
//! ```
//!
//! A NULL is a lone `-`, which no hex encoding produces because `-` is not a
//! hex digit. The shim decodes this back into the list shapes the suite
//! compares, so what it reports is the engine's actual values and not a
//! reformatting of them.

use std::io::Write;

use nsqlite::connection::{Connection, Outcome};
use nsqlite::Value;

/// Writes one value as a type tag and a hex payload, or `-` for NULL.
///
/// The type tag is what lets the shim tell a TEXT value from a BLOB that holds
/// the same bytes. Without it a blob of `11` and the text `11` are the same
/// field, and the suite has tests that compare exactly that: `select3.test`
/// checks `typeof()`, and `types.test` stores both and reads them back.
///
/// A NULL is a lone `-`, which no tagged field produces, and the empty string
/// is a tag with no payload, so the two stay distinct -- the suite compares
/// `{}` against real values often enough that conflating them would be a silent
/// wrong answer.
fn field(out: &mut String, v: &Value) {
    out.push(' ');
    match v {
        Value::Null => out.push('-'),
        Value::Blob(b) => {
            out.push('B');
            push_hex(out, b);
        }
        Value::Text(s) => {
            out.push('T');
            push_hex(out, s.as_bytes());
        }
        other => {
            out.push(match other {
                Value::Integer(_) => 'I',
                _ => 'F',
            });
            push_hex(out, render(other).as_bytes());
        }
    }
}

fn push_hex(out: &mut String, bytes: &[u8]) {
    let mut s = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        s.push_str(&format!("{byte:02X}"));
    }
    out.push_str(&s);
}

/// Renders a non-NULL value the way SQLite's CLI renders it in text.
///
/// The float form matters: SQLite prints a real with `%!.15g` and appends
/// `.0` when that leaves it looking like an integer, so `1.0` stays `1.0` and
/// does not collapse to `1`. Getting this wrong turns every real-valued
/// comparison in the suite into a mismatch that looks like an engine bug.
fn render(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Integer(i) => i.to_string(),
        Value::Real(r) => format_real(*r),
        Value::Text(s) => s.clone(),
        Value::Blob(b) => b.iter().map(|x| format!("{x:02X}")).collect(),
    }
}

/// SQLite's text form for a real.
///
/// A real is rendered with the fewest significant digits that read back as the
/// same double -- the shortest round-tripping form -- laid out positionally
/// unless the exponent is below -4 or at least 17. A value that comes out
/// looking like an integer gets a `.0`, so a real never reads back as an
/// integer.
///
/// It is not `%!.15g`, which is a separate path with different results:
/// `printf('%!.15g', 1.0/3.0)` is `0.333333333333333` where the value renders
/// as `0.33333333333333332`. It is also not a fixed 17 digits, which would
/// render `1.0e-05` as `1.0000000000000001e-05`.
///
/// There is a known divergence where SQLite's own decimal conversion is not
/// correctly rounded and needs more digits than the shortest form would. On
/// `1.0/3.0` -- exact expansion `0.3333333333333333148...` -- the engine emits
/// `...332` where the shortest round-tripping form is `...3333`; on
/// `8.142857142857142` it emits `...1424` where the shortest is `...1422`.
/// Reproducing that would mean porting the engine's conversion routine, and it
/// only shows up on values that are not short exact decimals.
///
/// Every expected value in the test below is what `sqlite3` itself printed,
/// except the ones in the divergence test.
fn format_real(r: f64) -> String {
    if r.is_nan() {
        // SQLite renders a NaN as NULL. A NaN that reaches here came out of an
        // expression, and saying so is more useful than an empty field would be.
        return "NaN".to_string();
    }
    if r.is_infinite() {
        return if r > 0.0 { "Inf".into() } else { "-Inf".into() };
    }
    // Negative zero prints without its sign. SQLite's own printf does this, and
    // a test comparing `0.0` against `-0.0` is checking that they are equal,
    // not that they render differently.
    let r = if r == 0.0 { 0.0 } else { r };
    // The shortest form that reads back as the same double, which is what
    // SQLite emits. The search is at most 17 steps because a double always
    // round-trips within 17 significant digits.
    let s = (1..=17)
        .map(|p| significant(r, p))
        .find(|s| s.parse::<f64>() == Ok(r))
        .unwrap_or_else(|| significant(r, 17));
    // A real that reads as an integer must say it is a real.
    if s.contains('.') || s.contains('e') {
        s
    } else {
        format!("{s}.0")
    }
}

/// The `%g` layout for `r` at `prec` significant digits.
///
/// `{:.*e}` gives the digits and a decimal exponent, which is then laid out
/// the way SQLite lays a real out as text: exponential form when the exponent
/// is below -4 or at least 17, positional otherwise, and trailing zeroes
/// removed from the fractional part in both cases.
///
/// The 17 is a fixed threshold, not `prec`. It looks like a coincidence that it
/// equals the widest precision, but the two are separate: `printf('%!.15g',
/// 1e15)` prints `1.0e+15` while `SELECT 1e15` prints `1000000000000000.0`,
/// so the value-to-text conversion does not take the format string's precision
/// as its exponent cutoff.
fn significant(r: f64, prec: usize) -> String {
    let formatted = format!("{r:.*e}", prec - 1);
    let (mantissa, exp) = formatted
        .split_once('e')
        .expect("Rust's LowerExp always writes an exponent");
    let exp: i32 = exp.parse().expect("Rust always writes a decimal exponent");
    let negative = mantissa.starts_with('-');
    let digits: String = mantissa.chars().filter(|c| c.is_ascii_digit()).collect();
    // The digits are kept at the full precision here and stripped only after the
    // point has been placed. Stripping first loses the scale: 0.0001 at one
    // significant digit is the digit `1` with an exponent of -4, and without
    // the padding the layout would read that as 0.1.
    let digits = if digits.is_empty() { "0" } else { &digits };
    let trimmed = digits.trim_end_matches('0');
    let digits = if trimmed.is_empty() { "0" } else { trimmed };

    let body = if exp < -4 || exp >= 17 {
        // The fractional part is always present in exponential form, even when
        // it is a single zero: SQLite writes `1.0e+17`, not `1e+17`.
        let frac = if digits.len() > 1 { &digits[1..] } else { "0" };
        format!(
            "{}.{}e{}{:02}",
            &digits[..1],
            frac,
            if exp < 0 { '-' } else { '+' },
            exp.abs()
        )
    } else if exp < 0 {
        // Below 1 and above the exponential cutoff: `0.`, then enough zeroes to
        // reach the exponent, then the digits.
        format!("0.{}{digits}", "0".repeat((-exp - 1) as usize))
    } else {
        let point = exp as usize + 1;
        if digits.len() <= point {
            // A whole number. The `.0` that marks it as a real is added by the
            // caller, so nothing is appended here.
            format!("{digits}{}", "0".repeat(point - digits.len()))
        } else {
            format!("{}.{}", &digits[..point], &digits[point..])
        }
    };
    if negative {
        format!("-{body}")
    } else {
        body
    }
}

/// Runs `sql` against `conn` and writes the record stream to `out`.
///
/// Returns true when every statement ran, so the caller can pick an exit
/// status. An error is reported as an `E` record and stops the script, which
/// matches SQLite: the statements before the failure have already run, and the
/// shell does not go on to the ones after it.
pub fn run(conn: &mut Connection, sql: &str, out: &mut impl Write) -> bool {
    let stmts = match nsqlite::parser::parse_script(sql) {
        Ok(s) => s,
        Err(e) => {
            // A parse error covers the whole script, so nothing ran.
            let _ = writeln!(out, "E {}", e.message);
            return false;
        }
    };
    for stmt in stmts {
        match conn.execute(&stmt) {
            Ok(o) => emit(out, &o),
            Err(e) => {
                // The bare message, not the Display form. The suite compares
                // error text verbatim against what sqlite3 reports, and that is
                // `no such table: t1` -- the result-code name that Display
                // prefixes is part of the library's own formatting.
                let _ = writeln!(out, "E {}", e.message);
                return false;
            }
        }
    }
    true
}

fn emit(out: &mut impl Write, o: &Outcome) {
    let mut line = String::new();
    match o {
        Outcome::Query { columns, rows } => {
            line.push_str(&format!("C {}", columns.len()));
            for c in columns {
                field(&mut line, &Value::Text(c.clone()));
            }
            line.push('\n');
            let _ = out.write_all(line.as_bytes());
            for row in rows {
                let mut r = String::from("R");
                for v in &row.values {
                    field(&mut r, v);
                }
                r.push('\n');
                let _ = out.write_all(r.as_bytes());
            }
        }
        // A row count is not a result the suite reads back, so it is reported
        // as "ran, produced no rows" rather than as a value that would land in
        // the middle of a flattened comparison.
        Outcome::Changed(_) => {
            let _ = out.write_all(b"X\n");
        }
        Outcome::Nothing => {
            let _ = out.write_all(b"N\n");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::format_real;

    #[test]
    fn real_matches_sqlite_text_form() {
        // Every expected value here is what sqlite3 itself printed.
        assert_eq!(format_real(44.0), "44.0");
        assert_eq!(format_real(1.0), "1.0");
        assert_eq!(format_real(1.5), "1.5");
        assert_eq!(format_real(-0.0), "0.0");
        assert_eq!(format_real(0.5), "0.5");
        assert_eq!(format_real(1e15), "1000000000000000.0");
        assert_eq!(format_real(1e16), "10000000000000000.0");
        assert_eq!(format_real(1e17), "1.0e+17");
        assert_eq!(format_real(1e18), "1.0e+18");
        assert_eq!(format_real(100.0), "100.0");
        assert_eq!(format_real(1e-5), "1.0e-05");
        assert_eq!(format_real(0.0001), "0.0001");
        assert_eq!(format_real(1e300), "1.0e+300");
        assert_eq!(format_real(1e-300), "1.0e-300");
        assert_eq!(format_real(2.5e-10), "2.5e-10");
        assert_eq!(format_real(1.1), "1.1");
        assert_eq!(format_real(2.2), "2.2");
        assert_eq!(format_real(0.1), "0.1");
        assert_eq!(format_real(-3.5), "-3.5");
        assert_eq!(format_real(123456789.0), "123456789.0");
        assert_eq!(format_real(1234567890123456.0), "1234567890123456.0");
        assert_eq!(format_real(12345678901234567.0), "12345678901234568.0");
        assert_eq!(format_real(0.00012345), "0.00012345");
        assert_eq!(format_real(f64::MAX), "1.7976931348623157e+308");
        assert_eq!(format_real(1.5e10), "15000000000.0");
        assert_eq!(format_real(-1.5e10), "-15000000000.0");
        assert_eq!(format_real(0.1 + 0.2), "0.30000000000000004");
        assert_eq!(format_real(0.3), "0.3");
        assert_eq!(format_real(1234567890123456789.0), "1.2345678901234568e+18");
        assert_eq!(format_real(1.0 / 9.0), "0.1111111111111111");
        assert_eq!(format_real(0.001), "0.001");
        assert_eq!(format_real(0.5e-5), "5.0e-06");
        assert_eq!(format_real(999999999999999.0), "999999999999999.0");
        assert_eq!(format_real(9999999999999999.0), "10000000000000000.0");
        assert_eq!(format_real(123456789012345678.0), "1.2345678901234568e+17");
        assert_eq!(format_real(355.0 / 113.0), "3.1415929203539825");
    }

    #[test]
    fn real_last_digit_diverges_from_sqlite() {
        // SQLite's decimal conversion is not correctly rounded, so on some
        // values it emits more digits than the shortest round-tripping form
        // needs. These are the cases found while standing up the harness, and
        // they are recorded here rather than left to surprise a test run.
        //
        // The exact expansion of 1.0/3.0 is 0.3333333333333333148..., which
        // round-trips as 0.3333333333333333; sqlite3 renders it
        // 0.33333333333333332. 5e-324 is the same story: the shortest form is
        // 5.0e-324, and sqlite3 renders the full 4.9406564584124654e-324.
        for (ours, theirs) in [
            (format_real(1.0 / 3.0), "0.33333333333333332"),
            (format_real(2.0 / 3.0), "0.66666666666666663"),
            (format_real(22.0 / 7.0), "3.1428571428571428"),
            (format_real(1.0 / 49.0), "0.020408163265306121"),
            (format_real(2.0 / 9.0), "0.22222222222222221"),
            (format_real(5e-324), "4.9406564584124654e-324"),
            (format_real(1.0e-310), "9.9999999999999695e-311"),
        ] {
            assert_ne!(ours, theirs, "this value now agrees with sqlite3");
        }
    }
}
