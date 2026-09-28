//! Text that is not valid UTF-8, and NULs inside text.
//!
//! sqlite3's text is a byte string, not a Rust `String`, and the two places
//! that shows most are `CAST(<blob> AS TEXT)` -- which keeps the bytes
//! verbatim and so can produce text no `String` can hold -- and the NUL, which
//! ends the text for `length` without being part of it.
//!
//! Every expectation here was measured against the real sqlite3 3.53.4 on this
//! machine, and the statements are written over blob literals so that nothing
//! depends on how a terminal reads a non-ASCII byte in a SQL script.

use crate::connection::Connection;
use crate::value::Value;

/// Runs one statement and returns the single value it produced.
fn one(sql: &str) -> Value {
    let mut c = Connection::open_memory().expect("open");
    let out = c.execute_script(sql).expect("run");
    let mut row = out.into_iter().find_map(|o| match o {
        crate::connection::Outcome::Query { rows, .. } => rows.into_iter().next(),
        _ => None,
    });
    row.take()
        .and_then(|r| r.values.into_iter().next())
        .unwrap_or(Value::Null)
}

/// The class and the `quote` of a value, so one assertion covers both.
fn typed(sql: &str) -> (String, String) {
    (
        one(&format!("SELECT typeof({sql})")).to_string(),
        one(&format!("SELECT quote({sql})")).to_string(),
    )
}

// -- CAST to text is a conversion ---------------------------------------

#[test]
fn cast_blob_to_text_changes_the_class_not_the_bytes() {
    // sqlite3: typeof(CAST(x'414243' AS TEXT)) is 'text' and quote of it is
    // 'ABC'. The cast is a conversion, so the value stops being a blob even
    // though the bytes are the same ones.
    assert_eq!(typed("CAST(x'414243' AS TEXT)"), ("text".into(), "'ABC'".into()));
    // A blob that is not valid UTF-8 is still converted, and the bytes are
    // kept rather than transcoded: hex(CAST(x'FF' AS TEXT)) is FF and
    // length(CAST(x'FF41' AS TEXT)) is 2.
    assert_eq!(one("SELECT hex(CAST(x'FF' AS TEXT))").to_string(), "FF");
    assert_eq!(one("SELECT typeof(CAST(x'FF' AS TEXT))").to_string(), "text");
    assert_eq!(one("SELECT length(CAST(x'FF41' AS TEXT))").to_string(), "2");
    // The three pairings the corpus checks, byte for byte.
    assert_eq!(one("SELECT hex(CAST(x'C3A9' AS TEXT))").to_string(), "C3A9");
    assert_eq!(one("SELECT length(CAST(x'C3A9' AS TEXT))").to_string(), "1");
    assert_eq!(one("SELECT length(CAST(x'E697A5' AS TEXT))").to_string(), "1");
    assert_eq!(one("SELECT length(CAST(x'68C3A96C6C6F' AS TEXT))").to_string(), "5");
}

#[test]
fn cast_to_text_is_the_identity_on_text() {
    // A value that is already text is not rebuilt, so multi-byte characters
    // keep their length and a cast to text of an integer still spells it.
    assert_eq!(one("SELECT length(CAST('abc' AS TEXT))").to_string(), "3");
    assert_eq!(one("SELECT hex(CAST(CAST(x'FF' AS TEXT) AS TEXT))").to_string(), "FF");
    assert_eq!(typed("CAST(123 AS TEXT)"), ("text".into(), "'123'".into()));
    assert_eq!(typed("CAST(1.5 AS TEXT)"), ("text".into(), "'1.5'".into()));
    // NULL stays NULL, which is the one thing a cast does not convert.
    assert_eq!(one("SELECT typeof(CAST(NULL AS TEXT))").to_string(), "null");
}

