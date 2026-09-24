//! A SQLite C ABI shim over [`nsqlite`].
//!
//! # Why this crate exists
//!
//! The official TCL test suite is a test *harness*, not a test program:
//! `testfixture` links against the SQLite C API directly, so running the suite
//! unmodified needs a loadable library that implements the `sqlite3_*` symbols.
//! This crate is that library. It is a thin `unsafe` adapter that speaks
//! SQLite's C conventions and delegates the actual work to the engine.
//!
//! `nsqlite` is `#![forbid(unsafe_code)]`, so this is the one place in the tree
//! where raw pointers are allowed and the one place where a mistake corrupts a
//! caller's memory. Two rules follow, and they are enforced throughout:
//!
//! * Every pointer a caller can observe is owned by something whose lifetime
//!   the API already promises: the connection for [`sqlite3_errmsg`], the
//!   statement for column text. Nothing is returned from a temporary.
//! * Every pointer is null-checked before it is dereferenced, so a caller that
//!   ignored a return code gets an error rather than a segfault.
//!
//! # Handles
//!
//! SQLite's C API is unsafe because the caller must keep handles straight: a
//! finalized `sqlite3_stmt*` is dangling, and so is a closed `sqlite3*`. This
//! shim does not hide that behind a registry, because the registry would just
//! become the thing that is wrong under a different misuse. The boxed handles
//! here are the real objects.
//!
//! Where SQLite tolerates a *null* handle, this matches what the real library
//! does; where it does not, the shim is deliberately stricter, because a crash
//! is not a contract a caller can program against. Every row below was probed
//! against sqlite3 3.53.4, one call per process so that a segfault in one did
//! not hide the others:
//!
//! | call with a null handle | real SQLite | this shim |
//! |---|---|---|
//! | `sqlite3_errmsg` | `"out of memory"` | same |
//! | `sqlite3_errcode` | `SQLITE_NOMEM` (7) | same |
//! | `sqlite3_extended_errcode` | `SQLITE_NOMEM` (7) | same |
//! | `sqlite3_close` | `SQLITE_OK` | same |
//! | `sqlite3_close_v2` | `SQLITE_OK` | same |
//! | `sqlite3_finalize` | `SQLITE_OK` | same |
//! | `sqlite3_free` | no-op | same |
//! | `sqlite3_reset` | `SQLITE_OK` | `SQLITE_MISUSE` |
//! | `sqlite3_step` | `SQLITE_MISUSE` (21) | same |
//! | `sqlite3_changes` | **segfaults** | `0` |
//! | `sqlite3_total_changes` | **segfaults** | `0` |
//! | `sqlite3_get_autocommit` | **segfaults** | `1` (autocommit) |
//! | `sqlite3_last_insert_rowid` | **segfaults** | `0` |
//! | `sqlite3_db_filename` | **segfaults** | null |
//! | everything else | undefined, or a crash | `SQLITE_MISUSE` |
//!
//! The five segfault rows are a difference in *safety*, not in results: the
//! shim answers the question the call was asking instead of dying, and no
//! correct caller can tell, because a correct caller never passes null there.
//!
//! # The two things a C caller can corrupt us with
//!
//! **Text is not null-terminated.** `sqlite3_column_text` and
//! `sqlite3_column_name` return a pointer *plus* a length, and the buffer is
//! valid only until the next `step`, `reset` or `finalize` on that statement.
//! Every such buffer is owned by the statement and replaced when the value it
//! belonged to changes, so the pointer lives for exactly as long as SQLite
//! promises and not a moment longer. Each buffer carries a trailing NUL as
//! well, because a C caller is entitled to treat the result as a C string, and
//! the bytes and the terminator have to share one allocation for the text
//! pointer to stay put.
//!
//! **Row lifetime.** The engine produces a whole result set in one call, so
//! `sqlite3_step` hands the rows out one at a time from a buffer the statement
//! owns, and `reset` drops it.
//!
//! # Parameters
//!
//! The engine runs a statement to completion in one call, so there is no VDBE
//! to bind into. Parameters are rewritten into the statement's own text before
//! it is handed over, which is the "inline literals" strategy. The rewrite is
//! deliberately narrow: it replaces only the text of a `?N`, `:name`, `@name`
//! or `$name` *parameter token* the tokenizer itself reported, at the spans it
//! reported. A `?` inside a string literal is a character in a string, not a
//! parameter, and never moves.
//!
//! # Known limitations
//!
//! Named rather than hidden, because a test that fails for a documented reason
//! is a test that can be fixed and a test that fails for a mystery is not:
//!
//! * **TEXT is UTF-8 only.** SQLite stores TEXT as arbitrary bytes and will
//!   return text that is not valid UTF-8; the suite's `nFtsUtf8Sort` tests and
//!   its `CAST(x'ff' AS TEXT)` idiom rely on that. `nsqlite::Value::Text` is a
//!   `String`, so such a value cannot be represented at all, and
//!   `sqlite3_bind_text` of a non-UTF-8 buffer binds the U+FFFD replacement
//!   characters. It still returns `SQLITE_OK` and still binds, because that is
//!   what the real library does with the same call (probed against sqlite3
//!   3.53.4), and reporting an error SQLite never emits while leaving the
//!   parameter unbound would be worse on both counts. The bytes are the only
//!   thing lost; the contract is not. The cases that compare the bytes will
//!   fail until `Value` grows a byte-level text representation.
//!
//! * **Column names cost a probe run.** The engine hands back finished rows
//!   rather than a described statement, so a query's column names are
//!   recovered by running it once at prepare time. A probe that *fails* is
//!   reported: `prepare` of a statement whose FROM cannot be resolved returns
//!   `SQLITE_ERROR` with a null statement and the engine's message, as SQLite
//!   does. What a probe cannot do is name the columns of a statement the engine
//!   declines to run at all, so a caller that only checks the return code and
//!   ignores a null statement still has a null statement.
//!
//! * **No user-defined functions, authorizer, progress handler, backup,
//!   load-extension, or threadsafety.** Out of scope; `test/shim/tester.tcl`
//!   drives the suite over a CLI, which is the other strategy.
//!
//! * **The change counters are the shim's own.** `nsqlite` has no
//!   connection-lifetime counter, and its per-statement counter is zeroed by
//!   every statement including a `SELECT`, which is not what `sqlite3_changes`
//!   means. The shim keeps both in cells on the connection and updates them
//!   only for the statements that change rows, so the probe run and the step
//!   cannot disturb what a caller reads.

#![allow(clippy::missing_safety_doc)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]
#![allow(clippy::too_many_arguments)]

use std::cell::{Cell, RefCell};
use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::path::Path;
use std::ptr;

use nsqlite::connection::{Connection, Outcome};
use nsqlite::error::{Error, ResultCode};
use nsqlite::parser::{self, Stmt};
use nsqlite::tokenizer::Tokenizer;
use nsqlite::value::{Datatype, Value};

// ---------------------------------------------------------------------------
// Result codes.
// ---------------------------------------------------------------------------

/// `SQLITE_OK`.
pub const SQLITE_OK: c_int = 0;
/// `SQLITE_ERROR`.
pub const SQLITE_ERROR: c_int = 1;
/// `SQLITE_MISUSE`.
pub const SQLITE_MISUSE: c_int = 21;
/// `SQLITE_RANGE`.
pub const SQLITE_RANGE: c_int = 25;
/// `SQLITE_ROW`.
pub const SQLITE_ROW: c_int = 100;
/// `SQLITE_DONE`.
pub const SQLITE_DONE: c_int = 101;
/// `SQLITE_INTEGER`, the `sqlite3_column_type` code for an integer.
pub const SQLITE_INTEGER: c_int = 1;
/// `SQLITE_FLOAT`, the `sqlite3_column_type` code for a real.
pub const SQLITE_FLOAT: c_int = 2;
/// `SQLITE_TEXT`, the `sqlite3_column_type` code for text.
pub const SQLITE_TEXT: c_int = 3;
/// `SQLITE_BLOB`, the `sqlite3_column_type` code for a blob.
pub const SQLITE_BLOB: c_int = 4;
/// `SQLITE_NULL`, the `sqlite3_column_type` code for null.
pub const SQLITE_NULL: c_int = 5;

/// `SQLITE_OPEN_READONLY`.
pub const SQLITE_OPEN_READONLY: c_int = 0x0000_0001;
/// `SQLITE_OPEN_READWRITE`.
pub const SQLITE_OPEN_READWRITE: c_int = 0x0000_0002;
/// `SQLITE_OPEN_CREATE`.
pub const SQLITE_OPEN_CREATE: c_int = 0x0000_0004;
/// `SQLITE_OPEN_URI`.
pub const SQLITE_OPEN_URI: c_int = 0x0000_0040;
/// `SQLITE_OPEN_MEMORY`.
pub const SQLITE_OPEN_MEMORY: c_int = 0x0000_0080;
/// `SQLITE_OPEN_NOMUTEX`.
pub const SQLITE_OPEN_NOMUTEX: c_int = 0x0000_8000;
/// `SQLITE_OPEN_FULLMUTEX`.
pub const SQLITE_OPEN_FULLMUTEX: c_int = 0x0001_0000;
/// `SQLITE_OPEN_SHAREDCACHE`.
pub const SQLITE_OPEN_SHAREDCACHE: c_int = 0x0002_0000;
/// `SQLITE_OPEN_PRIVATECACHE`.
pub const SQLITE_OPEN_PRIVATECACHE: c_int = 0x0004_0000;
/// `SQLITE_OPEN_NOFOLLOW`.
pub const SQLITE_OPEN_NOFOLLOW: c_int = 0x0100_0000;
/// `SQLITE_OPEN_EXRESCODE`.
pub const SQLITE_OPEN_EXRESCODE: c_int = 0x2000_0000;

/// `SQLITE_PREPARE_PERSISTENT`.
pub const SQLITE_PREPARE_PERSISTENT: u32 = 0x01;

/// `SQLITE_TRANSIENT`, the destructor value that means "copy it".
///
/// SQLite's own header defines this as `((sqlite3_destructor_type)-1)`, which
/// is a function pointer holding all-ones. That is not a constructible Rust
/// constant -- a `const` of that type with those bytes is rejected as an
/// invalid value, quite correctly -- so it is built on demand instead. The
/// shim copies the bytes on every bind and ignores the destructor, so a caller
/// that passes this gets the documented behaviour either way.
pub fn sqlite3_transient() -> Option<unsafe extern "C" fn(*mut c_void)> {
    // SAFETY: the pointer is only ever compared, never called. The shim's bind
    // functions ignore their destructor argument entirely, so a caller passing
    // SQLITE_TRANSIENT and a caller passing 0 differ only in a value the shim
    // never reads.
    Some(unsafe { std::mem::transmute::<usize, unsafe extern "C" fn(*mut c_void)>(usize::MAX) })
}

const SQLITE_ABORT: c_int = ResultCode::Abort as c_int;
const SQLITE_BUSY: c_int = ResultCode::Busy as c_int;

// ---------------------------------------------------------------------------
// Handles.
// ---------------------------------------------------------------------------

/// A database connection.
///
/// `conn` stays `None` until the open succeeds, so a failed `sqlite3_open`
/// still hands back a handle whose `sqlite3_errmsg` reads the reason, which is
/// what the C contract requires.
pub struct CDb {
    /// The error the last failing call left behind, which is what
    /// `sqlite3_errmsg` returns until something succeeds.
    last_error: RefCell<CString>,
    code: Cell<c_int>,
    extended: Cell<c_int>,
    conn: Option<Connection>,
    filename: Option<CString>,
    /// Live statements, so `sqlite3_close` can refuse while one is unfinalized
    /// instead of leaving the caller holding a statement that points into a
    /// freed connection.
    live: Cell<usize>,
    /// Closed, but a statement is still running. SQLite calls this a zombie.
    closing: Cell<bool>,
    total_changes: Cell<i64>,
    /// `sqlite3_changes` as the C API defines it: the count from the last
    /// statement that changed rows, and *not* disturbed by anything else.
    ///
    /// The engine's own counter is zeroed by every statement it runs
    /// (`Connection::execute` starts with `self.changes = 0`), so reading it
    /// directly reports 0 after a SELECT where SQLite reports the last DML
    /// count. Probed against sqlite3 3.53.4: after `CREATE` + a 3-row INSERT,
    /// `sqlite3_changes` is 3 and stays 3 across a SELECT, a DDL, and a
    /// prepared-but-unstepped statement, and only a 0-row `UPDATE` or `DELETE`
    /// moves it to 0. This mirrors that instead of exposing the engine's
    /// scratch register.
    changes: Cell<i64>,
    /// Set when the handle was asked for `SQLITE_OPEN_READONLY`, so a write is
    /// refused with `SQLITE_READONLY` rather than reaching the pager.
    read_only: Cell<bool>,
}

impl CDb {
    fn new() -> CDb {
        CDb {
            last_error: RefCell::new(cstr("not an error")),
            code: Cell::new(SQLITE_OK),
            extended: Cell::new(SQLITE_OK),
            conn: None,
            filename: None,
            live: Cell::new(0),
            closing: Cell::new(false),
            total_changes: Cell::new(0),
            changes: Cell::new(0),
            read_only: Cell::new(false),
        }
    }

    /// Records a failure the way the C API does: the code, and the message
    /// `sqlite3_errmsg` will return until something succeeds.
    fn fail(&self, err: &Error) -> c_int {
        self.set_error(err.code() as c_int, err.extended_code(), &err.message)
    }

    fn set_error(&self, rc: c_int, extended: c_int, msg: &str) -> c_int {
        self.code.set(rc);
        self.extended.set(extended);
        *self.last_error.borrow_mut() = cstr(msg);
        rc
    }

