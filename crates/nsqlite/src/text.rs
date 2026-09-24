//! Text encodings a database file can declare. Only the two that SQLite
//! writes by default are modelled; the header field exists for compatibility
//! with files produced elsewhere.

/// The value stored in byte 56 of the database header.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Encoding {
    Utf8 = 1,
    Utf16Le = 2,
    Utf16Be = 3,
    #[default]
    Unknown = 0,
}

impl Encoding {
    pub fn from_i32(v: i32) -> Encoding {
        match v {
            1 => Encoding::Utf8,
            2 => Encoding::Utf16Le,
            3 => Encoding::Utf16Be,
            _ => Encoding::Unknown,
        }
    }

    /// The number of bytes per character, which page payload accounting needs.
    pub fn bytes_per_char(self) -> usize {
        match self {
            Encoding::Utf16Le | Encoding::Utf16Be => 2,
            _ => 1,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Encoding::Utf8 => "UTF-8",
            Encoding::Utf16Le => "UTF-16le",
            Encoding::Utf16Be => "UTF-16be",
            Encoding::Unknown => "unknown",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn width_follows_the_encoding() {
        assert_eq!(Encoding::Utf8.bytes_per_char(), 1);
        assert_eq!(Encoding::Utf16Le.bytes_per_char(), 2);
        assert_eq!(Encoding::from_i32(0), Encoding::Unknown);
    }
}
