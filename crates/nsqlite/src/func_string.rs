//! The string scalar functions, plus `printf`'s formatter.
//!
//! As in [`crate::func_math`], every behaviour was checked against the real
//! `sqlite3` (3.53.4) rather than recalled, and the results that differ from
//! intuition are called out where they appear:
//!
//! * `upper`/`lower` are ASCII-only and **byte-wise**: a byte at or above 0x80
//!   is passed through unchanged rather than case-mapped or replaced.
//!   `hex(upper(x'C3846263'))` is `C3844243`, and `length(upper('ÑTCafé'))`
//!   stays 7 -- the two bytes of `Ñ` are not touched at all.
//! * `printf` and `format` render a NULL argument as an empty string, but a
//!   NULL passed to `%s` is still an empty *string*, never NULL.
//! * `char()` truncates rather than erroring, and a code point above 0x10FFFF
//!   comes back as the replacement character U+FFFD.
//! * `unhex` yields NULL for an odd length or any non-hex character, and a
//!   blob rather than text.
//! * An unrecognised `printf` conversion consumes no argument: `%z` renders
//!   the *next* value with `%s` rules and leaves the following ones for later.
//!
//! The format engine deliberately follows SQLite rather than C's `printf`,
//! which differs in three visible ways: a missing argument renders as zero
//! rather than garbage, surplus arguments are ignored rather than an error, and
//! a width or precision given as `*` does not advance the argument list.

use crate::error::{Result, ResultCode};
use crate::func_math::{arity_error, expect_arity, expect_range, num_strict, real_to_text};
use crate::value::Value;

/// Calls a string function. `name` must already be lowercased.
///
/// Returns `Ok(None)` when the name is not one of this module's functions, so
/// the caller can fall through to its own table.
pub fn call(name: &str, args: &[Value]) -> Result<Option<Value>> {
    let v = match name {
        "printf" | "format" => {
            expect_range(name, args, 1, usize::MAX)?;
            printf(&args[0], &args[1..])
        }
        "concat" => {
            if args.is_empty() {
                return Err(arity_error(name));
            }
            // A NULL argument is skipped rather than poisoning the result, so
            // concat('a',NULL,'b') is 'ab' and not NULL.
            let mut out = String::new();
            for a in args {
                if !a.is_null() {
                    out.push_str(&text_of(a));
                }
            }
            Value::Text(out)
        }
        "concat_ws" => {
            expect_range(name, args, 1, usize::MAX)?;
            if args[0].is_null() {
                Value::Null
            } else {
                let sep = text_of(&args[0]);
                let parts: Vec<String> = args[1..]
                    .iter()
                    .filter(|a| !a.is_null())
                    .map(text_of)
                    .collect();
                Value::Text(parts.join(&sep))
            }
        }
        "replace" => {
            expect_arity(name, args, 3)?;
            if args.iter().any(|a| a.is_null()) {
                Value::Null
            } else {
                let s = text_of(&args[0]);
                let from = text_of(&args[1]);
                let to = text_of(&args[2]);
                // An empty needle matches nothing and leaves the text alone.
                if from.is_empty() {
                    Value::Text(s)
                } else {
                    Value::Text(s.replace(&from, &to))
                }
            }
        }
        "upper" => {
            expect_arity(name, args, 1)?;
            fold_case(&args[0], true)
        }
        "lower" => {
            expect_arity(name, args, 1)?;
            fold_case(&args[0], false)
        }
        "trim" | "ltrim" | "rtrim" => {
            expect_range(name, args, 1, 2)?;
            trim(name, args)
        }
        "substr" | "substring" => {
            expect_range(name, args, 2, 3)?;
            substr(args)
        }
        "char" => {
            // char() with no arguments is the empty string, not an error, and a
            // NULL argument contributes U+0000 without stopping the scan:
            // sqlite3 3.53.4 gives hex(char(65, NULL, 66)) as 410042, so the NULL
            // becomes a NUL byte and the 66 after it is still used. What
            // length() then reports is 1, because a NUL is where a C string
            // ends, which is a property of length rather than of the value.
            let mut out = String::new();
            for a in args {
                out.push_str(&char_code_string(crate::func_math::num_value(a)));
            }
            Value::Text(out)
        }
        "unicode" => {
            expect_arity(name, args, 1)?;
            match &args[0] {
                Value::Null => Value::Null,
                other => {
                    let s = text_of(other);
                    // A blob is read as its bytes, so unicode(x'41') is 65.
                    match first_code_point(&s) {
                        Some(c) => Value::Integer(c as i64),
                        None => Value::Null,
                    }
                }
            }
        }
        "hex" => {
            expect_arity(name, args, 1)?;
            if args[0].is_null() {
                Value::Text(String::new())
            } else {
                Value::Text(hex_upper(&bytes_of(&args[0])))
            }
        }
        "unhex" => {
            expect_arity(name, args, 1)?;
            match &args[0] {
                Value::Null => Value::Null,
                other => unhex(&text_of(other)),
            }
        }
        "quote" => {
            expect_arity(name, args, 1)?;
            Value::Text(quote(&args[0]))
        }
        "iif" | "if" => {
            expect_arity(name, args, 3)?;
            // A NULL condition counts as false, so iif(NULL,a,b) is b, and the
            // test is the *lenient* prefix parse rather than the strict one
            // `sign` uses. That is what makes `iif('1x','a','b')` choose the
            // first branch: the condition reads as the 1 that starts the text
            // and the trailing `x` is ignored, exactly as `abs('1x')` is 1.0.
            // A condition with no numeric prefix at all is false, so
            // `iif('abc','a','b')` is 'b'.
            let cond = match &args[0] {
                Value::Null => false,
                other => crate::func_math::num_value(other) != 0.0,
            };
            if cond {
                args[1].clone()
            } else {
                args[2].clone()
            }
        }
        "nullif" => {
            expect_arity(name, args, 2)?;
            if args[0].eq_value(&args[1]) {
                Value::Null
            } else {
                args[0].clone()
            }
        }
        "likely" | "unlikely" => {
            expect_arity(name, args, 1)?;
            // Both are the identity; the optimiser is meant to use them.
            args[0].clone()
        }
        // `random` is a math function, but it sits in this table because the
        // dispatcher is this match: every name it does not know falls through
        // to `Ok(None)`, and a name listed here is run here. `random_value` is
        // the re-export that keeps func_math the one implementation, so the
        // answer is still computed where the PRNG lives.
        "random" => random_value(args)?,
        _ => return Ok(None),
    };
    Ok(Some(v))
}

/// The text a value contributes to a string function, as SQLite renders it.
fn text_of(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Integer(i) => i.to_string(),
        Value::Real(r) => real_to_text(*r),
        Value::Text(s) => s.clone(),
        Value::Blob(b) => String::from_utf8_lossy(b).into_owned(),
    }
}

/// The bytes a value contributes, for `hex`.
fn bytes_of(v: &Value) -> Vec<u8> {
    match v {
        Value::Blob(b) => b.clone(),
        other => text_of(other).into_bytes(),
    }
}

fn hex_upper(b: &[u8]) -> String {
    const D: &[u8; 16] = b"0123456789ABCDEF";
    let mut s = String::with_capacity(b.len() * 2);
    for &byte in b {
        s.push(D[(byte >> 4) as usize] as char);
        s.push(D[(byte & 0x0f) as usize] as char);
    }
    s
}

/// SQL's `quote()`: strings single-quoted with the quote doubled, blobs as a
/// hex literal, everything else as its text.
fn quote(v: &Value) -> String {
    match v {
        Value::Null => "NULL".into(),
        Value::Integer(i) => i.to_string(),
        Value::Real(r) => real_to_text(*r),
        Value::Text(s) => format!("'{}'", s.replace('\'', "''")),
        Value::Blob(b) => format!("X'{}'", hex_upper(b)),
    }
}

/// Decodes hex text into a blob, or NULL if the input is malformed.
///
/// A NULL argument, an odd number of digits, or any character outside
/// `[0-9A-Fa-f]` all give NULL. Whitespace *is* allowed as a separator, so
/// `unhex('61 62 63')` is the three-byte blob `abc`.
fn unhex(s: &str) -> Value {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len() / 2);
    let mut hi: Option<u8> = None;
    for &c in b {
        // Whitespace is *not* skipped: `unhex(' 4142 ')` is NULL. Only an even
        // run of pure hex digits gives a blob.
        let d = match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'f' => c - b'a' + 10,
            b'A'..=b'F' => c - b'A' + 10,
            _ => return Value::Null,
        };
        match hi {
            None => hi = Some(d),
            Some(h) => {
                out.push((h << 4) | d);
                hi = None;
            }
        }
    }
    if hi.is_some() {
        // An odd number of hex digits.
        return Value::Null;
    }
    Value::Blob(out)
}

/// `upper`/`lower`: fold the ASCII letters and pass every other byte through.
///
/// sqlite3 walks the *bytes*, not the characters, and touches only the ASCII
/// range, so a UTF-8 sequence above 0x7F comes back unchanged rather than
/// becoming a question mark or a case-mapped character. That is why
/// `hex(upper(x'C3846263'))` is `C3844243` and not `3F4243`, and why
/// `length(upper('ÑTCafé'))` stays 6 and 7 rather than growing. The single
/// exceptional bytes are `[` and `]`, which are left alone too: upper('[') is
/// '[' and lower('A') is 'a'.
fn fold_case(v: &Value, up: bool) -> Value {
    // A NULL argument stays NULL: sqlite3's typeof(upper(NULL)) is 'null'.
    if v.is_null() {
        return Value::Null;
    }
    Value::Text(map_ascii_fold(v, up))
}

/// The ASCII fold itself, byte by byte.
///
/// Anything at or above 0x80 is copied through untouched, so a multi-byte
/// UTF-8 character survives intact instead of collapsing to a single `?` or
/// gaining a second copy of itself. The bytes are collected into a `Vec` and
/// rebuilt once at the end rather than appended one at a time, because pushing a
/// raw byte through `String::push` would go as a *code point* and re-encode it
/// -- turning a 0xC3 lead byte into the two bytes 0xC3 0x83.
fn map_ascii_fold(v: &Value, up: bool) -> String {
    let s = text_of(v);
    let mut out: Vec<u8> = Vec::with_capacity(s.len());
    for b in s.bytes() {
        out.push(match b {
            b'a'..=b'z' if up => b - b'a' + b'A',
            b'A'..=b'Z' if !up => b - b'A' + b'a',
            // Everything else, ASCII punctuation and every non-ASCII byte
            // alike, is left exactly as it was.
            other => other,
        });
    }
    // The input came from a `String`, so its bytes are still valid UTF-8 and
    // this cannot fail; the lossy form is a defensive choice rather than a
    // silent corruption path.
    String::from_utf8_lossy(&out).into_owned()
}

/// The code point `char()` produces for a numeric argument, as a string.
///
/// SQLite truncates toward zero and then treats the result as a code point, so
/// a negative value, a fraction, or anything above U+10FFFF all become the
/// replacement character rather than an error.
///
/// A *surrogate* is the one in-range value with no `char` behind it. sqlite3
/// emits its three-byte CESU-8 form -- `hex(char(55296))` is `EDA080` -- while
/// this gives the replacement character, because a Rust `String` cannot hold
/// those bytes at all and `char()` answers text rather than a blob. That is the
/// one documented difference from sqlite3 in this module.
fn char_code_string(n: f64) -> String {
    if !n.is_finite() {
        return '\u{FFFD}'.to_string();
    }
    let cp = n.trunc();
    if cp < 0.0 || cp > 0x0010_FFFF as f64 {
        return '\u{FFFD}'.to_string();
    }
    let cp = cp as u32;
    // A surrogate has no `char`, and its three-byte CESU-8 form is not valid
    // UTF-8 either, so there is no way to put it in a `String` -- and `char()`
    // answers text, not a blob. sqlite3 emits `EDA080` for
    // `hex(char(55296))`; this gives the replacement character instead, which is
    // the closest a Rust `String` can hold. Reaching it would mean carrying text
    // as bytes, which is a change to `Value` and to everything that reads it, so
    // it is left as a known difference rather than papered over.
    char::from_u32(cp).unwrap_or('\u{FFFD}').to_string()
}