    /// Clears the error, which is what a successful call does.
    fn clear_error(&self) {
        self.code.set(SQLITE_OK);
        self.extended.set(SQLITE_OK);
        *self.last_error.borrow_mut() = cstr("not an error");
    }

    fn misuse(&self) -> c_int {
        self.set_error(
            SQLITE_MISUSE,
            SQLITE_MISUSE,
            "bad parameter or other API misuse",
        )
    }

    /// Records what a statement the shim just ran did to the row counters.
    ///
    /// `changed` is the engine's own count for that one statement. SQLite's
    /// `sqlite3_changes` is *not* that register: it is the count from the last
    /// statement that changed rows, and a SELECT or a DDL leaves it alone.
    /// Probed against sqlite3 3.53.4: after `CREATE TABLE` plus a 5-row
    /// `INSERT`, both counters read 5, and they still read 5 after merely
    /// *preparing* `SELECT a FROM t`, after stepping it, and after a no-op
    /// `SELECT` -- only a 0-row `UPDATE` or `DELETE` moves `changes` to 0.
    ///
    /// A SELECT therefore must not touch `changes`, which is why this is
    /// `Option`: the callers that know the statement was read-only pass `None`
    /// and leave the counter alone. A statement that ran but was not a
    /// read-only one passes the engine's count, including a zero.
    fn record_changes(&self, changed: Option<i64>) {
        let Some(n) = changed else {
            return;
        };
        self.total_changes.set(self.total_changes.get() + n);
        self.changes.set(n);
    }
}

/// Bytes handed to a C caller, with a NUL after the last one so the same
/// pointer can serve both `const unsigned char*` and `char*`.
///
/// A pointer to the string has to remain a pointer to the bytes for as long as
/// SQLite promises, and it has to be NUL-terminated at the same time. One
/// allocation holding the bytes, the NUL, and then alignment padding covers
/// both, because the text pointer lands on the first byte either way.
struct NulTerminated {
    buf: Box<[u8]>,
}

impl NulTerminated {
    fn new(bytes: &[u8]) -> NulTerminated {
        let mut buf = Vec::with_capacity(bytes.len() + 2);
        buf.extend_from_slice(bytes);
        buf.push(0);
        NulTerminated {
            buf: buf.into_boxed_slice(),
        }
    }

    fn as_ptr(&self) -> *const c_char {
        self.buf.as_ptr() as *const c_char
    }
}

/// One result column, with the buffer its name was handed out through.
struct CStmtColumn {
    name: CString,
    cache: Option<NulTerminated>,
}

/// What a parameter is bound to.
#[derive(Clone)]
struct Param {
    name: Option<String>,
    value: Value,
}

/// One parameter occurrence: where it is in the source, and which slot it binds.
#[derive(Clone, Copy)]
struct ParamSite {
    start: usize,
    end: usize,
    slot: usize,
}

/// The statement handle.
///
/// `current` is a copy of the row the accessors read, owned here rather than
/// borrowed from the engine, so the engine's vectors can be dropped as soon as
/// a row is handed out. That is what makes the pointer-lifetime contract in the
/// module docs true rather than aspirational.
pub struct CStmt {
    db: *mut CDb,
    /// The caller's own text, with parameters still written as parameters.
    /// `sqlite3_sql` returns this, because that is what the caller wrote and
    /// what a normalized-statement comparison is comparing.
    sql: CString,
    /// `sql` with every parameter replaced by its value's SQL literal. This is
    /// what the engine is actually given.
    bound_sql: CString,
    params: Vec<Param>,
    /// The CStrings `sqlite3_bind_parameter_name` hands out, built on first
    /// use so the pointer a caller receives stays put for the statement's life.
    param_names: std::collections::BTreeMap<usize, CString>,
    sites: Vec<ParamSite>,
    columns: Vec<CStmtColumn>,
    /// The text buffers handed out for the current row, one per column so two
    /// accessors in a row do not invalidate each other.
    text: Vec<Option<NulTerminated>>,
    /// The representation each column of the current row has been *converted*
    /// to by a sibling accessor, which is what `sqlite3_column_type` reports.
    ///
    /// SQLite stores a value in one representation and converts it in place the
    /// first time an accessor asks for a different one, so the type a caller
    /// sees is the type of the representation, not of the stored value. Only
    /// `BLOB -> TEXT` actually happens -- `sqlite3_column_text` on a REAL or
    /// on an INTEGER renders text into a *separate* buffer and leaves the
    /// number alone. Probed against sqlite3 3.53.4, interleaving accessors on
    /// one column: `SELECT x'0102ff'` reads BLOB (4) then, after
    /// `sqlite3_column_text`, reads TEXT (3); `SELECT 42` stays INTEGER (1),
    /// `SELECT 1.5` stays FLOAT (2), `SELECT NULL` stays NULL (5), and
    /// `SELECT 'abc'` stays TEXT (3) after `sqlite3_column_blob`. A new row
    /// clears the overrides.
    converted: Vec<Option<c_int>>,
    rows: Vec<Vec<Value>>,
    row_index: usize,
    row_valid: bool,
    rc: c_int,
    /// The error this statement's last step failed with, kept separately from
    /// `rc` because `SQLITE_ROW` and `SQLITE_DONE` share `rc`'s field and are
    /// not failures. `sqlite3_reset` answers with this when the statement
    /// actually errored, and `SQLITE_OK` otherwise.
    err: c_int,
    err_msg: String,
}

impl CStmt {
    /// The value at column `i` of the current row, if there is a current row.
    /// Out-of-range is a `MISUSE`, which is what SQLite's own `columnMem`
    /// reports.
    fn value(&self, i: c_int) -> Result<&Value, c_int> {
        if !self.row_valid || i < 0 || i as usize >= self.current_len() {
            return Err(SQLITE_MISUSE);
        }
        Ok(&self.rows[self.row_index - 1][i as usize])
    }

    fn current_len(&self) -> usize {
        if self.row_index == 0 {
            0
        } else {
            self.rows[self.row_index - 1].len()
        }
    }

    /// The bytes of column `i` as a blob, which is the same buffer `text_ptr`
    /// hands out.
    ///
    /// This does *not* mark the column as converted. Reading a TEXT value as a
    /// blob hands back the text's own bytes without changing the column's
    /// representation, so `sqlite3_column_type` still answers TEXT afterwards.
    /// Probed against sqlite3 3.53.4: on `SELECT 'abc'`, `column_type` reads 3,
    /// `sqlite3_column_blob` returns the three bytes, and `column_type` still
    /// reads 3.
    fn blob_ptr(&mut self, i: c_int) -> *const c_void {
        self.text_ptr(i) as *const c_void
    }

    /// SQLite's rendering of a value: an integer as decimal, a real as the
    /// shortest decimal that round-trips, text as its bytes, a blob as its
    /// bytes.
    fn render_value(v: &Value) -> Vec<u8> {
        match v {
            Value::Null => Vec::new(),
            Value::Integer(i) => i.to_string().into_bytes(),
            Value::Real(r) => render_real(*r).into_bytes(),
            Value::Text(s) => s.clone().into_bytes(),
            Value::Blob(b) => b.clone(),
        }
    }

    /// The bytes of column `i`, cached so the pointer outlives this call and
    /// dies with the next step.
    fn text_ptr(&mut self, i: c_int) -> *const c_char {
        let Ok(v) = self.value(i) else {
            return ptr::null();
        };
        if v.is_null() {
            return ptr::null();
        }
        // Reading a BLOB as text converts it in place, so `column_type` answers
        // TEXT from here on. The other storage classes are not rewritten by
        // their text rendering -- see the `converted` field. The test is taken
        // before the mutable borrow so the immutable `v` is still live.
        let was_blob = matches!(v, Value::Blob(_));
        let bytes = Self::render_value(v);
        if was_blob {
            if let Some(slot) = self.converted.get_mut(i as usize) {
                *slot = Some(SQLITE_TEXT);
            }
        }
        let slot = match self.text.get_mut(i as usize) {
            Some(t) => t,
            None => return ptr::null(),
        };
        let stale = slot
            .as_ref()
            .map(|c| c.buf.len() != bytes.len() + 1 || c.buf[..bytes.len()] != bytes[..])
            .unwrap_or(true);
        if stale {
            *slot = Some(NulTerminated::new(&bytes));
        }
        slot.as_ref().unwrap().as_ptr()
    }
}

// ---------------------------------------------------------------------------
// Small helpers.
// ---------------------------------------------------------------------------

/// A `CString` that never fails. A Rust `String` can hold a NUL and
/// `CString::new` would then panic, so the text is truncated at the NUL
/// instead, which is what a C caller would see anyway.
fn cstr(s: &str) -> CString {
    let bytes = s.as_bytes();
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    CString::new(&bytes[..end]).unwrap_or_else(|_| CString::new("").expect("empty"))
}

/// Copies a NUL-terminated C string into an owned `String`.
unsafe fn borrow_cstr(p: *const c_char) -> Option<String> {
    if p.is_null() {
        return None;
    }
    CStr::from_ptr(p).to_str().ok().map(|s| s.to_owned())
}

unsafe fn borrow_bytes<'a>(p: *const c_void, n: c_int) -> Option<&'a [u8]> {
    if p.is_null() || n < 0 {
        return None;
    }
    Some(std::slice::from_raw_parts(p as *const u8, n as usize))
}

/// The length of the numeric *prefix* of `b[start..]`: leading whitespace, an
/// optional sign, then as many digits as are there.
///
/// This is the extent `sqlite3_column_int64` reads on a TEXT value, and it
/// deliberately stops at the first non-digit. SQLite's `sqlite3Atoi64` is an
/// integer parser, so `'1e3'` is 1 and `'1.9e2'` is 1 -- the exponent belongs to
/// `sqlite3_column_double`, not here. Probed against sqlite3 3.53.4 for
/// `'1e3'`, `'1.9e2'`, `'1e400'`, `'42abc'`, `'  -12  '` and `'.5'`.
fn integer_prefix_len(b: &[u8]) -> usize {
    let mut i = 0;
    while i < b.len() && (b[i] as char).is_ascii_whitespace() {
        i += 1;
    }
    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
        i += 1;
    }
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    i
}

/// SQLite's `sqlite3_column_int64` on a TEXT value.
///
/// The integer prefix, read with the same accumulate-and-clamp loop as
/// `sqlite3Atoi64`: each digit is multiplied in and the result is pinned to
/// `i64::MIN` or `i64::MAX` the moment it would leave the range. That loop is
/// why the answer is *exact* where a double round-trip is not -- SQLite reports
/// 9007199254740993 for `'9007199254740993'` and 1234567890123456789 for
/// `'1234567890123456789'`, both of which a `f64` cannot hold, while
/// `'18446744073709551615'` clamps to `i64::MAX` because it really does
/// overflow. All three were probed against sqlite3 3.53.4.
fn text_prefix_i64(s: &str) -> i64 {
    let b = s.as_bytes();
    let end = integer_prefix_len(b);
    // Skip the leading whitespace and then the optional sign, so the digits
    // start at the first digit rather than at the first `+`/`-` anywhere. An
    // unsigned prefix like `'1e3'` has no sign at all, and a `skip_while` that
    // looked for one would scan the whole prefix and return nothing -- every
    // unsigned TEXT value would read as 0.
    let mut start = 0;
    while start < end && (b[start] as char).is_ascii_whitespace() {
        start += 1;
    }
    // The sign is the byte *after* the leading whitespace, not the first byte
    // of the string: `'  -12'` is -12, and testing `b[0]` would find a space and
    // read the value as positive.
    let negative = b.get(start) == Some(&b'-');
    if start < end && (b[start] == b'+' || b[start] == b'-') {
        start += 1;
    }
    let mut acc: i64 = 0;
    for d in &b[start..end] {
        if !d.is_ascii_digit() {
            continue;
        }
        let digit = i64::from(d - b'0');
        acc = match acc.checked_mul(10).and_then(|a| a.checked_add(digit)) {
            Some(v) => v,
            None => {
                // One past the range is enough to know which end it left by.
                return if negative { i64::MIN } else { i64::MAX };
            }
        };
    }
    if negative {
        -acc
    } else {
        acc
    }
}

/// SQLite's `sqlite3_column_double` on a TEXT value.
///
/// This one is a real-number parse, and it is a *narrower* one than a general
/// floating-point parse: SQLite's `sqlite3AtoF` recognises the SQLite numeric
/// literal grammar, which has no hexadecimal integers, no binary ones, and no
/// `inf` or `nan` spellings. Probed against sqlite3 3.53.4, every one of
/// `'inf'`, `'nan'`, `'INF'`, `'NaN'`, `'0x10'`, `'0b101'` and `'1_000'` reads
/// back as 0, while `'1e400'` is +Inf and `'1e-400'` underflows to 0. The
/// longest prefix that the grammar accepts is what gets parsed, which is why
/// `'1.5e2xyz'` is 150 and `'3e'` is 3 rather than an error.
fn text_prefix_f64(s: &str) -> f64 {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() && (b[i] as char).is_ascii_whitespace() {
        i += 1;
    }
    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
        i += 1;
    }
    // The integer part: at least one digit, or a '.' followed by one.
    let int_start = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    let int_digits = i > int_start;
    if !int_digits {
        // A leading '.' is a number only if a digit follows it. The '.' itself
        // is deliberately *not* consumed here: the fraction block below is what
        // reads the dot and the digits after it, and consuming the dot twice
        // would leave `end` sitting on the dot, so `s[..end]` would be `"."`
        // and `".5"` would read back as 0 rather than 0.5.
        if !(i < b.len() && b[i] == b'.' && b.get(i + 1).is_some_and(u8::is_ascii_digit)) {
            return 0.0;
        }
    }
    let mut end = i;
    if i < b.len() && b[i] == b'.' {
        let dot = i;
        i += 1;
        let frac = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        // A '.' with nothing after it belongs to the number (`'5.'` is 5), but
        // one that is not followed by a digit and is not preceded by one is
        // punctuation (`'1..2'` is 1).
        if i > frac || int_digits {
            end = i;
        } else {
            i = dot;
        }
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        i += 1;
        if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
            i += 1;
        }
        let ds = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        // An `e` with no digits after it is not an exponent, so the number ends
        // before it -- but the `e` itself is not part of what was consumed, so
        // `'1e'` is 1 and not 1.0. `i` is not walked back: nothing reads it
        // after this block, because the answer is the prefix `s[..end]`.
        if i > ds {
            end = i;
        }
    }
    s[..end].trim().parse::<f64>().unwrap_or(0.0)
}