#[test]
fn cast_to_blob_is_a_conversion_too_so_the_class_becomes_blob() {
    // CAST is a conversion in both directions, so `AS BLOB` produces a blob
    // whatever it went in as. Measured on sqlite3 3.53.4:
    // `typeof(CAST(123 AS BLOB))` is 'blob' and not 'integer', and
    // `typeof(CAST('abc' AS BLOB))` is 'blob' and not 'text'. The bytes are
    // the value rendered the way it would have been printed.
    assert_eq!(typed("CAST(123 AS BLOB)"), ("blob".into(), "X'313233'".into()));
    assert_eq!(typed("CAST(1.5 AS BLOB)"), ("blob".into(), "X'312E35'".into()));
    assert_eq!(typed("CAST('abc' AS BLOB)"), ("blob".into(), "X'616263'".into()));
    // Bytes survive the trip untranscoded, including a byte that is not valid
    // UTF-8: the one byte FF is still the one byte FF, so
    // `hex(CAST(CAST(x'FF' AS TEXT) AS BLOB))` is FF.
    assert_eq!(one("SELECT hex(CAST(CAST(x'FF' AS TEXT) AS BLOB))").to_string(), "FF");
    assert_eq!(one("SELECT hex(CAST(x'0041' AS BLOB))").to_string(), "0041");
    // A concatenation that carried text of its own still comes out a blob, and
    // the text bytes are in it verbatim rather than transcoded.
    assert_eq!(
        one("SELECT hex(CAST('a'||CAST(x'FF' AS TEXT)||'b' AS BLOB))").to_string(),
        "61FF62"
    );
    // NULL is the one thing no cast converts, in either direction.
    assert_eq!(one("SELECT typeof(CAST(NULL AS BLOB))").to_string(), "null");
    // A blob that is already a blob is unchanged, and stays equal to itself.
    assert_eq!(one("SELECT typeof(CAST(x'414243' AS BLOB))").to_string(), "blob");
    assert_eq!(one("SELECT CAST(x'414243' AS BLOB) = x'414243'").to_string(), "1");
}

#[test]
fn a_text_that_is_not_utf8_still_compares_and_sorts_by_its_bytes() {
    // The class is text, so it sorts after every number and before a blob,
    // and two values of it compare by bytes. `CAST(x'FF41' AS TEXT)` is the
    // same text as `CAST(x'FF42' AS TEXT)` with a larger second byte.
    assert_eq!(one("SELECT CAST(x'FF41' AS TEXT) < CAST(x'FF42' AS TEXT)").to_string(), "1");
    assert_eq!(one("SELECT CAST(x'FF41' AS TEXT) = CAST(x'FF41' AS TEXT)").to_string(), "1");
    assert_eq!(one("SELECT CAST(x'FF41' AS TEXT) < 999").to_string(), "0");
    assert_eq!(one("SELECT CAST(x'FF41' AS TEXT) < x'FF41'").to_string(), "1");
}

// -- functions over such text -------------------------------------------

#[test]
fn length_counts_characters_on_text_and_bytes_on_a_blob() {
    // The distinction is the whole point: the five bytes of "héllo" are five
    // bytes and five characters, the three bytes of 日 are one character, and
    // the four bytes of U+10FFFF are one. A blob is counted in bytes whatever
    // those bytes spell.
    assert_eq!(one("SELECT length(CAST(x'68C3A96C6C6F' AS TEXT))").to_string(), "5");
    assert_eq!(one("SELECT length(CAST(x'E697A5' AS TEXT))").to_string(), "1");
    assert_eq!(one("SELECT length(CAST(x'F09F9180' AS TEXT))").to_string(), "1");
    assert_eq!(one("SELECT length(CAST(x'FF41' AS TEXT))").to_string(), "2");
    assert_eq!(one("SELECT length(CAST(x'00' AS TEXT))").to_string(), "0");
    // A blob is its own byte count and is not read as text first.
    assert_eq!(one("SELECT length(x'616263')").to_string(), "3");
    assert_eq!(one("SELECT length(x'E697A5E69CACE8AA9E')").to_string(), "9");
    assert_eq!(one("SELECT length(x'00')").to_string(), "1");
}

#[test]
fn a_nul_ends_the_text_length_counts() {
    // sqlite3: length('a'||char(0)||'b') is 1, because a NUL is where a C
    // string ends. The bytes after it are still in the value -- hex of the
    // concatenation is 610062 -- so it is the count and not the storage that
    // stops.
    assert_eq!(one("SELECT length('a'||char(0)||'b')").to_string(), "1");
    assert_eq!(one("SELECT hex('a'||char(0)||'b')").to_string(), "610062");
    assert_eq!(one("SELECT length(char(0)||'ab')").to_string(), "0");
    assert_eq!(one("SELECT length('ab'||char(0))").to_string(), "2");
}