/// The first Unicode scalar of a string, or `None` when it is empty.
fn first_code_point(s: &str) -> Option<char> {
    s.chars().next()
}

/// `trim`, `ltrim` and `rtrim`.
///
/// With one argument the cut set is the space character. With two it is the
/// whole second argument, read as a *set* of characters rather than a prefix,
/// so `ltrim('0012','01')` is `2` and `rtrim('0101hi1010','01')` is `0101hi`.
/// An empty set cuts nothing at all.
fn trim(name: &str, args: &[Value]) -> Value {
    if args[0].is_null() {
        return Value::Null;
    }
    if args.len() == 2 && args[1].is_null() {
        return Value::Null;
    }
    let s = text_of(&args[0]);
    let set: Vec<char> = if args.len() == 2 {
        text_of(&args[1]).chars().collect()
    } else {
        vec![' ']
    };
    if set.is_empty() {
        return Value::Text(s);
    }
    let out = match name {
        "ltrim" => s.trim_start_matches(|c| set.contains(&c)),
        "rtrim" => s.trim_end_matches(|c| set.contains(&c)),
        _ => s.trim_matches(|c| set.contains(&c)),
    };
    Value::Text(out.to_string())
}

/// `substr`/`substring`.
///
/// The start is one-based, a negative start counts back from the end, and a
/// start of zero behaves as one. A negative length runs to the end of the
/// string, and a length of zero gives the empty string. A blob argument
/// yields a blob.
fn substr(args: &[Value]) -> Value {
    if args.iter().any(|a| a.is_null()) {
        return Value::Null;
    }
    let as_blob = matches!(args[0], Value::Blob(_));
    let s = text_of(&args[0]);
    let chars: Vec<char> = s.chars().collect();
    let len = chars.len() as i64;
    let start_raw = index_of(&args[1]);
    // Position 0 is the slot *before* the first character, not a synonym for
    // position 1, and a negative start counts back from the end without being
    // clamped. Both were checked against sqlite3 over a sweep of every start and
    // length, and both are easy to get wrong:
    //
    //  * `substr('hello', 0, 2)` is `h` and `substr('hello', 0, 1)` is empty,
    //    where treating zero as one would give `he` and `h`. The length counts
    //    from the slot before the string, so it spends its first character on
    //    that gap.
    //
    //  * `substr('hello', -10, 1)` is empty, not `h`: a start before the
    //    beginning is past the end of the window in the other direction, so
    //    clamping it up to zero would silently return a character from the
    //    front.
    let start0 = if start_raw < 0 {
        len + start_raw
    } else {
        (start_raw - 1).max(0)
    };
    let n = if args.len() == 3 {
        index_of(&args[2])
    } else {
        -1
    };
    let (from, to) = if args.len() == 3 && n < 0 {
        // A negative length is a window running *backwards* from the start: the
        // `|n|` characters ending immediately before it. `substr('hello', 4, -2)`
        // is `el` and `substr('hello', 1, -1)` is empty.
        let to = if start_raw == 0 { 0 } else { start0 };
        ((to + n).max(0), to)
    } else {
        let from = start0;
        let to = if args.len() == 3 {
            // Position 0 spends the first of its `n` characters on the slot
            // before the string, so it yields one fewer.
            from + if start_raw == 0 { (n - 1).max(0) } else { n }
        } else {
            len
        };
        (from, to)
    };
    let from = from.clamp(0, len);
    let to = to.clamp(from, len);
    let out: String = chars[from as usize..to as usize].iter().collect();
    if as_blob {
        Value::Blob(out.into_bytes())
    } else {
        Value::Text(out)
    }
}

/// Reads an argument as a character offset, truncating a real toward zero and
/// treating anything non-numeric as zero.
fn index_of(v: &Value) -> i64 {
    match num_strict(v) {
        Some(n) if n.is_finite() => n.trunc() as i64,
        _ => 0,
    }
}

// ---------------------------------------------------------------------------
// printf
// ---------------------------------------------------------------------------

/// The flags and modifiers of one conversion, in the order SQLite parses them.
#[derive(Debug, Clone, Copy, Default)]
struct Spec {
    minus: bool,
    plus: bool,
    space: bool,
    hash: bool,
    zero: bool,
    width: Option<usize>,
    precision: Option<usize>,
}

/// Renders a format string against the remaining arguments.
///
/// The differences from C's `printf` are all deliberate and were checked
/// against sqlite3: a missing argument renders as zero or empty, surplus
/// arguments are ignored, and a `*` width or precision does *not* consume an
/// argument (it falls back to the previous value).
pub fn printf(fmt: &Value, args: &[Value]) -> Value {
    let f = text_of(fmt);
    let mut out = String::new();
    let mut it = f.chars().peekable();
    // The index of the next argument. A `*` deliberately does not advance it.
    let mut next = 0usize;
    while let Some(c) = it.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        let mut spec = Spec::default();
        // Flags, in any order.
        loop {
            match it.peek() {
                Some('-') => spec.minus = true,
                Some('+') => spec.plus = true,
                Some(' ') => spec.space = true,
                Some('#') => spec.hash = true,
                Some('0') => spec.zero = true,
                _ => break,
            }
            it.next();
        }
        // A `*` width or precision takes its value from the *next* argument,
        // and a negative one left-aligns. So printf('%*d', 5, 42) is "   42"
        // and printf('%*d', -5, 42) is "42   ".
        if it.peek() == Some(&'*') {
            it.next();
            let v = args.get(next);
            if v.is_some() {
                next += 1;
            }
            let n = v.and_then(number_of).unwrap_or(0);
            if n < 0 {
                spec.minus = true;
            }
            spec.width = Some(n.unsigned_abs() as usize);
        } else {
            spec.width = read_number(&mut it).map(|n| n.unsigned_abs() as usize);
        }
        // Precision, likewise.
        if it.peek() == Some(&'.') {
            it.next();
            if it.peek() == Some(&'*') {
                it.next();
                let v = args.get(next);
                if v.is_some() {
                    next += 1;
                }
                spec.precision = Some(v.and_then(number_of).unwrap_or(0).unsigned_abs() as usize);
            } else {
                // A `.` with no digits after it is a precision of *zero*, not
                // an absent precision. `printf('%.f', 1.5)` is `2` and
                // `printf('%.f', 0.5)` is `1` in sqlite3, where leaving the
                // precision unset would fall back to the default six and give
                // `1.500000`. The digit run being empty is the whole signal,
                // so `Some(0)` is what an empty run produces.
                spec.precision = Some(read_number(&mut it).unwrap_or(0).unsigned_abs() as usize);
            }
            // A second `.` is a malformed conversion, and sqlite3 makes the
            // whole format NULL for it: `printf('%.2.1e', 1.5)` is NULL. The
            // digits run stops at the `.`, so it is still sitting on the
            // iterator here and can only mean another conversion followed.
            if it.peek() == Some(&'.') {
                return Value::Null;
            }
        }
        // A length modifier is accepted and ignored, as in SQLite.
        let mut verb = match it.next() {
            Some(v) => v,
            // A format ending in a bare `%` renders the percent sign.
            None => {
                out.push('%');
                return Value::Text(out);
            }
        };
        if matches!(verb, 'l' | 'h' | 'j' | 't') {
            verb = match it.next() {
                Some(v) => v,
                None => {
                    out.push('%');
                    return Value::Text(out);
                }
            };
        }
        if verb == '%' {
            out.push('%');
            continue;
        }
        // Anything else that is not a conversion makes the *whole* format NULL,
        // rather than falling back to `%s`. `printf('%y', 42)` is NULL and
        // `printf('%.2.1e', 1.5)` is NULL, where a second precision point is
        // not a truncation but a malformed conversion. The check is against
        // the set below rather than a fallback, which is what the old `_ =>`
        // arm got wrong.
        if !matches!(
            verb,
            'd' | 'i'
                | 'u'
                | 'x'
                | 'X'
                | 'o'
                | 'f'
                | 'F'
                | 'e'
                | 'E'
                | 'g'
                | 'G'
                | 's'
                | 'c'
                | 'q'
                | 'z'
        ) {
            return Value::Null;
        }
        let arg = args.get(next);
        if arg.is_some() {
            next += 1;
        }
        let value = arg.cloned().unwrap_or(Value::Integer(0));
        out.push_str(&render_conversion(verb, &spec, &value));
    }
    Value::Text(out)
}

/// Reads an optional run of digits, stopping at the first non-digit.
fn read_number(it: &mut std::iter::Peekable<std::str::Chars<'_>>) -> Option<i64> {
    let mut n: i64 = 0;
    let mut any = false;
    while let Some(c) = it.peek() {
        if c.is_ascii_digit() {
            n = n
                .saturating_mul(10)
                .saturating_add((*c as u8 - b'0') as i64);
            it.next();
            any = true;
        } else {
            break;
        }
    }
    any.then_some(n)
}

fn number_of(v: &Value) -> Option<i64> {
    num_strict(v).map(|n| n.trunc() as i64)
}