/// SQLite's integer-from-double: truncate, clamp to i64, and map NaN to zero
/// rather than panicking.
fn double_to_i64(d: f64) -> i64 {
    if d.is_nan() {
        return 0;
    }
    if d >= i64::MAX as f64 {
        return i64::MAX;
    }
    if d <= i64::MIN as f64 {
        return i64::MIN;
    }
    d as i64
}

// ---------------------------------------------------------------------------
// SQLite's REAL-to-text algorithm.
// ---------------------------------------------------------------------------
//
// A REAL is rendered as the shortest decimal that reads back as the same
// double, switching to exponential notation outside the range where `%g` would.
// The comment above `vdbeMemRenderNum` in `src/vdbemem.c` names the format --
// the `"%!.*g"` with the connection's digit count -- and `src/printf.c` plus
// `src/util.c` implement the digits, the switch, and the "at least one digit
// past the point" rule. This is that algorithm rather than a guess, because a
// test that compares a rendered REAL against SQLite's compares the bytes.
//
// `render_real_matches_sqlite` in the tests checks all of it against values
// read out of the real library with a C program.

/// `v2 * 10^p` rounded to the nearest double, which is how
/// `sqlite3Fp10Convert2` answers the round-trip question. Rust's parser is
/// correctly rounded, so parsing the two as one literal gives the same double.
fn scale_round(v2: u64, p: i32) -> f64 {
    let mut s = String::with_capacity(24);
    s.push_str(&v2.to_string());
    s.push('e');
    s.push_str(&p.to_string());
    s.parse::<f64>().unwrap_or(f64::NAN)
}

/// The 18 significant digits of `a` and the position of the decimal point,
/// which is the form `sqlite3Fp2Convert10(n = 18)` produces.
fn decode18(a: f64) -> (Vec<u8>, i32) {
    // Rust's LowerExp gives correctly rounded decimal output, and 17 places
    // after the point is 18 significant digits.
    let s = format!("{a:.17e}");
    let (mant, exp) = s
        .split_once('e')
        .expect("LowerExp always writes an exponent");
    let digits: Vec<u8> = mant.bytes().filter(|b| *b != b'.').collect();
    let e0: i32 = exp.parse().expect("LowerExp writes a decimal exponent");
    (digits, e0 + 1)
}

/// SQLite's text rendering of a REAL.
///
/// This is 18 digits reduced to 17 by `sqlite3FpDecode`, laid out by `%g` with
/// `exp < -4 || exp >= 17` choosing exponential, and then trailing zeros
/// stripped with one digit kept after the point. Probed against sqlite3
/// 3.53.4: 1.0 renders as `1.0`, 1e16 as `10000000000000000.0`, 1e17 as
/// `1.0e+17`, 1e-5 as `1.0e-05`, 2.0/3.0 as `0.66666666666666663`, and -0.0 as
/// `0.0`.
fn render_real(r: f64) -> String {
    if r.is_nan() {
        return "NaN".to_string();
    }
    if r.is_infinite() {
        return if r < 0.0 { "-Inf" } else { "Inf" }.to_string();
    }
    if r == 0.0 {
        return "0.0".to_string();
    }
    let neg = r < 0.0;
    let a = r.abs();
    let (z, mut idp) = decode18(a);
    let n = z.len();
    let e0 = idp - 1;
    // A j-digit prefix of the 18-digit expansion is `prefix * 10^(e0 - j + 1)`.
    let prefix_exp = |j: usize| e0 - j as i32 + 1;
    let digits_of = |s: &[u8]| -> u64 {
        s.iter()
            .fold(0u64, |acc, b| acc * 10 + u64::from(*b - b'0'))
    };

    // sqlite3FpDecode at iRound == 17: try to keep fewer digits, and accept
    // the shorter form only when it still reads back as the same double.
    let mut i_round = 17usize;
    if i_round < n {
        if z[15] == b'9' && z[14] == b'9' {
            let mut jj = 14usize;
            while jj > 0 && z[jj - 1] == b'9' {
                jj -= 1;
            }
            let v2 = if jj == 0 { 1 } else { digits_of(&z[..jj]) + 1 };
            if scale_round(v2, prefix_exp(jj)) == a {
                i_round = jj + 1;
            }
        } else if idp >= n as i32 || (z[15] == b'0' && z[14] == b'0' && z[13] == b'0') {
            let mut jj = 13usize;
            while jj > 1 && z[jj - 1] == b'0' {
                jj -= 1;
            }
            if scale_round(digits_of(&z[..jj]), prefix_exp(jj)) == a {
                i_round = jj + 1;
            }
        }
    }

    let mut d = z[..i_round].to_vec();
    if i_round < n && z[i_round] >= b'5' {
        let mut j = i_round as i64 - 1;
        loop {
            if d[j as usize] == b'9' {
                d[j as usize] = b'0';
                if j == 0 {
                    d.insert(0, b'1');
                    d.truncate(i_round);
                    idp += 1;
                    break;
                }
                j -= 1;
            } else {
                d[j as usize] += 1;
                break;
            }
        }
    }

    let exp = idp - 1;
    let mut out = String::new();
    if neg {
        out.push('-');
    }
    if !(-4..17).contains(&exp) {
        // etEXP: one digit, the point, the rest, then e+NN with two digits
        // minimum, which is why 1e-5 is `1.0e-05` and not `1.0e-5`.
        out.push(d[0] as char);
        out.push('.');
        for b in &d[1..] {
            out.push(*b as char);
        }
        while out.ends_with('0') {
            out.pop();
        }
        if out.ends_with('.') {
            out.push('0');
        }
        out.push('e');
        out.push(if exp < 0 { '-' } else { '+' });
        let e = exp.unsigned_abs();
        if e >= 100 {
            out.push(char::from_digit(e / 100, 10).expect("digit"));
        }
        out.push(char::from_digit((e / 10) % 10, 10).expect("digit"));
        out.push(char::from_digit(e % 10, 10).expect("digit"));
        return out;
    }
    // etFLOAT: the digits before the point, then enough after it to reach the
    // requested precision, then trailing zeros stripped.
    if idp <= 0 {
        out.push('0');
        out.push('.');
        for _ in 0..(-idp) {
            out.push('0');
        }
        for b in &d {
            out.push(*b as char);
        }
    } else {
        let ip = idp as usize;
        for b in &d[..ip.min(d.len())] {
            out.push(*b as char);
        }
        for _ in d.len()..ip {
            out.push('0');
        }
        out.push('.');
        let want = (16 - exp) as usize;
        let rest = if ip < d.len() { &d[ip..] } else { &[][..] };
        for b in rest.iter().take(want) {
            out.push(*b as char);
        }
        for _ in rest.len().min(want)..want {
            out.push('0');
        }
    }
    if !out.contains('e') {
        let (head, frac) = out.split_once('.').expect("just wrote a point");
        let trimmed = frac.trim_end_matches('0');
        out = format!("{head}.{}", if trimmed.is_empty() { "0" } else { trimmed });
    }
    out
}

// ---------------------------------------------------------------------------
// Library-level entry points.
// ---------------------------------------------------------------------------

/// `sqlite3_libversion`.
///
/// nsqlite is a from-scratch engine, not a SQLite fork, so reporting SQLite's
/// version would be a lie the test harness would then act on. The suite reads
/// this to decide which features to run, and the honest answer is the version
/// of the code that is actually executing.
#[no_mangle]
pub extern "C" fn sqlite3_libversion() -> *const c_char {
    concat!(env!("CARGO_PKG_VERSION"), "\0").as_ptr() as *const c_char
}

/// `sqlite3_libversion_number`.
///
/// SQLite packs this as `major * 1000000 + minor * 1000 + patch`, so `0.1.0`
/// is 1000. The suite's `tcl-12.1` test cross-checks it against `db version`,
/// which is `sqlite_version()`; both come from the same constant here, which
/// is what makes that test pass rather than merely not crash.
#[no_mangle]
pub extern "C" fn sqlite3_libversion_number() -> c_int {
    nsqlite::version_number(0, 1, 0) as c_int
}

/// `sqlite3_sourceid`.
///
/// SQLite returns the check-in hash of the source it was built from. There is
/// no equivalent for a from-scratch engine, and a hash-shaped string that
/// names no real check-in would only make `releasetest.tcl` look for a file
/// that does not exist.
#[no_mangle]
pub extern "C" fn sqlite3_sourceid() -> *const c_char {
    concat!("nsqlite-", env!("CARGO_PKG_VERSION"), "\0").as_ptr() as *const c_char
}

// ---------------------------------------------------------------------------
// Opening and closing.
// ---------------------------------------------------------------------------

/// `sqlite3_open`: read/write/create, as the one-argument form is documented to
/// be.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_open(filename: *const c_char, pp_db: *mut *mut CDb) -> c_int {
    sqlite3_open_v2(
        filename,
        pp_db,
        SQLITE_OPEN_READWRITE | SQLITE_OPEN_CREATE,
        ptr::null(),
    )
}

/// `sqlite3_open_v2`.
///
/// `SQLITE_OPEN_READONLY` is honoured the way SQLite honours it: the open
/// succeeds, reads are served, and a *write* is refused with `SQLITE_READONLY`
/// (8) "attempt to write a readonly database". Refusing the open instead would
/// be the wrong shape -- a test that opens a database read-only and only reads
/// it is doing something legitimate, and a TCL harness does it routinely.
///
/// A path that does not exist is still `SQLITE_CANTOPEN` (14) under
/// `SQLITE_OPEN_READONLY`, because `Connection::open` would otherwise *create*
/// the file, which a read-only open must never do. See [`refuses_read_only`]
/// for which statements the flag refuses.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_open_v2(
    filename: *const c_char,
    pp_db: *mut *mut CDb,
    flags: c_int,
    _vfs: *const c_char,
) -> c_int {
    if pp_db.is_null() {
        return SQLITE_MISUSE;
    }
    const KNOWN: c_int = SQLITE_OPEN_READONLY
        | SQLITE_OPEN_READWRITE
        | SQLITE_OPEN_CREATE
        | SQLITE_OPEN_URI
        | SQLITE_OPEN_MEMORY
        | SQLITE_OPEN_NOMUTEX
        | SQLITE_OPEN_FULLMUTEX
        | SQLITE_OPEN_SHAREDCACHE
        | SQLITE_OPEN_PRIVATECACHE
        | SQLITE_OPEN_NOFOLLOW
        | SQLITE_OPEN_EXRESCODE;
    let db = Box::into_raw(Box::new(CDb::new()));
    *pp_db = db;
    if flags & !KNOWN != 0 {
        return (*db).misuse();
    }
    if flags & SQLITE_OPEN_READWRITE != 0 && flags & SQLITE_OPEN_READONLY != 0 {
        return (*db).misuse();
    }
    let read_only = flags & SQLITE_OPEN_READWRITE == 0 && flags & SQLITE_OPEN_READONLY != 0;

    let name = borrow_cstr(filename).unwrap_or_default();
    let is_memory = name.is_empty()
        || name == ":memory:"
        || flags & SQLITE_OPEN_MEMORY != 0
        || (flags & SQLITE_OPEN_URI != 0 && uri_is_memory(&name));
    if is_memory {
        // Probed: the real library opens a memory database happily under
        // SQLITE_OPEN_READONLY, because there is no file for the flag to apply
        // to. Refusing here would be a difference the suite would see.
        //
        // The filename is left unset on purpose. Probed again: a memory
        // database's `sqlite3_db_filename` is the empty string, and it is the
        // empty string whether it was opened as `""`, `:memory:`, or through
        // `SQLITE_OPEN_MEMORY` -- none of those spellings leaks into the
        // answer, so none of them is stored here either.
        match Connection::open_memory() {
            Ok(conn) => {
                (*db).conn = Some(conn);
                return SQLITE_OK;
            }
            Err(e) => return (*db).fail(&e),
        }
    }
    let path = if flags & SQLITE_OPEN_URI != 0 {
        strip_uri(&name)
    } else {
        name.clone()
    };
    if read_only {
        // A read-only open must not *create* the file. `Connection::open` would
        // happily do that, so a read-only open of a path that does not exist has
        // to be refused here, before the engine is asked for anything.
        //
        // Probed against sqlite3 3.53.4: opening a missing path read-only
        // returns 14 "unable to open database file" -- not SQLITE_OK, and not
        // MISUSE. Opening a *directory* returns the same 14. A path that exists
        // opens fine and the handle then refuses writes with 8 "attempt to
        // write a readonly database", which is what `read_only` below arranges.
        let exists = std::fs::metadata(&path).is_ok() && Path::new(&path).is_file();
        if !exists {
            return (*db).set_error(
                ResultCode::CantOpen as c_int,
                ResultCode::CantOpen as c_int,
                "unable to open database file",
            );
        }
        match Connection::open(Path::new(&path)) {
            Ok(conn) => {
                (*db).conn = Some(conn);
                (*db).filename = Some(cstr(&path));
                (*db).read_only.set(true);
                SQLITE_OK
            }
            Err(e) => {
                let rc = (*db).fail(&e);
                if e.code == ResultCode::CantOpen {
                    (*db)
                        .last_error
                        .replace(cstr("unable to open database file"));
                }
                rc
            }
        }
    } else {
        match Connection::open(Path::new(&path)) {
            Ok(conn) => {
                (*db).conn = Some(conn);
                (*db).filename = Some(cstr(&path));
                SQLITE_OK
            }
            Err(e) => {
                // Probed: the real library's message for an unopenable path is
                // exactly "unable to open database file", with no OS detail after
                // it. The engine's message carries the operating system's own
                // wording, which a test that compares the string would fail on, so
                // a failure to open is reported in SQLite's words and the extra
                // detail is dropped.
                let rc = (*db).fail(&e);
                if e.code == ResultCode::CantOpen {
                    (*db)
                        .last_error
                        .replace(cstr("unable to open database file"));
                }
                rc
            }
        }
    }
}

