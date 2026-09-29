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
//! * [`connection`] — running statements against a database.
//! * [`aggregate`] — aggregate functions and grouping.
//! * [`index`] — index b-trees.
//! * [`journal`] — the rollback journal.
//! * [`catalog`] — the tables a connection knows about.
//! * [`eval`] — expression evaluation and the built-in functions.
//! * [`affinity`] — how a declared type is interpreted.
//! * [`btree`] — reading table b-trees, including overflow chains.
//! * [`btree_write`] — writing table leaf pages.
//! * [`btree_interior`] — table interior pages and leaf splitting.
//! * [`table_tree`] — the growing, splitting b-tree above those pages.
//! * [`text`] — the text encodings a file can declare.
//! * [`value`] / [`error`] — the runtime value and error types.
//!
//! Higher layers (tokenizer, SQL parser, b-tree, pager, VDBE) are built on top
//! of these and live in their own modules as they land.

pub mod affinity;
pub mod affinity_rules;
pub mod aggcheck;
pub mod aggregate;
pub mod btree;
pub mod btree_interior;
pub mod btree_write;
pub mod catalog;
pub mod connection;
pub mod error;
pub mod eval;
pub mod explain;
pub mod func_math;
pub mod func_string;
pub mod grouping;
pub mod index;
pub mod index_ddl;
pub mod index_interior;
pub mod insert_select;
pub mod join;
pub mod journal;
pub mod msg;
pub mod orderby;
pub mod page;
pub mod pager;
pub mod parser;
pub mod pragma;
pub mod record;
pub mod resolve;
pub mod table_tree;
pub mod text;
pub mod tokenizer;
pub mod value;
pub mod varint;
pub mod vtab;
pub mod vec0_bridge;

#[cfg(test)]
#[path = "binary_text_tests.rs"]
mod binary_text_tests;

#[cfg(test)]
#[path = "unique_constraint_tests.rs"]
mod unique_constraint_tests;

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