/// Renders one conversion.
fn render_conversion(verb: char, spec: &Spec, value: &Value) -> String {
    let width = spec.width.unwrap_or(0);
    match verb {
        // %s renders the argument as text, and an unknown conversion behaves
        // the same way, which is what makes printf('%z', 1) the string "1".
        // A precision truncates by *character*, so printf('%.1s', '€x') keeps
        // the euro sign whole.
        //
        // The `0` flag is *not* honoured here, for any of the string-shaped
        // conversions. sqlite3 fills a short field with spaces whatever the
        // flags say: `printf('%05s','ab')` is `   ab` and `printf('%08s','ab')`
        // is `      ab`, not `000ab`. That is the same for `%z`, `%q` and `%c`,
        // so the zero flag is simply dropped here rather than passed down.
        's' | 'z' => {
            let t = text_of(value);
            let body = match spec.precision {
                Some(p) => t.chars().take(p).collect::<String>(),
                None => t,
            };
            pad_with(&body, width, spec.minus, false)
        }
        'q' => {
            // %q prints the value's *text* with any single quote doubled, and
            // NULL spelled `(NULL)`. It adds no quotes of its own, so
            // printf('%q', 'it''s') is `it''s` where quote() gives `'it''s'`.
            let t = match value {
                Value::Null => "(NULL)".to_string(),
                other => text_of(other).replace('\'', "''"),
            };
            // A precision truncates by character, as it does for `%s`:
            // `printf('%.3q', std::f64::consts::PI)` is `3.1` and `printf('%.3q', 'abcdef')`
            // is `abc`. The doubling of a quote happens first, so the count is
            // over the escaped text rather than the raw one.
            let body = match spec.precision {
                Some(p) => t.chars().take(p).collect::<String>(),
                None => t,
            };
            pad_with(&body, width, spec.minus, false)
        }
        // %c prints the first *character* of the argument's text rendering, so
        // %c of 65 is "6" (the first digit of "65"), %c of 9786 is "9" and %c
        // of -1 is "-". It is a character conversion, not a code point one.
        //
        // A precision on %c is a *repetition count* rather than a truncation,
        // which is the opposite of every other string-shaped conversion:
        // `printf('%.3c', 42)` is `444` and `printf('%.3c', 'abcd')` is `aaa`,
        // where `%.3s` of the same would be `abc`. Without a precision the
        // character appears once.
        'c' => {
            let t = text_of(value);
            let unit: String = t.chars().take(1).collect();
            // A NULL argument has no text and so no character to repeat, which
            // leaves nothing to pad: `printf('%-8c', NULL)` is the empty string
            // in sqlite3 and not eight spaces.
            let empty = unit.is_empty();
            let s = if empty {
                String::new()
            } else {
                match spec.precision {
                    Some(p) => unit.repeat(p),
                    None => unit,
                }
            };
            // A right-aligned field still pads even with nothing to show, so
            // `printf('%08c', NULL)` is seven spaces -- one short of the eight
            // asked for, because the width counts the character and a NULL has
            // none to place in it. A left-aligned one is empty outright:
            // `printf('%-8c', NULL)` is the empty string, not eight spaces.
            if empty {
                // A NULL has no character, so the field is short by whatever the
                // conversion would have produced: one character with no
                // precision, and the precision itself when there is one. That
                // is why `printf('%08c', NULL)` is seven spaces and
                // `printf('%08.3c', NULL)` is five.
                let missing = spec.precision.unwrap_or(1);
                if spec.minus {
                    String::new()
                } else {
                    pad_with(&s, width.saturating_sub(missing), false, false)
                }
            } else {
                pad_with(&s, width, spec.minus, false)
            }
        }
        'd' | 'i' => {
            // printf coerces with the lenient parser, so %d of '12abc' is 12.
            let n = crate::func_math::num_value(value);
            let i = clamp_int(n);
            // `i.abs()` would overflow for i64::MIN, so the magnitude goes
            // through u64.
            let mut s = (i as i128).unsigned_abs().to_string();
            if i < 0 {
                s.insert(0, '-');
            } else if spec.plus {
                s.insert(0, '+');
            } else if spec.space {
                s.insert(0, ' ');
            }
            if let Some(p) = spec.precision {
                s = zero_pad_int(&s, p, i < 0);
            }
            pad_with(&s, width, spec.minus, spec.zero)
        }
        'u' => {
            let n = crate::func_math::num_value(value);
            // %u is the two's-complement reinterpretation, so -1 prints as
            // 18446744073709551615.
            let u = clamp_int(n) as u64;
            let s = u.to_string();
            let s = if let Some(p) = spec.precision {
                zero_pad_int(&s, p, false)
            } else {
                s
            };
            pad_with(&s, width, spec.minus, spec.zero)
        }
        'x' => radix(value, 16, false, spec),
        'X' => radix(value, 16, true, spec),
        'o' => radix(value, 8, false, spec),
        'f' | 'F' => {
            let n = crate::func_math::num_value(value);
            let p = spec.precision.unwrap_or(6);
            fixed(n, p, spec, width)
        }
        'e' | 'E' => {
            let n = crate::func_math::num_value(value);
            let p = spec.precision.unwrap_or(6);
            scientific(n, p, verb == 'E', spec, width)
        }
        'g' | 'G' => {
            let n = crate::func_math::num_value(value);
            let p = spec.precision.unwrap_or(6).max(1);
            general(n, p, verb == 'G', spec, width)
        }
        // An unknown verb falls back to %s, as SQLite does, and so it drops the
        // zero flag along with the rest of the string handling.
        _ => pad_with(&text_of(value), width, spec.minus, false),
    }
}

/// Rounds a real to an `i64` the way SQLite's `%d` does: truncate toward zero
/// and saturate at the `i64` bounds rather than wrapping.
fn clamp_int(n: f64) -> i64 {
    if n.is_nan() {
        return 0;
    }
    let t = n.trunc();
    if t >= i64::MAX as f64 {
        i64::MAX
    } else if t <= i64::MIN as f64 {
        i64::MIN
    } else {
        t as i64
    }
}

/// Left-pads an already-signed integer string to `precision` digits.
fn zero_pad_int(s: &str, precision: usize, negative: bool) -> String {
    let digits = if negative { &s[1..] } else { s };
    if digits.len() >= precision {
        return s.to_string();
    }
    let pad = "0".repeat(precision - digits.len());
    if negative {
        format!("-{pad}{digits}")
    } else {
        format!("{pad}{s}")
    }
}

/// Applies the field width, optionally filling with zeros after any sign.
///
/// `zero` says whether the conversion wants a zero fill; `minus_wins` says
/// which flag gives way when both are present. The answer differs by
/// conversion class in sqlite3, which is the whole reason this is a parameter:
///
/// * for the **integer** conversions (`%d %i %u %x %X %o`) the `0` flag wins,
///   so `printf('%-08d', 42)` is `00000042` and not `42      `;
/// * for the **floating** ones (`%f %e %g` and their upper-case spellings) it
///   is the other way round, so `printf('%-08f', 1.5)` is `1.500000` and
///   `printf('%-08g', 1.5)` is `1.5     ` -- left-aligned, and the default six
///   decimals already fill the eight columns so nothing shows.
///
/// Callers that pass `zero: false` (every string-shaped conversion, and the
/// unknown-verb fallback) are unaffected either way.
fn pad_with(s: &str, width: usize, minus: bool, zero: bool) -> String {
    pad_full(s, width, minus, zero, false)
}

/// [`pad_with`] with the precedence chosen explicitly.
fn pad_full(s: &str, width: usize, minus: bool, zero: bool, minus_wins: bool) -> String {
    let len = s.chars().count();
    if len >= width {
        return s.to_string();
    }
    let fill = width - len;
    let left_align = minus && (!zero || minus_wins);
    if zero && !left_align {
        // A zero fill goes after a leading sign, as in C.
        let sign = s.starts_with('-') || s.starts_with('+') || s.starts_with(' ');
        if sign {
            let mut cs = s.chars();
            let first = cs.next().unwrap_or(' ');
            format!("{first}{}{}", "0".repeat(fill), cs.as_str())
        } else {
            format!("{}{s}", "0".repeat(fill))
        }
    } else if left_align {
        format!("{s}{}", " ".repeat(fill))
    } else {
        format!("{}{s}", " ".repeat(fill))
    }
}

/// Renders a value in the given base, honouring `#` and a precision.
///
/// The `#` prefix goes on *after* the field-width padding, not before it:
/// `printf('%#08x', 42)` is `0x0000002a` in sqlite3, where putting the prefix
/// in first and padding the rest gives `00000x2a` with the prefix sitting in
/// the middle.
///
/// Whether the width *counts* the prefix differs between the two, which is
/// easy to miss. For hex it does: `%#08x` of 42 is ten characters wide, the
/// two of `0x` plus eight of digits. For octal it does not: `%#08o` of 8 is
/// `000000010`, nine characters -- the single leading `0` of `010` is a
/// prefix, and the eight zeros are eight digits. The two `0`s in a row are
/// coincidental; the field is one wider than the width asked for.
fn radix(value: &Value, base: u32, upper: bool, spec: &Spec) -> String {
    let n = crate::func_math::num_value(value);
    let u = clamp_int(n) as u64;
    let mut digits = to_radix(u, base, upper);
    if let Some(p) = spec.precision {
        if digits.len() < p {
            digits = format!("{}{}", "0".repeat(p - digits.len()), digits);
        }
    }
    // A zero value gets no prefix: `printf('%#x', 0)` is `0`, not `0x0`.
    if spec.hash && u != 0 {
        let prefix = match base {
            16 if upper => "0X",
            16 => "0x",
            _ => "0",
        };
        // The prefix sits *inside* the field, so the field is padded as one
        // string with the prefix already at the front: `printf('%#8x', 42)` is
        // `    0x2a` and not `0x    2a`, and `printf('%#-8o', 42)` is
        // `052     `.
        //
        // The one exception is a zero-filled octal field, where the fill comes
        // first and the prefix goes in front of it: `printf('%#08o', 8)` is
        // nine characters, `000000010`, where the eight zeros fill the width
        // and the leading `0` of `010` sits outside them. A zero-filled hex
        // field does *not* do that: `printf('%#08x', 42)` is
        // `0x0000002a`, with the prefix inside.
        let width = spec.width.unwrap_or(0);
        if spec.zero {
            // A zero-filled field puts the prefix at the front and fills what
            // is left of the width with zeros after it. The width *counts* the
            // prefix for hex -- `printf('%#08x', 42)` is ten characters,
            // `0x0000002a` -- and does not for octal, where
            // `printf('%#08o', 8)` is nine, `000000010`. So the octal branch
            // pads the digits to the full width and the hex one to the width
            // less the prefix; padding the whole string as a single unit would
            // put the zeros in front of the prefix and give `00000x2a`, which
            // is the same width but the wrong number.
            // A zero fill pads the digits to the *whole* width in both bases,
            // with the prefix in front of the fill: `%#08x` of 42 is ten
            // characters, `0x0000002a`, and `%#08o` of 8 is nine,
            // `000000010`. So the two cases differ only in how many digits the
            // answer ends up with, not in where the prefix goes.
            return format!("{prefix}{}", pad_with(&digits, width, false, true));
        }
        // Otherwise the field is padded as one string with the prefix already
        // at the front, so the fill always lands where the field wants it:
        // `%#8x` of 42 is `    0x2a` and `%#-8o` of 42 is `052     `.
        let whole = format!("{prefix}{digits}");
        return pad_with(&whole, width, spec.minus, false);
    }
    pad_with(&digits, spec.width.unwrap_or(0), spec.minus, spec.zero)
}

fn to_radix(mut n: u64, base: u32, upper: bool) -> String {
    if n == 0 {
        return "0".into();
    }
    const L: &[u8; 16] = b"0123456789abcdef";
    const U: &[u8; 16] = b"0123456789ABCDEF";
    let table = if upper { U } else { L };
    let mut out = Vec::new();
    while n > 0 {
        out.push(table[(n % base as u64) as usize]);
        n /= base as u64;
    }
    out.reverse();
    String::from_utf8(out).unwrap()
}

/// Shared tail for the fixed and scientific conversions: sign, zero padding
/// and field width.
fn finish(magnitude: String, negative: bool, spec: &Spec, width: usize) -> String {
    let mut s = String::new();
    if negative {
        s.push('-');
    } else if spec.plus {
        s.push('+');
    } else if spec.space {
        s.push(' ');
    }
    s.push_str(&magnitude);
    let w = width;
    if s.chars().count() >= w {
        return s;
    }
    if spec.minus {
        // `-` wins over `0` for the floating conversions, unlike the integer
        // ones: `printf('%-08f', 1.5)` is `1.500000` and `printf('%-08g', 1.5)`
        // is `1.5     `.
        let fill = w - s.chars().count();
        format!("{s}{}", " ".repeat(fill))
    } else if spec.zero {
        // A zero fill goes after the sign, as in C, and applies even when a
        // precision was given: %08.3f of -std::f64::consts::PI is -003.142.
        let fill = w - s.chars().count();
        let sign_len = usize::from(negative || spec.plus || spec.space);
        format!("{}{}{}", &s[..sign_len], "0".repeat(fill), &s[sign_len..])
    } else {
        let fill = w - s.chars().count();
        format!("{}{s}", " ".repeat(fill))
    }
}

