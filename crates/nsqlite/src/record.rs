//! The SQLite record format: a header of varint serial types followed by a
//! body of values, exactly as stored in a table leaf cell or an index leaf.

use super::error::{Error, Result};
use super::value::Value;
use super::varint;

/// The serial type SQLite would use to store `v`, and how many body bytes it
/// occupies. Integers use the narrowest type that holds them, and 0 and 1 get
/// the single-byte types 8 and 9.
pub fn serial_type(v: &Value) -> i64 {
    match v {
        Value::Null => 0,
        Value::Integer(i) => {
            let i = *i;
            if i == 0 {
                8
            } else if i == 1 {
                9
            } else if (-128..=127).contains(&i) {
                1
            } else if (-32_768..=32_767).contains(&i) {
                2
            } else if (-8_388_608..=8_388_607).contains(&i) {
                3
            } else if (-2_147_483_648..=2_147_483_647).contains(&i) {
                4
            } else if (-140_737_488_355_328..=140_737_488_355_327).contains(&i) {
                5
            } else {
                6
            }
        }
        Value::Real(_) => 7,
        Value::Text(s) => 13 + 2 * s.len() as i64,
        Value::Blob(b) => 12 + 2 * b.len() as i64,
    }
}

/// The number of body bytes the serial type `t` occupies.
pub const fn serial_size(t: i64) -> i64 {
    match t {
        0 | 8 | 9 => 0,
        1 => 1,
        2 => 2,
        3 => 3,
        4 => 4,
        5 => 6,
        6 | 7 => 8,
        10 | 11 => -1, // reserved
        t => (t - 12) / 2,
    }
}

/// Decodes the body bytes of a single value described by serial type `t`.
///
/// Text is returned as bytes; the caller decides the database encoding. A NaN
/// real is normalised here so that a value read from a file always compares the
/// same way as one produced in memory.
pub fn decode(t: i64, body: &[u8]) -> Result<Value> {
    if t < 0 {
        return Err(Error::corrupt("record serial type out of range"));
    }
    let size = serial_size(t);
    if size < 0 {
        return Err(Error::corrupt("record serial type is reserved"));
    }
    let need = size as usize;
    if body.len() < need {
        return Err(Error::corrupt(
            "record body is shorter than its header claims",
        ));
    }
    let raw = &body[..need];
    Ok(match t {
        0 => Value::Null,
        8 => Value::Integer(0),
        9 => Value::Integer(1),
        1..=6 => Value::Integer(sign_extend(raw)),
        7 => {
            let mut b = [0u8; 8];
            b.copy_from_slice(raw);
            Value::real(f64::from_be_bytes(b))
        }
        t if t % 2 == 0 => Value::Blob(raw.to_vec()),
        _t => Value::Text(String::from_utf8_lossy(raw).into_owned()),
    })
}

/// Interprets 1-, 2-, 3-, 4- or 6-byte big-endian two's complement.
fn sign_extend(raw: &[u8]) -> i64 {
    match raw.len() {
        1 => raw[0] as i8 as i64,
        2 => i16::from_be_bytes([raw[0], raw[1]]) as i64,
        // A 3-byte field is the low three bytes of a 32-bit two's complement
        // value, so the missing high byte is filled with the sign bit.
        3 => {
            let hi = if raw[0] & 0x80 != 0 { 0xffu8 } else { 0x00 };
            let b = [hi, raw[0], raw[1], raw[2]];
            i32::from_be_bytes(b) as i64
        }
        4 => i32::from_be_bytes([raw[0], raw[1], raw[2], raw[3]]) as i64,
        6 => {
            let hi = if raw[0] & 0x80 != 0 { 0xffu8 } else { 0x00 };
            let b = [hi, hi, raw[0], raw[1], raw[2], raw[3], raw[4], raw[5]];
            i64::from_be_bytes(b)
        }
        8 => {
            let mut b = [0u8; 8];
            b.copy_from_slice(raw);
            i64::from_be_bytes(b)
        }
        _ => unreachable!("caller guarantees a serial size of 1, 2, 3, 4, 6 or 8"),
    }
}

