//! Result codes and the error type used throughout the engine.
//!
//! Values match SQLite's public `SQLITE_*` constants so that the C-compatible
//! API layer can hand them out without translation.

use std::fmt;

/// A SQLite primary result code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(i32)]
pub enum ResultCode {
    Ok = 0,
    Error = 1,
    Internal = 2,
    Perm = 3,
    Abort = 4,
    Busy = 5,
    Locked = 6,
    NoMem = 7,
    ReadOnly = 8,
    Interrupt = 9,
    IoErr = 10,
    Corrupt = 11,
    NotFound = 12,
    Full = 13,
    CantOpen = 14,
    Protocol = 15,
    Empty = 16,
    Schema = 17,
    TooBig = 18,
    Constraint = 19,
    Mismatch = 20,
    Misuse = 21,
    NoLfs = 22,
    Auth = 23,
    Format = 24,
    Range = 25,
    NotADb = 26,
    Notice = 27,
    Warning = 28,
    Row = 100,
    Done = 101,
}

impl ResultCode {
    /// Maps a numeric SQLite result code to its variant, masking off the
    /// extended-code bits so that an extended code yields its primary code.
    /// Codes this engine does not define fall back to `Error`.
    pub fn from_i32(v: i32) -> ResultCode {
        use ResultCode::*;
        match v & 0xff {
            0 => Ok,
            2 => Internal,
            3 => Perm,
            4 => Abort,
            5 => Busy,
            6 => Locked,
            7 => NoMem,
            8 => ReadOnly,
            9 => Interrupt,
            10 => IoErr,
            11 => Corrupt,
            12 => NotFound,
            13 => Full,
            14 => CantOpen,
            15 => Protocol,
            16 => Empty,
            17 => Schema,
            18 => TooBig,
            19 => Constraint,
            20 => Mismatch,
            21 => Misuse,
            22 => NoLfs,
            23 => Auth,
            24 => Format,
            25 => Range,
            26 => NotADb,
            27 => Notice,
            28 => Warning,
            100 => Row,
            101 => Done,
            _ => Error,
        }
    }

    /// The primary code with its extended bits masked off.
    pub fn primary(self) -> ResultCode {
        ResultCode::from_i32(self as i32 & 0xff)
    }
}

impl fmt::Display for ResultCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl ResultCode {
    /// The unprefixed symbolic name, e.g. `CONSTRAINT`.
    pub fn name(self) -> &'static str {
        use ResultCode::*;
        match self {
            Ok => "OK",
            Error => "ERROR",
            Internal => "INTERNAL",
            Perm => "PERM",
            Abort => "ABORT",
            Busy => "BUSY",
            Locked => "LOCKED",
            NoMem => "NOMEM",
            ReadOnly => "READONLY",
            Interrupt => "INTERRUPT",
            IoErr => "IOERR",
            Corrupt => "CORRUPT",
            NotFound => "NOTFOUND",
            Full => "FULL",
            CantOpen => "CANTOPEN",
            Protocol => "PROTOCOL",
            Empty => "EMPTY",
            Schema => "SCHEMA",
            TooBig => "TOOBIG",
            Constraint => "CONSTRAINT",
            Mismatch => "MISMATCH",
            Misuse => "MISUSE",
            NoLfs => "NOLFS",
            Auth => "AUTH",
            Format => "FORMAT",
            Range => "RANGE",
            NotADb => "NOTADB",
            Notice => "NOTICE",
            Warning => "WARNING",
            Row => "ROW",
            Done => "DONE",
        }
    }
}

/// The engine's error type: a result code plus a message in SQLite's style.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    pub code: ResultCode,
    /// Extended code, when the failure came from one of the constraint families.
    pub extended: i32,
    pub message: String,
}

impl Error {
    pub fn new(code: ResultCode, message: impl Into<String>) -> Error {
        Error {
            code,
            extended: 0,
            message: message.into(),
        }
    }

    pub fn with_extended(mut self, extended: i32) -> Error {
        self.extended = extended;
        self
    }

    /// The value returned by `sqlite3_errcode`: primary code only.
    pub fn code(&self) -> i32 {
        self.code as i32
    }

    /// The value returned by `sqlite3_extended_errcode`.
    pub fn extended_code(&self) -> i32 {
        if self.extended != 0 {
            self.extended
        } else {
            self.code as i32
        }
    }

    // Constructors for the failures raised most often. Each keeps the message
    // wording aligned with SQLite, because the official test suite matches on it.

    pub fn misuse(msg: impl Into<String>) -> Error {
        Error::new(ResultCode::Misuse, msg)
    }

    pub fn range(msg: impl Into<String>) -> Error {
        Error::new(ResultCode::Range, msg)
    }

    pub fn corrupt(msg: impl Into<String>) -> Error {
        Error::new(ResultCode::Corrupt, msg)
    }

    pub fn not_a_db() -> Error {
        Error::new(ResultCode::NotADb, "file is not a database")
    }

    pub fn no_mem() -> Error {
        Error::new(ResultCode::NoMem, "out of memory")
    }

    pub fn full() -> Error {
        Error::new(ResultCode::Full, "database or disk is full")
    }

    pub fn read_only() -> Error {
        Error::new(ResultCode::ReadOnly, "attempt to write a readonly database")
    }

    pub fn io(msg: impl Into<String>) -> Error {
        Error::new(ResultCode::IoErr, msg)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code.name(), self.message)
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Error {
        use std::io::ErrorKind::*;
        let code = match e.kind() {
            NotFound => ResultCode::CantOpen,
            PermissionDenied => ResultCode::Perm,
            UnexpectedEof => ResultCode::Corrupt,
            _ => ResultCode::IoErr,
        };
        Error::new(code, e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primary_masks_extended_bits() {
        // 19 | (5 << 8) is an extended constraint code; the primary stays 19.
        assert_eq!(ResultCode::from_i32(19 | (5 << 8)), ResultCode::Constraint);
        assert_eq!(ResultCode::Constraint.primary(), ResultCode::Constraint);
    }

    #[test]
    fn extended_code_defaults_to_primary() {
        let e = Error::new(ResultCode::Constraint, "UNIQUE constraint failed: t.a");
        assert_eq!(e.code(), 19);
        assert_eq!(e.extended_code(), 19);
        let e = e.with_extended(2067);
        assert_eq!(e.code(), 19);
        assert_eq!(e.extended_code(), 2067);
    }

    #[test]
    fn io_errors_map_to_sensible_codes() {
        let e: Error = std::io::Error::new(std::io::ErrorKind::NotFound, "nope").into();
        assert_eq!(e.code(), ResultCode::CantOpen as i32);
    }
}