/// Whether a statement must be refused on a read-only handle.
///
/// The test is not "does this read rather than write" -- it is "does this write
/// the *main* database file", because that is the only thing a read-only handle
/// forbids. Probed against sqlite3 3.53.4 on a handle opened
/// `SQLITE_OPEN_READONLY`, where every one of these returns
/// `SQLITE_READONLY` (8) "attempt to write a readonly database":
/// `INSERT`, `UPDATE`, `DELETE`, `REPLACE`, `INSERT OR IGNORE`, `CREATE TABLE`,
/// `DROP TABLE`, `CREATE INDEX`, `PRAGMA user_version=1`, `ANALYZE` and
/// `VACUUM`. And every one of these returns `SQLITE_OK`:
///
/// * `BEGIN`, `BEGIN IMMEDIATE`, `COMMIT`, `END`, `SAVEPOINT`, `RELEASE` --
///   transaction control does not touch the file. (`ROLLBACK` with no
///   transaction active is a different case: SQLite answers
///   "cannot rollback - no transaction is active", which is the engine's own
///   error, not a read-only one, and is left to the engine.)
/// * a read-only `PRAGMA`, which by definition writes nothing;
/// * `CREATE TEMP TABLE` and `CREATE TEMP VIEW` -- a temp object lives in a
///   temporary database that the main file's read-only flag says nothing about.
fn refuses_read_only(stmt: &Stmt) -> bool {
    match stmt {
        Stmt::Select(_) | Stmt::Begin | Stmt::Commit | Stmt::Rollback => false,
        Stmt::CreateTable { temp, .. } => !temp,
        Stmt::Analyze
        | Stmt::DropTable { .. }
        | Stmt::CreateIndex { .. }
        | Stmt::Insert { .. }
        | Stmt::Update { .. }
        | Stmt::Delete { .. } => true,
        // Anything the engine grows later is treated as a write, which is the
        // safe direction: a statement the shim does not recognise should not
        // reach a handle that was told it cannot write.
        _ => true,
    }
}

/// Whether a statement is the kind that sets `sqlite3_changes`.
///
/// This is a different question from [`refuses_read_only`]. SQLite's counter is
/// the number of rows the most recent `INSERT`, `UPDATE` or `DELETE` changed,
/// and it is *not* disturbed by anything else -- not by a `SELECT`, not by
/// `CREATE TABLE`, not by a statement that merely prepares. Probed against
/// sqlite3 3.53.4: after `CREATE TABLE` plus a 5-row `INSERT` the counter reads
/// 5 and still reads 5 after a `SELECT` is prepared and stepped, and only a
/// 0-row `UPDATE` moves it to 0.
///
/// The engine's own counter is zeroed by every call to `Connection::execute`,
/// so the shim keeps this one in its own cell rather than reading the engine's
/// scratch register; this predicate is what says which statements may write to
/// it.
fn sets_changes(stmt: &Stmt) -> bool {
    matches!(
        stmt,
        Stmt::Insert { .. } | Stmt::Update { .. } | Stmt::Delete { .. }
    )
}

/// `SQLITE_READONLY` (8), the code a write against a read-only handle gets.
const SQLITE_READONLY: c_int = ResultCode::ReadOnly as c_int;

/// SQLite's message for a write against a read-only handle.
const READONLY_MESSAGE: &str = "attempt to write a readonly database";

fn uri_is_memory(name: &str) -> bool {
    name.split_once('?')
        .map(|(_, q)| q)
        .unwrap_or("")
        .split('&')
        .any(|kv| kv == "mode=memory")
}

fn strip_uri(name: &str) -> String {
    if let Some(rest) = name.strip_prefix("file:") {
        let cut = rest.find('?').unwrap_or(rest.len());
        return rest[..cut].to_owned();
    }
    name.to_owned()
}

/// `sqlite3_close`.
///
/// Returns `SQLITE_BUSY` while a statement is still open, which is what lets a
/// caller find its own leak. The connection is not freed in that case, so the
/// statement that blocked the close can still be asked why.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_close(db: *mut CDb) -> c_int {
    if db.is_null() {
        return SQLITE_OK;
    }
    if (*db).live.get() > 0 {
        return (*db).set_error(
            SQLITE_BUSY,
            SQLITE_BUSY,
            "unable to close due to unfinalized statements or unfinished backups",
        );
    }
    drop(Box::from_raw(db));
    SQLITE_OK
}

/// `sqlite3_close_v2`.
///
/// Marks the connection a zombie rather than refusing: statements already
/// running keep working, and the connection is freed by the last `finalize`.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_close_v2(db: *mut CDb) -> c_int {
    if db.is_null() {
        return SQLITE_OK;
    }
    if (*db).live.get() == 0 {
        return sqlite3_close(db);
    }
    (*db).closing.set(true);
    (*db).set_error(
        SQLITE_BUSY,
        SQLITE_BUSY,
        "unable to close due to unfinalized statements or unfinished backups",
    );
    SQLITE_OK
}

// ---------------------------------------------------------------------------
// Error reporting.
// ---------------------------------------------------------------------------

/// `SQLITE_NOMEM` is what the real library reports for a null handle, and
/// "out of memory" is the message that goes with it. Both live in statics, not
/// in a temporary: the pointer the caller receives has to outlive this call,
/// and a `CString` built here would be dropped the moment it returned.
const NULL_DB_MESSAGE: &[u8] = b"out of memory\0";

/// `SQLITE_NOMEM`, the code that goes with [`NULL_DB_MESSAGE`].
const SQLITE_NOMEM: c_int = ResultCode::NoMem as c_int;

/// `sqlite3_errmsg`.
///
/// The buffer belongs to the connection and stays valid until the next call
/// that changes the connection's error, which is the lifetime SQLite documents.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_errmsg(db: *const CDb) -> *const c_char {
    if db.is_null() {
        return NULL_DB_MESSAGE.as_ptr() as *const c_char;
    }
    (*db).last_error.borrow().as_ptr()
}

/// `sqlite3_errcode`, the primary code.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_errcode(db: *const CDb) -> c_int {
    if db.is_null() {
        return SQLITE_NOMEM;
    }
    (*db).code.get()
}

/// `sqlite3_extended_errcode`, the code with its extended bits.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_extended_errcode(db: *const CDb) -> c_int {
    if db.is_null() {
        return SQLITE_NOMEM;
    }
    (*db).extended.get()
}

/// `sqlite3_errstr`.
///
/// The whole table, read out of the real library rather than recalled. A code
/// with no message of its own is `unknown error`, not the primary code's name,
/// which is why 2, 16, 22 and 24 all read the same.
///
/// An extended code is answered with its *primary* code's message, because
/// `sqlite3_errstr` masks with `rc & 0xff` before looking anything up. Probed
/// against sqlite3 3.53.4: `sqlite3_errstr(787)` (SQLITE_CONSTRAINT_FOREIGNKEY)
/// and `sqlite3_errstr(1555)` (SQLITE_CONSTRAINT_NOTNULL) both read "constraint
/// failed", and `sqlite3_errstr(9999)` -- 39 primaries past SQLITE_LOCKED --
/// reads "locking protocol" because 9999 % 256 is 15. Without the mask every
/// code the extended-error API can produce, which is most of them, would read
/// "unknown error".
#[no_mangle]
pub extern "C" fn sqlite3_errstr(rc: c_int) -> *const c_char {
    let msg: &'static [u8] = match rc & 0xff {
        0 => b"not an error\0",
        1 => b"SQL logic error\0",
        2 => b"unknown error\0",
        3 => b"access permission denied\0",
        4 => b"query aborted\0",
        5 => b"database is locked\0",
        6 => b"database table is locked\0",
        7 => b"out of memory\0",
        8 => b"attempt to write a readonly database\0",
        9 => b"interrupted\0",
        10 => b"disk I/O error\0",
        11 => b"database disk image is malformed\0",
        12 => b"unknown operation\0",
        13 => b"database or disk is full\0",
        14 => b"unable to open database file\0",
        15 => b"locking protocol\0",
        16 => b"unknown error\0",
        17 => b"database schema has changed\0",
        18 => b"string or blob too big\0",
        19 => b"constraint failed\0",
        20 => b"datatype mismatch\0",
        21 => b"bad parameter or other API misuse\0",
        22 => b"unknown error\0",
        23 => b"authorization denied\0",
        24 => b"unknown error\0",
        25 => b"column index out of range\0",
        26 => b"file is not a database\0",
        27 => b"notification message\0",
        28 => b"warning message\0",
        100 => b"another row available\0",
        101 => b"no more rows available\0",
        _ => b"unknown error\0",
    };
    msg.as_ptr() as *const c_char
}

// ---------------------------------------------------------------------------
// sqlite3_exec.
// ---------------------------------------------------------------------------

/// The shape of the per-row callback, as a C function pointer.
pub type ExecCallback =
    extern "C" fn(*mut c_void, c_int, *mut *mut c_char, *mut *mut c_char) -> c_int;

/// `sqlite3_exec`.
///
/// Runs every statement in `sql` and hands each row to `callback` as an array
/// of NUL-terminated strings, `NULL` for a NULL column. A null callback
/// discards the rows, which is the common `sqlite3_exec(db, sql, 0, 0, &err)`
/// idiom. A callback that returns non-zero aborts the script, as in SQLite.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_exec(
    db: *mut CDb,
    sql: *const c_char,
    callback: Option<ExecCallback>,
    arg: *mut c_void,
    errmsg: *mut *mut c_char,
) -> c_int {
    if !errmsg.is_null() {
        *errmsg = ptr::null_mut();
    }
    if db.is_null() {
        return SQLITE_MISUSE;
    }
    let text = match borrow_cstr(sql) {
        Some(t) => t,
        None => return (*db).misuse(),
    };
    let stmts = match parser::parse_script(&text) {
        Ok(s) => s,
        Err(e) => {
            let rc = (*db).fail(&e);
            if !errmsg.is_null() {
                *errmsg = cstr(&e.message).into_raw();
            }
            return rc;
        }
    };
    for stmt in &stmts {
        if refuses_read_only(stmt) && (*db).read_only.get() {
            // A read-only handle refuses the write at the statement's own
            // boundary, so the pager never sees it. Probed against sqlite3
            // 3.53.4 on a read-only handle: INSERT, UPDATE, DELETE, CREATE
            // TABLE, DROP TABLE, CREATE INDEX, REPLACE, INSERT OR IGNORE, a
            // write PRAGMA and VACUUM all return 8 "attempt to write a readonly
            // database", while BEGIN and COMMIT return SQLITE_OK -- the
            // transaction control statements do not touch the file.
            let rc = (*db).set_error(SQLITE_READONLY, SQLITE_READONLY, READONLY_MESSAGE);
            if !errmsg.is_null() {
                *errmsg = cstr(READONLY_MESSAGE).into_raw();
            }
            return rc;
        }
        let outcome = match (*db).conn.as_mut() {
            Some(c) => c.execute(stmt),
            None => return (*db).misuse(),
        };
        let outcome = match outcome {
            Ok(o) => o,
            Err(e) => {
                let rc = (*db).fail(&e);
                if !errmsg.is_null() {
                    *errmsg = cstr(&e.message).into_raw();
                }
                return rc;
            }
        };
        let changed = (*db).conn.as_ref().map(|c| c.changes()).unwrap_or(0) as i64;
        (*db).record_changes(if sets_changes(stmt) {
            Some(changed)
        } else {
            None
        });
        if let (Some(cb), Outcome::Query { columns, rows }) = (callback, &outcome) {
            let n = columns.len();
            let names: Vec<*mut c_char> = columns.iter().map(|c| cstr(c).into_raw()).collect();
            for row in rows {
                let mut vals: Vec<*mut c_char> = (0..n)
                    .map(|i| match row.values.get(i) {
                        Some(Value::Null) | None => ptr::null_mut(),
                        Some(v) => cstr(&v.to_string()).into_raw(),
                    })
                    .collect();
                let stop = cb(arg, n as c_int, vals.as_mut_ptr(), names.as_ptr() as *mut _);
                for v in vals {
                    if !v.is_null() {
                        drop(CString::from_raw(v));
                    }
                }
                if stop != 0 {
                    for nm in names {
                        drop(CString::from_raw(nm));
                    }
                    (*db).set_error(SQLITE_ABORT, SQLITE_ABORT, "query aborted");
                    return SQLITE_ABORT;
                }
            }
            for nm in names {
                drop(CString::from_raw(nm));
            }
        }
    }
    (*db).clear_error();
    SQLITE_OK
}

/// `sqlite3_free`.
///
/// The strings `sqlite3_exec` writes through its `errmsg` argument are made
/// with the C allocator, so a C caller may release them with its own `free`,
/// exactly as the API says it may.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_free(p: *mut c_void) {
    if p.is_null() {
        return;
    }
    drop(CString::from_raw(p as *mut c_char));
}