#[test]
fn the_string_functions_read_a_blob_as_its_bytes() {
    // A function handed a blob reads the bytes the blob holds, not the
    // `x'..'` literal it would print as.
    assert_eq!(typed("trim(x'616263')"), ("text".into(), "'abc'".into()));
    assert_eq!(
        typed("replace(x'616263',x'62',x'58')"),
        ("text".into(), "'aXc'".into())
    );
    assert_eq!(one("SELECT instr(x'616263',x'62')").to_string(), "2");
    assert_eq!(typed("substr(x'616263',2,1)"), ("blob".into(), "X'62'".into()));
    // And the same after a cast, where the answer is text rather than a blob
    // only if the value is a blob to begin with.
    assert_eq!(typed("trim(CAST(x'2061626320' AS TEXT))"), ("text".into(), "'abc'".into()));
    assert_eq!(one("SELECT instr(CAST(x'414243' AS TEXT),'B')").to_string(), "2");
    assert_eq!(one("SELECT hex(lower(CAST(x'414243' AS TEXT)))").to_string(), "616263");
}

#[test]
fn instr_matches_a_whole_character_but_may_end_inside_one() {
    // `instr` searches the two operands' bytes, and a match may not begin part
    // way through a character. sqlite3 measures both directions, and a lossy
    // decode gets both wrong, so they are pinned here byte for byte.
    //
    // The two bytes C3 A9 are the one character `é`, and they are found as a
    // unit at position 2 -- while the single byte A9 inside that character is
    // not text of its own and is not found at all.
    assert_eq!(
        one("SELECT instr(CAST(x'61C3A9' AS TEXT),CAST(x'C3A9' AS TEXT))").to_string(),
        "2"
    );
    assert_eq!(
        one("SELECT instr(CAST(x'61C3A9' AS TEXT),CAST(x'A9' AS TEXT))").to_string(),
        "0"
    );
    // A match may still *end* part way through a character: BC reaches into the
    // `é` of the haystack, and A5 reaches into the 日 of it.
    assert_eq!(
        one("SELECT instr(CAST(x'FF414243' AS TEXT),CAST(x'4243' AS TEXT))").to_string(),
        "3"
    );
    assert_eq!(
        one("SELECT instr(CAST(x'E697A5' AS TEXT),CAST(x'A5' AS TEXT))").to_string(),
        "0"
    );
    // A byte that is not valid UTF-8 is a character of its own, and is not the
    // three bytes EF BF BD: a lossy decode rewrites the haystack's FF as
    // U+FFFD and reports a match at 1, where sqlite3 reports none.
    assert_eq!(
        one("SELECT instr(CAST(x'FF' AS TEXT),CAST(x'EFBFBD' AS TEXT))").to_string(),
        "0"
    );
    assert_eq!(one("SELECT instr(CAST(x'FF' AS TEXT),CAST(x'FF' AS TEXT))").to_string(), "1");
    assert_eq!(one("SELECT instr(CAST(x'FF414243' AS TEXT),'A')").to_string(), "2");
    assert_eq!(one("SELECT instr(CAST(x'FF414243' AS TEXT),'ABC')").to_string(), "2");
}