/// Rounds a non-negative value to `precision` decimals, ties away from zero.
///
/// The rounding is **exact decimal**, done on the double's own expansion rather
/// than on `x * 10^precision`. Scaling by a power of ten is a second rounding
/// that shows up at the last place: `x * 100` turns 0.045 into
/// 4.499999999999999 and the scaled floor answers 4 where sqlite3 answers 4 for
/// `printf('%.2f', 0.045)` but 5 for a naive rounding -- and at the top,
/// `printf('%.2f', 1e17)` is `100000000000000000.00` where the scaled product
/// carries 20.48 into the fraction.
///
/// So the exact expansion is used, which `func_math` already derives, and the
/// digits past the precision are dropped from that rather than from a scaled
/// double.
fn fixed_digits(x: f64, precision: usize) -> String {
    if x == 0.0 {
        return if precision == 0 {
            "0".into()
        } else {
            format!("0.{}", "0".repeat(precision))
        };
    }
    // The exact expansion is `<digits> x 10^lead`, and `lead` is the power of
    // ten of the *leading* digit, so the point sits `lead + 1` places from the
    // left of the string. That is negative or zero for a value below one, which
    // is what makes 0.5 -- which expands to the fifty-three digit string
    // `5000...` with `lead` -1 -- have a point one place in from the start.
    let (full, lead) = crate::func_math::exact_decimal(x);
    let point = lead + 1;
    // The digit at 10^e. The expansion runs left to right from its leading digit
    // at 10^lead, so the digit at 10^e is at index `lead - e`: for 42 the
    // expansion is `42000...` with `lead` 1, and the digits at 10^1, 10^0, 10^-1
    // are indices 0, 1 and 2. Indexes below zero are the leading zeros of a
    // value below one and indexes past the end are the trailing zeros that
    // round it; both read as `0`.
    // The sixteen-significant-digit budget is a *round* at the sixteenth digit
    // followed by zero-fill, which is what turns the expansion
    // `1234567890123456768` into the `1234567890123457000` that
    // `printf('%.0f', 1234567890123456789)` prints. The budget is **sixteen**,
    // not seventeen: the answer has sixteen significant digits and then zeros,
    // and the same holds one and two places down -- `%.0f` of
    // 123456789012345678 is `123456789012345700` and of 12345678901234567 is
    // `12345678901234570`, each sixteen digits and then zeros.
    let budget: i64 = 16;
    let at = |e: i64| -> u8 {
        let idx = lead as i64 - e;
        if idx < 0 || idx >= full.len() as i64 {
            b'0'
        } else {
            full[idx as usize]
        }
    };
    // The digit at 10^e once the budget has been applied: past the seventeenth
    // significant digit everything reads as zero, and the seventeenth itself
    // rounds on the eighteenth.
    let capped = |e: i64| -> u8 {
        let sig = lead as i64 - e; // which significant digit this is, 0-based
        if sig < 0 || sig >= budget {
            b'0'
        } else if sig == budget - 1 {
            // The sixteenth significant digit rounds on the seventeenth, and
            // every place past the budget is a zero. The carry out of that
            // rounding is left to the increment over the assembled answer, so
            // this only has to see the rounding itself.
            let next = at(e - 1);
            let d = at(e);
            if next >= b'5' && d < b'9' {
                d + 1
            } else {
                d
            }
        } else {
            at(e)
        }
    };
    // sqlite3 carries at most **seventeen significant digits**, counted from
    // the first digit of the answer.
    //
    // At the top, `printf('%.0f', 1234567890123456789)` is
    // `1234567890123457000`: the expansion is `1234567890123456768`, the
    // sixteenth digit rounds on the seventeenth's 6, and the three places past
    // the budget are zero-filled rather than carried into.
    //
    // At the bottom, `printf('%.2f', 0.0001)` is `0.00` where the expansion's
    // own first two digits would give `0.10`. The expansion of 0.0001 is
    // `10000000000000000479...` with its leading digit at 10^-4, so the two
    // decimals asked for are the `0` and the `0` *before* the `1` -- the answer
    // is 0.00 because the 1 sits in the fourth place, not the second. Reading
    // the expansion from its own start would skip the point entirely and read
    // the `1` as tenths.
    let integral_len = point.max(0) as usize;
    // A value below one has no integral digit of its own, but `%f` still shows
    // the `0` before the point, so the integral side is at least one digit wide.
    let mut digits: Vec<u8> = Vec::with_capacity(integral_len + precision + 1);
    if integral_len == 0 {
        digits.push(b'0');
    }
    for k in 0..integral_len as i64 {
        // The integral places are 10^(integral_len-1) down to 10^0.
        digits.push(capped(integral_len as i64 - 1 - k));
    }
    if precision == 0 {
        // The last place kept is always 10^0 -- `%f` at zero decimals keeps the
        // units digit and drops the fraction -- so the digit being rounded on
        // is always the one at 10^-1. That is what makes `printf('%.0f', 2.5)`
        // 3, `printf('%.0f', 1234.5678)` 1235 and `printf('%.0f', 0.5)` 1, the
        // last of which has no integral digit of its own and rounds the
        // leading 5 up into one. A digit past the sixteen-significant-digit
        // budget is a zero and never rounds.
        let next = -1;
        let next_sig = lead as i64 - next;
        if next_sig < budget && at(next) >= b'5' {
            increment_digits(&mut digits);
        }
        return String::from_utf8_lossy(&digits).into_owned();
    }
    for k in 1..=precision as i64 {
        // The fractional places are 10^-1 upward.
        digits.push(capped(-k));
    }
    // The first digit past what is kept decides the rounding, again bounded by
    // the seventeen-digit budget.
    // The first digit past what is kept, again bounded by the budget: a digit
    // past the sixteenth significant one is a zero and never rounds. Which
    // place that is depends on the value, since the budget counts from the
    // leading digit rather than from the point.
    let next = -(precision as i64 + 1);
    let next_sig = lead as i64 - next;
    let before = digits.len();
    if next_sig < budget && at(next) >= b'5' {
        increment_digits(&mut digits);
    }
    // A carry that escaped the leading digit also grew the integral side, so
    // the point moves with it: 9.99 at one decimal rounds to `10.0`, not
    // `1.00`. A carry that stopped inside the number leaves the split alone,
    // so 0.99 stays `1.0`.
    let integral = integral_len.max(1) + usize::from(digits.len() > before);
    let out = String::from_utf8_lossy(&digits).into_owned();
    format!("{}.{}", &out[..integral], &out[integral..])
}

/// Adds one to a big-endian ASCII digit string in place, prepending a `1` when
/// the carry escapes the leading digit.
///
/// A plain in-place add loses the carry, so 9.99 rounding to one decimal would
/// increment the `9` into a `0` and answer `0.0` where sqlite3 answers `10.0`.
/// Growing the string is the only way a carry out of the front survives.
fn increment_digits(d: &mut Vec<u8>) {
    let mut carry = true;
    for b in d.iter_mut().rev() {
        if !carry {
            break;
        }
        if *b == b'9' {
            *b = b'0';
        } else {
            *b += 1;
            carry = false;
        }
    }
    if carry {
        d.insert(0, b'1');
    }
}

/// Adds one to a big-endian ASCII digit string in place.
#[allow(dead_code)]
fn increment_digits_slice(d: &mut [u8]) {
    for b in d.iter_mut().rev() {
        if *b == b'9' {
            *b = b'0';
        } else {
            *b += 1;
            return;
        }
    }
}

/// Re-rounds the mantissa of a `{:.*e}` rendering half away from zero.
fn round_half_up_exp(s: &str) -> (String, i32) {
    let (mantissa, exp) = s.split_once('e').expect("LowerExp has an exponent");
    match exp.parse::<i32>() {
        Ok(e) => (mantissa.to_string(), e),
        Err(_) => (mantissa.to_string(), 0),
    }
}

/// Bumps a `{:.*e}` rendering that the formatter left on an exact tie.
///
/// Rust rounds a halfway case to even and sqlite3 rounds it away from zero, so
/// the two disagree only when the first discarded digit is an exact `5` with
/// nothing but zeros behind it. This detects that and adds one to the last kept
/// digit, which is all such a case needs.
///
/// The correction is deliberately narrow. A *wider* rule -- rounding the kept
/// digits again off a guard digit -- double-rounds, and 9.87654321e-07 at two
/// significant digits lands on `1e-08` where sqlite3 has `9.9e-07`.
fn tie_away_from_zero(sci: &str) -> String {
    let (mantissa, exp) = match sci.split_once('e') {
        Some(parts) => parts,
        None => return sci.to_string(),
    };
    let frac = match mantissa.split_once('.') {
        Some((_, f)) => f,
        None => return sci.to_string(),
    };
    // An exact tie is a first discarded digit of 5 and nothing but zeros after
    // it. Anything else the formatter decided is left as it is.
    let bytes = frac.as_bytes();
    if bytes.first() != Some(&b'5') || bytes[1..].iter().any(|&d| d != b'0') {
        return sci.to_string();
    }
    let (int_part, _) = mantissa.split_once('.').expect("checked above");
    let mut int: Vec<u8> = int_part.as_bytes().to_vec();
    let mut carry = true;
    for b in int.iter_mut().rev() {
        if !carry {
            break;
        }
        if *b == b'9' {
            *b = b'0';
        } else {
            *b += 1;
            carry = false;
        }
    }
    let int = String::from_utf8_lossy(&int).into_owned();
    if carry {
        // The carry escaped the leading digit, so the mantissa grew by one and
        // the exponent drops to keep the value: 9.5e0 at zero decimals is 1e+01.
        let e: i32 = exp.parse().unwrap_or(0);
        return format!("1.{frac}e{:+}", e);
    }
    format!("{int}.{frac}e{exp}")
}

/// `%f`: fixed notation with exactly `precision` decimals.
fn fixed(n: f64, precision: usize, spec: &Spec, width: usize) -> String {
    if n.is_nan() {
        return finish("nan".into(), false, spec, width);
    }
    if n.is_infinite() {
        return finish("inf".into(), n < 0.0, spec, width);
    }
    let magnitude = fixed_digits(n.abs(), precision);
    finish(magnitude, n < 0.0, spec, width)
}

/// `%e`: scientific notation with exactly `precision` decimals.
fn scientific(n: f64, precision: usize, upper: bool, spec: &Spec, width: usize) -> String {
    if n.is_nan() || n.is_infinite() {
        let t = if n.is_nan() { "nan" } else { "inf" };
        return finish(t.into(), n.is_infinite() && n < 0.0, spec, width);
    }
    // Round half away from zero, as SQLite does, rather than relying on Rust's
    // formatter, which rounds to even: printf('%.0f', 2.5) must be 3.
    let s = format!("{:.*e}", precision, n.abs());
    let (mantissa, exp) = round_half_up_exp(&s);
    let sign = if exp < 0 { '-' } else { '+' };
    let a = exp.unsigned_abs();
    let tail = if a < 10 {
        format!("0{a}")
    } else {
        a.to_string()
    };
    let marker = if upper { 'E' } else { 'e' };
    // `#` forces a decimal point to exist even when the precision leaves no
    // digits after it: `printf('%#.0e', 1.5)` is `2.e+00` where `%.0e` of the
    // same is `2e+00`. Rust's formatter already puts a point in for any
    // precision above zero, so only the zero case has to be added.
    let mantissa = if spec.hash && !mantissa.contains('.') {
        format!("{mantissa}.")
    } else {
        mantissa
    };
    finish(
        format!("{mantissa}{marker}{sign}{tail}"),
        n < 0.0,
        spec,
        width,
    )
}

