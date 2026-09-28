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
use crate::text::Encoding;
use crate::func_math::{
    arity_error, expect_arity, expect_range, num_strict, num_value, real_to_text,
};
use crate::value::Value;

/// Calls a string function. `name` must already be lowercased.
///
/// `enc` is the database's declared encoding and reaches the functions that
/// walk text a character at a time -- `substr`, `trim` -- because what a
/// character is depends on it. See [`char_offsets`].
///
/// Returns `Ok(None)` when the name is not one of this module's functions, so
/// the caller can fall through to its own table.
pub fn call(name: &str, args: &[Value], enc: Encoding) -> Result<Option<Value>> {
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
                // Replaced over bytes: all three arguments contribute their
                // bytes, so a needle or a replacement that is not valid UTF-8
                // is spliced in as it is. `replace(x'414243', x'42', x'FF')`
                // is 41 FF 43 in sqlite3, and a `String` splice would have
                // written U+FFFD there instead.
                let s = value_bytes(&args[0]);
                let from = value_bytes(&args[1]);
                let to = value_bytes(&args[2]);
                // An empty needle matches nothing and leaves the text alone.
                if from.is_empty() {
                    text_result(s)
                } else {
                    text_result(replace_bytes(&s, &from, &to))
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
            trim(name, args, enc)
        }
        "substr" | "substring" => {
            expect_range(name, args, 2, 3)?;
            substr(args, enc)
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
                // The *bytes* of the value, not the bytes of its rendering:
                // `hex(CAST(x'FF' AS TEXT))` is FF, one byte per byte, where
                // reading the rendered text would hex the three bytes of the
                // U+FFFD the lossy decode writes in the FF's place.
                Value::Text(hex_upper(&value_bytes(&args[0])))
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
        // The bytes, whatever class they arrived in: `trim(CAST(x'FF' AS
        // TEXT))` reads the one byte FF rather than a rendering of it.
        //
        // A byte that is not valid UTF-8 is **kept**, not replaced. sqlite3
        // renders a string as its own bytes, so `printf('%s', x'FF')` is the
        // one byte FF and not the three of U+FFFD. The `String` here cannot
        // hold FF, so this returns the lossy rendering *only* for the callers
        // that genuinely need a `String`; the `printf` conversions run over
        // [`printf_bytes_of`] instead, so a raw byte never reaches this arm.
        Value::TextBytes(b) | Value::Blob(b) => String::from_utf8_lossy(b).into_owned(),
    }
}

/// The text a value contributes to `printf`, which is `text_of` with the one
/// difference a NUL makes: sqlite3 renders an argument as a C string, so the
/// run of bytes *up to* the first NUL is what reaches the format, and the NUL
/// is not one of the bytes that gets there.
///
/// This is why `hex(printf('%s', char(0)))` is empty rather than `00`, and it
/// is the same rule `quote` and `length` already apply -- `printf` is simply
/// the third place in this module that renders text and the one that was
/// still reading past the terminator. Measured against sqlite3 3.53.4:
///
///   * the truncation is on the **argument**, not the format, so
///     `printf('a'||char(0)||'b%s', 'x')` is `ax` -- the format's own NUL only
///     ends the format, and `printf('a'||char(0)||'b')` is `a`;
///   * `%s`, `%z`, `%w`, `%q` and the whole-format read all cut, so
///     `printf('%s%s', char(0), 'x')` is `x` -- the empty first argument still
///     consumes its own and the second is not shifted onto it;
///   * a **blob** is read by its bytes but is *also* a C string here, unlike
///     in `length` and `substr`: `printf('%s', x'00FF')` is empty rather than
///     `FF`, and `printf('%s', x'FF0041')` is `FF` -- the run up to the NUL,
///     the non-UTF-8 byte included. A blob is the one case where a NUL ends
///     the argument *and* the bytes before it are kept verbatim;
///   * the cut happens before padding, so `printf('[%5s]', char(0))` is five
///     spaces around nothing rather than a NUL padded to five.
///
/// The answer is **bytes**, not a `String`. A `String` cannot hold a byte that
/// is not valid UTF-8, and sqlite3 renders one verbatim: `printf('%s', x'FF')`
/// is the single byte FF, which a lossy decode here would have widened to the
/// three of U+FFFD. That was a real defect -- it was there before the NUL rule
/// was added -- so the byte rendering is what this returns and the caller
/// finishes it into a [`Value`].
fn printf_bytes_of(v: &Value) -> Vec<u8> {
    match v {
        Value::Text(s) => s.as_bytes()[..head_before_nul(s.as_bytes()).len()].to_vec(),
        Value::TextBytes(b) | Value::Blob(b) => head_before_nul(b).to_vec(),
        other => text_of(other).into_bytes(),
    }
}

/// The bytes `%c` picks its first character from, which is [`printf_bytes_of`]
/// with the one difference that a lone NUL **survives**.
///
/// This is the asymmetry that makes `printf('%c', char(0))` the single byte
/// `00` while `printf('%s', char(0))` is empty. Both agree that the argument
/// is *not* an empty string -- `length(CAST(printf('%c', char(0)) AS BLOB))`
/// is 1 and `quote` of it is `''` only because `quote` is itself a C string --
/// but `%c` copies its one character out of the value while `%s` copies the
/// run of bytes in front of the terminator. So the terminator is dropped on
/// the way out of a `%s` and kept on the way out of a `%c`.
///
/// Measured against sqlite3 3.53.4: `hex(printf('%c', char(0)))` is `00` and
/// `hex(printf('%c', x'00'))` is `00`, while `hex(printf('%s', char(0)))` and
/// `hex(printf('%s', x'00'))` are both empty.
fn printf_c_bytes_of(v: &Value) -> Vec<u8> {
    match v {
        Value::Text(s) => s.as_bytes().to_vec(),
        Value::TextBytes(b) | Value::Blob(b) => b.clone(),
        other => text_of(other).into_bytes(),
    }
}

/// The byte offset at which each character of `b` starts, plus a final
/// sentinel, so the slice for characters `[from, to)` is
/// `b[off[from] .. off[to]]` and the character count is `off.len() - 1`.
///
/// This is the crate's one definition of what a character is, and every
/// byte-position-sensitive function takes its offsets from here so that
/// `substr` and `length` cannot drift apart about where a character begins.
///
/// The segmentation is a **byte-run** rule, not a decode, and that is what
/// makes the two cases sqlite3 treats as one character come back whole. A
/// byte at or above 0xC0 is a lead byte and takes as many following
/// continuation bytes (0x80..=0xBF) as are actually there; a byte below
/// 0x80, and a continuation byte that is not preceded by one, is a character
/// of its own. So a truncated sequence is one character, and an invalid byte
/// is one character:
///
/// ```text
/// length(CAST(x'E697' AS TEXT))                     == 1
/// hex(substr(CAST(x'E697' AS TEXT), 1, 1))          == E697
/// length(CAST(x'41FF42' AS TEXT))                   == 3
/// hex(substr(CAST(x'41FF42' AS TEXT), 2, 1))        == FF
/// ```
///
/// Neither answer is a `String`: round-tripping through `char` would turn the
/// two-byte E6 97 into U+FFFD, which is why this works in bytes.
///
/// A NUL ends the text, because text is read as a C string, so the walk stops
/// there and every offset past it is out of range: `length(CAST(x'00' AS
/// TEXT))` is 0.
///
/// `enc` is the database's declared encoding, read from bytes 56..59 of the
/// file header by [`crate::page`] into the pager header. It is a parameter
/// rather than an assumption because the count genuinely depends on it: the
/// same nine bytes are three characters in a UTF-8 database and four in a
/// UTF-16le one, and `substr(x, 2, 1)` is `E69CAC` under the first and `A5E6`
/// under the second. Only the UTF-8 walk is implemented, because that is the
/// only encoding this engine writes (see `page.rs`), so the other arms are a
/// local change rather than a rewrite of every call site.
pub fn char_offsets(b: &[u8], enc: Encoding) -> Vec<usize> {
    match enc {
        Encoding::Utf16Le | Encoding::Utf16Be => utf16_offsets(b),
        // UTF-8 and an unrecognised header field are both read byte by byte.
        // SQLite treats encoding 0 as UTF-8 rather than refusing the file, and
        // `Encoding::Unknown` is what a header that is not 1, 2 or 3 becomes.
        Encoding::Utf8 | Encoding::Unknown => utf8_offsets(b),
    }
}

fn utf8_offsets(b: &[u8]) -> Vec<usize> {
    let mut off = Vec::with_capacity(b.len() + 1);
    let mut i = 0;
    while i < b.len() {
        // A character may only START at a byte that is not a continuation.
        // Without this, every byte in 0x80..=0xBF became a character start of
        // its own, and the measurable consequence is in `instr`:
        //
        //     instr(CAST(x'4180' AS TEXT), CAST(x'80' AS TEXT))
        //         sqlite3  ->  0    the needle cannot begin mid-character
        //         before   ->  2    it matched the trailing byte
        //
        // and the same for every continuation byte: 0x80 and 0xBF answer 0
        // where 0x7F, 0xC0 and 0xFF answer 2. A lead byte with no
        // continuations after it still starts a character, and a stray
        // continuation at the start of the value is a character of its own,
        // because there is nothing for it to continue.
        if !(0x80..=0xBF).contains(&b[i]) {
            off.push(i);
        }
        if b[i] >= 0xC0 {
            // A lead byte swallows the continuation run that follows it, and
            // that run may be short, empty, or run to the end of the value --
            // however many are actually there is however long the character is.
            i += 1;
            while i < b.len() && (0x80..=0xBF).contains(&b[i]) {
                i += 1;
            }
        } else {
            i += 1;
        }
    }
    off.push(i.min(b.len()));
    // A lead byte that ran off the end leaves the last two offsets equal, which
    // would count a zero-width character. Collapse them.
    //
    // A NUL is *not* a terminator here. The walk is a character count over
    // bytes the caller has already decided are in scope, and the two callers
    // do that decision themselves: `length` cuts a text value at its NUL
    // before it gets here, and `substr` cuts its text argument the same way
    // while leaving a blob's NULs alone. Stopping here as well would make
    // `substr(x'610062', 1, 2)` the byte 61 instead of the two bytes 61 00,
    // because a blob's NUL is an ordinary byte and not the end of anything.
    if off.len() >= 2 && off[off.len() - 1] == off[off.len() - 2] && off[off.len() - 1] < b.len() {
        off.pop();
    }
    off
}

/// The UTF-16 walk: two bytes per character, a surrogate pair being one.
///
/// Reached only for a database whose header declares UTF-16, which this engine
/// does not write.
fn utf16_offsets(b: &[u8]) -> Vec<usize> {
    let mut off = Vec::with_capacity(b.len() / 2 + 1);
    let mut i = 0;
    while i < b.len() {
        off.push(i);
        if b[i] == 0 && b.get(i + 1).copied().unwrap_or(0) == 0 {
            break;
        }
        // A high surrogate D800..=DBFF is half of a pair, so it takes the two
    // bytes after it as well; anything else is a character of its own.
        let unit = b.get(i + 1).copied().unwrap_or(0);
        if (0xD8..=0xDB).contains(&b[i]) && unit >= 0xDC && unit <= 0xDF {
            i += 4;
        } else {
            i += 2;
        }
    }
    off.push(i.min(b.len()));
    if off.len() >= 2 && off[off.len() - 1] == off[off.len() - 2] && off[off.len() - 1] < b.len() {
        off.pop();
    }
    off
}