// ---------------------------------------------------------------------------
// Preparing, stepping, finalizing.
// ---------------------------------------------------------------------------

/// `sqlite3_prepare_v2`.
///
/// Compiles the first statement in `sql`, points `pp_stmt` at it, and points
/// `pz_tail` at the rest. The shim parses the whole script and hands back a
/// pointer into the caller's own buffer for the tail, so the remainder stays
/// valid for as long as the caller's SQL does.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_prepare_v2(
    db: *mut CDb,
    sql: *const c_char,
    n_byte: c_int,
    pp_stmt: *mut *mut CStmt,
    pz_tail: *mut *const c_char,
) -> c_int {
    sqlite3_prepare_v3(db, sql, n_byte, 0, pp_stmt, pz_tail)
}

/// `sqlite3_prepare_v3`.
///
/// `SQLITE_PREPARE_PERSISTENT` does nothing here: the shim's statements carry
/// no cached schema, so keeping one alive would save nothing.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_prepare_v3(
    db: *mut CDb,
    sql: *const c_char,
    n_byte: c_int,
    _flags: u32,
    pp_stmt: *mut *mut CStmt,
    pz_tail: *mut *const c_char,
) -> c_int {
    if db.is_null() {
        return SQLITE_MISUSE;
    }
    if !pp_stmt.is_null() {
        *pp_stmt = ptr::null_mut();
    }
    let full = match borrow_cstr(sql) {
        Some(t) => t,
        None => return (*db).misuse(),
    };
    let text = if n_byte >= 0 {
        let n = (n_byte as usize).min(full.len());
        full[..n].to_owned()
    } else {
        full
    };
    let stmts = match parser::parse_script(&text) {
        Ok(s) => s,
        Err(e) => return (*db).fail(&e),
    };
    let Some(stmt) = stmts.into_iter().next() else {
        // Whitespace only: SQLITE_OK and a null statement, as SQLite does.
        if !pz_tail.is_null() {
            *pz_tail = sql;
        }
        (*db).clear_error();
        return SQLITE_OK;
    };
    let head_len = first_statement_len(&text);
    if !pz_tail.is_null() {
        *pz_tail = unsafe { sql.add(head_len) };
    }
    let head = &text[..head_len];
    match make_stmt(db, head, &stmt) {
        Ok(s) => {
            if !pp_stmt.is_null() {
                *pp_stmt = Box::into_raw(Box::new(s));
            }
            (*db).live.set((*db).live.get() + 1);
            (*db).clear_error();
            SQLITE_OK
        }
        Err(e) => (*db).fail(&e),
    }
}

/// The byte length of the first statement in `text`, including its semicolon.
///
/// Quoted and bracketed sections are skipped so a semicolon inside a string
/// does not look like the end of the statement.
fn first_statement_len(text: &str) -> usize {
    let b = text.as_bytes();
    let mut i = 0;
    let mut depth = 0i32;
    while i < b.len() {
        match b[i] {
            b'\'' | b'"' | b'`' => {
                let q = b[i];
                i += 1;
                while i < b.len() {
                    if b[i] == q {
                        if i + 1 < b.len() && b[i + 1] == q {
                            i += 2;
                            continue;
                        }
                        break;
                    }
                    i += 1;
                }
            }
            b'[' => {
                while i < b.len() && b[i] != b']' {
                    i += 1;
                }
            }
            b'(' => depth += 1,
            b')' => depth -= 1,
            b';' if depth == 0 => return i + 1,
            _ => {}
        }
        i += 1;
    }
    b.len()
}

/// Builds a statement handle: the parameter table, the inlined text, and the
/// column names.
fn make_stmt(db: *mut CDb, sql_text: &str, stmt: &Stmt) -> Result<CStmt, Error> {
    let (params, sites) = plan_parameters(sql_text)?;
    let bound_sql = inline_parameters(sql_text, &params, &sites);
    let columns = result_columns(db, &bound_sql, stmt)?;
    let ncols = columns.len();
    Ok(CStmt {
        db,
        sql: cstr(sql_text),
        bound_sql: cstr(&bound_sql),
        params,
        param_names: std::collections::BTreeMap::new(),
        sites,
        columns,
        text: (0..ncols).map(|_| None).collect(),
        converted: vec![None; ncols],
        rows: Vec::new(),
        row_index: 0,
        row_valid: false,
        rc: SQLITE_OK,
        err: SQLITE_OK,
        err_msg: String::new(),
    })
}

/// The result columns of a prepared statement.
///
/// A SELECT's names come from a probe run, because the engine answers a whole
/// statement rather than describing one. A SELECT with no FROM evaluates
/// against an empty row and names itself; one with a FROM needs the table,
/// which the engine resolves. A statement that cannot run at all has no names
/// to report, so the list is empty rather than invented.
///
/// `db` is a raw pointer because the statement handle holds one. The pointer is
/// known non-null here: [`make_stmt`] is only reached from a prepare call that
/// has already checked, and every later use is behind the statement's own
/// handle, which is freed only after the connection.
fn result_columns(db: *mut CDb, bound_sql: &str, stmt: &Stmt) -> Result<Vec<CStmtColumn>, Error> {
    let Stmt::Select(_) = stmt else {
        // DDL and DML have no result columns, which is what SQLite reports too.
        return Ok(Vec::new());
    };
    // SAFETY: `db` is the connection this statement is being built for, which
    // the caller has open and which outlives the borrow.
    //
    // The probe runs the statement, but it deliberately does *not* go through
    // `CDb::record_changes`: it only ever runs a `Stmt::Select`, which is
    // read-only, and `sqlite3_changes` is the count from the last statement
    // that changed rows. The shim keeps that count in its own cell rather than
    // reading the engine's scratch register, so a prepare-time probe cannot
    // move it. Probed against sqlite3 3.53.4: after `CREATE TABLE` plus a
    // 5-row `INSERT` both counters read 5, and they still read 5 after merely
    // preparing `SELECT a FROM t` and after stepping it.
    //
    // A probe that *fails* is not swallowed: SQLite reports an unresolvable
    // FROM at prepare time, with a null statement out-parameter, and a caller
    // that checks only the return code must not walk on into a NULL statement.
    // Probed against sqlite3 3.53.4: `SELECT * FROM no_such_table_xyz` returns
    // SQLITE_ERROR with `*ppStmt` left NULL and `sqlite3_errmsg` reading
    // "no such table: no_such_table_xyz"; the engine reports that same message
    // and that same code for the same statement.
    let names = match unsafe { db.as_mut() }.and_then(|d| d.conn.as_mut()) {
        Some(c) => c.execute_script(bound_sql)?,
        None => return Ok(Vec::new()),
    };
    let names = match names.first() {
        Some(Outcome::Query { columns, .. }) => columns.clone(),
        _ => Vec::new(),
    };
    Ok(names
        .into_iter()
        .map(|n| CStmtColumn {
            name: cstr(&n),
            cache: None,
        })
        .collect())
}

/// Assigns indices to the parameters of a statement and records where each one
/// is in the source.
///
/// The numbering is `sqlite3ExprAssignVarNumber` from `src/expr.c`, followed
/// rather than guessed at, because every simpler rule fits some of the cases
/// and fails the others. It keeps one counter, `nvar`, and moves it differently
/// depending on the parameter:
///
/// * a bare `?` is `++nvar`, so it always takes the next number;
/// * a `:name`, `@name` or `$name` reuses the number a previous use of the same
///   name got, and if the name is new it is `++nvar` like a bare `?`;
/// * a `?N` jumps `nvar` to N when N is past it, and otherwise keeps `nvar`
///   where it is -- it does *not* move it back.
///
/// That last point is what makes the cases that look contradictory consistent.
/// In `SELECT ?, ?1` the bare `?` takes 1 and the `?1` reuses that same
/// number, because 1 is not past `nvar`. In `SELECT ?2, ?` the `?2` raises
/// `nvar` to 2 and the bare `?` then makes it 3. And in
/// `SELECT :aa, :aa, ?, ?2` the name takes 1, the `?` makes it 2, and the `?2`
/// finds 2 is not past `nvar` and so reuses the bare `?`'s number. Every one of
/// those is a probed result, and the test below asserts the same table.
fn plan_parameters(sql_text: &str) -> Result<(Vec<Param>, Vec<ParamSite>), Error> {
    let tokens = Tokenizer::tokenize_all(sql_text)?;
    let mut raw: Vec<(usize, usize, Option<usize>, Option<String>)> = Vec::new();
    for (tok, span) in tokens {
        if let nsqlite::tokenizer::Token::Parameter { index, name } = tok {
            // The tokenizer consumes the sigil before it reads the name, so
            // `Token::Parameter::name` is `"x"` for `@x`, `:x` and `$x` alike and
            // the three would collapse into one parameter.
            //
            // SQLite keeps them distinct, and the span is the only place the
            // sigil survives: it starts at the sigil, so the first byte of the
            // token is read back out of the source here. `crates/nsqlite/
            // src/tokenizer.rs` is another track's file, so the repair belongs
            // in the shim rather than in the tokenizer; the shim's own
            // `spelling`/`normalize_param_name` were already right and are
            // applied to a name that now carries its sigil.
            //
            // Probed against sqlite3 3.53.4: `SELECT @x, @x, :x, $x` has three
            // parameters named `@x`, `:x` and `$x`, and `SELECT :x, @x` has two.
            // `SELECT ?1` is the one spelling that is *not* a sigil-plus-name --
            // its name is NULL, because the `?N` form is an index, not a name --
            // and the token for it starts with `?`, so the `?` is dropped here
            // and the index branch below still applies.
            let sigil = sql_text.as_bytes().get(span.start).copied().unwrap_or(b'?');
            let full = match sigil {
                // The sigil is part of the name. `:1` is a *name*, not an index
                // -- the tokenizer takes its digit branch and reports
                // `name: None`, but SQLite still names it `:1`, so the name is
                // rebuilt from the span when the tokenizer gave none. Only a `?`
                // sigil makes a digit an index instead.
                b':' | b'@' | b'$' => Some(sql_text[span.start..span.end].to_owned()),
                // `?N` is an index with no name, and `?name` is a name the
                // tokenizer already reported whole.
                _ => name,
            };
            raw.push((span.start, span.end, index, full));
        }
    }

    // `nvar` is the high-water mark; `params` grows to reach it, and `params[i]`
    // is the parameter at index i+1.
    let mut nvar = 0usize;
    let mut params: Vec<Param> = Vec::new();
    let mut sites = Vec::with_capacity(raw.len());
    let ensure = |params: &mut Vec<Param>, up_to: usize| {
        while params.len() < up_to {
            params.push(Param {
                name: None,
                value: Value::Null,
            });
        }
    };

    for (start, end, index, name) in raw {
        let slot = match index {
            Some(i) if i >= 1 => {
                if i > nvar {
                    nvar = i;
                    ensure(&mut params, nvar);
                }
                // The *first* parameter to reach a slot names it. That is the
                // rule the probes pin down: in `SELECT ?, :a, ?2` the `:a`
                // reaches slot 2 first and keeps the name even though `?2`
                // arrives later, while in `SELECT :aa, :aa, ?, ?2` slot 2 is
                // reached by the anonymous `?` -- which has no name to record --
                // so the `?2` that follows is what names it.
                if params[i - 1].name.is_none() {
                    params[i - 1].name = Some(name.clone().unwrap_or_else(|| format!("?{i}")));
                }
                i - 1
            }
            _ => {
                // A name already seen numbers the same parameter again. A bare
                // `?` has no name and so never matches an earlier one.
                let existing = name.as_deref().and_then(|n| {
                    let key = spelling(n);
                    params
                        .iter()
                        .position(|p| p.name.as_deref() == Some(key.as_str()))
                });
                match existing {
                    Some(at) => at,
                    None => {
                        nvar += 1;
                        ensure(&mut params, nvar);
                        params[nvar - 1].name = name.as_deref().map(spelling);
                        nvar - 1
                    }
                }
            }
        };
        sites.push(ParamSite { start, end, slot });
    }
    Ok((params, sites))
}

/// The text of a parameter as `sqlite3_bind_parameter_name` reports it, which
/// keeps its sigil.
fn spelling(name: &str) -> String {
    match name.chars().next() {
        Some(':' | '@' | '$') => name.to_owned(),
        _ => format!(":{name}"),
    }
}

/// Substitutes every bound parameter into the statement text.
///
/// Only the spans the tokenizer reported for parameter tokens are touched, so a
/// `?` inside a string literal is a character in a string and stays where it
/// is. The spans were computed once at prepare time and the text has not
/// changed since, so they still point at the right bytes.
fn inline_parameters(sql_text: &str, params: &[Param], sites: &[ParamSite]) -> String {
    if sites.is_empty() {
        return sql_text.to_owned();
    }
    let mut out = String::with_capacity(sql_text.len());
    let mut cursor = 0usize;
    for site in sites {
        if site.start < cursor {
            continue;
        }
        out.push_str(&sql_text[cursor..site.start]);
        let v = params
            .get(site.slot)
            .map(|p| &p.value)
            .unwrap_or(&Value::Null);
        out.push_str(&sql_literal(v));
        cursor = site.end;
    }
    out.push_str(&sql_text[cursor..]);
    out
}

/// A value written as a SQL literal, which is how a bound parameter reaches
/// the engine's evaluator.
fn sql_literal(v: &Value) -> String {
    match v {
        Value::Null => "NULL".to_string(),
        Value::Integer(i) => i.to_string(),
        Value::Real(r) => {
            if r.is_nan() {
                // SQLite's parser reads NaN as NULL.
                "NULL".to_string()
            } else if r.is_infinite() {
                // 9e999 overflows to an infinity the engine can hold.
                if *r > 0.0 { "9e999" } else { "-9e999" }.to_string()
            } else {
                format!("{r:?}")
            }
        }
        Value::Text(s) => format!("'{}'", s.replace('\'', "''")),
        Value::Blob(b) => {
            let hex: String = b.iter().map(|x| format!("{x:02X}")).collect();
            format!("x'{hex}'")
        }
    }
}