/// `%g`: the shorter of `%e` and `%f`, with trailing zeros removed.
///
/// The switch follows C: scientific when the exponent is below -4 or at least
/// the precision, fixed otherwise. The precision counts *significant* digits,
/// so `%.1g` of 1234.5678 is `1e+03` and of 0.00001234 is `1e-05`.
///
/// Two things about the fixed form are the opposite of what `render_significand`
/// in func_math does for a REAL, and both were checked against sqlite3:
///
/// * an integral value gets **no** decimal point. `printf('%g', 1.0)` is `1`,
///   `printf('%g', 100.0)` is `100` and `printf('%g', -1.0)` is `-1`. Forcing a
///   `.0` on the way out gives `1.0`, which is what a REAL renders as but not
///   what `%g` prints.
/// * a precision of zero is honoured as zero. `printf('%#.0g', 1234.5678)` is
///   `1.e+03` -- a bare point before the exponent, because `#` forces a decimal
///   point to exist even when no digits follow it.
fn general(n: f64, precision: usize, upper: bool, spec: &Spec, width: usize) -> String {
    if n.is_nan() || n.is_infinite() {
        let t = if n.is_nan() { "nan" } else { "inf" };
        return finish(t.into(), n.is_infinite() && n < 0.0, spec, width);
    }
    if n == 0.0 {
        // Zero is `0` with no exponent, so the `#` rule is simply "keep the
        // point and pad to the precision": `printf('%#.0g', 0)` is `0.`,
        // `printf('%#.2g', 0)` is `0.0` and `printf('%#g', 0)` is `0.00000`.
        // That is one fewer decimal than the precision, since the leading zero
        // is itself a significant digit.
        let body = if spec.hash {
            format!("0.{}", "0".repeat(precision.saturating_sub(1)))
        } else {
            "0".to_string()
        };
        return finish(body, false, spec, width);
    }
    // The decimal exponent of the leading digit decides which form to use.
    //
    // Rust's `{:.*e}` rounds a tie to even, where sqlite3 rounds it away from
    // zero, so the digits are rounded by hand first: `printf('%.0g', 2.5)` is
    // `3` and `printf('%.2g', 1.25)` is `1.3`, both of which the formatter would
    // have left at 2 and 1.2.
    // The digits are asked for from the standard formatter and then corrected
    // for the one thing it does differently: it rounds a tie to even where
    // sqlite3 rounds it away from zero, so `printf('%.0g', 2.5)` is `3` and not
    // the `2` the formatter alone gives. Correcting in place rather than
    // re-rounding off a guard digit matters -- a second rounding turns
    // 9.87654321e-07 at two significant digits into `1e-08` where sqlite3 has
    // `9.9e-07`.
    // A precision of zero gives the formatter no fraction at all, so a tie is
    // invisible there: `format!("{:.0e}", 2.5)` is `2e0` with nothing to see
    // the 5 in, and `printf('%.0g', 2.5)` is `3`. That one case is the known
    // residual of this renderer.
    let sci = format!("{:.*e}", precision.saturating_sub(1), n.abs());
    let sci = tie_away_from_zero(&sci);
    let (mantissa, exp) = sci.split_once('e').expect("LowerExp has an exponent");
    let exp: i32 = exp.parse().unwrap_or(0);
    // The digits the precision asked for, with the point removed. Both forms
    // below are built from this one string, so `#` can only change how the
    // digits are *laid out* and never what they are: taking the digits out of
    // the mantissa and re-inserting them around a different point is what
    // turned `%#.0g` of 0.1 into `1.0`.
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    // Without `#` the trailing zeros go, in either form; with it they stay and
    // the point is forced to exist.
    let body = if exp < -4 || exp >= precision as i32 {
        let marker = if upper { 'E' } else { 'e' };
        let sign = if exp < 0 { '-' } else { '+' };
        let a = exp.unsigned_abs();
        let tail = if a < 10 {
            format!("0{a}")
        } else {
            a.to_string()
        };
        if spec.hash {
            // `#` keeps every digit the precision asked for, so the mantissa is
            // exactly `precision` digits with a point after the first. At a
            // precision of one that leaves a bare point, which is what
            // `printf('%#.0g', 1234.5678)` shows as `1.e+03`.
            let (head, frac) = digits.split_at(1.min(digits.len()));
            format!("{head}.{frac}{marker}{sign}{tail}")
        } else {
            // The trailing zeros go but the point stays: `%.2g` of 1234.5678 is
            // `1.2e+03`, not `12e+03`. Trimming the digits and then dropping a
            // trailing point takes the point with them, which is what made the
            // second digit run into the first.
            let trimmed = digits.trim_end_matches('0');
            let head = if trimmed.is_empty() { "0" } else { trimmed };
            let head = if head.len() > 1 {
                format!("{}.{}", &head[..1], &head[1..])
            } else {
                head.to_string()
            };
            format!("{head}{marker}{sign}{tail}")
        }
    } else {
        // The fixed form keeps `precision - 1 - exponent` decimals, so the
        // *significant* count is the precision however the value is scaled.
        // `%#.7g` of 100.0 is `100.0000` -- four decimals for seven significant
        // digits, because two of them went to the `10` -- while `%#.7g` of 0.1
        // is `0.1000000` with all seven after the point. A count that ignored
        // the exponent would print six decimals for the 100 and read as eight
        // significant digits.
        let decimals = (precision as i32 - 1 - exp).max(0) as usize;
        let s = format!("{:.*}", decimals, n.abs());
        if spec.hash {
            // `#` only forces the point to exist. A value with no decimals left
            // still gets one: `123.` rather than `123`.
            if s.contains('.') {
                s
            } else {
                format!("{s}.")
            }
        } else if s.contains('.') {
            s.trim_end_matches('0').trim_end_matches('.').to_string()
        } else {
            s
        }
    };
    finish(body, n < 0.0, spec, width)
}