/// A record laid out for writing: the header varints and body bytes, ready to
/// be handed to the b-tree layer.
#[derive(Debug, Clone, Default)]
pub struct EncodedRecord {
    pub bytes: Vec<u8>,
    /// The body length, which is what the cell's payload-size varint counts.
    pub body_len: i64,
    /// Number of columns the caller supplied, including any trailing NULLs that
    /// were dropped. The reader pads back out to this width.
    pub column_count: usize,
}

impl EncodedRecord {
    /// The total encoded length, header plus body.
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }
}

/// Encodes `values` into a record.
///
/// Trailing NULL columns are omitted, as SQLite's writer does, so a row of
/// `(1, NULL)` costs no more on disk than `(1)`. The single exception is an
/// all-NULL row, which still stores one serial type so that it occupies a cell:
/// a table's row count is defined by cells, not by column values.
pub fn encode(values: &[Value]) -> EncodedRecord {
    let column_count = values.len();
    // Drop trailing NULLs, but always keep at least the first column.
    let mut end = values.len();
    while end > 1 && values[end - 1].is_null() {
        end -= 1;
    }
    let kept = &values[..end];

    // A record with no columns has no header at all, which is how SQLite
    // encodes a zero-column row: an absent header size reads back as zero.
    if kept.is_empty() {
        return EncodedRecord {
            bytes: Vec::new(),
            body_len: 0,
            column_count,
        };
    }

    let mut types: Vec<u8> = Vec::new();
    for v in kept {
        varint::append(&mut types, serial_type(v) as u64);
    }
    // The header size varint counts itself, so its own width is not known until
    // the total is computed. Adding a byte can push the total into a longer
    // varint, so iterate until the width is stable; two rounds always suffice.
    let mut size_len = 1;
    let header_size;
    loop {
        let candidate = types.len() + size_len;
        let needed = varint::len_for(candidate as u64);
        if needed <= size_len {
            header_size = candidate;
            break;
        }
        size_len = needed;
    }

    let mut bytes = Vec::with_capacity(header_size);
    let mut size_buf = [0u8; varint::MAX];
    let written = varint::put(&mut size_buf, header_size as u64);
    debug_assert_eq!(
        written, size_len,
        "header size varint width changed under us"
    );
    bytes.extend_from_slice(&size_buf[..written]);
    bytes.extend_from_slice(&types);

    let body_start = bytes.len();
    for v in kept {
        match v {
            Value::Null => {}
            Value::Integer(_) => {
                // The serial type is a zero-length constant for 0 and 1.
                let t = serial_type(v);
                if t == 8 || t == 9 {
                    continue;
                }
                write_integer_body(&mut bytes, t, v.as_i64().unwrap());
            }
            Value::Real(r) => bytes.extend_from_slice(&r.to_be_bytes()),
            Value::Text(s) => bytes.extend_from_slice(s.as_bytes()),
            Value::Blob(b) => bytes.extend_from_slice(b),
        }
    }
    let body_len = (bytes.len() - body_start) as i64;
    EncodedRecord {
        bytes,
        body_len,
        column_count,
    }
}

fn write_integer_body(out: &mut Vec<u8>, t: i64, v: i64) {
    // A 3-byte field is the sign-extended low three bytes of the value, so the
    // high byte is dropped rather than shifted away.
    match t {
        1 => out.push(v as u8),
        2 => out.extend_from_slice(&(v as i16).to_be_bytes()),
        3 => out.extend_from_slice(&v.to_be_bytes()[5..]),
        4 => out.extend_from_slice(&(v as i32).to_be_bytes()),
        5 => out.extend_from_slice(&v.to_be_bytes()[2..]),
        _ => out.extend_from_slice(&v.to_be_bytes()),
    }
}