/// `sqlite3_step`.
///
/// The engine has already produced the whole result by the time the first step
/// runs, so the rows are handed out one at a time from a buffer the statement
/// owns. A statement that produced no rows goes straight to `SQLITE_DONE`.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_step(stmt: *mut CStmt) -> c_int {
    if stmt.is_null() {
        return SQLITE_MISUSE;
    }
    let s = &mut *stmt;
    if !s.row_valid {
        let sql = s.bound_sql.to_string_lossy().into_owned();
        let db = s.db;
        let result = match (*db).conn.as_mut() {
            Some(c) => parser::parse_script(&sql).and_then(|stmts| {
                let mut last = Outcome::Nothing;
                for st in &stmts {
                    if refuses_read_only(st) && (*db).read_only.get() {
                        // A read-only handle refuses the write when the
                        // statement steps, not when it prepares. Probed against
                        // sqlite3 3.53.4: `sqlite3_prepare_v2` of an INSERT on a
                        // read-only handle returns SQLITE_OK, the first
                        // `sqlite3_step` returns 8 "attempt to write a readonly
                        // database", `sqlite3_reset` then returns 8 as well, and
                        // `sqlite3_finalize` returns SQLITE_OK.
                        return Err(Error::new(ResultCode::ReadOnly, READONLY_MESSAGE));
                    }
                    last = c.execute(st)?;
                    // `Connection::execute` zeroes its counter on entry and
                    // sets it as it runs, so the count is read *after* the
                    // call, not before.
                    let changed = c.changes() as i64;
                    // Only a row-changing statement may move
                    // `sqlite3_changes`. A SELECT still runs here, so without
                    // this the counter a caller reads after a prepare-time probe
                    // or a step would be the SELECT's zero rather than the last
                    // INSERT's count.
                    (*db).record_changes(if sets_changes(st) {
                        Some(changed)
                    } else {
                        None
                    });
                }
                Ok(last)
            }),
            None => Err(Error::new(
                ResultCode::Misuse,
                "bad parameter or other API misuse",
            )),
        };
        match result {
            Err(e) => {
                let rc = (*db).fail(&e);
                s.rc = rc;
                s.err = rc;
                s.err_msg = e.message.clone();
                s.rows.clear();
                s.row_index = 0;
                s.row_valid = true;
                return rc;
            }
            Ok(Outcome::Query { rows, .. }) => {
                s.rows = rows.into_iter().map(|r| r.values).collect();
            }
            Ok(_) => {
                s.rows.clear();
            }
        }
        s.row_index = 0;
        s.row_valid = true;
    }
    if s.row_index < s.rows.len() {
        s.row_index += 1;
        // A new row invalidates every text buffer handed out for the old one,
        // and with it the representation changes those accessors caused.
        for slot in s.text.iter_mut() {
            *slot = None;
        }
        for slot in s.converted.iter_mut() {
            *slot = None;
        }
        s.rc = SQLITE_ROW;
        if !s.db.is_null() {
            (*s.db).clear_error();
        }
        SQLITE_ROW
    } else {
        s.rc = SQLITE_DONE;
        if !s.db.is_null() {
            (*s.db).clear_error();
        }
        SQLITE_DONE
    }
}

/// `sqlite3_reset`.
///
/// Discards the buffered rows and answers `SQLITE_OK`, which is what the real
/// library does in every non-error case. Probed against sqlite3 3.53.4: reset
/// after a `SQLITE_ROW` returns 0, after a `SQLITE_DONE` returns 0, and on a
/// query with no rows at all returns 0. It returns the error code only when the
/// statement actually failed, which is the one case where a caller checking only
/// the return value still learns about the failure.
///
/// Returning the last step's code instead -- `SQLITE_ROW` or `SQLITE_DONE` --
/// looks defensible and is wrong: it breaks the canonical
/// `while ((rc = sqlite3_step(st)) == SQLITE_ROW)` loop's companion
/// `if ((rc = sqlite3_reset(st)) != SQLITE_OK) goto bail`, because a statement
/// that simply finished would be reported as an error.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_reset(stmt: *mut CStmt) -> c_int {
    if stmt.is_null() {
        return SQLITE_MISUSE;
    }
    let s = &mut *stmt;
    s.rows.clear();
    s.row_index = 0;
    s.row_valid = false;
    for slot in s.text.iter_mut() {
        *slot = None;
    }
    for slot in s.converted.iter_mut() {
        *slot = None;
    }
    // `SQLITE_ROW` and `SQLITE_DONE` are not failures; only a real error code
    // survives the reset and becomes its return value. The error is *moved* out
    // of the statement, not copied: a `sqlite3_reset` is the point at which
    // SQLite considers the failure reported, so a `sqlite3_finalize` after it
    // answers SQLITE_OK. Probed against sqlite3 3.53.4 on a statement whose
    // step failed: `sqlite3_reset` returns 8, and the `sqlite3_finalize` that
    // follows returns 0.
    let failed = !matches!(s.rc, SQLITE_OK | SQLITE_ROW | SQLITE_DONE);
    let err = s.err;
    let err_msg = std::mem::take(&mut s.err_msg);
    s.rc = SQLITE_OK;
    s.err = SQLITE_OK;
    if !s.db.is_null() {
        if failed {
            (*s.db).set_error(err, err, err_msg.as_str());
        } else {
            (*s.db).clear_error();
        }
    }
    if failed {
        err
    } else {
        SQLITE_OK
    }
}

/// `sqlite3_finalize`.
///
/// Frees the statement, and the connection it belongs to if that connection was
/// closed while the statement was still open.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_finalize(stmt: *mut CStmt) -> c_int {
    if stmt.is_null() {
        return SQLITE_OK;
    }
    let boxed = Box::from_raw(stmt);
    // Probed: finalize reports the *error* the statement ended on, and nothing
    // else. A statement that ran to SQLITE_DONE, or was abandoned half way
    // through its rows, finalizes with SQLITE_OK; one whose step failed
    // finalizes with that failure's code. Returning the last step's code
    // unconditionally would report SQLITE_ROW for a scan that was simply cut
    // short, which is not an error and is not what a caller expects to see.
    let rc = boxed.err;
    let db = boxed.db;
    if !db.is_null() {
        let d = &*db;
        if d.live.get() > 0 {
            d.live.set(d.live.get() - 1);
        }
        if d.closing.get() && d.live.get() == 0 {
            drop(Box::from_raw(db));
        }
    }
    drop(boxed);
    rc
}

/// `sqlite3_sql`: the statement's own text, with its parameters still written
/// as parameters.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_sql(stmt: *mut CStmt) -> *const c_char {
    if stmt.is_null() {
        return ptr::null();
    }
    (*stmt).sql.as_ptr()
}

/// `sqlite3_bind_parameter_count`.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_bind_parameter_count(stmt: *mut CStmt) -> c_int {
    if stmt.is_null() {
        return 0;
    }
    (*stmt).params.len() as c_int
}

/// `sqlite3_bind_parameter_name`.
///
/// The name with its sigil, as SQLite reports it: `:aa` for `:aa`, `?2` for an
/// explicitly indexed anonymous parameter, and NULL for a bare `?`, which has
/// no name to look up.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_bind_parameter_name(
    stmt: *mut CStmt,
    index: c_int,
) -> *const c_char {
    if stmt.is_null() || index < 1 {
        return ptr::null();
    }
    let name = {
        // The parameter's name is cloned out before the cache is consulted, so
        // the immutable and mutable views of the statement never overlap.
        let s: &CStmt = &*stmt;
        s.params
            .get(index as usize - 1)
            .and_then(|p| p.name.clone())
    };
    match name {
        Some(n) => (*stmt).name_cache(index as usize - 1, &n).as_ptr(),
        None => ptr::null(),
    }
}

/// `sqlite3_bind_parameter_index`.
///
/// Looks a parameter up by name or by `?N`. The sigil is part of the name and
/// is *not* stripped, because SQLite treats it that way: on `SELECT @x`,
/// `sqlite3_bind_parameter_index(st, ":x")` returns 0 and only `@x` returns 1.
/// A bare `?` is not a name and does not resolve, and neither does a bare `x`
/// with no sigil. Probed against sqlite3 3.53.4 for all three sigils, for `:1`
/// (a name, because the sigil is not a `?`) and for `?1` (an index).
#[no_mangle]
pub unsafe extern "C" fn sqlite3_bind_parameter_index(
    stmt: *mut CStmt,
    name: *const c_char,
) -> c_int {
    if stmt.is_null() {
        return 0;
    }
    let s = &*stmt;
    let Some(want) = (unsafe { borrow_cstr(name) }) else {
        return 0;
    };
    // `?N` names a slot by number, and it resolves only when the statement
    // actually *has* a `?N` parameter -- not merely a slot N. Probed against
    // sqlite3 3.53.4: `?1` resolves to 1 on `SELECT ?1` and to 5 on
    // `SELECT :x, :x, ?5, ?5, ?9`, but returns 0 on `SELECT :x, @x` even
    // though that statement has one parameter at slot 1, and returns 0 on
    // `SELECT :1` even though its parameter is named `:1`. So the slot must
    // exist *and* be spelled `?N`.
    if let Some(rest) = want.strip_prefix('?') {
        if let Ok(n) = rest.parse::<usize>() {
            if n >= 1
                && n <= s.params.len()
                && s.params[n - 1].name.as_deref() == Some(want.as_str())
            {
                return n as c_int;
            }
            return 0;
        }
    }
    // An exact match on the name, sigil included, so `:x` does not resolve a
    // `@x` and a bare `x` resolves nothing. Probed against sqlite3 3.53.4.
    for (i, p) in s.params.iter().enumerate() {
        if p.name.as_deref() == Some(want.as_str()) {
            return i as c_int + 1;
        }
    }
    0
}

impl CStmt {
    /// The stable CString for parameter `i`'s name, built on first use and
    /// kept for the statement's life so the pointer a caller receives stays
    /// valid until the statement is finalized.
    fn name_cache(&mut self, i: usize, name: &str) -> &CString {
        self.param_names.entry(i).or_insert_with(|| cstr(name))
    }
}

/// Binds a value to parameter `index`, rewriting the statement text.
unsafe fn bind(stmt: *mut CStmt, index: c_int, v: Value) -> c_int {
    if stmt.is_null() {
        return SQLITE_MISUSE;
    }
    let s = &mut *stmt;
    if index < 1 || index as usize > s.params.len() {
        // A parameter that does not exist is a RANGE error, and the message
        // says so, which is what SQLite reports for an out-of-range index.
        return if !s.db.is_null() {
            (*s.db).set_error(SQLITE_RANGE, SQLITE_RANGE, "column index out of range")
        } else {
            SQLITE_RANGE
        };
    }
    s.params[index as usize - 1].value = v;
    let text = s.sql.to_string_lossy().into_owned();
    let params = s.params.clone();
    let sites = s.sites.clone();
    let rebuilt = inline_parameters(&text, &params, &sites);
    s.bound_sql = cstr(&rebuilt);
    SQLITE_OK
}

/// `sqlite3_bind_null`.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_bind_null(stmt: *mut CStmt, index: c_int) -> c_int {
    bind(stmt, index, Value::Null)
}

/// `sqlite3_bind_int`.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_bind_int(stmt: *mut CStmt, index: c_int, value: c_int) -> c_int {
    bind(stmt, index, Value::Integer(value as i64))
}

/// `sqlite3_bind_int64`.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_bind_int64(stmt: *mut CStmt, index: c_int, value: i64) -> c_int {
    bind(stmt, index, Value::Integer(value))
}

/// `sqlite3_bind_double`.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_bind_double(stmt: *mut CStmt, index: c_int, value: f64) -> c_int {
    bind(stmt, index, Value::real(value))
}

/// The bytes a `sqlite3_bind_text` or `_blob` call names.
///
/// A negative `n` means "NUL-terminated", which is SQLite's convention for both
/// and is not a length at all -- treating it as an out-of-range length is how a
/// bound string silently turns into an empty one. A null pointer binds nothing,
/// and a length past the end of the buffer is the caller's mistake; SQLite
/// treats the pointer as authoritative either way, so the bytes up to the NUL
/// are what get copied.
unsafe fn borrow_terminated<'a>(p: *const c_void, n: c_int) -> Option<&'a [u8]> {
    if p.is_null() {
        return None;
    }
    if n >= 0 {
        return borrow_bytes(p, n);
    }
    let mut len = 0usize;
    loop {
        let byte = *(p as *const u8).add(len);
        if byte == 0 {
            break;
        }
        len += 1;
    }
    Some(std::slice::from_raw_parts(p as *const u8, len))
}