#[test]
fn instr_searches_a_blob_haystack_as_bytes_and_answers_a_byte_offset() {
    // The class of the *haystack* is what selects the search, and a blob is a
    // byte string: the match may begin at any offset at all, and the answer
    // counts bytes rather than characters. Both halves of that are measured,
    // because the character rule answers the opposite on each.
    //
    // A9 is the last byte of `é`, so it is not text of its own and the text
    // haystack does not contain it. In the blob it is byte 2, and the two
    // bytes before it are the *one* character `é` -- which is why the answer
    // is 3 and not 2.
    assert_eq!(one("SELECT instr(x'61C3A9',x'A9')").to_string(), "3");
    assert_eq!(
        one("SELECT instr(CAST(x'61C3A9' AS TEXT),x'A9')").to_string(),
        "0"
    );
    // A9 is byte 2 of `日` here too, where the byte before it is 97.
    assert_eq!(one("SELECT instr(x'C3A961',x'A9')").to_string(), "2");
    assert_eq!(one("SELECT instr(x'E697A5A5',x'A5')").to_string(), "3");
    // The answer is a byte offset and not a character position: 61 is byte 3
    // of `日a`, and character 2, so the two disagree by exactly one.
    assert_eq!(one("SELECT instr(x'E697A561',x'61')").to_string(), "4");
    assert_eq!(one("SELECT instr(CAST(x'E697A561' AS TEXT),x'61')").to_string(), "2");
    // The *whole* character is still found in a blob, at its own position, so
    // this one is the same answer on either class.
    assert_eq!(one("SELECT instr(x'61E697A561',x'E697A5')").to_string(), "2");
    assert_eq!(one("SELECT instr(x'61E697A561',x'61')").to_string(), "1");
    // The two operands' classes decide this together, not separately: two blobs
    // are compared as bytes, and if *either* one is text the pair is compared
    // as text. The haystack here is 61 E6 97 A5 61 -- the three characters
    // `a`, `日`, `a` over five bytes -- and the needles sit inside the `日`.
    //
    // Two blobs: a match anywhere, counted in bytes.
    assert_eq!(one("SELECT instr(x'61E697A561',x'A5')").to_string(), "4");
    assert_eq!(one("SELECT instr(x'61E697A561',x'97A5')").to_string(), "3");
    assert_eq!(one("SELECT instr(x'61E697A561',x'E697A5')").to_string(), "2");
    // A text needle over the same blob haystack: the pair is compared as text,
    // so the needle may not begin inside `日` and the answer counts characters.
    assert_eq!(
        one("SELECT instr(x'61E697A561',CAST(x'A5' AS TEXT))").to_string(),
        "0"
    );
    assert_eq!(
        one("SELECT instr(x'61E697A561',CAST(x'97A5' AS TEXT))").to_string(),
        "0"
    );
    // The other 61 is a character start, so it matches -- and here the byte
    // count and the character count are the same, which is why this pair does
    // not separate the two rules on its own.
    assert_eq!(
        one("SELECT instr(x'61E697A561',CAST(x'61' AS TEXT))").to_string(),
        "1"
    );
    assert_eq!(one("SELECT instr(x'61E697A561',x'61')").to_string(), "1");
    // A blob haystack whose match sits at byte 3 of `日a` is where a byte
    // count and a character count part company: 4 against 2. The text needle
    // is what selects the character answer here.
    assert_eq!(
        one("SELECT instr(x'E697A561',CAST(x'61' AS TEXT))").to_string(),
        "2"
    );
    assert_eq!(one("SELECT instr(x'E697A561',x'61')").to_string(), "4");
    // A text haystack with a blob needle is compared as text too, so the
    // needle is not allowed to begin inside `é`.
    assert_eq!(
        one("SELECT instr(CAST(x'61C3A9' AS TEXT),x'A9')").to_string(),
        "0"
    );
    assert_eq!(one("SELECT instr(CAST(x'61C3A9' AS TEXT),x'61')").to_string(), "1");
    // A needle that straddles two characters is not in a blob either, since a
    // blob is never segmented.
    assert_eq!(one("SELECT instr(x'61C3A9',x'61A9')").to_string(), "0");
    // An empty needle is 1 rather than a byte offset, on both classes.
    assert_eq!(one("SELECT instr(x'61C3A9',x'')").to_string(), "1");
    assert_eq!(one("SELECT instr(CAST(x'61C3A9' AS TEXT),x'')").to_string(), "1");
    // A blob's NUL is an ordinary byte to search for, which is what makes this
    // the same answer `instr('a'||char(0)||'b',char(0))` gives.
    assert_eq!(one("SELECT instr(x'610062',x'00')").to_string(), "2");
    assert_eq!(one("SELECT instr('a'||char(0)||'b',char(0))").to_string(), "2");
}

