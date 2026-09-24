//! # nsqlite
//!
//! A from-scratch embedded SQL database engine that reads and writes SQLite's
//! own file format.
//!
//! The crate is built bottom-up in layers, each of which is independently
//! testable:
//!
//! * [`varint`] — SQLite's variable-length integer encoding.
//! * [`record`] — the serial-type record format used inside b-tree cells.
//! * [`page`] — the database header, b-tree page headers, and the payload
//!   overflow thresholds.
//! * [`pager`] — the page cache over a database file.
//! * [`btree`] — reading table b-trees, including overflow chains.
//! * [`text`] — the text encodings a file can declare.
//! * [`value`] / [`error`] — the runtime value and error types.
//!
//! Higher layers (tokenizer, SQL parser, b-tree, pager, VDBE) are built on top
//! of these and live in their own modules as they land.

pub mod btree;
pub mod error;
pub mod page;
pub mod pager;
pub mod record;
pub mod text;
pub mod value;
pub mod varint;

pub use error::{Error, Result, ResultCode};
pub use text::Encoding;
pub use value::{Datatype, Value};

/// The library version, reported the way SQLite reports its own in the header
/// and through `sqlite_version()`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Encodes a `u32` as the SQLITE_VERSION_NUMBER the file header carries.
///
/// SQLite packs the number as `major * 1000000 + minor * 1000 + patch`, so
/// 3.45.1 becomes 3045001.
pub const fn version_number(major: u32, minor: u32, patch: u32) -> u32 {
    major * 1_000_000 + minor * 1_000 + patch
}