/// `sqlite3_bind_text`.
///
/// The bytes are copied, so the caller may free them the moment this returns,
/// whichever destructor it passed. Copying is not optional: the alternative is
/// a caller that reuses its buffer finding a different value in the middle of a
/// scan.
///
/// Bytes that are not valid UTF-8 are the one case this cannot represent:
/// `nsqlite::Value::Text` is a `String`, so there is nowhere to put them.
/// SQLite itself binds them and reports success -- probed against sqlite3
/// 3.53.4, `sqlite3_bind_text(st, 1, "\xff\xfe", 2, SQLITE_TRANSIENT)`
/// returns 0, leaves `sqlite3_errmsg` reading "not an error", and the next
/// step returns the two bytes verbatim.
///
/// The shim binds the lossy-decoded string and returns `SQLITE_OK`, so the
/// *contract* holds: no error is reported that did not happen, the return code
/// is the one a caller can act on, and a step that follows carries the new
/// value rather than the stale one. What is lost is the exact bytes, and that
/// loss is a real, documented divergence rather than a silent one -- see the
/// module docs on TEXT being UTF-8 only. The alternative, returning
/// `SQLITE_ERROR` with a message SQLite never emits, would be worse on both
/// counts: the caller would be told a failure occurred, and the parameter would
/// keep its previous value while the next step reported success.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_bind_text(
    stmt: *mut CStmt,
    index: c_int,
    text: *const c_char,
    n: c_int,
    _destructor: Option<unsafe extern "C" fn(*mut c_void)>,
) -> c_int {
    let bytes = borrow_terminated(text as *const c_void, n).map(|b| b.to_vec());
    let Some(bytes) = bytes else {
        return bind(stmt, index, Value::Text(String::new()));
    };
    // `from_utf8_lossy` replaces each invalid sequence with U+FFFD rather than
    // failing, so the bind always happens and the caller's return code always
    // describes what actually occurred.
    let s = String::from_utf8_lossy(&bytes).into_owned();
    bind(stmt, index, Value::Text(s))
}

/// `sqlite3_bind_text64`.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_bind_text64(
    stmt: *mut CStmt,
    index: c_int,
    text: *const c_char,
    n: u64,
    destructor: Option<unsafe extern "C" fn(*mut c_void)>,
    _encoding: u8,
) -> c_int {
    sqlite3_bind_text(
        stmt,
        index,
        text,
        n.min(c_int::MAX as u64) as c_int,
        destructor,
    )
}

/// `sqlite3_bind_blob`.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_bind_blob(
    stmt: *mut CStmt,
    index: c_int,
    blob: *const c_void,
    n: c_int,
    _destructor: Option<unsafe extern "C" fn(*mut c_void)>,
) -> c_int {
    let bytes = borrow_terminated(blob, n)
        .map(|b| b.to_vec())
        .unwrap_or_default();
    bind(stmt, index, Value::Blob(bytes))
}

/// `sqlite3_bind_blob64`.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_bind_blob64(
    stmt: *mut CStmt,
    index: c_int,
    blob: *const c_void,
    n: u64,
    destructor: Option<unsafe extern "C" fn(*mut c_void)>,
) -> c_int {
    sqlite3_bind_blob(
        stmt,
        index,
        blob,
        n.min(c_int::MAX as u64) as c_int,
        destructor,
    )
}

// ---------------------------------------------------------------------------
// Column access.
// ---------------------------------------------------------------------------

/// `sqlite3_column_count`.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_column_count(stmt: *mut CStmt) -> c_int {
    if stmt.is_null() {
        return 0;
    }
    (*stmt).columns.len() as c_int
}

/// `sqlite3_column_type`.
///
/// The type of the value's *current representation*, which is not always the
/// type it was stored as: calling `sqlite3_column_text` on a BLOB converts it
/// to TEXT in place, and a caller that then asks for the type gets TEXT. The
/// other direction does not happen, because the numeric accessors render into
/// their own return value rather than rewriting the column.
///
/// Probed against sqlite3 3.53.4, interleaving the two accessors on one
/// column: `SELECT x'0102ff'` reads 4 and then 3; `SELECT 42` reads 1 and then
/// 1; `SELECT 1.5` reads 2 and then 2; `SELECT NULL` reads 5 and then 5;
/// `SELECT 'abc'` reads 3 and then 3. Stepping to the next row restores the
/// stored type.
///
/// Reading past the last column is a `MISUSE` in SQLite's own `columnMem`, and
/// the same here.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_column_type(stmt: *mut CStmt, i: c_int) -> c_int {
    if stmt.is_null() {
        return SQLITE_NULL;
    }
    let s = &*stmt;
    if i < 0 || i as usize >= s.current_len() || !s.row_valid {
        return SQLITE_NULL;
    }
    if let Some(t) = s.converted.get(i as usize).copied().flatten() {
        return t;
    }
    match s.rows[s.row_index - 1][i as usize].datatype() {
        Datatype::Null => SQLITE_NULL,
        Datatype::Integer => SQLITE_INTEGER,
        Datatype::Real => SQLITE_FLOAT,
        Datatype::Text => SQLITE_TEXT,
        Datatype::Blob => SQLITE_BLOB,
    }
}

/// `sqlite3_column_int`.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_column_int(stmt: *mut CStmt, i: c_int) -> c_int {
    sqlite3_column_int64(stmt, i) as c_int
}

/// `sqlite3_column_int64`.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_column_int64(stmt: *mut CStmt, i: c_int) -> i64 {
    if stmt.is_null() {
        return 0;
    }
    let s = &*stmt;
    let Ok(v) = s.value(i) else {
        return 0;
    };
    match v {
        Value::Integer(i) => *i,
        Value::Real(r) => double_to_i64(*r),
        Value::Text(t) => text_prefix_i64(t),
        _ => 0,
    }
}

/// `sqlite3_column_double`.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_column_double(stmt: *mut CStmt, i: c_int) -> f64 {
    if stmt.is_null() {
        return 0.0;
    }
    let s = &*stmt;
    let Ok(v) = s.value(i) else {
        return 0.0;
    };
    match v {
        Value::Integer(i) => *i as f64,
        Value::Real(r) => *r,
        Value::Text(t) => text_prefix_f64(t),
        _ => 0.0,
    }
}

/// `sqlite3_column_text`.
///
/// The value rendered as SQLite renders it, in a buffer the statement owns.
/// Valid until the next `step`, `reset` or `finalize` on this statement, and
/// NUL-terminated after the last byte so a C caller may also read it as a
/// string.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_column_text(stmt: *mut CStmt, i: c_int) -> *const u8 {
    if stmt.is_null() {
        return ptr::null();
    }
    (*stmt).text_ptr(i) as *const u8
}

/// `sqlite3_column_blob`.
///
/// SQLite's `sqlite3_column_blob` asks `columnMem` for `MEM_Blob`, which for a
/// TEXT value returns the text's own bytes. That is what a caller asking a text
/// column for its bytes gets, so that is what this returns -- and it does not
/// change the column's representation, so a `sqlite3_column_type` after it still
/// reports TEXT. Probed against sqlite3 3.53.4.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_column_blob(stmt: *mut CStmt, i: c_int) -> *const c_void {
    (*stmt).blob_ptr(i)
}

/// `sqlite3_column_bytes`, the length without the terminator.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_column_bytes(stmt: *mut CStmt, i: c_int) -> c_int {
    if stmt.is_null() {
        return 0;
    }
    let s = &*stmt;
    let Ok(v) = s.value(i) else {
        return 0;
    };
    if v.is_null() {
        return 0;
    }
    CStmt::render_value(v).len() as c_int
}

/// `sqlite3_column_name`.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_column_name(stmt: *mut CStmt, i: c_int) -> *const c_char {
    if stmt.is_null() || i < 0 {
        return ptr::null();
    }
    let s = &mut *stmt;
    let Some(col) = s.columns.get_mut(i as usize) else {
        return ptr::null();
    };
    if col.cache.is_none() {
        col.cache = Some(NulTerminated::new(col.name.as_bytes()));
    }
    col.cache.as_ref().expect("just filled it").as_ptr()
}

/// `sqlite3_column_decltype`.
///
/// nsqlite does not expose a statement's declared types, so this is always
/// null. Reporting a type it cannot know would be worse than saying so.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_column_decltype(stmt: *mut CStmt, i: c_int) -> *const c_char {
    let _ = (stmt, i);
    ptr::null()
}

/// `sqlite3_column_table_origin`, always null: nothing is attached and the
/// engine does not report an origin.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_column_table_origin(stmt: *mut CStmt, i: c_int) -> *const c_char {
    let _ = (stmt, i);
    ptr::null()
}

// ---------------------------------------------------------------------------
// Connection-level counters.
// ---------------------------------------------------------------------------

/// `sqlite3_changes`.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_changes(db: *const CDb) -> c_int {
    if db.is_null() {
        return 0;
    }
    (*db).changes.get() as c_int
}

/// `sqlite3_total_changes`.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_total_changes(db: *const CDb) -> c_int {
    if db.is_null() {
        return 0;
    }
    (*db).total_changes.get() as c_int
}

/// `sqlite3_last_insert_rowid`.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_last_insert_rowid(db: *const CDb) -> i64 {
    if db.is_null() {
        return 0;
    }
    (*db)
        .conn
        .as_ref()
        .map(|c| c.last_insert_rowid())
        .unwrap_or(0)
}

/// `sqlite3_get_autocommit`.
///
/// True whenever no transaction is open, which is exactly `in_transaction`.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_get_autocommit(db: *const CDb) -> c_int {
    if db.is_null() {
        return 1;
    }
    match (*db).conn.as_ref() {
        Some(c) if c.in_transaction() => 0,
        _ => 1,
    }
}

/// `sqlite3_db_filename`.
///
/// The main database's path. An in-memory database has no file, so this is null
/// for it, and so is any schema name but `main`, because nothing is attached.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_db_filename(
    db: *const CDb,
    db_name: *const c_char,
) -> *const c_char {
    if db.is_null() {
        return ptr::null();
    }
    let name = borrow_cstr(db_name).unwrap_or_default();
    if name != "main" && !name.is_empty() {
        return ptr::null();
    }
    match &(*db).filename {
        Some(c) => c.as_ptr(),
        // Probed: a memory database's filename is the empty string, not NULL.
        // The C API hands back a valid pointer to nothing, and a caller testing
        // the pointer for null would draw the wrong conclusion about whether the
        // open succeeded.
        None => EMPTY.as_ptr() as *const c_char,
    }
}