/// A parsed record, with the column count recovered from the header.
#[derive(Debug, Clone)]
pub struct DecodedRecord {
    pub values: Vec<Value>,
    /// Bytes the body occupied, so a multi-record blob can be walked.
    pub body_len: usize,
}

/// Parses a record from the start of `bytes`.
///
/// `text_encoding` selects how blob-backed text is interpreted; only UTF-8 and
/// UTF-16LE are distinguished here, which is what the file header records.
pub fn decode_record(bytes: &[u8], text_encoding: super::text::Encoding) -> Result<DecodedRecord> {
    if bytes.is_empty() {
        return Ok(DecodedRecord {
            values: Vec::new(),
            body_len: 0,
        });
    }
    let (header_size, n) =
        varint::get_checked(bytes).ok_or_else(|| Error::corrupt("truncated record header"))?;
    let header_size = header_size as usize;
    if header_size < n || header_size > bytes.len() {
        return Err(Error::corrupt("record header size is out of range"));
    }
    let mut types = Vec::new();
    let mut p = n;
    while p < header_size {
        let (t, used) = varint::get_checked(&bytes[p..header_size])
            .ok_or_else(|| Error::corrupt("truncated serial type in record header"))?;
        types.push(t as i64);
        p += used;
    }

    let body = &bytes[header_size..];
    let mut values = Vec::with_capacity(types.len());
    let mut off = 0usize;
    for &t in &types {
        let size = serial_size(t);
        if size < 0 {
            return Err(Error::corrupt(format!(
                "reserved serial type {t} in record"
            )));
        }
        let v = decode(t, &body[off..])?;
        values.push(coerce_text(v, text_encoding));
        off += size as usize;
    }
    Ok(DecodedRecord {
        values,
        body_len: header_size + off,
    })
}

