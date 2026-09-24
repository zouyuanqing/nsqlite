//! SQLite's variable-length integer encoding.
//!
//! A varint is a big-endian base-128 sequence. Each byte carries seven payload
//! bits; the high bit signals that another byte follows. The encoding is 1 to 9
//! bytes: the first eight bytes carry seven bits each (56 bits total) and the
//! ninth, present only when any of the top eight bits of the value is set,
//! carries a full eight bits.

/// The largest number of bytes a varint can occupy.
pub const MAX: usize = 9;

/// Encodes `v` into the start of `buf`, returning the number of bytes written.
///
/// # Panics
///
/// Panics if `buf` is shorter than the encoded length.
#[inline]
pub fn put(buf: &mut [u8], v: u64) -> usize {
    // The first eight bytes carry seven bits each (56 bits of payload). The
    // ninth byte is only needed when the value does not fit in 56 bits, and it
    // contributes all eight of its bits, giving room for the full u64.
    if v > 0x00ff_ffff_ffff_ffff {
        assert!(buf.len() >= 9, "varint buffer too small");
        buf[8] = v as u8;
        let mut x = v >> 8;
        let mut i = 8;
        while i > 0 {
            i -= 1;
            buf[i] = (x & 0x7f) as u8 | 0x80;
            x >>= 7;
        }
        9
    } else {
        let n = len_for(v);
        assert!(buf.len() >= n, "varint buffer too small");
        let mut x = v;
        let mut i = n;
        while i > 0 {
            i -= 1;
            let byte = (x & 0x7f) as u8;
            x >>= 7;
            buf[i] = if i == n - 1 { byte } else { byte | 0x80 };
        }
        n
    }
}

/// Returns the number of bytes [`put`] would write for `v`.
#[inline]
pub const fn len_for(v: u64) -> usize {
    // Derived from the bit width rather than tabulated, because the boundaries
    // are easy to get wrong by hand: each byte carries seven bits, so a value
    // needs ceil(bits / 7) bytes, except that anything wider than 56 bits takes
    // the nine-byte form, whose last byte carries all eight of its bits.
    let bits = 64 - v.leading_zeros() as usize;
    if bits > 56 {
        9
    } else {
        // Round up, and keep at least one byte so zero encodes as 0x00.
        let n = bits.div_ceil(7);
        if bits == 0 {
            1
        } else {
            n
        }
    }
}

/// Decodes the varint at the start of `buf`, returning the value and its length.
///
/// # Panics
///
/// Panics if `buf` is too short to hold a complete varint.
#[inline]
pub fn get(buf: &[u8]) -> (u64, usize) {
    match get_checked(buf) {
        Some(pair) => pair,
        None => panic!("truncated varint"),
    }
}

/// Decodes the varint at the start of `buf`, or returns `None` if it is
/// truncated. The second element of the pair is the number of bytes consumed.
#[inline]
pub fn get_checked(buf: &[u8]) -> Option<(u64, usize)> {
    let mut v: u64 = 0;
    for i in 0..8 {
        let b = *buf.get(i)?;
        v = (v << 7) | (b & 0x7f) as u64;
        if b & 0x80 == 0 {
            return Some((v, i + 1));
        }
    }
    // Ninth byte: the high bit is part of the payload, not a continuation flag.
    Some(((v << 8) | *buf.get(8)? as u64, 9))
}

/// Decodes a varint that is known to fit in 32 bits, as the record header does.
///
/// SQLite stores record header fields as at most nine-byte varints, but values
/// below 2^32 are what actually occur; the two-byte truncation this permits is
/// how the reference implementation reads them.
#[inline]
pub fn get32(buf: &[u8]) -> (u32, usize) {
    let (v, n) = get(buf);
    (v as u32, n)
}

/// Decodes a varint into a `usize`, for rowids and payload lengths.
#[inline]
pub fn get_usize(buf: &[u8]) -> (usize, usize) {
    let (v, n) = get(buf);
    (v as usize, n)
}

/// Appends the varint encoding of `v` to `out`.
#[inline]
pub fn append(out: &mut Vec<u8>, v: u64) {
    let mut tmp = [0u8; MAX];
    let n = put(&mut tmp, v);
    out.extend_from_slice(&tmp[..n]);
}

/// Advances past the varint at the start of `buf` and returns the remainder.
#[inline]
pub fn skip(buf: &[u8]) -> (u64, &[u8]) {
    let (v, n) = get(buf);
    (v, &buf[n..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_boundaries() {
        let cases: &[u64] = &[
            0,
            1,
            0x7f,
            0x80,
            0x3fff,
            0x4000,
            0x1f_ffff,
            0x20_0000,
            0xfff_ffff,
            0x1000_0000,
            0x7fff_ffff,
            0x8000_0000,
            0x3ff_ffff_ffff,
            0x4000_0000_0000,
            0x1_ffff_ffff_ffff,
            0x2000_0000_0000,
            0x0fff_ffff_ffff_ffff,
            0x1000_0000_0000_0000,
            0xffff_ffff_ffff_ffff,
            u64::MAX,
        ];
        for &v in cases {
            let mut buf = [0u8; MAX];
            let n = put(&mut buf, v);
            assert_eq!(n, len_for(v), "length mismatch for {v:#x}");
            assert_eq!(get(&buf[..n]), (v, n), "round trip failed for {v:#x}");
        }
    }

    #[test]
    fn short_buffers_are_rejected() {
        assert!(get_checked(&[]).is_none());
        assert!(get_checked(&[0x80]).is_none());
        assert!(get_checked(&[0x80, 0x80]).is_none());
        assert!(get_checked(&[0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80]).is_none());
        // Eight continuation bytes with empty payloads, then a payload byte:
        // the ninth byte is the low byte, so this encodes plain 1 in nine bytes.
        assert_eq!(
            get_checked(&[0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x01]),
            Some((1, 9))
        );
    }

    #[test]
    fn nine_byte_form_is_used_only_when_needed() {
        let mut buf = [0u8; MAX];
        // Values up to 56 bits fit in eight bytes; bit 56 is the first that does
        // not, and only then is the ninth byte emitted.
        assert_eq!(put(&mut buf, 1u64 << 55), 8);
        assert_eq!(put(&mut buf, (1u64 << 56) - 1), 8);
        assert_eq!(put(&mut buf, 1u64 << 56), 9);
        assert_eq!(put(&mut buf, u64::MAX), 9);
    }

    #[test]
    fn ninth_byte_contributes_all_eight_bits() {
        // The 9-byte form of 1<<56 sets bit 48 of the 56-bit field, which lands
        // in the second seven-bit group, and leaves the trailing byte zero.
        let mut buf = [0u8; MAX];
        let n = put(&mut buf, 1u64 << 56);
        assert_eq!(n, 9);
        assert_eq!(buf[0], 0x80);
        assert_eq!(buf[1], 0xc0, "bit 48 sits in the second seven-bit group");
        assert_eq!(buf[2..8], [0x80; 6]);
        assert_eq!(buf[8], 0x00);
        assert_eq!(get(&buf[..n]), (1u64 << 56, 9));
    }

    #[test]
    fn append_matches_put() {
        let mut out = Vec::new();
        append(&mut out, 300);
        assert_eq!(out.len(), 2);
        assert_eq!(get(&out), (300, 2));
    }
}