/// The characters in a run of text bytes, counted the way sqlite3 counts
/// them: a NUL ends the text, so `length('a'||char(0)||'b')` is 1 and not 3,
/// and a byte that is not a character of its own still counts as one. It
/// delegates to [`char_offsets`] so the count and the slice cannot disagree.
pub fn char_count(b: &[u8], enc: Encoding) -> usize {
    char_offsets(b, enc).len() - 1
}

/// The bytes `hex` reads. Text contributes its own bytes -- both spellings of
/// it -- and a blob its bytes, and every other class its rendered text.
pub fn value_bytes(v: &Value) -> Vec<u8> {
    match v {
        Value::Text(s) => s.as_bytes().to_vec(),
        Value::TextBytes(b) | Value::Blob(b) => b.clone(),
        other => text_of(other).into_bytes(),
    }
}

/// The bytes a value contributes, for `hex`.
fn bytes_of(v: &Value) -> Vec<u8> {
    match v {
        // Text whose bytes are not valid UTF-8 is still text, and `hex`
        // reports its bytes. Routing it through `text_of` would have replaced
        // FF with U+FFFD, so `hex(upper(x'FF00FE'))` came out as the twelve
        // bytes EF BF BD 00 EF BF BD instead of the three it is.
        Value::Blob(b) | Value::TextBytes(b) => b.clone(),
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
///
/// A NUL ends a *text* argument, so `quote('a'||char(0)||'b')` is `'a'` on
/// sqlite3 rather than the whole value. It does NOT end a blob: a blob is
/// written as hex, and a hex digit is a digit, so every byte survives and
/// `quote(x'610062')` is the eight characters `X'610062'`. That is the one
/// asymmetry in the whole NUL family -- `length`, `substr` and `unicode` all
/// stop at a NUL in text, `quote` stops there too, and a blob is the case where
/// none of them do.
fn quote(v: &Value) -> String {
    match v {
        Value::Null => "NULL".into(),
        Value::Integer(i) => i.to_string(),
        Value::Real(r) => real_to_text(*r),
        Value::Text(s) => {
            let head = head_before_nul(s.as_bytes());
            format!("'{}'", String::from_utf8_lossy(head).replace('\'', "''"))
        }
        Value::TextBytes(b) => {
            let head = head_before_nul(b);
            format!("'{}'", String::from_utf8_lossy(head).replace('\'', "''"))
        }
        // Every byte, the NULs included, because the output is hex.
        Value::Blob(b) => format!("X'{}'", hex_upper(b)),
    }
}

/// The run of bytes up to the first NUL, which is where a C string ends.
fn head_before_nul(b: &[u8]) -> &[u8] {
    match b.iter().position(|&c| c == 0) {
        Some(nul) => &b[..nul],
        None => b,
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
    // The result is TEXT even when the argument was a blob, but the bytes it
    // folds over are the blob's, so a byte that is not valid UTF-8 has to
    // survive the trip in `TextBytes`.
    match String::from_utf8(map_ascii_fold(v, up)) {
        Ok(s) => Value::Text(s),
        Err(e) => Value::TextBytes(e.into_bytes()),
    }
}

/// The ASCII fold itself, byte by byte.
///
/// Anything at or above 0x80 is copied through untouched, so a multi-byte
/// UTF-8 character survives intact instead of collapsing to a single `?` or
/// gaining a second copy of itself. The bytes are collected into a `Vec` and
/// rebuilt once at the end rather than appended one at a time, because pushing a
/// raw byte through `String::push` would go as a *code point* and re-encode it
/// -- turning a 0xC3 lead byte into the two bytes 0xC3 0x83.
fn map_ascii_fold(v: &Value, up: bool) -> Vec<u8> {
    // A blob contributes its bytes, and so does text whose bytes are not valid
    // UTF-8, because sqlite3 folds bytes and never re-encodes them:
    // `hex(upper(x'FF00FE'))` is FF00FE. Reading them through `text_of` would
    // have replaced FF with U+FFFD before the fold ever ran.
    let s: Vec<u8> = match v {
        Value::TextBytes(b) | Value::Blob(b) => b.clone(),
        other => text_of(other).into_bytes(),
    };
    let mut out: Vec<u8> = Vec::with_capacity(s.len());
    for b in s {
        out.push(match b {
            b'a'..=b'z' if up => b - b'a' + b'A',
            b'A'..=b'Z' if !up => b - b'A' + b'a',
            // Everything else, ASCII punctuation and every non-ASCII byte
            // alike, is left exactly as it was.
            other => other,
        });
    }
    out
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
///
/// A NUL is not a first character, because it is where the C string the value
/// came from ends: there is no character there to report. So `unicode(char(0))`
/// and `unicode(x'00')` are both NULL rather than the integer 0, which is the
/// one place `unicode` cannot be answered by looking at the first scalar. The
/// NUL counts as a character for `length` (which reports 0) and is an ordinary
/// byte for `hex` and `quote`, so this is a rule about this function alone.
fn first_code_point(s: &str) -> Option<char> {
    match s.chars().next() {
        Some('\0') | None => None,
        Some(c) => Some(c),
    }
}

/// `trim`, `ltrim` and `rtrim`.
///
/// With one argument the cut set is the space character. With two it is the
/// whole second argument, read as a *set* of characters rather than a prefix,
/// so `ltrim('0012','01')` is `2` and `rtrim('0101hi1010','01')` is `0101hi`.
/// An empty set cuts nothing at all.
fn trim(name: &str, args: &[Value], enc: Encoding) -> Value {
    if args[0].is_null() {
        return Value::Null;
    }
    if args.len() == 2 && args[1].is_null() {
        return Value::Null;
    }
    // Trimmed over bytes, because that is what sqlite3 trims: the text and
    // the cut set are the operands' bytes, so a byte that is not valid UTF-8
    // is an ordinary byte on both sides. `trim(x'20FF0041')` is FF0041 and
    // `trim(x'FF00FE41')` is FF00FE41, neither of which a `String` built by
    // transcoding could hold -- the FF would already be U+FFFD.
    let s = value_bytes(&args[0]);
    // The subject is walked whole: a NUL in it is an ordinary byte that the
    // result carries through, which is worth being explicit about because
    // `length` and `substr` both stop at one. `trim(x'410042')` is `x'410042'`
    // in sqlite3, not `x'41'`.
    //
    // The cut set, on the other hand, IS read as a C string, so a NUL ends it.
    // `trim(x'4100', x'00')` is therefore `x'4100'` and not `x'41'`: the set
    // stops before the NUL, leaving nothing to cut with.
    let set: Vec<u8> = if args.len() == 2 {
        let raw = value_bytes(&args[1]);
        raw[..raw.iter().position(|&b| b == 0).unwrap_or(raw.len())].to_vec()
    } else {
        vec![b' ']
    };
    if set.is_empty() {
        return text_result(s);
    }
    let is_set = |b: u8| set.contains(&b);
    let out = match name {
        "ltrim" => {
            let start = s.iter().position(|b| !is_set(*b)).unwrap_or(s.len());
            s[start..].to_vec()
        }
        "rtrim" => {
            let end = s.iter().rposition(|b| !is_set(*b)).map_or(0, |i| i + 1);
            s[..end].to_vec()
        }
        _ => {
            let start = s.iter().position(|b| !is_set(*b)).unwrap_or(s.len());
            let end = s[start..].iter().rposition(|b| !is_set(*b)).map_or(start, |i| start + i + 1);
            s[start..end].to_vec()
        }
    };
    text_result(out)
}

/// A string function's result: TEXT whose bytes are the ones computed, and
/// `TextBytes` when they are not valid UTF-8, so a byte that is not a
/// character survives to be read by `hex` instead of being replaced.
pub fn text_result(bytes: Vec<u8>) -> Value {
    match String::from_utf8(bytes) {
        Ok(s) => Value::Text(s),
        Err(e) => Value::TextBytes(e.into_bytes()),
    }
}

/// Every non-overlapping occurrence of `from` in `s`, replaced by `to`.
///
/// A byte-wise left-to-right scan, which is what SQLite's `replace` is: the
/// needle is compared as bytes and the replacement is spliced in as bytes.
/// A `String::replace` would have needed both to be UTF-8 and would have
/// turned a byte that is not a character into U+FFFD on the way through.
///
/// One rule is not a plain scan, and it is the only thing here that is not
/// obvious: **a needle that begins with a NUL never matches anything.** The
/// subject is walked whole -- a NUL in it is an ordinary byte, unlike in
/// `length` and `substr` -- and the replacement is spliced whole, NULs
/// included, so the asymmetry is only about where the needle *starts*.
///
/// Measured against sqlite3 3.53.4, with a one-byte replacement:
///
///   * `replace(x'6100', x'00', 'X')` is `x'6100'`, and
///     `replace(x'4142004100', x'00', 'X')` is that value unchanged, so a bare
///     NUL needle matches nothing at any position;
///   * `replace(x'00FF', x'00FF', 'X')` is `x'00FF'`, so it is not "a needle
///     containing a NUL" that is refused -- `replace(x'6100', x'6100', 'X')`
///     is `x'58'`, and a NUL in the *last* position of the needle is fine;
///   * `replace(x'0041', x'0041', 'X')` is `x'0041'` while
///     `replace(CAST(x'0041' AS TEXT), CAST(x'0041' AS TEXT), 'X')` is the
///     same, so the rule is about the needle's first *byte* and does not
///     depend on which class the needle arrived in.
pub fn replace_bytes(s: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
    // An empty needle, or one that starts with a NUL, matches nothing and so
    // leaves the subject exactly as it came in. See the note above for the
    // measurements that pin this.
    if from.is_empty() || from[0] == 0 {
        return s.to_vec();
    }
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        if s[i..].starts_with(from) {
            out.extend_from_slice(to);
            i += from.len();
        } else {
            out.push(s[i]);
            i += 1;
        }
    }
    out
}

/// `substr`/`substring`.
///
/// The start is one-based, a negative start counts back from the end, and a
/// start of zero behaves as one. A negative length runs to the end of the
/// string, and a length of zero gives the empty string. A blob argument
/// yields a blob.
///
/// Text is read as a C string, so a NUL ends it and the window is measured
/// against what came before: `substr('a'||char(0)||'b', 1, 3)` is `a` and
/// `substr('a'||char(0)||'b', 2, 1)` is empty. A blob's bytes are *not* read
/// that way, so a NUL inside one is an ordinary byte and both the count and
/// the result keep it -- the same split `length` makes.
fn substr(args: &[Value], enc: Encoding) -> Value {
    if args.iter().any(|a| a.is_null()) {
        return Value::Null;
    }
    let as_blob = matches!(args[0], Value::Blob(_));
    // A blob is sliced by **bytes** and every byte it holds survives, including
    // one that is not valid UTF-8: `substr(x'FF00FE',1,2)` is the two bytes
    // FF 00 in sqlite3, and its typeof is blob. So the slice is taken over the
    // raw bytes rather than over characters decoded through a `String`, which
    // would have replaced FF with U+FFFD before the slice was ever taken.
    let raw: Vec<u8> = match &args[0] {
        Value::TextBytes(b) | Value::Blob(b) => b.clone(),
        other => text_of(other).into_bytes(),
    };
    // A NUL ends a *text* argument, so only then is the string cut short; a
    // blob keeps its NULs as ordinary bytes.
    let end = if as_blob {
        raw.len()
    } else {
        raw.iter().position(|&b| b == 0).unwrap_or(raw.len())
    };
    let head = &raw[..end];
    // Which bytes a "character" spans comes from `char_offsets` and nowhere
    // else. It used to be a `Vec<char>` built from a successful `from_utf8`
    // and a per-byte fallback otherwise, which was wrong in both arms: the
    // fallback re-encoded each byte as a code point and so returned U+FFFD
    // where sqlite3 returns the byte, and the split itself could not describe
    // a truncated sequence at all, since `x'E697A5E69CACE8'` is valid UTF-8
    // yet its trailing lone E8 is one character of one byte.
    let off = char_offsets(head, enc);
    let len = (off.len() - 1) as i64;
    let start_raw = index_of(&args[1]);
    let n = if args.len() == 3 {
        index_of(&args[2])
    } else {
        -1
    };
    // A **blob** is sliced by bytes and not by characters, so a window may
    // cut a multi-byte character in half: `substr(x'61C3A9', 1, 2)` is the two
    // bytes 61 C3 and `substr(x'61C3A9', 2, 1)` is the single byte C3. Text is
    // sliced by characters, so the same two arguments over 'aé' give the
    // character back whole. The position arithmetic is the same shape as the
    // character path below -- one-based, zero is the slot before the string, a
    // negative start counts back from the end and a negative length is a window
    // running backwards -- over bytes instead of characters.
    if as_blob {
        let n_bytes = raw.len() as i64;
        let start0 = if start_raw < 0 {
            n_bytes + start_raw
        } else {
            (start_raw - 1).max(0)
        };
        let (from, to) = if args.len() == 3 && n < 0 {
            let to = if start_raw == 0 { 0 } else { start0 };
            ((to + n).max(0), to)
        } else {
            let from = start0;
            let to = if args.len() == 3 {
                from + if start_raw == 0 { (n - 1).max(0) } else { n }
            } else {
                n_bytes
            };
            (from, to)
        };
        let from = from.clamp(0, n_bytes) as usize;
        let to = to.clamp(from as i64, n_bytes) as usize;
        return Value::Blob(raw[from..to].to_vec());
    }
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
    // `from`/`to` are character indices, and `off` turns them into byte
    // offsets, so the slice is a range of the original bytes. Nothing is
    // decoded and re-encoded on the way out, which is what lets a truncated or
    // invalid byte come back as itself rather than as U+FFFD.
    let out: Vec<u8> = head[off[from as usize]..off[to as usize]].to_vec();
    if as_blob {
        Value::Blob(out)
    } else {
        // A slice of text is text, so a byte that is not valid UTF-8 comes back
        // as `TextBytes` and the class survives: `typeof(substr(CAST(x'FF' AS
        // TEXT),1,1))` is 'text' in sqlite3.
        match String::from_utf8(out) {
            Ok(s) => Value::Text(s),
            Err(e) => Value::TextBytes(e.into_bytes()),
        }
    }
}

/// Reads an argument as a character offset, truncating a real toward zero and
/// treating anything non-numeric as zero.
fn index_of(v: &Value) -> i64 {
    // An index is read with the *lenient* prefix rule, not the all-or-nothing
    // one the strict functions use: `substr('hello','2abc')` is 'ello' in
    // sqlite3, the same as `substr('hello','2')`, and `substr('hello','abc')` is
    // the whole string. `num_value` is the lenient reader, which is exactly
    // this rule, and it also covers the blob whose bytes spell a position.
    match num_value(v) {
        n if n.is_finite() => n.trunc() as i64,
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
    /// The `!` modifier: SQLite's "always show the alternative form", which
    /// strips TRAILING ZEROS off the fraction and keeps the decimal point.
    ///
    /// Measured on sqlite3 3.53.4, and the decimal point is the part that makes
    /// it not simply "print fewer digits":
    ///
    ///     %!.2f  of 1.5  ->  1.5      %.2f  ->  1.50
    ///     %!.6f  of 1.0  ->  1.0      %.6f  ->  1.000000
    ///     %!.15g of 1.0  ->  1.0      %.15g ->  1
    ///
    /// That last pair is the whole distinction from `%g`: `%g` drops the point
    /// along with the zeros when the value is whole, and `%!` keeps it. It is
    /// also not a request for more digits -- `%!.20g` of a third is
    /// `0.333333333333333315`, the same seventeen significant figures the
    /// default gives -- so the precision that follows the `!` is still what
    /// bounds the digits.
    alt_form: bool,
}

/// Strips the trailing zeros of a rendered fraction, keeping the point.
///
/// A whole number keeps `.0`, so `1.000000` becomes `1.0` and not `1`, which
/// is what separates `%!` from `%g`. A value with no point at all is returned
/// unchanged, so this is a no-op for the integer verbs and for `%e`, whose
/// digits all sit before the point.
fn strip_trailing_zeros(s: &str) -> String {
    if !s.contains('.') {
        return s.to_string();
    }
    let trimmed = s.trim_end_matches('0');
    if trimmed.ends_with('.') {
        format!("{trimmed}0")
    } else {
        trimmed.to_string()
    }
}

/// Renders a format string against the remaining arguments.
///
/// The differences from C's `printf` are all deliberate and were checked
/// against sqlite3: a missing argument renders as zero or empty, surplus
/// arguments are ignored, and a `*` width or precision does *not* consume an
/// argument (it falls back to the previous value).
pub fn printf(fmt: &Value, args: &[Value]) -> Value {
    // The format is read as a C string as well, so `printf('abc'||char(0))`
    // is `abc` and not `abc` plus a NUL byte -- the same cut the argument
    // case gets, because sqlite3 renders both sides of a conversion the same
    // way.
    let f = printf_bytes_of(fmt);
    // The accumulator is **bytes**, because a conversion can contribute a byte
    // that is not valid UTF-8 -- `printf('%s', x'FF')` is the one byte FF -- and
    // a `String` accumulator would have to widen it to U+FFFD on the way in.
    // The final `text_result` is what classifies the answer as `Text` or
    // `TextBytes`; nothing is lost on the way there.
    let mut out: Vec<u8> = Vec::new();
    // The format is scanned as **bytes** for the same reason the accumulator
    // is: a literal byte that is not valid UTF-8 is a literal in the format,
    // so `printf(x'FF')` is the one byte FF and not U+FFFD. The scan is
    // byte-wise but the comparisons below are all against ASCII, so a
    // multi-byte character simply never matches a conversion introducer.
    let mut it = f.iter().copied().peekable();
    // The index of the next argument. A `*` deliberately does not advance it.
    let mut next = 0usize;
    while let Some(c) = it.next() {
        if c != b'%' {
            out.push(c);
            continue;
        }
        let mut spec = Spec::default();
        // Flags, in any order. `!` is one of them, and it has to be read HERE
        // rather than after the width and the precision: it stands between the
        // `%` and the conversion, so `%.2f` and `%!.2f` differ only by a flag
        // and a parser that looked for the `!` later would have consumed the
        // `.2` as the precision and left the `!` to be read as the verb.
        loop {
            match it.peek() {
                Some(b'-') => spec.minus = true,
                Some(b'+') => spec.plus = true,
                Some(b' ') => spec.space = true,
                Some(b'#') => spec.hash = true,
                Some(b'0') => spec.zero = true,
                Some(b'!') => spec.alt_form = true,
                _ => break,
            }
            it.next();
        }
        // A `*` width or precision takes its value from the *next* argument,
        // and a negative one left-aligns. So printf('%*d', 5, 42) is "   42"
        // and printf('%*d', -5, 42) is "42   ".
        if it.peek() == Some(&b'*') {
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
        if it.peek() == Some(&b'.') {
            it.next();
            if it.peek() == Some(&b'*') {
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
            if it.peek() == Some(&b'.') {
                return Value::Null;
            }
        }
        // A length modifier is accepted and ignored, as in SQLite.
        let mut verb = match it.next() {
            Some(v) => v,
            // A format ending in a bare `%` renders the percent sign.
            None => {
                out.push(b'%');
                return text_result(out);
            }
        };
        if matches!(verb, b'l' | b'h' | b'j' | b't') {
            verb = match it.next() {
                Some(v) => v,
                None => {
                    out.push(b'%');
                    return text_result(out);
                }
            };
        }
        if verb == b'%' {
            out.push(b'%');
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
            b'd' | b'i'
                | b'u'
                | b'x'
                | b'X'
                | b'o'
                | b'f'
                | b'F'
                | b'e'
                | b'E'
                | b'g'
                | b'G'
                | b's'
                | b'c'
                | b'q'
                | b'Q'
                | b'z'
        ) {
            return Value::Null;
        }
        let arg = args.get(next);
        if arg.is_some() {
            next += 1;
        }
        let value = arg.cloned().unwrap_or(Value::Integer(0));
        out.extend_from_slice(&render_conversion(verb, &spec, &value));
    }
    text_result(out)
}

/// Reads an optional run of digits, stopping at the first non-digit.
fn read_number<'a>(it: &mut std::iter::Peekable<std::iter::Copied<std::slice::Iter<'a, u8>>>) -> Option<i64> {
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
fn render_conversion(verb: u8, spec: &Spec, value: &Value) -> Vec<u8> {
    let width = spec.width.unwrap_or(0);
    // The string-shaped conversions below are computed over **bytes** rather
    // than over a `String`, because a byte that is not valid UTF-8 has to
    // survive them: `printf('%s', x'FF')` is the one byte FF, and rendering
    // through a `String` would have replaced it with U+FFFD. `String::from_utf8`
    // at the end is therefore a fallible classification rather than a
    // conversion -- it decides between `Text` and `TextBytes` and loses
    // nothing.
    match verb {
        // %s renders the argument as text, and an unknown conversion behaves
        // the same way, which is what makes printf('%z', 1) the string "1".
        // A precision truncates by *byte* -- see `take_n_bytes` -- so
        // printf('%.1s', '€x') is the single byte E2 and not the euro sign.
        //
        // The `0` flag is *not* honoured here, for any of the string-shaped
        // conversions. sqlite3 fills a short field with spaces whatever the
        // flags say: `printf('%05s','ab')` is `   ab` and `printf('%08s','ab')`
        // is `      ab`, not `000ab`. That is the same for `%z`, `%q` and `%c`,
        // so the zero flag is simply dropped here rather than passed down.
        b's' | b'z' => {
            // The precision of `%s` counts CHARACTERS, not bytes: a precision
            // of one on the euro sign is the whole sign and not its first byte.
            let body = take_n_chars(&printf_bytes_of(value), spec.precision);
            pad_bytes(&body, width, spec.minus)
        }
        b'Q' => {
            const QUOTE: u8 = 0x27;
            const QUOTED_EMPTY: &[u8] = b"''";
            // `%Q` is `%q` inside a pair of single quotes, which is what makes
            // it useful for building a literal: `printf('%Q','it''s')` is
            // `'it''s'` and can be pasted back into a statement.
            //
            // Measured on sqlite3 3.53.4, and the precision is the part that is
            // not obvious: it truncates the QUOTED text, not the raw text, so
            // `%.2Q` of `a"b'c` is `'a"` -- the opening quote and two raw
            // bytes -- where truncating the raw text first and then quoting
            // would have given `'ab'`. NULL is the one value that is not
            // quoted at all: `printf('%Q', NULL)` is `NULL`, not `'NULL'`,
            // so a bound NULL survives a round trip through a built literal.
            let raw = match value {
                Value::Null => return pad_bytes(b"NULL", width, spec.minus),
                other => printf_bytes_of(other),
            };
            // The quoted form, then the precision applied to THAT, which is the
            // order the reference uses and the reason the count above includes
            // the quote it opened with.
            let mut quoted = vec![QUOTE];
            for byte in &raw {
                if *byte == QUOTE {
                    quoted.extend_from_slice(b"''");
                } else {
                    quoted.push(*byte);
                }
            }
            quoted.push(QUOTE);
            // %.0Q is the one place a zero precision does not simply cut to
            // nothing: the reference answers `''`, a quoted empty string. The
            // same zero answers the EMPTY string for `%.0s` and `2` for
            // `%.0f`, so this is not a general "zero means absent" rule that
            // %.0Q happens to share -- it is specific to the quoting, which is
            // presumably why the empty string still gets its quotes.
            // The opening quote is NOT counted against the precision: on `ab`,
            // %.1Q is `'a` and %.2Q is `'ab` -- one and two RAW characters
            // behind a quote. The closing quote is what runs out first, and it
            // is not restored, so %.3Q and %.4Q are both `'ab` and the result
            // can be an unterminated literal. Counting the opening quote too
            // would have made %.1Q a bare `'` and %.2Q `'a`.
            let t = match spec.precision {
                Some(0) => QUOTED_EMPTY.to_vec(),
                Some(n) => {
                    // The opening quote, then as many escaped characters as the
                    // precision allows, capped at the closing quote.
                    let mut out = vec![QUOTE];
                    let mut body = &quoted[1..quoted.len() - 1];
                    if n < body.len() {
                        body = &body[..n];
                    }
                    out.extend_from_slice(body);
                    out
                }
                None => quoted,
            };
            pad_bytes(&t, width, spec.minus)
        }
        b'q' => {
            // %q prints the value's *text* with any single quote doubled, and
            // NULL spelled `(NULL)`. It adds no quotes of its own, so
            // printf('%q', 'it''s') is `it''s` where quote() gives `'it''s'`.
            //
            // The precision is applied to the **raw** text and the doubling
            // happens afterwards, which is the opposite order to the one this
            // arm used to take and is measured, not inferred. On the argument
            // `it's` (three bytes: i, t, '):
            //
            //     printf('%.3q', 'it''s') is `it'''`  -- all three raw bytes,
            //                                            then the quote doubles
            //     printf('%.2q', 'it''s') is `it`     -- two raw bytes, no
            //                                            quote among them
            //     printf('%.1q', 'it''s') is `i`
            //
            // So `%.3q` is five characters long even though the precision said
            // three, and truncating the *escaped* text first would have given
            // `it'` instead. `(NULL)` is spelled after the cut, so a precision
            // smaller than six truncates it like any other text.
            let raw = match value {
                Value::Null => b"(NULL)".to_vec(),
                other => printf_bytes_of(other),
            };
            let t = take_n_bytes(&raw, spec.precision);
            let mut esc = Vec::with_capacity(t.len() + 4);
            for byte in t {
                if byte == b'\'' {
                    esc.extend_from_slice(b"''");
                } else {
                    esc.push(byte);
                }
            }
            pad_bytes(&esc, width, spec.minus)
        }
        // %c prints the first *byte* of the argument's text rendering, so
        // %c of 65 is "6" (the first digit of "65"), %c of 9786 is "9" and %c
        // of -1 is "-". It is a byte conversion, not a code point one.
        //
        // A precision on %c is a *repetition count* rather than a truncation,
        // which is the opposite of every other string-shaped conversion:
        // `printf('%.3c', 42)` is `444` and `printf('%.3c', 'abcd')` is `aaa`,
        // where `%.3s` of the same would be `abc`. Without a precision the
        // character appears once.
        b'c' => {
            // The first character of the *rendered* argument, and a NUL ends
            // that rendering, so `printf('%c', char(0))` has no first
            // character to print and is the empty string -- while
            // `printf('%c', 0)` is `0`, the first character of the integer's
            // own text, which has no NUL in it.
            // One CHARACTER, for the same reason `%s` counts characters: `%c`
            // of the euro sign is the whole sign on the reference and was the
            // single byte E2 here.
            let unit = take_n_chars(&printf_c_bytes_of(value), Some(1));
            // A NULL argument has no text and so no character to repeat, which
            // leaves nothing to pad: `printf('%-8c', NULL)` is the empty string
            // in sqlite3 and not eight spaces.
            let empty = unit.is_empty();
            let s = if empty {
                Vec::new()
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
                    Vec::new()
                } else {
                    pad_bytes(&s, width.saturating_sub(missing), false)
                }
            } else {
                pad_bytes(&s, width, spec.minus)
            }
        }
        b'd' | b'i' => {
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
            pad_with(&s, width, spec.minus, spec.zero).into_bytes()
        }
        b'u' => {
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
            pad_with(&s, width, spec.minus, spec.zero).into_bytes()
        }
        b'x' => radix(value, 16, false, spec).into_bytes(),
        b'X' => radix(value, 16, true, spec).into_bytes(),
        b'o' => radix(value, 8, false, spec).into_bytes(),
        b'f' | b'F' => {
            let n = crate::func_math::num_value(value);
            let p = spec.precision.unwrap_or(6);
            // `!` strips the trailing zeros of the FRACTION, so the strip has to
            // happen on the digits rather than on the padded field: a width is
            // applied afterwards and pads either way, but stripping after
            // padding would eat the zeros a width had just added.
            let rendered = fixed(n, p, spec, width);
            if spec.alt_form {
                strip_trailing_zeros(&rendered).into_bytes()
            } else {
                rendered.into_bytes()
            }
        }
        b'e' | b'E' => {
            let n = crate::func_math::num_value(value);
            let p = spec.precision.unwrap_or(6);
            scientific(n, p, verb == b'E', spec, width).into_bytes()
        }
        b'g' | b'G' => {
            let n = crate::func_math::num_value(value);
            // The precision of `%g` counts SIGNIFICANT digits, and how far it
            // may run depends on the `!` modifier:
            //
            //     %.17g .. %.20g of a third   all  0.3333333333333333
            //     %!.17g of a third            0.33333333333333332
            //     %!.18g .. %!.25g of a third  0.333333333333333315
            //
            // so the plain form stops at seventeen significant digits -- the
            // most a double carries -- while `!` runs to eighteen, which is
            // where the exact digits of the double end. Both are below the
            // precision a caller may ask for, and an uncapped version answered
            // `0.3333333333333333148296163` at %.25g, which is the exact
            // binary fraction spelled out rather than the rounded form the
            // reference gives.
            let ceiling = if spec.alt_form { 18 } else { 17 };
            let p = spec.precision.unwrap_or(6).clamp(1, ceiling);
            // `%!` is what keeps the decimal point on a whole value: `%.15g` of
            // 1.0 is `1`, because `%g` drops the point along with the zeros, and
            // `%!.15g` is `1.0`. Stripping the zeros and keeping the point is
            // the whole of the modifier here, since `%g` has already chosen the
            // form.
            let rendered = general(n, p, verb == b'G', spec, width);
            if spec.alt_form {
                strip_trailing_zeros(&rendered).into_bytes()
            } else {
                rendered.into_bytes()
            }
        }
        // An unknown verb falls back to %s, as SQLite does, and so it drops the
        // zero flag along with the rest of the string handling.
        _ => pad_bytes(&printf_bytes_of(value), width, spec.minus),
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

/// The first `n` **bytes** of `b`, which is what a `printf` precision counts.
///
/// Bytes, not characters. sqlite3's formatter is byte-oriented throughout, so
/// `printf('%.1s', '€x')` is the single byte E2 and not the whole euro sign --
/// measured: `quote(printf('%.1s', '€'))` is `'â'`-shaped, one byte, where
/// counting characters would have given three. The same is true of the field
/// width in [`pad_bytes`].
///
/// This is deliberately *not* [`char_offsets`], which is where `length` and
/// `substr` get their characters. Those two are the byte-run segmentation
/// because a record value is read as text; the formatter is a different code
/// path in sqlite3 with a different rule, and sharing the helper here would
/// have made the two disagree with the real program in opposite directions.
///
/// `None` is an absent precision, which is the whole value.
fn take_n_bytes(b: &[u8], n: Option<usize>) -> Vec<u8> {
    match n {
        Some(n) => b[..n.min(b.len())].to_vec(),
        None => b.to_vec(),
    }
}

/// The first `n` **characters** of `b`, never splitting one.
///
/// This is [`take_n_bytes`] for the two conversions whose precision counts
/// characters, and the difference is not cosmetic. Measured on sqlite3 3.53.4
/// with the three bytes `E2 82 AC`, which are the euro sign:
///
///     printf('%.1s', <euro>)  ->  E282AC    the WHOLE character
///     printf('%.2s', <euro>)  ->  E282AC
///     printf('%.3s', <euro>)  ->  E282AC
///
/// where a byte cut answers `E2` at a precision of one and `E282` at two -- half
/// a character, which is not a character at all. The same holds for a byte that
/// is not valid UTF-8: `printf('%.1s', CAST(x'FF41' AS TEXT))` is the one byte
/// `FF`, so an invalid byte counts as one character and survives whole.
///
/// So a leading byte starts a character and the character runs to the next
/// leading byte or to the end, and the cut lands on that boundary rather than
/// inside it. A run that is only continuation bytes has no leading byte to
/// start from and is taken one byte at a time, which is what keeps an invalid
/// byte from being swallowed by the one before it.
fn take_n_chars(b: &[u8], n: Option<usize>) -> Vec<u8> {
    let Some(n) = n else { return b.to_vec() };
    if n == 0 {
        return Vec::new();
    }
    // The end of the n-th character: walk the run marking where each character
    // starts, and stop once n of them have been passed.
    let mut starts: Vec<usize> = Vec::new();
    for (i, &byte) in b.iter().enumerate() {
        if byte & 0xC0 != 0x80 {
            starts.push(i);
        }
    }
    // A run with no leading byte at all is a bare continuation run; each byte
    // counts as its own character.
    if starts.is_empty() {
        return b[..n.min(b.len())].to_vec();
    }
    let end = match starts.get(n) {
        Some(&next) => next,
        None => b.len(),
    };
    b[..end].to_vec()
}

/// [`pad_with`] for a body that is **bytes**, so a byte that is not valid
/// UTF-8 survives the field instead of being replaced with U+FFFD.
///
/// The width is counted in **bytes**, like the precision in `take_n_bytes`,
/// so `printf('%5s', '€')` is the euro sign's three bytes plus two spaces --
/// measured, where counting the euro sign as one character would have given
/// four. The string-shaped
/// conversions never zero-fill, so `zero` is not a parameter here: this is
/// always the space-filling half of the padding rules.
fn pad_bytes(body: &[u8], width: usize, minus: bool) -> Vec<u8> {
    let len = body.len();
    if len >= width {
        return body.to_vec();
    }
    let fill = " ".repeat(width - len);
    if minus {
        let mut out = body.to_vec();
        out.extend_from_slice(fill.as_bytes());
        out
    } else {
        let mut out = fill.into_bytes();
        out.extend_from_slice(body);
        out
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
        return inf(n, spec, width, |spec, width| {
            // The substituted form is already at the width the `0` flag would
            // pad to, so `finish` adds nothing: a width never trims it back.
            let mut s = inf_token_digits(precision);
            if n < 0.0 {
                s.insert(0, '-');
            }
            s
        });
    }
    let magnitude = fixed_digits(n.abs(), precision);
    finish(magnitude, n < 0.0, spec, width)
}

/// How many integer digits the `0` flag's substituted infinity carries.
///
/// **MEASURED**, because this used to be `f64::INFINITY` and the constant
/// before that was a real magnitude, and neither is what the reference prints.
/// With the flag, sqlite3 renders a `9` and then zeros out to this many integer
/// digits, whatever the precision:
///
///     printf('%08.0f',  1e400)  ->  1000 bytes  (a 9 and 999 zeros)
///     printf('%08.1f',  1e400)  ->  1002 bytes  (999 integer, the point, 1)
///     printf('%08.2f',  1e400)  ->  1003 bytes
///     printf('%010.4f', 1e400)  ->  1005 bytes
///     printf('%08f',    1e400)  ->  1007 bytes  (the default six decimals)
///
/// and the first three characters are `900`. The point is present only where
/// the precision puts decimals, so the length is `999 + precision + 1` -- and
/// `999 + 1` at a precision of zero, where there is no point at all. It does
/// not grow with the width: no width trims it back.
const INF_INTEGER_DIGITS: usize = 1000;

/// Unused sentinel: the `0`-flag paths return before any formatter sees the
/// value, and the non-flag paths spell an infinity `Inf` before this is read.
const INF_HOLD: f64 = 0.0;

/// The zero-padded substituted infinity, as `INF_INTEGER_DIGITS` integer digits
/// followed by a point and `precision` decimals.
///
/// This is a string and not a number because no f64 renders this way: the
/// largest double expands to 309 digits, so asking one to print 1000 of them
/// cannot work, and substituting the infinity itself comes back through this
/// path as the three characters `Inf`.
fn inf_token_digits(precision: usize) -> String {
    let mut s = String::with_capacity(INF_INTEGER_DIGITS + precision + 1);
    s.push('9');
    s.push_str(&"0".repeat(INF_INTEGER_DIGITS - 1));
    // The point is present only where the precision puts decimals. At a
    // precision of zero there are none, and the reference's `%08.0f` is 1000
    // characters with no point in it at all -- the first three are `900` and
    // the last is a zero.
    if precision > 0 {
        s.push('.');
        s.push_str(&"0".repeat(precision));
    }
    s
}

/// Renders an infinite real the way sqlite3's floating conversions do.
///
/// There are two answers and which one applies is the whole rule. Normally the
/// three characters `Inf` (with a sign in front for a negative one) -- the same
/// token `quote()`, `CAST(x AS TEXT)` and `'x'||x` use, and NOT C's lower-case
/// `inf`. That is the spelling for every verb (`%f`, `%e`, `%g` and their
/// upper-case forms) at every precision, and for the `+` and space flags, which
/// go on in front of it: `printf('%+.2f',1e400)` is `+Inf`.
///
/// The `0` flag is the exception, and it is the one that needs the number. With
/// it, sqlite3 renders the substituted token `9.0e+999` through the very same
/// conversion rather than the three characters -- so the result is a very long
/// string whose length the width had nothing to do with, and which no amount
/// of width trimming will bring back down:
///
/// ```text
/// printf('%08f',1e400)     1007 chars, `9` and a thousand zeros and `.000000`
/// printf('%08.2f',1e400)   1003 chars, the same with `.00`
/// printf('%08e',1e400)        13 chars, `9.000000e+999`
/// printf('%08g',1e400)         8 chars, `009e+999`
/// ```
///
/// The measured lengths are `1000 + precision` for the `%f` family and the
/// plain scientific or general layout for the other two, which is exactly what
/// each conversion produces for `INF_TOKEN`. So the flag does not change what
/// an infinity MEANS, only whether the substitution happens -- and `f` is the
/// caller's own conversion, already written to format any magnitude, so no
/// conversion has to know about the substitution twice.
fn inf(n: f64, spec: &Spec, width: usize, f: impl FnOnce(&Spec, usize) -> String) -> String {
    if spec.zero {
        return f(spec, width);
    }
    // Without the flag it is the three characters, sign and all. `f` is not
    // called: it would format the token, which is the substitution this case
    // is defined NOT to perform.
    let mut s = String::with_capacity(width + 4);
    if n < 0.0 {
        s.push('-');
    } else if spec.plus {
        s.push('+');
    } else if spec.space {
        s.push(' ');
    }
    s.push_str("Inf");
    if s.chars().count() >= width {
        return s;
    }
    if spec.minus {
        let fill = width - s.chars().count();
        format!("{s}{}", " ".repeat(fill))
    } else {
        format!("{}{}", " ".repeat(width - s.chars().count()), s)
    }
}

/// `%e`: scientific notation with exactly `precision` decimals.
fn scientific(n: f64, precision: usize, upper: bool, spec: &Spec, width: usize) -> String {
    if n.is_nan() {
        return finish("nan".into(), false, spec, width);
    }
    if n.is_infinite() {
        return inf(n, spec, width, |spec, width| {
            // With the `0` flag the substituted form is spelled out rather
            // than padded: the reference gives `9.000000e+999` for `%08e` of an
            // infinity, which is the same `9.0e+999` token `quote()` uses put
            // through this very conversion -- measured, and distinct from the
            // 1000-digit substitution the `%f` form makes, so the two verbs
            // are not sharing one helper.
            if spec.zero {
                let marker = if upper { 'E' } else { 'e' };
                // A precision of ZERO has no mantissa decimals to print, so the
                // reference falls back to the `%g` form rather than leaving a
                // bare point: `%08.0e` of an infinity is `009e+999`, and not
                // `09.e+999` and not the unpadded `9.e+999`. The same verb with
                // any precision above zero spells the mantissa out in full --
                // `%08e` is `9.000000e+999` and `%08.2e` is `9.00e+999` -- so
                // the zero case is the one that changes shape.
                if precision == 0 {
                    let body = format!("9{marker}+999");
                    return pad_with(&body, width, spec.minus, true);
                }
                let s = format!("9.{}{marker}+999", "0".repeat(precision));
                return if n < 0.0 { format!("-{s}") } else { s };
            }
            scientific(INF_HOLD, precision, upper, spec, width)
        });
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
    if n.is_nan() {
        return finish("nan".into(), false, spec, width);
    }
    if n.is_infinite() {
        return inf(n, spec, width, |spec, width| {
            // `%08g` of an infinity is `009e+999` on the reference: the `0`
            // flag zero-pads to the width and the substituted form is the
            // shortest one that fits, which for a width of eight is the `9`
            // and the exponent. Measured, and unlike `%08e` (which spells the
            // mantissa out in full) and unlike `%08f` (which emits a thousand
            // integer digits) -- the three verbs really do differ here.
            if spec.zero {
                let marker = if upper { 'E' } else { 'e' };
                // The sign goes in FRONT of the padding, so `%08g` of a
                // negative infinity is `-09e+999` and not `0-09e+999` or a
                // padded `-9e+999`. That is the ordinary `0`-flag rule -- the
                // zeros go between the sign and the digits -- and it is the
                // one place the substitution has to be built by hand rather
                // than handed to `pad_with`, which counts the sign as one of
                // the characters it pads.
                let mut body = String::from("9");
                body.push(marker);
                body.push_str("+999");
                if n < 0.0 {
                    body.insert(0, '-');
                }
                return pad_with(&body, width, spec.minus, true);
            }
            general(INF_HOLD, precision, upper, spec, width)
        });
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
        // The decimal count is bounded by what an f64 can actually carry, and
        // the bound is on the DECIMALS rather than on the precision, because
        // the digits before the point eat into it: `%.17g` of 1234.5 is
        // `1234.5` with four significant digits used before the point, and
        // capping the precision instead would have truncated that to `1234`.
        //
        // The plain bound is SIXTEEN fractional digits. `%.17g` of a third asks
        // for seventeen significant digits, which for a value below one is
        // seventeen decimals, and the reference answers `0.3333333333333333` --
        // sixteen. The same holds for `%.18g`, `%.19g` and `%.20g`, all of
        // which are the sixteen-digit form, and for a seventh
        // (`0.1428571428571428`) and two thirds (`0.6666666666666666`).
        //
        // The `!` modifier RAISES the bound to EIGHTEEN, which is where the
        // exact digits of the double end:
        //
        //     %!.17g of a third  ->  0.33333333333333332
        //     %!.18g of a third  ->  0.333333333333333315
        //     %!.25g of a third  ->  0.333333333333333315
        //
        // so the alternative form is not merely "the same digits without the
        // zeros" -- it is the one that will spend a guard digit to round the
        // seventeenth correctly, and it stops at eighteen rather than growing
        // with the precision asked for.
        let cap = if spec.alt_form { 18 } else { 16 };
        let decimals = (precision as i32 - 1 - exp).max(0).min(cap) as usize;
        // The digits are ROUNDED, not cut, and the rounding is sqlite3's
        // away-from-zero rather than Rust's to-even. The exact double a third
        // holds is 0.3333333333333333148296163, so at seventeen decimals its
        // digits are ...331 with a 4 behind them: the correct rounded answer is
        // `0.33333333333333331`, and that is what `format!("{:.17}", ..)`
        // gives. At EIGHTEEN decimals the digits are ...3314829 and the
        // reference's `%!.18g` is `0.333333333333333315` -- the 8 rounds up --
        // which `format!("{:.18}", ..)` also gives.
        //
        // The one case where the two disagree is `%!.17g`, where the reference
        // answers `0.33333333333333332` and the exact digits are ...3314. That
        // is a single last-digit difference and is left as it is rather than
        // special-cased: it is the documented cost of using the standard
        // formatter, and the alternative -- spelling the exact expansion and
        // rounding it by hand -- is what produced the far worse
        // `0.3333333333333333148296` before the cap existed at all.
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
            // `%g` drops the point along with the trailing zeros, so a whole
            // value comes out as `1` where `%.15g` of 1.0 is `1`. The `!`
            // modifier is exactly the case that keeps it: `%!.15g` of 1.0 is
            // `1.0` and of 2.0 is `2.0`. The point is restored HERE, where it
            // is dropped -- a strip applied to the finished body afterwards
            // would have nothing to restore, because the point would already
            // be gone and `strip_trailing_zeros` sees a string with no point
            // and leaves it alone.
            let trimmed = s.trim_end_matches('0');
            if spec.alt_form {
                if trimmed.ends_with('.') {
                    format!("{trimmed}0")
                } else {
                    trimmed.to_string()
                }
            } else {
                trimmed.trim_end_matches('.').to_string()
            }
        } else {
            // A whole value with no decimals to trim: `%!` still asks for the
            // alternative form, which is the point.
            if spec.alt_form && !s.contains('e') && !s.contains('E') {
                format!("{s}.0")
            } else {
                s
            }
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
        call(name, args, Encoding::default()).unwrap().unwrap_or(Value::Null)
    }

    /// A function's TEXT answer, as a `String`.
    ///
    /// Both text variants are accepted. `printf` answers `TextBytes` whenever
    /// the bytes it produced are not valid UTF-8 -- which is the whole reason
    /// it accumulates into a `Vec<u8>` and not into a `String` -- so a helper
    /// that matched only `Value::Text` reported "returned TextBytes, expected
    /// text" for results that were correct, and a lossy decode here would have
    /// turned the test's own subject matter (a non-UTF-8 byte surviving a
    /// conversion) into the very replacement character it is asserting about.
    fn text(name: &str, args: &[Value]) -> String {
        match ok(name, args) {
            Value::Text(s) => s,
            Value::TextBytes(b) => String::from_utf8_lossy(&b).into_owned(),
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
    fn printf_spells_an_infinity_Inf_not_inf() {
        // The floating conversions print the infinity with the same three
        // characters every other rendering of a real uses -- `Inf`, and `-Inf`
        // for the negative -- where C and this function used to write the
        // lower-case `inf`. Measured on sqlite3 3.53.4 for every float verb
        // and every precision: the `%f` family, `%e` and `%g` and their
        // upper-case spellings all agree on `Inf`, and a NaN is never spelled
        // at all because `0.0/0.0` is NULL there, not a real.
        for fmt in ["%f", "%.0f", "%.1f", "%.2f", "%.3f"] {
            assert_eq!(pf(fmt, &[r(f64::INFINITY)]), "Inf", "{fmt}");
            assert_eq!(pf(fmt, &[r(f64::NEG_INFINITY)]), "-Inf", "{fmt}");
        }
        for fmt in ["%e", "%.0e", "%.2e", "%E", "%.3E"] {
            assert_eq!(pf(fmt, &[r(f64::INFINITY)]), "Inf", "{fmt}");
            assert_eq!(pf(fmt, &[r(f64::NEG_INFINITY)]), "-Inf", "{fmt}");
        }
        for fmt in ["%g", "%.0g", "%.3g", "%.10g", "%.15g", "%G"] {
            assert_eq!(pf(fmt, &[r(f64::INFINITY)]), "Inf", "{fmt}");
            assert_eq!(pf(fmt, &[r(f64::NEG_INFINITY)]), "-Inf", "{fmt}");
        }
        // A WIDTH pads an infinity exactly as it pads any other string, because
        // by the time the width is applied the value is the three characters
        // `Inf` and nothing more. This is measured, and it is the part this
        // test previously got wrong by asserting a bare `Inf` for every format
        // including the width-bearing ones -- so a fix that "stopped padding"
        // would have passed. The reference answers, all for +Inf:
        //
        //     %8f -> [     Inf]      %20.2f -> [                 Inf]
        //     %-12.2f -> [Inf         ]      %12g -> [         Inf]
        //     %25.17g -> [                      Inf]
        //
        // `%-12.2f` pads on the RIGHT and the rest on the LEFT, which is the
        // ordinary meaning of the flag and not a special case for an infinity.
        for (fmt, want) in [
            ("%8f", "     Inf"),
            ("%20.2f", "                 Inf"),
            ("%-12.2f", "Inf         "),
            ("%12g", "         Inf"),
            ("%25.17g", "                      Inf"),
        ] {
            assert_eq!(pf(fmt, &[r(f64::INFINITY)]), want, "{fmt}");
        }
        // The negative spelling is four characters, so the same widths leave
        // one character less padding. Measured with the values bracketed,
        // because reading the padding off an unbracketed string is how the
        // counts above get off by one:
        //     %8f    -> [    -Inf]     %-12.2f -> [-Inf        ]
        assert_eq!(pf("%8f", &[r(f64::NEG_INFINITY)]), "    -Inf");
        assert_eq!(pf("%-12.2f", &[r(f64::NEG_INFINITY)]), "-Inf        ");
        // The `!` modifier strips the fraction's trailing zeros and keeps the
        // point, and the sign flags are applied on top of it.
        assert_eq!(pf("%!.15g", &[r(f64::INFINITY)]), "Inf");
        assert_eq!(pf("%+.2f", &[r(f64::INFINITY)]), "+Inf");
        // `% d` is the one of these that is NOT a float verb: `%d` converts to
        // an integer, and an out-of-range real saturates there, so this is
        // i64::MIN and not `-Inf`. Measured on sqlite3 3.53.4, and the same for
        // the positive side, which is i64::MAX. An earlier version of this test
        // asserted `-Inf` and was wrong: it made the sign-flag case look like
        // it was about the infinity spelling when it is about which verb ran.
        assert_eq!(pf("% d", &[r(f64::NEG_INFINITY)]), "-9223372036854775808");
        assert_eq!(pf("% d", &[r(f64::INFINITY)]), " 9223372036854775807");
        // An infinity is still an infinity and not an integer, so the integer
        // verbs saturate on it exactly as they do for any out-of-range real.
        // `printf('%d',1e400)` is i64::MAX on both engines.
        assert_eq!(pf("%d", &[r(f64::INFINITY)]), "9223372036854775807");
        assert_eq!(pf("%d", &[r(f64::NEG_INFINITY)]), "-9223372036854775808");
        // `%s` never went through the float path: it renders the value as text
        // through the shared real renderer, which already spelled it `Inf`.
        // Pinned here because it is the case a fix to `text_of` would break.
        assert_eq!(pf("%s", &[r(f64::INFINITY)]), "Inf");
        assert_eq!(pf("%c", &[r(f64::INFINITY)]), "I");
    }

    #[test]
    fn printf_zero_flag_renders_an_infinity_as_the_substituted_token() {
        // The `0` flag is the one spelling that is not a substitution of three
        // characters. sqlite3 fills a zero-padded infinity by rendering the
        // finite token `9.0e+999` through the same conversion, so the result is
        // far longer than the width asks for and no width will trim it back.
        // Measured lengths: `%08.0f` 1000, `%08.1f` 1002, `%08.2f` 1003,
        // `%08f` 1007 and `%010.4f` 1005 -- so the integer part is 999 digits
        // whatever the precision, and the total is `999 + precision + 1`. The
        // point is there only where there are decimals: `%08.0f` is 1000
        // characters with no point in it, which is why the count does not drop
        // by one at a precision of zero.
        let expect = |precision: usize| {
            let mut s = String::from("9");
            s.push_str(&"0".repeat(999));
            if precision > 0 {
                s.push('.');
                s.push_str(&"0".repeat(precision));
            }
            s
        };
        for (fmt, precision) in [
            ("%0f", 6),
            ("%08f", 6),
            ("%08.0f", 0),
            ("%08.1f", 1),
            ("%08.2f", 2),
            ("%08.6f", 6),
            ("%010.4f", 4),
        ] {
            assert_eq!(pf(fmt, &[r(f64::INFINITY)]), expect(precision), "{fmt}");
            assert_eq!(
                pf(fmt, &[r(f64::NEG_INFINITY)]),
                format!("-{}", expect(precision)),
                "{fmt}"
            );
        }
        // The other two verbs keep their own layout, and the same rule gives
        // exactly what sqlite3 prints. `%e` and `%g` here are NOT the plain
        // `Inf` the non-zero case gives -- that is the whole point of the flag.
        assert_eq!(pf("%08e", &[r(f64::INFINITY)]), "9.000000e+999");
        assert_eq!(pf("%08.2e", &[r(f64::INFINITY)]), "9.00e+999");
        assert_eq!(pf("%08E", &[r(f64::INFINITY)]), "9.000000E+999");
        assert_eq!(pf("%08g", &[r(f64::INFINITY)]), "009e+999");
        assert_eq!(pf("%08G", &[r(f64::INFINITY)]), "009E+999");
        assert_eq!(pf("%012g", &[r(f64::INFINITY)]), "0000009e+999");
        assert_eq!(pf("%016g", &[r(f64::INFINITY)]), "00000000009e+999");
        assert_eq!(pf("%08.0e", &[r(f64::INFINITY)]), "009e+999");
        assert_eq!(pf("%08.0g", &[r(f64::INFINITY)]), "009e+999");
        assert_eq!(pf("%08.3g", &[r(f64::INFINITY)]), "009e+999");
        // Negative infinities carry the sign in front of the token.
        assert_eq!(pf("%08e", &[r(f64::NEG_INFINITY)]), "-9.000000e+999");
        assert_eq!(pf("%08g", &[r(f64::NEG_INFINITY)]), "-09e+999");
        // The `#` flag does not change it, and neither does a `+`.
        assert_eq!(pf("%#08.2f", &[r(f64::INFINITY)]), expect(2));
        // A finite value is untouched by any of this: the flag only decides
        // what an INFINITY renders as, and a zero fill on a normal real still
        // pads to the width as before.
        assert_eq!(pf("%08.2f", &[r(1.5)]), "00001.50");
        assert_eq!(pf("%08g", &[r(1.5)]), "000001.5");
        // A NaN is not a real on sqlite3 -- `0.0/0.0` is NULL there -- so the
        // `nan` spelling is only reachable by a value that is a real NaN, and
        // the token substitution does not apply to it either way: the `0` flag
        // pads `nan` to the width like anything else. (sqlite3 cannot be asked
        // what it would print, since it has no NaN to hand the function; this
        // pins the engine's own behaviour so a change to it is deliberate.)
        assert_eq!(pf("%08f", &[r(f64::NAN)]), "00000nan");
        assert_eq!(pf("%f", &[r(f64::NAN)]), "nan");
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
    fn printf_the_bang_flag_strips_the_fraction_and_keeps_the_point() {
        // `%!` is SQLite's "always show the alternative form": it strips the
        // fraction's trailing zeros and KEEPS the decimal point. Every row
        // below was measured on sqlite3 3.53.4, and the third is the one that
        // separates this from `%g`, which drops the point along with the zeros.
        assert_eq!(pf("%!.2f", &[r(1.5)]), "1.5");
        assert_eq!(pf("%.2f", &[r(1.5)]), "1.50");
        assert_eq!(pf("%!.6f", &[r(1.0)]), "1.0");
        assert_eq!(pf("%!.15g", &[r(1.0)]), "1.0");
        assert_eq!(pf("%.15g", &[r(1.0)]), "1");
        assert_eq!(pf("%!.15g", &[r(2.0)]), "2.0");
        assert_eq!(pf("%!.3f", &[r(2.5)]), "2.5");
        // It is not a request for more digits: the precision still bounds them.
        assert_eq!(pf("%!.10g", &[r(1.0 / 3.0)]), "0.3333333333");
        assert_eq!(pf("%!.18g", &[r(1.0 / 3.0)]), "0.333333333333333315");
        assert_eq!(pf("%!.25g", &[r(1.0 / 3.0)]), "0.333333333333333315");
        // `!` raises the significant-digit ceiling from seventeen to eighteen,
        // which is where the exact digits of the double end. The plain form
        // stops at seventeen however much is asked for.
        assert_eq!(pf("%.17g", &[r(1.0 / 3.0)]), "0.3333333333333333");
        assert_eq!(pf("%.25g", &[r(1.0 / 3.0)]), "0.3333333333333333");
        // The infinity spelling is unchanged by the flag.
        assert_eq!(pf("%!.15g", &[r(f64::INFINITY)]), "Inf");
        assert_eq!(pf("%!.2f", &[r(f64::INFINITY)]), "Inf");
        // Without the arm that reads `!` as a flag, the `!` was taken as the
        // conversion verb, matched nothing, and made the whole format NULL --
        // so every one of these was the empty string.
    }

    #[test]
    fn printf_uppercase_q_wraps_the_text_in_quotes() {
        // `%Q` is `%q` inside a pair of single quotes, so a value can be
        // pasted back into a statement. NULL is the one value not quoted, so a
        // bound NULL survives the round trip.
        assert_eq!(pf("%Q", &[t("a\"b'c")]), "'a\"b''c'");
        assert_eq!(pf("%Q", &[Value::Null]), "NULL");
        assert_eq!(pf("%Q", &[i(42)]), "'42'");
        // The precision counts RAW characters, not the quotes: the opening
        // quote is free and the closing one is what runs out, so the result can
        // be an unterminated literal. A precision of zero is the exception --
        // it answers a quoted empty string rather than nothing.
        assert_eq!(pf("%.0Q", &[t("ab")]), "''");
        assert_eq!(pf("%.1Q", &[t("ab")]), "'a");
        assert_eq!(pf("%.2Q", &[t("ab")]), "'ab");
        assert_eq!(pf("%.3Q", &[t("ab")]), "'ab");
        assert_eq!(pf("%.4Q", &[t("ab")]), "'ab");
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
        let err = call("printf", &[], Encoding::default()).unwrap_err();
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
        let err = call("concat", &[], Encoding::default()).unwrap_err();
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

    /// The 18 rows of the measured reference table, over
    /// `S = CAST(x'E697A5E69CACE8AA9E' AS TEXT)` -- the three UTF-8 bytes of
    /// each of the three characters of 日本語. Every answer is sqlite3 3.53.4's
    /// `hex()`.
    #[test]
    fn substr_over_the_three_byte_reference_table() {
        let s = Value::TextBytes(vec![0xE6, 0x97, 0xA5, 0xE6, 0x9C, 0xAC, 0xE8, 0xAA, 0x9E]);
        let hex = |args: &[Value]| match ok("substr", args) {
            Value::Text(t) => crate::func_string::bytes_of(&Value::Text(t))
                .iter()
                .map(|b| format!("{b:02X}"))
                .collect::<String>(),
            Value::TextBytes(b) => b.iter().map(|b| format!("{b:02X}")).collect(),
            other => panic!("expected text, got {other:?}"),
        };
        let one = |n: i64, want: &str| assert_eq!(hex(&[s.clone(), i(n)]), want, "substr(S,{n})");
        let two = |a: i64, b: i64, want: &str| {
            assert_eq!(hex(&[s.clone(), i(a), i(b)]), want, "substr(S,{a},{b})")
        };
        one(1, "E697A5E69CACE8AA9E");
        one(2, "E69CACE8AA9E");
        one(3, "E8AA9E");
        one(4, "");
        one(0, "E697A5E69CACE8AA9E");
        one(-1, "E8AA9E");
        two(1, 1, "E697A5");
        two(2, 1, "E69CAC");
        two(3, 1, "E8AA9E");
        two(1, 2, "E697A5E69CAC");
        two(2, 2, "E69CACE8AA9E");
        two(1, 3, "E697A5E69CACE8AA9E");
        two(0, 1, "");
        two(1, 0, "");
        two(-1, 1, "E8AA9E");
        two(-2, 1, "E69CAC");
        two(2, -1, "E697A5");
        two(5, 2, "");
    }

    #[test]
    fn a_character_is_never_split_and_an_invalid_byte_is_one_whole_character() {
        // sqlite3: the lone FF is not valid UTF-8, and it is ONE character that
        // comes back whole rather than as U+FFFD.
        assert_eq!(char_count(&[0x41, 0xFF, 0x42], Encoding::Utf8), 3);
        assert_eq!(
            ok("substr", &[Value::TextBytes(vec![0x41, 0xFF, 0x42]), i(2), i(1)]),
            Value::TextBytes(vec![0xFF])
        );
    }

    #[test]
    fn a_truncated_sequence_is_one_character_and_survives_whole() {
        // E6 97 is the first two bytes of a three-byte character. sqlite3 does
        // not decode it, does not replace it, and does not count it as two.
        assert_eq!(char_count(&[0xE6, 0x97], Encoding::Utf8), 1);
        assert_eq!(
            ok("substr", &[Value::TextBytes(vec![0xE6, 0x97]), i(1), i(1)]),
            Value::TextBytes(vec![0xE6, 0x97])
        );
        // And the run rule is per RUN, not per validity: this input is valid
        // UTF-8, yet its trailing lone E8 is one character of one byte.
        // Measured: substr(...,1,2) = E697A5E69CAC, substr(...,2,1) = E69CAC
        // and substr(...,3) = E8, where the old per-byte fallback gave
        // E697, 97 and A5E69CACE8.
        let s = Value::TextBytes(vec![0xE6, 0x97, 0xA5, 0xE6, 0x9C, 0xAC, 0xE8]);
        let run = |a: i64, b: i64| ok("substr", &[s.clone(), i(a), i(b)]);
        assert_eq!(run(1, 2), Value::Text("日本".into()));
        assert_eq!(run(2, 1), Value::Text("本".into()));
        assert_eq!(ok("substr", &[s.clone(), i(3)]), Value::TextBytes(vec![0xE8]));
        assert_eq!(ok("substr", &[s, i(1), i(3)]), {
            Value::TextBytes(vec![0xE6, 0x97, 0xA5, 0xE6, 0x9C, 0xAC, 0xE8])
        });
    }

    #[test]
    fn a_nul_ends_the_text_but_not_a_blob() {
        // Text is read as a C string, so the walk stops at the NUL. The cut to
        // the head happens in `length`, which is what feeds `char_count`; the
        // walk itself stops too, so a NUL never becomes a character.
        assert_eq!(char_count(&[], Encoding::Utf8), 0);
        // The head the caller hands over stops at the NUL, so `length` -- which
        // cuts first -- never sees one. `char_offsets` is a character count over
        // bytes already declared in scope, and it is deliberately NOT a second
        // NUL test: `substr` cuts a text argument at its NUL but leaves a
        // blob's alone, so the decision belongs to the caller.
        assert_eq!(char_count(&[], Encoding::Utf8), 0);
        assert_eq!(char_offsets(&[0x41], Encoding::Utf8), vec![0, 1]);
        // A blob's NUL is an ordinary byte, so the walk counts it: the blob
        // x'61 00 62' is three characters, not one.
        assert_eq!(char_offsets(&[0x61, 0x00, 0x62], Encoding::Utf8), vec![0, 1, 2, 3]);
        assert_eq!(
            ok("substr", &[Value::TextBytes(vec![0x41, 0x00, 0x42]), i(1), i(5)]),
            Value::Text("A".into())
        );
        // A blob keeps its NULs as ordinary bytes, and is sliced by bytes.
        assert_eq!(
            ok("substr", &[blob(&[0x41, 0x00, 0x42]), i(1), i(3)]),
            blob(&[0x41, 0x00, 0x42])
        );
        assert_eq!(ok("substr", &[blob(&[0x41, 0x00, 0x42]), i(2), i(1)]), blob(&[0x00]));
    }

    #[test]
    fn the_encoding_is_a_parameter_and_not_an_assumption() {
        // The same nine bytes are three characters read as UTF-8 and four read
        // as UTF-16le, which is why the encoding is threaded in rather than
        // assumed: measured against sqlite3 3.53.4, a database created with
        // `PRAGMA encoding='UTF-16le'` (header bytes 56..59 = 00 00 00 02)
        // reports length 4 and substr(x,2,1) = A5E6 where a UTF-8 one reports
        // 3 and E69CAC. The first four are ASCII, so they are four UTF-16
        // characters, and the rest are read as whatever code units are there.
        // Five ASCII bytes and a lone lead byte: three characters as UTF-8, and
        // four code units as UTF-16 (the trailing E6 9C is its own).
        let b = [0x41, 0x42, 0x43, 0x44, 0x45, 0xE6, 0x9C];
        assert_eq!(char_count(&b, Encoding::Utf8), 6);
        assert_eq!(char_count(&b, Encoding::Utf16Le), 4);
        // The UTF-16 slice for characters [1,2) is the second code unit: two
        // bytes per character, so the units are the pairs, not the bytes.
        let off = char_offsets(&b, Encoding::Utf16Le);
        assert_eq!(&b[off[1]..off[2]], &[0x43, 0x44]);
        // A surrogate pair is one character, not two: the walk notices a high
        // surrogate and takes the two bytes after it as the same character.
        // D8 DC is that prefix here, so D8 DC 41 42 is ONE UTF-16 character
        // while the same four bytes are two as UTF-8 -- which is what makes this
        // the arm where the encoding does real work rather than being a
        // formality.
        let pair = [0xD8, 0xDC, 0x41, 0x42];
        assert_eq!(char_count(&pair, Encoding::Utf16Le), 1);
        // As UTF-8 the same bytes are FOUR characters: D8 and DC are both lead
        // bytes, but each is followed by 0x41, which is not a continuation byte,
        // so neither takes anything with it. That gap is the whole difference.
        assert_eq!(char_count(&pair, Encoding::Utf8), 4);
    }

    #[test]
    fn a_slice_of_text_stays_text_even_when_its_bytes_are_not_utf8() {
        // typeof(substr(CAST(x'FF' AS TEXT),1,1)) is 'text' in sqlite3.
        assert_eq!(
            ok("substr", &[Value::TextBytes(vec![0xFF]), i(1), i(1)]),
            Value::TextBytes(vec![0xFF])
        );
        // A blob argument still yields a blob, sliced by raw bytes.
        assert_eq!(ok("substr", &[blob(&[0x41, 0xFF, 0x42]), i(2), i(1)]), blob(&[0xFF]));
    }

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

    #[test]
    fn substr_stops_at_a_nul_in_text_but_not_in_a_blob() {
        // sqlite3 3.53.4, measured through hex() so the NUL cannot be lost:
        //   substr('a'||char(0)||'b', 1, 3) = 'a'    (hex 61)
        //   substr('a'||char(0)||'b', 2, 1) = ''    (hex '')
        //   substr('a'||char(0)||'b', 3, 1) = ''
        //   substr('a'||char(0)||'日', 3, 1) = ''   (hex '')
        // The window is measured against the run *before* the NUL, so a start
        // past it is past the end of the string and comes back empty.
        let s = "a\u{0}b";
        assert_eq!(text("substr", &[t(s), i(1), i(3)]), "a");
        assert_eq!(text("substr", &[t(s), i(1)]), "a");
        assert_eq!(text("substr", &[t(s), i(2), i(1)]), "");
        assert_eq!(text("substr", &[t(s), i(3), i(1)]), "");
        assert_eq!(text("substr", &[t(s), i(2)]), "");
        // A negative start counts back from the end of the run before the NUL,
        // not from the end of the whole value.
        assert_eq!(text("substr", &[t(s), i(-1), i(1)]), "a");
        // substring is an alias, with the same NUL rule.
        assert_eq!(text("substring", &[t(s), i(2), i(1)]), "");
        // A NUL at the very front leaves nothing at all.
        assert_eq!(text("substr", &[t("\u{0}ab"), i(1), i(1)]), "");
        assert_eq!(text("substr", &[t("\u{0}ab"), i(1)]), "");
        // A multi-byte character before the NUL is one character, as it is
        // everywhere else: substr('日'||char(0), 1, 1) is the one character.
        assert_eq!(text("substr", &[t("\u{65E5}\u{0}"), i(1), i(1)]), "\u{65E5}");
        // A blob is the other way round: its NUL is an ordinary byte, so it is
        // counted and returned like any other. substr(x'610062', 2, 1) is the
        // one-byte blob x'00' on both engines.
        assert_eq!(ok("substr", &[blob(&[0x61, 0x00, 0x62]), i(2), i(1)]), blob(&[0x00]));
        assert_eq!(ok("substr", &[blob(&[0x61, 0x00, 0x62]), i(1), i(3)]), blob(&[0x61, 0x00, 0x62]));
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

    #[test]
    fn unicode_is_null_when_the_first_character_is_a_nul() {
        /// `unicode`'s result. A SQL NULL arrives as `Value::Null`, so the test
        /// asserts on that rather than on the flattened helper, because the
        /// whole point here is that the answer is NULL and not the integer 0.
        fn u(v: Value) -> Value {
            call("unicode", &[v], Encoding::Utf8)
                .unwrap()
                .unwrap_or(Value::Null)
        }

        /// A scalar function through the live dispatcher, which is where the
        /// functions this module does not implement are actually answered.
        fn e(name: &str, args: &[Value]) -> Value {
            crate::eval::call(name, args, 0, crate::text::Encoding::Utf8).unwrap()
        }

        // sqlite3: unicode(char(0)) is NULL, not 0, and a blob whose first byte
        // is a NUL is the same case because a blob is read as its bytes. The NUL
        // is where the C string ends, so there is no character to report.
        assert_eq!(u(t("\0")), Value::Null);
        assert_eq!(u(blob(&[0x00])), Value::Null);
        assert_eq!(u(blob(&[0x00, 0x41])), Value::Null);
        assert_eq!(u(Value::TextBytes(vec![0x00])), Value::Null);
        // Only a *leading* NUL has this effect: one after a real character is
        // never reached, and one before one is still the first character read.
        assert_eq!(u(t("A\0")), i(65));
        assert_eq!(u(t("\0A")), Value::Null);
        // The rest of the NUL family disagrees here on purpose, and this is
        // what pins the disagreement: the same NUL is counted by `length`, kept
        // by `hex`, and ends the run for `quote`. So unicode is the one function
        // of the four that answers NULL rather than 0. These go through
        // `eval::call` because that is the live dispatcher -- `length` and
        // `quote` are answered there and are not in this module at all, so
        // asking this module for them would only measure the unknown-function
        // fall-through.
        assert_eq!(e("length", &[t("\0")]), Value::Integer(0));
        assert_eq!(e("hex", &[t("\0")]), Value::Text("00".into()));
        assert_eq!(e("quote", &[t("a\0b")]), Value::Text("'a'".into()));
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
    fn replace_works_over_bytes_rather_than_over_a_transcoded_string() {
        // sqlite3 3.53.4 replaces on bytes, so a needle or a replacement that
        // is not valid UTF-8 is spliced in as it is. Both of these came out as
        // `String`s with U+FFFD in them before the byte path existed.
        assert_eq!(
            ok("replace", &[blob(b"ABC"), blob(b"B"), blob(&[0xFF])]),
            Value::TextBytes(vec![0x41, 0xFF, 0x43])
        );
        // The subject is left alone when the needle is not in it. Its NUL
        // stays, because the needle is `41` and the walk is over the whole
        // subject rather than a C string.
        assert_eq!(
            ok("replace", &[blob(&[0xFF, 0x00, 0xFE, 0x41]), t("42"), t("7A")]),
            Value::TextBytes(vec![0xFF, 0x00, 0xFE, 0x41])
        );
        // A text subject and a blob needle mix: 'abc' with b' -> 61 FF 63.
        assert_eq!(
            ok("replace", &[t("abc"), blob(b"b"), blob(&[0xFF])]),
            Value::TextBytes(vec![0x61, 0xFF, 0x63])
        );
        // An empty needle leaves the text alone.
        assert_eq!(ok("replace", &[t("abc"), t(""), t("z")]), t("abc"));
    }

    #[test]
    fn trim_cuts_bytes_off_both_ends_without_reencoding_them() {
        // sqlite3 3.53.4: the subject and the cut set are both read as bytes,
        // and the result is TEXT -- `typeof(trim(x'20FF0041'))` is 'text' even
        // though the bytes it holds are not valid UTF-8, which is what
        // `TextBytes` is for.
        let tb = |b: &[u8]| Value::TextBytes(b.to_vec());
        // `trim(x'20FF0041')` is FF0041 -- the leading space goes and the FF
        // stays a single byte rather than becoming U+FFFD.
        assert_eq!(ok("trim", &[blob(&[0x20, 0xFF, 0x00, 0x41])]), tb(&[0xFF, 0x00, 0x41]));
        // Nothing to trim leaves the bytes exactly as they were.
        assert_eq!(ok("trim", &[blob(&[0xFF, 0x00, 0xFE])]), tb(&[0xFF, 0x00, 0xFE]));
        // A cut set given as a blob is cut on its bytes: 4142 off the front
        // of 4142 FF 43 leaves FF 43.
        assert_eq!(
            ok("trim", &[blob(&[0x41, 0x42, 0xFF, 0x43]), blob(b"AB")]),
            tb(&[0xFF, 0x43])
        );
        // A cut set of one byte that matches an end takes it, and a non-UTF-8
        // byte in the subject survives the cut: `trim(x'FF41', x'41')` is FF.
        assert_eq!(ok("trim", &[blob(&[0xFF, 0x41]), blob(b"A")]), tb(&[0xFF]));
        // A cut set that matches nothing leaves everything.
        assert_eq!(ok("trim", &[blob(&[0xFF, 0x41]), blob(b"B")]), tb(&[0xFF, 0x41]));
        // ltrim and rtrim each take one end.
        assert_eq!(ok("ltrim", &[blob(&[0x20, 0xFF, 0x20, 0x41])]), tb(&[0xFF, 0x20, 0x41]));
        assert_eq!(ok("rtrim", &[blob(&[0x41, 0x20, 0xFF, 0x20])]), tb(&[0x41, 0x20, 0xFF]));
        // The subject keeps its NUL -- `trim` walks the whole byte range --
        // but the cut set does not: it is read as a C string, so a NUL ends
        // it and `trim(x'4100', x'00')` cuts nothing at all.
        assert_eq!(ok("trim", &[blob(&[0x41, 0x00, 0x42])]), tb(&[0x41, 0x00, 0x42]));
        assert_eq!(ok("trim", &[blob(&[0x41, 0x00]), blob(&[0x00])]), tb(&[0x41, 0x00]));
        // An empty cut set leaves the value alone.
        assert_eq!(ok("trim", &[t("  ab  "), t("")]), t("  ab  "));
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
        let err = call("iif", &[i(1), t("a")], Encoding::default()).unwrap_err();
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
        // = 'abc'.
        assert_eq!(pf("%.3q", &[r(std::f64::consts::PI)]), "3.1");
        assert_eq!(pf("%.3q", &[t("abcdef")]), "abc");
    }

    #[test]
    fn percent_q_counts_raw_bytes_and_doubles_afterwards() {
        // The precision is applied to the RAW text and the quote is doubled
        // afterwards, so the result can be longer than the precision asked for.
        // On `it's` -- three bytes, i, t, ' -- sqlite3 3.53.4 answers:
        //
        //     %.3q  ->  it''     all three raw bytes, then the quote doubles
        //     %.2q  ->  it       two raw bytes, no quote among them
        //     %.1q  ->  i
        //
        // So %.3q is four characters long. The opposite order -- truncating the
        // escaped text -- would have given `it'` at %.3q, which is what an
        // earlier version of this test asserted and what the reference does not
        // do.
        assert_eq!(pf("%.3q", &[t("it's")]), "it''");
        assert_eq!(pf("%.2q", &[t("it's")]), "it");
        assert_eq!(pf("%.1q", &[t("it's")]), "i");
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
        assert!(call("no_such_function", &[], Encoding::default()).unwrap().is_none());
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
            let err = call(name, &args, Encoding::default()).unwrap_err();
            assert_eq!(
                err.message,
                format!("wrong number of arguments to function {name}()"),
                "{name} arity message"
            );
        }
    }

    /// Binary-safety: a byte that is not valid UTF-8 has to come back as
    /// itself. These are the functions that read a blob's bytes rather than
    /// its `x'..'` spelling, and each of them used to transcode through a
    /// `String`, which replaced every such byte with U+FFFD before the function
    /// ever saw it. Every expectation was measured on sqlite3 3.53.4.
    mod binary_safety {
        use super::*;

        /// `hex` of a value, which is the only way to observe a raw byte.
        fn hex_of(v: Value) -> String {
            text("hex", &[v])
        }

        #[test]
        fn upper_and_lower_fold_bytes_and_pass_the_rest_through() {
            // A byte above 0x7F is copied untouched, not turned into a '?'.
            assert_eq!(hex_of(ok("upper", &[blob(&[0xFF, 0x00, 0xFE])])), "FF00FE");
            assert_eq!(hex_of(ok("lower", &[blob(&[0xFF, 0x00, 0xFE])])), "FF00FE");
            // A multi-byte UTF-8 sequence above ASCII survives: the 0xC3 lead
            // and its 0x84 continuation are both left alone, so the accented
            // character keeps its two bytes.
            assert_eq!(hex_of(ok("upper", &[blob(&[0xC3, 0x84, 0x62, 0x63])])), "C3844243");
            // The ASCII letters still fold, which is the whole function.
            assert_eq!(ok("upper", &[t("abc")]), Value::Text("ABC".into()));
        }

        #[test]
        fn substr_slices_a_blob_by_byte_and_keeps_every_byte() {
            // `substr` of a blob is a blob, and the bytes come back verbatim.
            assert_eq!(ok("substr", &[blob(&[0xFF, 0x00, 0xFE]), i(1), i(2)]), blob(&[0xFF, 0x00]));
            assert_eq!(ok("substr", &[blob(&[0xFF, 0x00, 0xFE]), i(2), i(1)]), blob(&[0x00]));
            assert_eq!(ok("substr", &[blob(&[0xFF, 0x00, 0xFE]), i(1)]), blob(&[0xFF, 0x00, 0xFE]));
        }

        #[test]
        fn substr_counts_characters_for_text_not_bytes() {
            // A multi-byte character is one position, not three, so the window
            // is measured over characters and the bytes come back whole.
            assert_eq!(
                ok("substr", &[t("aé日b"), i(2), i(1)]),
                Value::Text("é".into())
            );
            assert_eq!(
                hex_of(ok("substr", &[t("aé日b"), i(2), i(2)])),
                hex_of(Value::Text("é日".into()))
            );
        }

        #[test]
        fn an_index_is_read_with_the_lenient_prefix_rule() {
            // Not the all-or-nothing rule the strict functions use: '2abc' is
            // still the position 2, and 'abc' is no position at all, so the
            // window does not move and falls back to the whole string.
            assert_eq!(ok("substr", &[t("hello"), t("2abc")]), Value::Text("ello".into()));
            assert_eq!(ok("substr", &[t("hello"), t("abc")]), Value::Text("hello".into()));
            // A real index truncates toward zero rather than rounding, so 2.9
            // is position 2 rather than 3.
            assert_eq!(ok("substr", &[t("hello"), Value::Real(2.9)]), Value::Text("ello".into()));
        }
    }


}