/// Applies the file's declared text encoding to a decoded value.
///
/// The decoder already interprets text payload bytes as UTF-8, so this only
/// documents the seam: a future UTF-16 reader will decode the raw bytes here,
/// and today the encoding affects nothing beyond that.
fn coerce_text(v: Value, _enc: super::text::Encoding) -> Value {
    v
}
#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(vals: &[Value]) -> DecodedRecord {
        let enc = encode(vals);
        decode_record(&enc.bytes, super::super::text::Encoding::Utf8).unwrap()
    }

    #[test]
    fn every_storage_class_round_trips() {
        let vals = vec![
            Value::Null,
            Value::Integer(0),
            Value::Integer(1),
            Value::Integer(-1),
            Value::Integer(i64::MIN),
            Value::Integer(i64::MAX),
            Value::Integer(300),
            Value::real(1.5),
            Value::real(-0.0),
            Value::Text("héllo".into()),
            Value::Blob(vec![0, 1, 2, 255]),
        ];
        let d = round_trip(&vals);
        assert_eq!(d.values.len(), vals.len());
        for (got, want) in d.values.iter().zip(&vals) {
            assert!(got.eq_value(want), "{got:?} != {want:?}");
        }
    }

    #[test]
    fn narrow_integers_use_narrow_serial_types() {
        assert_eq!(serial_type(&Value::Integer(0)), 8);
        assert_eq!(serial_type(&Value::Integer(1)), 9);
        assert_eq!(serial_type(&Value::Integer(127)), 1);
        assert_eq!(serial_type(&Value::Integer(128)), 2);
        assert_eq!(serial_type(&Value::Integer(32_767)), 2);
        assert_eq!(serial_type(&Value::Integer(32_768)), 3);
        assert_eq!(serial_type(&Value::Integer(8_388_607)), 3);
        assert_eq!(serial_type(&Value::Integer(8_388_608)), 4);
        assert_eq!(serial_type(&Value::Integer(140_737_488_355_327)), 5);
        assert_eq!(serial_type(&Value::Integer(140_737_488_355_328)), 6);
    }

    #[test]
    fn sign_extension_is_correct_at_every_width() {
        // Each width is exercised with values it can actually represent: the
        // writer would never emit a narrow type for a wider value, so feeding
        // one here would be testing an impossible encoding.
        let cases: &[(i64, i64, i64)] = &[
            (1, -128, 127),
            (2, -32_768, 32_767),
            (3, -8_388_608, 8_388_607),
            (4, -2_147_483_648, 2_147_483_647),
            (5, -140_737_488_355_328, 140_737_488_355_327),
            (6, i64::MIN, i64::MAX),
        ];
        for &(t, lo, hi) in cases {
            for v in [lo, lo / 2, 0, hi / 2, hi] {
                let mut body = Vec::new();
                write_integer_body(&mut body, t, v);
                assert_eq!(
                    body.len(),
                    serial_size(t) as usize,
                    "t={t}: wrong body width"
                );
                let got = decode(t, &body).unwrap();
                assert_eq!(got, Value::Integer(v), "t={t} v={v}");
            }
        }
    }

    #[test]
    fn trailing_nulls_are_trimmed_but_the_count_is_preserved() {
        let enc = encode(&[Value::Integer(7), Value::Null, Value::Null]);
        // The two NULL columns cost nothing in the body, and the column count
        // survives separately so the reader can pad the row back out.
        let d = decode_record(&enc.bytes, super::super::text::Encoding::Utf8).unwrap();
        assert_eq!(d.values, vec![Value::Integer(7)]);
        assert_eq!(enc.column_count, 3);
    }

    #[test]
    fn an_all_null_row_still_stores_one_column() {
        let enc = encode(&[Value::Null, Value::Null]);
        assert_eq!(enc.bytes.len(), 2, "header size plus one serial type");
        let d = decode_record(&enc.bytes, super::super::text::Encoding::Utf8).unwrap();
        assert_eq!(d.values, vec![Value::Null]);
        assert_eq!(enc.column_count, 2);
    }

    #[test]
    fn an_empty_row_stores_no_bytes_at_all() {
        // A record with no columns has an empty payload: the header size field
        // is absent, and decode reads a zero-length buffer as zero columns.
        let enc = encode(&[]);
        assert!(enc.bytes.is_empty());
        let d = decode_record(&enc.bytes, super::super::text::Encoding::Utf8).unwrap();
        assert!(d.values.is_empty());
        assert_eq!(enc.column_count, 0);
    }

    #[test]
    fn header_size_varint_accounts_for_its_own_width() {
        // Enough columns that the header size needs a two-byte varint: 127
        // one-byte serial types plus the varint itself is the boundary.
        for n in [126usize, 127, 128, 200, 16_000] {
            let vals: Vec<Value> = (0..n).map(|i| Value::Integer(i as i64)).collect();
            let enc = encode(&vals);
            let (size, used) = varint::get(&enc.bytes);
            assert_eq!(
                size as usize,
                used + n,
                "n={n}: header must end after the serial types"
            );
            let d = decode_record(&enc.bytes, super::super::text::Encoding::Utf8).unwrap();
            assert_eq!(d.values.len(), n);
        }
    }

    #[test]
    fn reserved_serial_types_are_rejected() {
        let mut bytes = Vec::new();
        varint::append(&mut bytes, 2);
        varint::append(&mut bytes, 10);
        bytes.extend_from_slice(&[0u8; 8]);
        assert!(decode_record(&bytes, super::super::text::Encoding::Utf8).is_err());
    }

    #[test]
    fn truncated_body_is_reported_as_corruption() {
        // A one-byte integer serial type with an empty body must not read back.
        let mut bytes = Vec::new();
        varint::append(&mut bytes, 2);
        varint::append(&mut bytes, 1);
        assert!(decode_record(&bytes, super::super::text::Encoding::Utf8).is_err());

        // A declared body longer than the available bytes is also rejected.
        let mut bytes = Vec::new();
        varint::append(&mut bytes, 2);
        varint::append(&mut bytes, 6); // eight-byte integer
        bytes.extend_from_slice(&[0u8; 4]);
        assert!(decode_record(&bytes, super::super::text::Encoding::Utf8).is_err());
    }
}