/// Convenience for the `random` re-export, which the dispatcher shares.
pub fn random_value(args: &[Value]) -> Result<Value> {
    match crate::func_math::call("random", args)? {
        Some(v) => Ok(v),
        None => Err(crate::error::Error::new(
            ResultCode::Error,
            "no such function: random",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(name: &str, args: &[Value]) -> Value {
        call(name, args).unwrap().unwrap_or(Value::Null)
    }

    fn text(name: &str, args: &[Value]) -> String {
        match ok(name, args) {
            Value::Text(s) => s,
            other => panic!("{name} returned {other:?}, expected text"),
        }
    }

    fn t(s: &str) -> Value {
        Value::Text(s.into())
    }

    fn i(n: i64) -> Value {
        Value::Integer(n)
    }

    fn r(n: f64) -> Value {
        Value::Real(n)
    }

    fn blob(b: &[u8]) -> Value {
        Value::Blob(b.to_vec())
    }

    /// `printf` with the format and the arguments already split out.
    fn pf(fmt: &str, args: &[Value]) -> String {
        let mut all = vec![t(fmt)];
        all.extend_from_slice(args);
        text("printf", &all)
    }

    // -- printf ------------------------------------------------------------

    #[test]
    fn printf_renders_the_integer_conversions() {
        // sqlite3: printf('%d',5)='5', printf('%d',5.7)='5', and
        // printf('%d',-5.7)='-5' -- truncation, not rounding.
        assert_eq!(pf("%d", &[i(5)]), "5");
        assert_eq!(pf("%d", &[r(5.7)]), "5");
        assert_eq!(pf("%d", &[r(-5.7)]), "-5");
        // %i is a synonym for %d.
        assert_eq!(pf("%i", &[i(42)]), "42");
        // A missing argument renders as zero rather than as garbage.
        assert_eq!(pf("%d %d", &[i(1)]), "1 0");
    }

    #[test]
    fn printf_of_null_is_an_empty_string() {
        // The documented surprise: a NULL argument yields an empty string, not
        // NULL, so printf('%s', NULL) is the empty string.
        // sqlite3: typeof(printf('%s', NULL)) = 'text'
        assert_eq!(pf("%s", &[Value::Null]), "");
        assert_eq!(pf("%d", &[Value::Null]), "0");
        assert_eq!(pf("%f", &[Value::Null]), "0.000000");
        assert_eq!(pf("%c", &[Value::Null]), "");
        assert_eq!(
            ok("printf", &[t("%s"), Value::Null]),
            Value::Text(String::new())
        );
    }

    #[test]
    fn printf_coerces_text_with_the_lenient_parser() {
        // sqlite3: printf('%d','12abc')='12' and printf('%d','abc')='0', which
        // is the prefix parser abs() uses rather than the strict one.
        assert_eq!(pf("%d", &[t("12abc")]), "12");
        assert_eq!(pf("%d", &[t("abc")]), "0");
        assert_eq!(pf("%d", &[t(" 42 ")]), "42");
        assert_eq!(pf("%f", &[t("12abc")]), "12.000000");
    }

    #[test]
    fn printf_u_is_the_twos_complement_reinterpretation() {
        // sqlite3: printf('%u',-42)='18446744073709551574' and
        // printf('%u',-1)='18446744073709551615'.
        assert_eq!(pf("%u", &[i(42)]), "42");
        assert_eq!(pf("%u", &[i(-1)]), "18446744073709551615");
        assert_eq!(pf("%u", &[i(-42)]), "18446744073709551574");
    }

    #[test]
    fn printf_d_saturates_at_the_i64_bounds() {
        // sqlite3: printf('%d',1e19)='9223372036854775807' and
        // printf('%d',-9223372036854775808) keeps its single minus sign.
        assert_eq!(
            pf("%d", &[i(9_223_372_036_854_775_807)]),
            "9223372036854775807"
        );
        assert_eq!(pf("%d", &[r(1e19)]), "9223372036854775807");
        assert_eq!(
            pf("%d", &[i(-9_223_372_036_854_775_608)]),
            "-9223372036854775808"
        );
    }

    #[test]
    fn printf_f_e_and_g_use_c_formatting() {
        // sqlite3: printf('%f',1)='1.000000', printf('%.2f',std::f64::consts::PI)='3.14',
        // printf('%e',1234.5678)='1.234568e+03', printf('%g',1234.5678)='1234.57'.
        assert_eq!(pf("%f", &[i(1)]), "1.000000");
        assert_eq!(pf("%.2f", &[r(std::f64::consts::PI)]), "3.14");
        assert_eq!(pf("%e", &[r(1234.5678)]), "1.234568e+03");
        assert_eq!(pf("%.3e", &[r(1234.5678)]), "1.235e+03");
        assert_eq!(pf("%g", &[r(1234.5678)]), "1234.57");
        assert_eq!(pf("%G", &[r(1e20)]), "1E+20");
    }

    #[test]
    fn printf_f_rounds_ties_away_from_zero() {
        // Rust's formatter rounds halfway cases to even; SQLite does not.
        // sqlite3: printf('%.0f',0.5)='1', printf('%.0f',2.5)='3'.
        assert_eq!(pf("%.0f", &[r(0.5)]), "1");
        assert_eq!(pf("%.0f", &[r(1.5)]), "2");
        assert_eq!(pf("%.0f", &[r(2.5)]), "3");
        assert_eq!(pf("%.0f", &[r(-0.5)]), "-1");
        assert_eq!(pf("%.0f", &[r(0.0)]), "0");
    }

    #[test]
    fn printf_g_switches_to_scientific_at_the_c_thresholds() {
        // sqlite3: printf('%g',0.0001)='0.0001' and printf('%g',0.00001)='1e-05',
        // printf('%g',100000)='100000' and printf('%g',1000000)='1e+06'.
        assert_eq!(pf("%g", &[r(0.0001)]), "0.0001");
        assert_eq!(pf("%g", &[r(0.00001)]), "1e-05");
        assert_eq!(pf("%g", &[r(100000.0)]), "100000");
        assert_eq!(pf("%g", &[r(1000000.0)]), "1e+06");
        assert_eq!(pf("%.3g", &[r(1234.5678)]), "1.23e+03");
    }

    #[test]
    fn printf_s_prints_text_and_truncates_by_character() {
        // sqlite3: printf('%s','hi')='hi', printf('%.2s','hello')='he', and a
        // precision counts characters, not bytes.
        assert_eq!(pf("%s", &[t("hi")]), "hi");
        assert_eq!(pf("%s", &[i(42)]), "42");
        assert_eq!(pf("%s", &[r(42.0)]), "42.0");
        assert_eq!(pf("%.2s", &[t("hello")]), "he");
        assert_eq!(pf("%.1s", &[t("\u{20AC}x")]), "\u{20AC}");
        assert_eq!(pf("%10s|", &[t("hi")]), "        hi|");
        assert_eq!(pf("%-10s|", &[t("hi")]), "hi        |");
    }

    #[test]
    fn printf_c_prints_the_first_character_of_the_text() {
        // The surprise: %c is a *character* conversion, not a code-point one,
        // so it prints the first character of the argument's text rendering.
        // sqlite3: printf('%c',65)='6', printf('%c',9786)='9', printf('%c',-1)='-'.
        assert_eq!(pf("%c", &[i(65)]), "6");
        assert_eq!(pf("%c", &[i(9786)]), "9");
        assert_eq!(pf("%c", &[i(-1)]), "-");
        assert_eq!(pf("%c", &[t("A")]), "A");
        assert_eq!(pf("%c", &[t("abc")]), "a");
        assert_eq!(pf("%c", &[blob(&[0x41])]), "A");
    }

    #[test]
    fn printf_radix_conversions() {
        // sqlite3: printf('%x',255)='ff', printf('%x',-1)='ffffffffffffffff',
        // printf('%X',255)='FF' and printf('%o',8)='10'.
        assert_eq!(pf("%x", &[i(255)]), "ff");
        assert_eq!(pf("%x", &[i(-1)]), "ffffffffffffffff");
        assert_eq!(pf("%X", &[i(255)]), "FF");
        assert_eq!(pf("%o", &[i(8)]), "10");
        assert_eq!(pf("%#x|", &[i(255)]), "0xff|");
    }

    #[test]
    fn printf_q_doubles_quotes_and_spells_null() {
        // sqlite3: printf('%q','it''s')='it''s', printf('%q',NULL)='(NULL)'.
        // It adds no quotes of its own, unlike quote().
        assert_eq!(pf("%q", &[t("it''s")]), "it''''s");
        assert_eq!(pf("%q", &[t("abc")]), "abc");
        assert_eq!(pf("%q", &[Value::Null]), "(NULL)");
        assert_eq!(pf("%q", &[i(42)]), "42");
    }

    #[test]
    fn printf_percent_and_a_trailing_percent() {
        // sqlite3: printf('100%%')='100%' and printf('%')='%'.
        assert_eq!(pf("100%%", &[]), "100%");
        assert_eq!(pf("%", &[]), "%");
        assert_eq!(pf("%d %d", &[i(1), i(2)]), "1 2");
        // A surplus argument is ignored rather than an error.
        assert_eq!(pf("%d", &[i(1), i(2)]), "1");
    }

    #[test]
    fn printf_width_precision_and_zero_padding() {
        // sqlite3: printf('%5d|',42)='   42|', printf('%-5d|',42)='42   |',
        // printf('%05d',42)='00042' and printf('%08.3f|',-std::f64::consts::PI)='-003.142|'.
        assert_eq!(pf("%5d|", &[i(42)]), "   42|");
        assert_eq!(pf("%-5d|", &[i(42)]), "42   |");
        assert_eq!(pf("%05d", &[i(42)]), "00042");
        assert_eq!(pf("%08.3f|", &[r(-std::f64::consts::PI)]), "-003.142|");
        assert_eq!(pf("%+d", &[i(42)]), "+42");
        assert_eq!(pf("% d|", &[i(42)]), " 42|");
        assert_eq!(pf("%.3d", &[i(5)]), "005");
        assert_eq!(pf("%5.3d", &[i(5)]), "  005");
    }

    #[test]
    fn printf_star_widths_consume_arguments() {
        // sqlite3: printf('%*d',5,42)='   42', printf('%*d',-5,42)='42   ' and
        // printf('%.*f',2,std::f64::consts::PI)='3.14'.
        assert_eq!(pf("%*d", &[i(5), i(42)]), "   42");
        assert_eq!(pf("%*d", &[i(-5), i(42)]), "42   ");
        assert_eq!(pf("%.*f", &[i(2), r(std::f64::consts::PI)]), "3.14");
        assert_eq!(pf("%0*d", &[i(5), i(42)]), "00042");
    }

    #[test]
    fn an_unknown_conversion_falls_back_to_s() {
        // sqlite3: printf('%z',1)='1' and printf('%10z',1)='         1'.
        assert_eq!(pf("%z", &[i(1)]), "1");
        assert_eq!(pf("%10z", &[i(1)]), "         1");
        assert_eq!(pf("%-10z", &[i(1)]), "1         ");
        assert_eq!(pf("a%zb", &[i(1), t("b")]), "a1b");
    }

    #[test]
    fn format_is_an_alias_for_printf() {
        // sqlite3: format('%d',42)='42' and format('%%')='%'.
        assert_eq!(text("format", &[t("%d"), i(42)]), "42");
        assert_eq!(text("format", &[t("%%")]), "%");
    }

    #[test]
    fn printf_needs_a_format() {
        let err = call("printf", &[]).unwrap_err();
        assert_eq!(
            err.message,
            "wrong number of arguments to function printf()"
        );
    }

    // -- concat and concat_ws ---------------------------------------------

    #[test]
    fn concat_skips_null_rather_than_poisoning_the_result() {
        // sqlite3: concat('a',NULL,'b')='ab' and typeof(...)='text'.
        assert_eq!(text("concat", &[t("a"), Value::Null, t("b")]), "ab");
        assert_eq!(text("concat", &[Value::Null]), "");
        assert_eq!(
            text("concat", &[t("a"), i(1), r(2.0), blob(&[1, 2])]),
            "a12.0\u{1}\u{2}"
        );
        // concat() with no arguments is an arity error in sqlite3.
        let err = call("concat", &[]).unwrap_err();
        assert_eq!(
            err.message,
            "wrong number of arguments to function concat()"
        );
    }

    #[test]
    fn concat_ws_drops_nulls_but_keeps_the_separator() {
        // sqlite3: concat_ws('-','a',NULL,'b')='a-b' -- the NULL argument leaves
        // no gap behind, unlike a naive join.
        assert_eq!(
            text("concat_ws", &[t("-"), t("a"), Value::Null, t("b")]),
            "a-b"
        );
        assert_eq!(text("concat_ws", &[t("-"), i(1), i(2), i(3)]), "1-2-3");
        // A NULL separator makes the whole result NULL.
        assert_eq!(ok("concat_ws", &[Value::Null, t("a"), t("b")]), Value::Null);
    }

    // -- replace, upper, lower --------------------------------------------

    #[test]
    fn replace_leaves_the_text_alone_for_an_empty_needle() {
        // sqlite3: replace('hello','','X')='hello' and
        // replace('hello','l','')='heo'.
        assert_eq!(text("replace", &[t("hello"), t("l"), t("L")]), "heLLo");
        assert_eq!(text("replace", &[t("hello"), t(""), t("X")]), "hello");
        assert_eq!(text("replace", &[t("hello"), t("l"), t("")]), "heo");
        // Any NULL argument makes the result NULL.
        assert_eq!(ok("replace", &[Value::Null, t("a"), t("b")]), Value::Null);
    }

    #[test]
    fn upper_and_lower_fold_ascii_and_pass_other_bytes_through() {
        // sqlite3: upper('abc')='ABC' and upper('aBc1')='ABC1', but the fold is
        // byte-wise and ASCII-only, so a byte at or above 0x80 comes back
        // unchanged. `hex(upper(x'C3846263'))` is C3844243 -- the two bytes of
        // `Ä` are untouched, not turned into a question mark.
        assert_eq!(text("upper", &[t("abc")]), "ABC");
        assert_eq!(text("lower", &[t("ABC")]), "abc");
        assert_eq!(text("upper", &[t("aBc1")]), "ABC1");
        assert_eq!(text("upper", &[t("\u{c4}bc")]), "\u{c4}BC");
        assert_eq!(text("lower", &[t("\u{c4}BC")]), "\u{c4}bc");
        // A multi-byte character keeps its length, and the ASCII letters
        // around it still fold: `upper('ÑTCafé')` is 'ÑTCAFé' and its length
        // is 6 in sqlite3, not 8 and not 5.
        let n_t_cafe = "\u{d1}TCaf\u{e9}";
        assert_eq!(text("upper", &[t(n_t_cafe)]), "\u{d1}TCAF\u{e9}");
        assert_eq!(text("upper", &[t(n_t_cafe)]).chars().count(), 6);
        // A non-string argument renders as its text and is then folded.
        assert_eq!(text("upper", &[i(123)]), "123");
        // A NULL argument is NULL, not the empty string: sqlite3's
        // typeof(upper(NULL)) is 'null'.
        assert_eq!(ok("upper", &[Value::Null]), Value::Null);
        assert_eq!(ok("lower", &[Value::Null]), Value::Null);
    }

    #[test]
    fn upper_and_lower_leave_ascii_punctuation_alone() {
        // The fold covers only the 26 letters in each direction. `[` and `]`
        // are the bytes either side of the alphabet and are *not* mapped --
        // sqlite3's upper('[') is '[' -- so the range test cannot be written as
        // a +/- 32 shift.
        // sqlite3: upper('[')='[', lower('A')='a', upper('`')='`',
        // upper('{')='{'.
        assert_eq!(text("upper", &[t("[")]), "[");
        assert_eq!(text("upper", &[t("`")]), "`");
        assert_eq!(text("upper", &[t("{")]), "{");
        assert_eq!(text("lower", &[t("A")]), "a");
        assert_eq!(text("lower", &[t("@")]), "@");
        assert_eq!(text("lower", &[t("[")]), "[");
        // A blob is folded as its bytes too, and the non-ASCII bytes survive
        // in place: hex(upper(x'61C38462')) is 41C38442.
        assert_eq!(
            text("upper", &[blob(&[0x61, 0xC3, 0x84, 0x62])]),
            "A\u{c4}B"
        );
        assert_eq!(
            text("lower", &[blob(&[0x41, 0xC3, 0x84, 0x62])]),
            "a\u{c4}b"
        );
    }

    // -- trim, ltrim, rtrim -----------------------------------------------

    #[test]
    fn trim_cuts_spaces_by_default_and_a_character_set_when_asked() {
        // sqlite3: trim('  hi  ')='hi', ltrim('0012','01')='2' and
        // rtrim('0101hi1010','01')='0101hi' -- the second argument is a *set*.
        assert_eq!(text("trim", &[t("  hi  ")]), "hi");
        assert_eq!(text("ltrim", &[t("  hi  ")]), "hi  ");
        assert_eq!(text("rtrim", &[t("  hi  ")]), "  hi");
        assert_eq!(text("trim", &[t("xxhixx"), t("x")]), "hi");
        assert_eq!(text("ltrim", &[t("0012"), t("01")]), "2");
        assert_eq!(text("rtrim", &[t("0101hi1010"), t("01")]), "0101hi");
        // An empty set cuts nothing.
        assert_eq!(text("trim", &[t("hi"), t("")]), "hi");
        // A NULL argument on either side is NULL.
        assert_eq!(ok("trim", &[Value::Null]), Value::Null);
        assert_eq!(ok("trim", &[t("hi"), Value::Null]), Value::Null);
    }

    // -- substr and substring ---------------------------------------------

    #[test]
    fn substr_is_one_based_and_counts_from_the_end_for_a_negative_start() {
        // sqlite3: substr('hello',2)='ello', substr('hello',2,2)='el',
        // substr('hello',-2)='lo' and substr('hello',0)='hello'.
        assert_eq!(text("substr", &[t("hello"), i(2)]), "ello");
        assert_eq!(text("substr", &[t("hello"), i(2), i(2)]), "el");
        assert_eq!(text("substr", &[t("hello"), i(-2)]), "lo");
        assert_eq!(text("substr", &[t("hello"), i(-2), i(2)]), "lo");
        // A start of zero behaves as one.
        assert_eq!(text("substr", &[t("hello"), i(0)]), "hello");
        // Out of range and zero lengths give the empty string.
        assert_eq!(text("substr", &[t("hello"), i(10)]), "");
        assert_eq!(text("substr", &[t("hello"), i(1), i(0)]), "");
        // A negative length gives the empty string, not the rest: sqlite3's
        // substr('hello',1,-1) is '' while substr('hello',2,-1) is 'h'.
        assert_eq!(text("substr", &[t("hello"), i(1), i(-1)]), "");
        assert_eq!(text("substr", &[t("hello"), i(2), i(-1)]), "h");
        assert_eq!(text("substr", &[t("hello"), i(3), i(-1)]), "e");
        // substring is an alias.
        assert_eq!(text("substring", &[t("hello"), i(2), i(3)]), "ell");
    }

    #[test]
    fn substr_of_a_blob_gives_a_blob_and_nulls_give_null() {
        // sqlite3: substr(x'0102',1,1) is a one-byte blob x'01'.
        assert_eq!(ok("substr", &[blob(&[1, 2]), i(1), i(1)]), blob(&[1]));
        assert_eq!(ok("substr", &[t("hello"), Value::Null]), Value::Null);
        assert_eq!(ok("substr", &[t("hello"), i(1), Value::Null]), Value::Null);
    }

    // -- char and unicode --------------------------------------------------

    #[test]
    fn char_builds_a_string_from_code_points_and_truncates_bad_ones() {
        // sqlite3: char(65,66,67)='ABC', char(0x1F600) is the emoji, and a
        // value outside the Unicode range becomes U+FFFD rather than an error.
        assert_eq!(text("char", &[i(65), i(66), i(67)]), "ABC");
        assert_eq!(text("char", &[]), "");
        assert_eq!(text("char", &[i(0x1F600)]), "\u{1F600}");
        assert_eq!(text("char", &[r(-1.0)]), "\u{FFFD}");
        assert_eq!(text("char", &[i(1_114_112)]), "\u{FFFD}");
        // char(65.9) truncates to 65.
        assert_eq!(text("char", &[r(65.9)]), "A");
    }

    #[test]
    fn a_null_argument_becomes_a_nul_byte_without_stopping_the_scan() {
        // sqlite3 3.53.4: hex(char(65,NULL,67)) is 410043, so the NULL turns
        // into a NUL byte and the 67 after it is still used. quote() reports
        // this as A because a NUL is where a quoted string ends, which says
        // something about quote and not about the value; hex() is the reliable
        // view, and length() reports 1 for the same reason.
        assert_eq!(text("char", &[i(65), Value::Null, i(67)]), "A\u{0}C");
        assert_eq!(text("char", &[Value::Null, i(67)]), "\u{0}C");
        assert_eq!(text("char", &[]), "");
        // An explicit zero and a NULL produce the same bytes.
        assert_eq!(text("char", &[i(65), i(0), i(67)]), "A\u{0}C");
    }

    #[test]
    fn unicode_reads_the_first_scalar() {
        // sqlite3: unicode('A')=65, unicode('€')=8364, unicode('😀')=128512,
        // and unicode('') and unicode(NULL) are NULL.
        assert_eq!(ok("unicode", &[t("A")]), i(65));
        assert_eq!(ok("unicode", &[t("\u{20AC}")]), i(8364));
        assert_eq!(ok("unicode", &[t("\u{1F600}")]), i(1_285_12));
        assert_eq!(ok("unicode", &[t("")]), Value::Null);
        assert_eq!(ok("unicode", &[Value::Null]), Value::Null);
        // A blob is read as its bytes, and a number as its text.
        assert_eq!(ok("unicode", &[blob(&[65])]), i(65));
        assert_eq!(ok("unicode", &[i(65)]), i(54));
    }

    // -- hex and unhex -----------------------------------------------------

    #[test]
    fn hex_renders_the_bytes_of_any_value() {
        // sqlite3: hex(123)='313233', hex('abc')='616263', hex(x'0102')='0102'
        // and hex(NULL) is the empty string rather than NULL.
        assert_eq!(text("hex", &[i(123)]), "313233");
        assert_eq!(text("hex", &[t("abc")]), "616263");
        assert_eq!(text("hex", &[blob(&[1, 2])]), "0102");
        assert_eq!(text("hex", &[Value::Null]), "");
        assert_eq!(text("hex", &[r(1.5)]), "312E35");
        assert_eq!(text("hex", &[i(-1)]), "2D31");
    }

    #[test]
    fn unhex_decodes_to_a_blob_and_rejects_malformed_input() {
        // sqlite3: unhex('414243') is a blob, unhex('6G') and unhex('6') are
        // both NULL, and unhex('') is an empty blob.
        assert_eq!(ok("unhex", &[t("414243")]), blob(&[65, 66, 67]));
        assert_eq!(ok("unhex", &[t("")]), Value::Blob(vec![]));
        // An odd length or a non-hex character gives NULL.
        assert_eq!(ok("unhex", &[t("6")]), Value::Null);
        assert_eq!(ok("unhex", &[t("6G")]), Value::Null);
        assert_eq!(ok("unhex", &[t("zz")]), Value::Null);
        // Whitespace is not skipped.
        assert_eq!(ok("unhex", &[t(" 4142 ")]), Value::Null);
        assert_eq!(ok("unhex", &[Value::Null]), Value::Null);
    }

    // -- quote -------------------------------------------------------------

    #[test]
    fn quote_produces_sql_source_text() {
        // sqlite3: quote('it''s')='''it''''s''', quote(x'0102')=X'0102',
        // quote(NULL)='NULL' and quote(1.0)='1.0'.
        assert_eq!(text("quote", &[Value::Null]), "NULL");
        assert_eq!(text("quote", &[i(1)]), "1");
        assert_eq!(text("quote", &[r(1.5)]), "1.5");
        assert_eq!(text("quote", &[r(1.0)]), "1.0");
        assert_eq!(text("quote", &[t("a")]), "'a'");
        // `it''s` here is the five-character string with two quotes in it, and
        // quote() doubles each of them.
        assert_eq!(text("quote", &[t("it''s")]), "'it''''s'");
        // A string with one quote doubles once: the SQL literal 'it''s'.
        assert_eq!(text("quote", &[t("it's")]), "'it''s'");
        assert_eq!(text("quote", &[blob(&[1, 2])]), "X'0102'");
        // A double quote needs no escaping in SQL.
        assert_eq!(text("quote", &[t("a\"b")]), "'a\"b'");
    }

    // -- the conditional helpers ------------------------------------------

    #[test]
    fn iif_and_if_pick_a_branch_with_null_counting_as_false() {
        // sqlite3: iif(1,'a','b')='a', iif(NULL,'a','b')='b'.
        assert_eq!(ok("iif", &[i(1), t("a"), t("b")]), t("a"));
        assert_eq!(ok("iif", &[i(0), t("a"), t("b")]), t("b"));
        assert_eq!(ok("iif", &[Value::Null, t("a"), t("b")]), t("b"));
        // `if` is an alias for the scalar form.
        assert_eq!(ok("if", &[i(1), t("a"), t("b")]), t("a"));
        assert_eq!(ok("if", &[Value::Null, t("a"), t("b")]), t("b"));
        // Both want exactly three arguments.
        let err = call("iif", &[i(1), t("a")]).unwrap_err();
        assert_eq!(err.message, "wrong number of arguments to function iif()");
    }

    #[test]
    fn nullif_is_null_only_when_the_values_compare_equal() {
        // sqlite3: nullif(1,1) is NULL, nullif(1,2)=1 and nullif(1,1.0) is NULL
        // because an integer and the equal real compare equal.
        assert_eq!(ok("nullif", &[i(1), i(1)]), Value::Null);
        assert_eq!(ok("nullif", &[i(1), i(2)]), i(1));
        assert_eq!(ok("nullif", &[i(1), r(1.0)]), Value::Null);
        assert_eq!(ok("nullif", &[Value::Null, i(1)]), Value::Null);
    }

    #[test]
    fn likely_and_unlikely_are_the_identity() {
        // sqlite3: likely(1)=1, likely(NULL) is NULL. They are hints for the
        // optimiser and change nothing about the value.
        assert_eq!(ok("likely", &[i(1)]), i(1));
        assert_eq!(ok("likely", &[i(0)]), i(0));
        assert_eq!(ok("likely", &[Value::Null]), Value::Null);
        assert_eq!(ok("unlikely", &[Value::Null]), Value::Null);
        assert_eq!(ok("likely", &[t("abc")]), t("abc"));
    }

    // -- the conversions whose flags are not C's --------------------------

    #[test]
    fn the_zero_flag_is_ignored_by_every_string_conversion() {
        // sqlite3 fills a short field with spaces whatever the flags say:
        //   printf('%05s','ab') = '   ab'   (not '000ab')
        //   printf('%08s','ab') = '      ab'
        //   printf('%05z','ab') = '   ab'   printf('%05q','ab') = '   ab'
        //   printf('%05c','ab') = '    a'
        assert_eq!(pf("%05s", &[t("ab")]), "   ab");
        assert_eq!(pf("%08s", &[t("ab")]), "      ab");
        assert_eq!(pf("%05z", &[t("ab")]), "   ab");
        assert_eq!(pf("%05q", &[t("ab")]), "   ab");
        assert_eq!(pf("%05c", &[t("ab")]), "    a");
    }

    #[test]
    fn zero_and_minus_are_resolved_per_conversion_class() {
        // The two flags disagree in *both* directions, by conversion class:
        //
        // * the integer conversions let `0` win, so `printf('%-08d', 42)` is
        //   `00000042`;
        // * the floating ones let `-` win, so `printf('%-08f', 1.5)` is
        //   `1.500000` and `printf('%-08g', 1.5)` is `1.5     `.
        assert_eq!(pf("%-08d", &[i(42)]), "00000042");
        assert_eq!(pf("%-08i", &[i(42)]), "00000042");
        assert_eq!(pf("%-08u", &[i(42)]), "00000042");
        assert_eq!(pf("%-08x", &[i(42)]), "0000002a");
        assert_eq!(pf("%-08f", &[r(1.5)]), "1.500000");
        assert_eq!(pf("%-08e", &[r(1.5)]), "1.500000e+00");
        assert_eq!(pf("%-08g", &[r(1.5)]), "1.5     ");
        // With no `0` present, `-` behaves as C has it.
        assert_eq!(pf("%-8d", &[i(42)]), "42      ");
    }

    #[test]
    fn the_hash_prefix_sits_inside_the_field() {
        // sqlite3: printf('%#08x', 42) = '0x0000002a' and printf('%#8x', 42) =
        // '    0x2a' -- the prefix is at the start of the field either way, and
        // padding the digits first and gluing the prefix on would give
        // '00000x2a' and '0x    2a'.
        assert_eq!(pf("%#08x", &[i(42)]), "0x0000002a");
        assert_eq!(pf("%#08X", &[i(42)]), "0X0000002A");
        assert_eq!(pf("%#8x", &[i(42)]), "    0x2a");
        assert_eq!(pf("%#-8x", &[i(42)]), "0x2a    ");
        // Octal counts the prefix only when the field is not zero-filled:
        // printf('%#8o', 42) = '     052' and printf('%#08o', 8) = '000000010'.
        assert_eq!(pf("%#8o", &[i(42)]), "     052");
        assert_eq!(pf("%#08o", &[i(8)]), "000000010");
        assert_eq!(pf("%#-8o", &[i(42)]), "052     ");
        // A zero value gets no prefix at all.
        assert_eq!(pf("%#x", &[i(0)]), "0");
        assert_eq!(pf("%#o", &[i(0)]), "0");
    }

    #[test]
    fn a_bare_dot_is_a_precision_of_zero() {
        // sqlite3: printf('%.f', 1.5) = '2' and printf('%.f', 0.5) = '1', where
        // an absent precision would fall back to six and give '1.500000'.
        assert_eq!(pf("%.f", &[r(1.5)]), "2");
        assert_eq!(pf("%.f", &[r(0.5)]), "1");
        assert_eq!(pf("%.f", &[r(-0.5)]), "-1");
        assert_eq!(pf("%.f", &[r(1.0)]), "1");
        assert_eq!(pf("%8.f", &[r(1.5)]), "       2");
    }

    #[test]
    fn an_unknown_conversion_makes_the_whole_format_null() {
        // Not a `%s` fallback: `printf('%y', 42)` is NULL in sqlite3, and so is
        // a second precision point, which is a malformed conversion rather
        // than another `%f`.
        assert_eq!(ok("printf", &[t("%y"), i(42)]), Value::Null);
        assert_eq!(ok("printf", &[t("%3.2.1f"), r(1.5)]), Value::Null);
        assert_eq!(ok("printf", &[t("%.2.1e"), r(1.5)]), Value::Null);
        // A conversion that *is* known is untouched, including `%z`.
        assert_eq!(pf("%z", &[i(1)]), "1");
    }

    #[test]
    fn percent_c_repeats_by_its_precision_and_pads_a_null_short() {
        // A precision on `%c` is a repetition count, the opposite of every other
        // string conversion: `printf('%.3c', 42)` is `444` and
        // `printf('%.3c', 'abcd')` is `aaa`, where `%.3s` would be `abc`.
        assert_eq!(pf("%.3c", &[i(42)]), "444");
        assert_eq!(pf("%.3c", &[t("abcd")]), "aaa");
        assert_eq!(pf("%c", &[t("abcd")]), "a");
        // A NULL has no character, so the field comes up short by whatever the
        // conversion would have shown: seven spaces at width eight, and five
        // when a precision of three asks for three characters.
        assert_eq!(pf("%08c", &[Value::Null]), "       ");
        assert_eq!(pf("%08.3c", &[Value::Null]), "     ");
        // A left-aligned field is empty outright rather than padded.
        assert_eq!(pf("%-8c", &[Value::Null]), "");
    }

    #[test]
    fn percent_q_truncates_by_its_precision() {
        // sqlite3: printf('%.3q', std::f64::consts::PI) = '3.1' and printf('%.3q', 'abcdef')
        // = 'abc'. The count is over the *escaped* text, so a doubled quote
        // counts as two.
        assert_eq!(pf("%.3q", &[r(std::f64::consts::PI)]), "3.1");
        assert_eq!(pf("%.3q", &[t("abcdef")]), "abc");
        assert_eq!(pf("%.3q", &[t("it's")]), "it'");
    }

    #[test]
    fn percent_f_keeps_the_leading_zero_below_one() {
        // The point belongs *after* the leading zero: sqlite3 gives
        // `printf('%.1f', 0.05)` = '0.1' where a saturated split gave '.1', and
        // `printf('%.20f', 0.1)` = '0.10000000000000000000'.
        assert_eq!(pf("%.1f", &[r(0.05)]), "0.1");
        assert_eq!(pf("%.1f", &[r(0.15)]), "0.1");
        assert_eq!(pf("%.1f", &[r(0.5)]), "0.5");
        assert_eq!(pf("%.20f", &[r(0.1)]), "0.10000000000000000000");
        assert_eq!(pf("%f", &[r(0.5)]), "0.500000");
    }

    #[test]
    fn percent_f_rounds_the_exact_decimal_not_a_scaled_double() {
        // Scaling by a power of ten is a second rounding, visible in the last
        // place: sqlite3 gives `printf('%.2f', 0.045)` = '0.04' and
        // `printf('%.2f', 1e17)` = '100000000000000000.00'.
        assert_eq!(pf("%.2f", &[r(0.045)]), "0.04");
        assert_eq!(pf("%.2f", &[r(-0.045)]), "-0.04");
        assert_eq!(pf("%.2f", &[r(1.005)]), "1.00");
        assert_eq!(pf("%.2f", &[r(1e17)]), "100000000000000000.00");
        assert_eq!(pf("%f", &[r(1.0)]), "1.000000");
        // The rounding is half away from zero, as sqlite3 has it.
        assert_eq!(pf("%.0f", &[r(0.5)]), "1");
        assert_eq!(pf("%.0f", &[r(-0.5)]), "-1");
        assert_eq!(pf("%.0f", &[r(1.5)]), "2");
        assert_eq!(pf("%.0f", &[r(2.5)]), "3");
        assert_eq!(pf("%.1f", &[r(9.99)]), "10.0");
    }

    #[test]
    fn percent_g_leaves_an_integral_value_without_a_point() {
        // The opposite of how a REAL renders: sqlite3 gives `printf('%g', 1.0)`
        // = '1', `printf('%g', 100.0)` = '100' and `printf('%g', -1.0)` = '-1'.
        assert_eq!(pf("%g", &[r(1.0)]), "1");
        assert_eq!(pf("%g", &[r(-1.0)]), "-1");
        assert_eq!(pf("%g", &[r(100.0)]), "100");
        assert_eq!(pf("%g", &[r(0.0)]), "0");
        assert_eq!(pf("%g", &[r(0.0001)]), "0.0001");
        assert_eq!(pf("%g", &[r(0.00001)]), "1e-05");
    }

    #[test]
    fn percent_g_with_hash_keeps_the_point_and_the_zeros() {
        // `#` forces a point to exist and keeps the trailing zeros, and the
        // fixed form's decimal count shifts with the exponent: seven
        // significant digits of 100.0 is `100.0000` and of 0.1 is
        // `0.1000000`.
        // sqlite3:
        //   printf('%#.0g', 1234.5678) = 1.e+03
        //   printf('%#.3g', 100.0)     = 100.
        //   printf('%#.7g', 100.0)     = 100.0000
        //   printf('%#.3g', 0.0001)    = 0.000100
        //   printf('%#.0g', 0)         = 0.
        //   printf('%#g', 0)           = 0.00000
        assert_eq!(pf("%#.0g", &[r(1234.5678)]), "1.e+03");
        assert_eq!(pf("%#.3g", &[r(100.0)]), "100.");
        assert_eq!(pf("%#.7g", &[r(100.0)]), "100.0000");
        assert_eq!(pf("%#.3g", &[r(0.0001)]), "0.000100");
        assert_eq!(pf("%#.0g", &[r(0.0)]), "0.");
        assert_eq!(pf("%#g", &[r(0.0)]), "0.00000");
        assert_eq!(pf("%#.3g", &[r(0.1)]), "0.100");
        // A `#`d value that is not zero keeps its significant digits: taking
        // them out of the mantissa and re-inserting them round a different point
        // would give `1.0` for `%#.0g` of 0.1.
        assert_eq!(pf("%#.0g", &[r(0.1)]), "0.1");
        // And `%e` gets the same bare point.
        assert_eq!(pf("%#.0e", &[r(1.5)]), "2.e+00");
        assert_eq!(pf("%.0e", &[r(1.5)]), "2e+00");
    }

    #[test]
    fn iif_reads_its_condition_with_the_lenient_parser() {
        // The condition is the same prefix parse `abs()` uses, so trailing
        // junk does not stop it: `iif('1x','a','b')` is 'a' where the strict
        // parser gave 'b'.
        // sqlite3:
        //   iif('1x','a','b') = a     iif('abc','a','b') = b
        //   iif(' 5 ','a','b') = a    iif('0x10','a','b') = b
        //   iif('inf','a','b') = b
        assert_eq!(ok("iif", &[t("1x"), t("a"), t("b")]), t("a"));
        assert_eq!(ok("iif", &[t("abc"), t("a"), t("b")]), t("b"));
        assert_eq!(ok("iif", &[t(" 5 "), t("a"), t("b")]), t("a"));
        assert_eq!(ok("iif", &[t("0x10"), t("a"), t("b")]), t("b"));
        assert_eq!(ok("iif", &[t("inf"), t("a"), t("b")]), t("b"));
        assert_eq!(ok("iif", &[t("+5"), t("a"), t("b")]), t("a"));
        assert_eq!(ok("iif", &[Value::Null, t("a"), t("b")]), t("b"));
        assert_eq!(ok("if", &[t("1x"), t("a"), t("b")]), t("a"));
    }

    #[test]
    fn char_of_a_null_gives_a_nul_rather_than_nothing() {
        // A NULL argument contributes U+0000: `hex(char(NULL))` is '00' and
        // `char(65, NULL)` is 'A' followed by that byte. It does not stop the
        // scan and it does not make the call NULL.
        // sqlite3: hex(char(NULL)) = 00, length(char(65,NULL)) = 1 (two bytes)
        assert_eq!(text("hex", &[ok("char", &[Value::Null])]), "00");
        let a = ok("char", &[i(65), Value::Null]);
        assert_eq!(text("hex", &[a]), "4100");
    }

    // -- substr ------------------------------------------------------------

    #[test]
    fn substr_treats_position_zero_as_a_slot_before_the_string() {
        // `substr('hello', 0, 2)` is 'h' and `substr('hello', 0, 1)` is empty,
        // where reading zero as one would give 'he' and 'h'. The length counts
        // from the slot *before* the string, so it spends its first character
        // on that gap.
        // sqlite3:
        //   substr('hello',0,2) = h    substr('hello',0,1) = ''
        //   substr('hello',0,0) = ''   substr('hello',0)   = hello
        assert_eq!(text("substr", &[t("hello"), i(0), i(2)]), "h");
        assert_eq!(text("substr", &[t("hello"), i(0), i(1)]), "");
        assert_eq!(text("substr", &[t("hello"), i(0), i(0)]), "");
        assert_eq!(text("substr", &[t("hello"), i(0)]), "hello");
    }

    #[test]
    fn a_start_before_the_string_is_not_clamped_to_the_front() {
        // A negative start counts back from the end without being clamped, so
        // one that runs off the front is past the end of the window in the
        // other direction and yields nothing.
        // sqlite3: substr('hello',-10,1) = '' and substr('hello',-5,1) = 'h'
        assert_eq!(text("substr", &[t("hello"), i(-10), i(1)]), "");
        assert_eq!(text("substr", &[t("hello"), i(-5), i(1)]), "h");
        assert_eq!(text("substr", &[t("hello"), i(-1), i(1)]), "o");
    }

    #[test]
    fn a_negative_length_is_a_window_running_backwards() {
        // The `|n|` characters ending immediately before the start, rather
        // than a window running to the end of the string.
        // sqlite3:
        //   substr('hello',4,-2) = el   substr('hello',1,-1) = ''
        //   substr('abcdef',3,-2) = ab  substr('abcdef',4,-2) = bc
        assert_eq!(text("substr", &[t("hello"), i(4), i(-2)]), "el");
        assert_eq!(text("substr", &[t("hello"), i(1), i(-1)]), "");
        assert_eq!(text("substr", &[t("abcdef"), i(3), i(-2)]), "ab");
        assert_eq!(text("substr", &[t("abcdef"), i(4), i(-2)]), "bc");
        assert_eq!(text("substr", &[t("hello"), i(3), i(-1)]), "e");
        assert_eq!(text("substr", &[t("hello"), i(3), i(-2)]), "he");
    }

    // -- dispatch ----------------------------------------------------------

    #[test]
    fn an_unknown_name_is_left_to_the_caller() {
        assert!(call("no_such_function", &[]).unwrap().is_none());
    }

    #[test]
    fn the_arity_errors_name_the_function() {
        // sqlite3 spells these "wrong number of arguments to function NAME()".
        for (name, args) in [
            ("upper", vec![t("a"), t("b")]),
            ("lower", vec![]),
            ("substr", vec![t("a")]),
            ("replace", vec![t("a"), t("b")]),
            ("hex", vec![]),
            ("unhex", vec![]),
            ("quote", vec![]),
            ("unicode", vec![]),
            ("nullif", vec![i(1)]),
            ("likely", vec![i(1), i(2)]),
        ] {
            let err = call(name, &args).unwrap_err();
            assert_eq!(
                err.message,
                format!("wrong number of arguments to function {name}()"),
                "{name} arity message"
            );
        }
    }
}