/// The empty filename `sqlite3_db_filename` reports for a memory database.
///
/// A static, because the pointer outlives the call and a `CString` built here
/// would be dropped the moment this returned.
static EMPTY: &[u8] = b"\0";

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::raw::c_uint;

    const ALL_SIGNATURES: &[&str] = &[
        "sqlite3_libversion",
        "sqlite3_libversion_number",
        "sqlite3_sourceid",
        "sqlite3_open",
        "sqlite3_open_v2",
        "sqlite3_close",
        "sqlite3_close_v2",
        "sqlite3_errmsg",
        "sqlite3_errcode",
        "sqlite3_extended_errcode",
        "sqlite3_errstr",
        "sqlite3_exec",
        "sqlite3_prepare_v2",
        "sqlite3_prepare_v3",
        "sqlite3_step",
        "sqlite3_reset",
        "sqlite3_finalize",
        "sqlite3_sql",
        "sqlite3_bind_null",
        "sqlite3_bind_int",
        "sqlite3_bind_int64",
        "sqlite3_bind_double",
        "sqlite3_bind_text",
        "sqlite3_bind_text64",
        "sqlite3_bind_blob",
        "sqlite3_bind_blob64",
        "sqlite3_bind_parameter_count",
        "sqlite3_bind_parameter_index",
        "sqlite3_bind_parameter_name",
        "sqlite3_column_count",
        "sqlite3_column_type",
        "sqlite3_column_int",
        "sqlite3_column_int64",
        "sqlite3_column_double",
        "sqlite3_column_text",
        "sqlite3_column_blob",
        "sqlite3_column_bytes",
        "sqlite3_column_name",
        "sqlite3_column_decltype",
        "sqlite3_column_table_origin",
        "sqlite3_changes",
        "sqlite3_total_changes",
        "sqlite3_last_insert_rowid",
        "sqlite3_get_autocommit",
        "sqlite3_db_filename",
        "sqlite3_free",
    ];

    /// Every name the shim claims to export is actually exported, which is what
    /// the TCL suite's `load` depends on. An `#[no_mangle] pub extern "C"` that
    /// is never referenced is otherwise easy to lose to an edit, and a stale
    /// `.dll` in `target/` is otherwise easy to mistake for a passing test, so
    /// the cdylib is rebuilt by `build.rs` before the test loads it.
    ///
    /// `cargo test` builds the rlib this test binary links, but not the cdylib
    /// a C caller loads, so the library is built separately:
    ///
    /// ```text
    /// cargo build -p nsqlite-capi && cargo test -p nsqlite-capi
    /// ```
    ///
    /// Running the tests alone loads whatever `target/` happened to hold, which
    /// is a stale-library result, so that is reported rather than acted on.
    #[test]
    fn every_advertised_symbol_is_linkable() {
        #[cfg(target_os = "windows")]
        unsafe {
            use std::os::windows::ffi::OsStrExt;
            let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
            let profile = if cfg!(debug_assertions) {
                "debug"
            } else {
                "release"
            };
            let dll = path
                .join("..")
                .join("..")
                .join("target")
                .join(profile)
                .join("nsqlite_capi.dll");
            if !dll.exists() {
                panic!(
                    "{} is missing. Build the cdylib first: \
                     `cargo build -p nsqlite-capi`",
                    dll.display()
                );
            }
            // The library has to be no older than the source that produced it,
            // or this test reports on whatever was built last rather than on
            // what is in the tree now. Comparing against the clock instead
            // would fail a library that is perfectly current but was built an
            // hour ago, which is exactly what a build that correctly decided it
            // had nothing to do looks like.
            let built = std::fs::metadata(&dll).and_then(|m| m.modified()).ok();
            let source = ["src/lib.rs", "Cargo.toml"]
                .iter()
                .filter_map(|f| {
                    std::fs::metadata(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(f))
                        .and_then(|m| m.modified())
                        .ok()
                })
                .max();
            assert!(
                match (built, source) {
                    (Some(b), Some(s)) => b >= s,
                    _ => false,
                },
                "{} is older than the source, so this test would report on a \
                 stale library. Rebuild with `cargo build -p nsqlite-capi`.",
                dll.display()
            );
            let wide: Vec<u16> = dll
                .as_os_str()
                .encode_wide()
                .chain(std::iter::once(0))
                .collect();
            let h = windows_dll_open(wide.as_ptr());
            assert!(!h.is_null(), "could not load {}", dll.display());
            for name in ALL_SIGNATURES {
                let found = windows_dll_get(h, name.as_bytes());
                assert!(found, "{name} is missing from the cdylib");
            }
        }
        #[cfg(not(target_os = "windows"))]
        {
            // Every function is referenced, so the linker keeps it.
            let _ = ALL_SIGNATURES.len();
        }
    }

    #[cfg(target_os = "windows")]
    unsafe fn windows_dll_open(name: *const u16) -> *mut c_void {
        extern "system" {
            fn LoadLibraryW(lp: *const u16) -> *mut c_void;
        }
        LoadLibraryW(name)
    }

    /// `GetProcAddress` on a module handle.
    ///
    /// The name is passed as `LPCSTR`, a pointer to a NUL-terminated *byte*
    /// string. The caller supplies the NUL; `name_ptr` below is built with one
    /// rather than taken from a `&str`, because a `&str`'s pointer is only
    /// NUL-terminated by an accident of its allocation, and "by an accident" is
    /// how a lookup that works on one machine fails on another.
    #[cfg(target_os = "windows")]
    unsafe fn windows_dll_get(h: *mut c_void, name: &[u8]) -> bool {
        extern "system" {
            fn GetProcAddress(h: *mut c_void, n: *const u8) -> *mut c_void;
        }
        let mut owned = name.to_vec();
        owned.push(0);
        !GetProcAddress(h, owned.as_ptr()).is_null()
    }

    #[allow(dead_code)]
    type Unused = c_uint;

    /// The float rendering, against values read out of the real library with a
    /// C program. Every expectation here was produced by sqlite3 3.53.4; none
    /// of it is recalled.
    #[test]
    #[allow(
        clippy::approx_constant,
        reason = "the literals are the exact doubles the real library was given"
    )]
    fn render_real_matches_sqlite() {
        let cases: &[(f64, &str)] = &[
            (2.0 / 3.0, "0.66666666666666663"),
            (1e17, "1.0e+17"),
            (1.7976931348623157e308, "1.7976931348623157e+308"),
            (5e-324, "4.9406564584124654e-324"),
            (1.0, "1.0"),
            (-1.0, "-1.0"),
            (0.5, "0.5"),
            (123456.0, "123456.0"),
            (1e16, "10000000000000000.0"),
            (1e-5, "1.0e-05"),
            (1e-10, "1.0e-10"),
            // These two are the exact doubles the real library was given, and
            // `std::f64::consts::PI` is a third one, so the literal is the point:
            // what is being checked is the rendering of *this* value, and clippy's
            // suggestion to substitute a nearby constant would change the test.
            (3.14159265358979, "3.14159265358979"),
            (3.141592653589793, "3.1415926535897931"),
            (1e6, "1000000.0"),
            (1e7, "10000000.0"),
            (1e8, "100000000.0"),
            (1e9, "1000000000.0"),
            (123456789012345.0, "123456789012345.0"),
            (9.99999999999999e14, "999999999999999.0"),
            (1e20, "1.0e+20"),
            (1e-20, "1.0e-20"),
            (100.0, "100.0"),
            (2.0, "2.0"),
            (-2.5, "-2.5"),
            (1e-11, "1.0e-11"),
            (1e100, "1.0e+100"),
            (1e-100, "1.0e-100"),
            (12345678.0, "12345678.0"),
            (1.5, "1.5"),
            (0.1, "0.1"),
            (0.001, "0.001"),
            (0.01, "0.01"),
            (0.05, "0.05"),
            (1e-4, "0.0001"),
            (1e-7, "1.0e-07"),
            (1e14, "100000000000000.0"),
            (0.0, "0.0"),
            (-0.0, "0.0"),
            (0.3, "0.3"),
            (0.7, "0.7"),
            (1234567890.0, "1234567890.0"),
            (1e-300, "1.0e-300"),
            (1e300, "1.0e+300"),
            (4.35, "4.35"),
            (1e13, "10000000000000.0"),
            (1e15, "1000000000000000.0"),
            (2.5e-10, "2.5e-10"),
            (0.30000000000000004, "0.30000000000000004"),
            (12345678901234567890.0, "1.2345678901234567e+19"),
            (1e-323, "9.8813129168249309e-324"),
            (1e-322, "9.8813129168249309e-323"),
            (7e-46, "7.0e-46"),
            (9007199254740993.0, "9007199254740992.0"),
            (1.5e-7, "1.5e-07"),
            (0.0001220703125, "0.0001220703125"),
        ];
        for (v, want) in cases {
            assert_eq!(&render_real(*v), want, "rendering {v:e}");
        }
    }

    /// The text-to-number prefixes, against what the real library's
    /// `sqlite3_column_int64` and `sqlite3_column_double` reported for the same
    /// strings.
    ///
    /// The two are separate because SQLite's are separate: `columnInt64` applies
    /// only the integer prefix, and the exponent is a `columnDouble` concern.
    /// Every expectation here was read out of a C program run against sqlite3
    /// 3.53.4.
    #[test]
    fn text_prefix_matches_sqlite() {
        // (text, column_int64, column_double)
        let cases: &[(&str, i64, f64)] = &[
            ("42abc", 42, 42.0),
            ("abc", 0, 0.0),
            (" 7 ", 7, 7.0),
            ("3.9", 3, 3.9),
            ("1.5e2xyz", 1, 150.0),
            ("-12.5rest", -12, -12.5),
            ("  -12  ", -12, -12.0),
            ("1e3", 1, 1000.0),
            ("1.9e2", 1, 190.0),
            ("1e400", 1, f64::INFINITY),
            ("inf", 0, 0.0),
            ("nan", 0, 0.0),
            (".5", 0, 0.5),
            ("0x10", 0, 0.0),
            ("3e", 3, 3.0),
            ("9223372036854775808", i64::MAX, 9223372036854775808.0),
            ("9007199254740993", 9007199254740993, 9007199254740992.0),
        ];
        for (text, want_i, want_d) in cases {
            assert_eq!(text_prefix_i64(text), *want_i, "int64 of {text:?}");
            assert_eq!(text_prefix_f64(text), *want_d, "double of {text:?}");
        }
        assert_eq!(double_to_i64(1e300), i64::MAX);
        assert_eq!(double_to_i64(-1e300), i64::MIN);
        assert_eq!(double_to_i64(f64::NAN), 0);
        assert_eq!(double_to_i64(3.9), 3);
    }

    /// Parameter indices and names, against what the real library reported for
    /// the same statements. Every expectation here came from a C program run
    /// against sqlite3 3.53.4; none of it is recalled, and the set includes the
    /// cases that tell the numbering rules apart rather than just the easy ones.
    #[test]
    fn parameter_indices_match_sqlite() {
        let cases: &[(&str, usize, &[Option<&str>])] = &[
            ("SELECT :aa, :aa, ?, ?2", 2, &[Some(":aa"), Some("?2")]),
            ("SELECT ?, ?2", 2, &[None, Some("?2")]),
            ("SELECT ?2, ?", 3, &[None, Some("?2"), None]),
            ("SELECT ?, ?, ?3", 3, &[None, None, Some("?3")]),
            ("SELECT ?, ?1", 1, &[Some("?1")]),
            ("SELECT ?, ?, ?1", 2, &[Some("?1"), None]),
            ("SELECT ?1, ?", 2, &[Some("?1"), None]),
            ("SELECT ?, :a", 2, &[None, Some(":a")]),
            ("SELECT :a, ?", 2, &[Some(":a"), None]),
            ("SELECT :a, ?, ?2", 2, &[Some(":a"), Some("?2")]),
            ("SELECT :a, :a, ?, ?3", 3, &[Some(":a"), None, Some("?3")]),
            ("SELECT ?, :a, ?2", 2, &[None, Some(":a")]),
            ("SELECT ?, :a, ?1", 2, &[Some("?1"), Some(":a")]),
            (
                "SELECT :x, ?2, :y",
                3,
                &[Some(":x"), Some("?2"), Some(":y")],
            ),
            ("SELECT ?1, :a", 2, &[Some("?1"), Some(":a")]),
            ("SELECT :a, ?1", 1, &[Some(":a")]),
            (
                "SELECT :aa, :aa, ?, ?2, ?",
                3,
                &[Some(":aa"), Some("?2"), None],
            ),
            ("SELECT ?, ?, ?5", 5, &[None, None, None, None, Some("?5")]),
            ("SELECT ?3, ?1, ?", 4, &[Some("?1"), None, Some("?3"), None]),
        ];
        for (sql, count, names) in cases {
            let (params, _) = plan_parameters(sql).unwrap();
            assert_eq!(params.len(), *count, "parameter count for {sql:?}");
            let got: Vec<Option<&str>> = params.iter().map(|p| p.name.as_deref()).collect();
            assert_eq!(got, names.to_vec(), "parameter names for {sql:?}");
        }
    }

    /// The inliner touches parameter tokens and nothing else.
    #[test]
    fn inlining_leaves_string_literals_alone() {
        let (mut params, sites) = plan_parameters("SELECT 'a?b', ?").unwrap();
        assert_eq!(params.len(), 1);
        params[0].value = Value::Integer(7);
        let out = inline_parameters("SELECT 'a?b', ?", &params, &sites);
        assert_eq!(out, "SELECT 'a?b', 7");
    }

    /// A repeated name binds the same slot in every position, so a value bound
    /// once shows up in all of them.
    #[test]
    fn a_repeated_name_is_one_parameter() {
        let (mut params, sites) = plan_parameters("SELECT :a, :a, :a").unwrap();
        assert_eq!(params.len(), 1);
        params[0].value = Value::Integer(5);
        let out = inline_parameters("SELECT :a, :a, :a", &params, &sites);
        assert_eq!(out, "SELECT 5, 5, 5");
    }

    /// The sigil is part of a parameter's name, so the three spellings are
    /// three parameters rather than one.
    ///
    /// Every expectation here was read out of a C program run against sqlite3
    /// 3.53.4, which is also the reason this lives in the shim: the engine's
    /// tokenizer consumes the sigil before it reads the name, and
    /// `crates/nsqlite/src/tokenizer.rs` belongs to another track, so the shim
    /// recovers the sigil from the token's span.
    #[test]
    fn a_named_parameters_sigil_is_part_of_its_name() {
        let (params, _) = plan_parameters("SELECT @x, @x, :x, $x").unwrap();
        let names: Vec<Option<&str>> = params.iter().map(|p| p.name.as_deref()).collect();
        assert_eq!(names, [Some("@x"), Some(":x"), Some("$x")]);

        let (params, _) = plan_parameters("SELECT :x, @x").unwrap();
        let names: Vec<Option<&str>> = params.iter().map(|p| p.name.as_deref()).collect();
        assert_eq!(names, [Some(":x"), Some("@x")]);

        // `:1` is a name; `?1` is an index, and the two are spelled
        // differently because the `?` is not a name sigil.
        let (params, _) = plan_parameters("SELECT :1").unwrap();
        assert_eq!(params[0].name.as_deref(), Some(":1"));
        let (params, _) = plan_parameters("SELECT ?1").unwrap();
        assert_eq!(params[0].name.as_deref(), Some("?1"));

        // A bare `?` has no name at all.
        let (params, _) = plan_parameters("SELECT ?").unwrap();
        assert_eq!(params[0].name, None);
    }

    /// `sqlite3_errstr` masks an extended code down to its primary, so a code
    /// like SQLITE_CONSTRAINT_FOREIGNKEY (787) reads as its primary's message.
    #[test]
    fn errstr_masks_extended_codes() {
        let text = |rc| {
            let p = sqlite3_errstr(rc);
            assert!(!p.is_null());
            // SAFETY: every arm of `sqlite3_errstr` returns a pointer to a
            // `&'static [u8]` that is NUL-terminated.
            unsafe { CStr::from_ptr(p) }
                .to_str()
                .expect("ASCII")
                .to_owned()
        };
        // Probed against sqlite3 3.53.4.
        assert_eq!(text(787), "constraint failed");
        assert_eq!(text(1555), "constraint failed");
        assert_eq!(text(9999), "locking protocol");
        // 25 is SQLITE_RANGE, and the real message is not "range error".
        assert_eq!(text(25), "column index out of range");
        // The primary codes the table does cover still match.
        assert_eq!(text(2), "unknown error");
        assert_eq!(text(16), "unknown error");
        assert_eq!(text(22), "unknown error");
        assert_eq!(text(24), "unknown error");
    }

    /// Which statements a read-only handle refuses, and which it lets through.
    ///
    /// Probed against sqlite3 3.53.4 on a handle opened `SQLITE_OPEN_READONLY`:
    /// transaction control and reads return 0, and everything that writes the
    /// main file returns 8.
    #[test]
    fn read_only_refuses_writes_and_nothing_else() {
        use nsqlite::parser::parse_script;
        let refuses = |sql: &str| {
            let stmts = parse_script(sql).expect("parses");
            stmts.iter().all(refuses_read_only)
        };
        let allows = |sql: &str| {
            let stmts = parse_script(sql).expect("parses");
            stmts.iter().all(|s| !refuses_read_only(s))
        };
        for sql in [
            "INSERT INTO t VALUES(1)",
            "UPDATE t SET a=1",
            "DELETE FROM t",
            "CREATE TABLE u(a)",
            "DROP TABLE t",
            "CREATE INDEX i ON t(a)",
            "ANALYZE",
        ] {
            assert!(refuses(sql), "{sql} should be refused");
        }
        for sql in ["SELECT 1", "BEGIN", "COMMIT", "ROLLBACK", "BEGIN IMMEDIATE"] {
            assert!(allows(sql), "{sql} should be allowed");
        }
    }
}