#[test]
fn lower_and_upper_fold_only_ascii_of_a_cast_text() {
    // `lower` reaches the same implementation as `upper`, and both leave a
    // byte at or above 0x80 exactly as it was: the C3 A9 of é is not mapped.
    assert_eq!(one("SELECT hex(lower(CAST(x'48C3A94C4C4F' AS TEXT)))").to_string(), "68C3A96C6C6F");
    assert_eq!(one("SELECT hex(upper(CAST(x'68C3A96C6C6F' AS TEXT)))").to_string(), "48C3A94C4C4F");
    assert_eq!(one("SELECT hex(lower(CAST(x'48C383894C4C4F' AS TEXT)))").to_string(), "68C383896C6C6F");
    // A byte outside ASCII is not a character to either function, so `lower`
    // answers 123 for the integer and the two classes agree.
    assert_eq!(typed("lower(123)"), ("text".into(), "'123'".into()));
    assert_eq!(typed("lower(x'414243')"), ("text".into(), "'abc'".into()));
    assert_eq!(typed("upper(x'414243')"), ("text".into(), "'ABC'".into()));
}

#[test]
fn quote_of_a_cast_text_quotes_it_as_text() {
    // The cast made it text, so `quote` uses the text rules and doubles the
    // quote rather than switching to the blob form.
    assert_eq!(one("SELECT quote(CAST(x'414243' AS TEXT))").to_string(), "'ABC'");
    assert_eq!(one("SELECT quote(CAST(x'616263' AS TEXT))").to_string(), "'abc'");
    assert_eq!(one("SELECT quote(CAST(x'27' AS TEXT))").to_string(), "''''");
    assert_eq!(one("SELECT quote(x'414243')").to_string(), "X'414243'");
}

#[test]
fn a_number_read_out_of_a_blob_or_text_is_the_whole_numeric_prefix() {
    // x'31' is "1", so x'31'+x'32' is 3 and not 0; and a prefix that runs into
    // junk still counts, so '1abc' is 1. `abs` answers a real, because every
    // class other than an integer is read as a number first.
    assert_eq!(one("SELECT x'31'+x'32'").to_string(), "3");
    assert_eq!(one("SELECT x'31'+1").to_string(), "2");
    assert_eq!(typed("abs(x'2D33')"), ("real".into(), "3.0".into()));
    assert_eq!(typed("round(x'332E35')"), ("real".into(), "4.0".into()));
    assert_eq!(typed("abs(x'FF')"), ("real".into(), "0.0".into()));
    assert_eq!(typed("abs(CAST(x'2D33' AS TEXT))"), ("real".into(), "3.0".into()));
    assert_eq!(typed("round(CAST(x'332E35' AS TEXT))"), ("real".into(), "4.0".into()));
    assert_eq!(one("SELECT '1abc'+0").to_string(), "1");
}

// -- the bytes survive a round trip through a record -------------------

#[test]
fn a_cast_text_is_stored_and_read_back_as_text_with_its_bytes() {
    // The record format stores text by bytes and length, so a value that is
    // text and not UTF-8 reads back as the same text rather than as the blob
    // its bytes would otherwise spell.
    let mut c = Connection::open_memory().expect("open");
    c.execute_script("CREATE TABLE t(v)").expect("create");
    c.execute_script("INSERT INTO t VALUES(CAST(x'FF41' AS TEXT))")
        .expect("insert");
    let out = c.execute_script("SELECT typeof(v), hex(v), length(v) FROM t").expect("select");
    let crate::connection::Outcome::Query { rows, .. } = out.into_iter().next().unwrap() else {
        panic!("expected a query")
    };
    let got: Vec<String> = rows[0].values.iter().map(|v| v.to_string()).collect();
    assert_eq!(got, vec!["text".to_string(), "FF41".to_string(), "2".to_string()]);
}

#[test]
fn replace_and_trim_keep_a_byte_that_is_not_a_character() {
    // Both read their operands as bytes, so a byte that is not valid UTF-8 is
    // an ordinary byte on both sides rather than something a `String` had to
    // replace with U+FFFD before the function ever ran.
    assert_eq!(one("SELECT hex(replace(x'414243', x'42', x'FF'))").to_string(), "41FF43");
    assert_eq!(one("SELECT hex(trim(x'20FF0041'))").to_string(), "FF0041");
    assert_eq!(one("SELECT hex(trim(x'FF00FE41'))").to_string(), "FF00FE41");
    assert_eq!(
        one("SELECT hex(trim(x'4142FF43', x'4142'))").to_string(),
        "FF43"
    );
    assert_eq!(one("SELECT hex(ltrim(x'20FF2041'))").to_string(), "FF2041");
    assert_eq!(one("SELECT hex(rtrim(x'4120FF20'))").to_string(), "4120FF");
    // A NULL argument still answers NULL.
    assert_eq!(one("SELECT typeof(replace(x'414243', NULL, 'z'))").to_string(), "null");
    assert_eq!(one("SELECT typeof(trim(NULL))").to_string(), "null");
}

#[test]
fn substr_slices_a_blob_by_bytes_and_text_by_characters() {
    // The two classes are sliced differently and this is the whole point of
    // the split. A blob window may cut a multi-byte character in half, because
    // the blob is a byte string: `substr(x'61C3A9', 1, 2)` is the two bytes
    // 61 C3 and `substr(x'61C3A9', 2, 1)` is the lone lead byte C3.
    assert_eq!(one("SELECT hex(substr(x'61C3A9',1,2))").to_string(), "61C3");
    assert_eq!(one("SELECT hex(substr(x'61C3A9',2,1))").to_string(), "C3");
    assert_eq!(one("SELECT hex(substr(x'61C3A9',3,1))").to_string(), "A9");
    // The same window over the CAST text is the whole character, because text
    // is a character string.
    assert_eq!(
        one("SELECT hex(substr(CAST(x'61C3A9' AS TEXT),1,2))").to_string(),
        "61C3A9"
    );
    // The position arithmetic is the same shape over bytes: one-based, a
    // negative start counts back from the end, and out of range is empty.
    assert_eq!(one("SELECT hex(substr(x'61C3A9',-1,1))").to_string(), "A9");
    assert_eq!(one("SELECT hex(substr(x'61C3A9',-2,2))").to_string(), "C3A9");
    assert_eq!(one("SELECT hex(substr(x'61C3A9',1,-1))").to_string(), "");
    assert_eq!(one("SELECT hex(substr(x'616263',10,2))").to_string(), "");
    // The class is preserved either way.
    assert_eq!(one("SELECT typeof(substr(x'61C3A9',2,1))").to_string(), "blob");
}

#[test]
fn a_blobs_nul_is_an_ordinary_byte_while_a_texts_ends_it() {
    // The same split as the slicing above, applied to the NUL. A blob keeps
    // its NULs -- `substr(x'610062', 1, 2)` is the two bytes 61 00 -- while a
    // CAST text stops at one, so `substr(CAST(x'610062' AS TEXT), 2)` is the
    // empty string. `length` splits the same way.
    assert_eq!(one("SELECT hex(substr(x'610062',1,2))").to_string(), "6100");
    assert_eq!(one("SELECT hex(substr(x'610062',2))").to_string(), "0062");
    assert_eq!(one("SELECT hex(length(x'610062'))").to_string(), "33");
    assert_eq!(one("SELECT hex(substr(CAST(x'610062' AS TEXT),2))").to_string(), "");
    // `length` splits the same way, so the CAST text is one character long
    // and the blob is three bytes.
    assert_eq!(one("SELECT hex(length(CAST(x'610062' AS TEXT)))").to_string(), "31");
    assert_eq!(one("SELECT hex(length('a'||char(0)||'b'))").to_string(), "31");
}

#[test]
fn group_concat_joins_blob_elements_and_separators_by_their_bytes() {
    // Both the elements and the separator are joined as bytes. Before this,
    // the element was transcoded on the way into the accumulator and the
    // separator was rendered with `to_string`, which put the literal `x'FF'`
    // between the elements where sqlite3 puts one byte.
    let mut c = Connection::open_memory().expect("open");
    c.execute_script("CREATE TABLE t(v)").expect("create");
    c.execute_script("INSERT INTO t VALUES(x'FF'),(x'41')").expect("insert");
    let out = c
        .execute_script("SELECT hex(group_concat(v)), hex(group_concat(v, x'C3')) FROM t")
        .expect("select");
    let crate::connection::Outcome::Query { rows, .. } = out.into_iter().next().unwrap() else {
        panic!("expected a query")
    };
    let got: Vec<String> = rows[0].values.iter().map(|v| v.to_string()).collect();
    assert_eq!(got, vec!["FF2C41".to_string(), "FFC341".to_string()]);
}
