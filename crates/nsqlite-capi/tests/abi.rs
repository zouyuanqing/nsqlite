//! The ABI surface, declared independently and checked against the shim.
//!
//! `extern "C"` blocks in the shim's own source are the shim's opinion about
//! its signatures. A C caller has its own, from `sqlite3.h`, and the two have to
//! agree or a caller compiled against the real header misbehaves in ways no
//! Rust test would catch. So this file declares every symbol the way
//! `sqlite3.h` declares it, links the shim, and calls each one.
//!
//! A signature that does not match is a link error or a wrong answer here, not
//! a silent mismatch: the point is that the declarations below are the only
//! ones in the test, and they were written from the header rather than from the
//! implementation.

#![allow(non_camel_case_types)]

use std::ffi::{c_char, c_int, c_void};
use std::os::raw::{c_longlong, c_uint};

// The opaque types. A C caller only ever handles pointers to them, so their
// layout does not matter -- but they must be distinct types, because a caller
// that passed a `sqlite3*` where a `sqlite3_stmt*` was expected would be
// relying on the shim noticing.
#[repr(C)]
pub struct sqlite3 {
    _private: [u8; 0],
}
#[repr(C)]
pub struct sqlite3_stmt {
    _private: [u8; 0],
}

/// `sqlite3_destructor_type`, as the header declares it.
pub type sqlite3_destructor_type = Option<unsafe extern "C" fn(*mut c_void)>;

/// `sqlite3_int64`.
pub type sqlite3_int64 = c_longlong;

// Result codes, from sqlite3.h. Declared here rather than imported, so this
// test would notice if the shim's constants drifted from the header's.
pub const SQLITE_OK: c_int = 0;
pub const SQLITE_ERROR: c_int = 1;
pub const SQLITE_ABORT: c_int = 4;
pub const SQLITE_CANTOPEN: c_int = 14;
pub const SQLITE_READONLY: c_int = 8;
pub const SQLITE_MISUSE: c_int = 21;
pub const SQLITE_RANGE: c_int = 25;
pub const SQLITE_ROW: c_int = 100;
pub const SQLITE_DONE: c_int = 101;
pub const SQLITE_INTEGER: c_int = 1;
pub const SQLITE_FLOAT: c_int = 2;
pub const SQLITE_TEXT: c_int = 3;
pub const SQLITE_BLOB: c_int = 4;
pub const SQLITE_NULL: c_int = 5;
pub const SQLITE_OPEN_READONLY: c_int = 0x0000_0001;
pub const SQLITE_OPEN_READWRITE: c_int = 0x0000_0002;
pub const SQLITE_OPEN_CREATE: c_int = 0x0000_0004;
pub const SQLITE_OPEN_URI: c_int = 0x0000_0040;

// SQLITE_TRANSIENT is `((sqlite3_destructor_type)-1)`: a function pointer
// holding all-ones. A `const` of that type is rejected by rustc as an invalid
// value, which is the compiler being right -- so, like a C caller, the value is
// produced at the point of use. The shim builds it in `sqlite3_transient`, and
// this test calls that rather than keeping a private second copy, so what is
// checked is the value a real caller ends up passing. It is a Rust item, not a
// C symbol, so it is called through the crate rather than through `extern`.
fn sqlite3_transient() -> sqlite3_destructor_type {
    nsqlite_capi::sqlite3_transient()
}

extern "C" {
    // Library level.
    pub fn sqlite3_libversion() -> *const c_char;
    pub fn sqlite3_libversion_number() -> c_int;
    pub fn sqlite3_sourceid() -> *const c_char;

    // Opening and closing.
    pub fn sqlite3_open(filename: *const c_char, pp_db: *mut *mut sqlite3) -> c_int;
    pub fn sqlite3_open_v2(
        filename: *const c_char,
        pp_db: *mut *mut sqlite3,
        flags: c_int,
        vfs: *const c_char,
    ) -> c_int;
    pub fn sqlite3_close(db: *mut sqlite3) -> c_int;
    pub fn sqlite3_close_v2(db: *mut sqlite3) -> c_int;

    // Errors.
    pub fn sqlite3_errmsg(db: *const sqlite3) -> *const c_char;
    pub fn sqlite3_errcode(db: *const sqlite3) -> c_int;
    pub fn sqlite3_extended_errcode(db: *const sqlite3) -> c_int;
    pub fn sqlite3_errstr(rc: c_int) -> *const c_char;

    // Running a script.
    pub fn sqlite3_exec(
        db: *mut sqlite3,
        sql: *const c_char,
        callback: Option<
            unsafe extern "C" fn(*mut c_void, c_int, *mut *mut c_char, *mut *mut c_char) -> c_int,
        >,
        arg: *mut c_void,
        errmsg: *mut *mut c_char,
    ) -> c_int;
    pub fn sqlite3_free(p: *mut c_void);

    // Statements.
    pub fn sqlite3_prepare_v2(
        db: *mut sqlite3,
        sql: *const c_char,
        n_byte: c_int,
        pp_stmt: *mut *mut sqlite3_stmt,
        pz_tail: *mut *const c_char,
    ) -> c_int;
    pub fn sqlite3_prepare_v3(
        db: *mut sqlite3,
        sql: *const c_char,
        n_byte: c_int,
        flags: c_uint,
        pp_stmt: *mut *mut sqlite3_stmt,
        pz_tail: *mut *const c_char,
    ) -> c_int;
    pub fn sqlite3_step(stmt: *mut sqlite3_stmt) -> c_int;
    pub fn sqlite3_reset(stmt: *mut sqlite3_stmt) -> c_int;
    pub fn sqlite3_finalize(stmt: *mut sqlite3_stmt) -> c_int;
    pub fn sqlite3_sql(stmt: *mut sqlite3_stmt) -> *const c_char;

    // Binding.
    pub fn sqlite3_bind_null(stmt: *mut sqlite3_stmt, index: c_int) -> c_int;
    pub fn sqlite3_bind_int(stmt: *mut sqlite3_stmt, index: c_int, value: c_int) -> c_int;
    pub fn sqlite3_bind_int64(stmt: *mut sqlite3_stmt, index: c_int, value: sqlite3_int64)
        -> c_int;
    pub fn sqlite3_bind_double(stmt: *mut sqlite3_stmt, index: c_int, value: f64) -> c_int;
    pub fn sqlite3_bind_text(
        stmt: *mut sqlite3_stmt,
        index: c_int,
        text: *const c_char,
        n: c_int,
        destructor: sqlite3_destructor_type,
    ) -> c_int;
    pub fn sqlite3_bind_text64(
        stmt: *mut sqlite3_stmt,
        index: c_int,
        text: *const c_char,
        n: u64,
        destructor: sqlite3_destructor_type,
        encoding: u8,
    ) -> c_int;
    pub fn sqlite3_bind_blob(
        stmt: *mut sqlite3_stmt,
        index: c_int,
        blob: *const c_void,
        n: c_int,
        destructor: sqlite3_destructor_type,
    ) -> c_int;
    pub fn sqlite3_bind_blob64(
        stmt: *mut sqlite3_stmt,
        index: c_int,
        blob: *const c_void,
        n: u64,
        destructor: sqlite3_destructor_type,
    ) -> c_int;
    pub fn sqlite3_bind_parameter_count(stmt: *mut sqlite3_stmt) -> c_int;
    pub fn sqlite3_bind_parameter_index(stmt: *mut sqlite3_stmt, name: *const c_char) -> c_int;
    pub fn sqlite3_bind_parameter_name(stmt: *mut sqlite3_stmt, index: c_int) -> *const c_char;

    // Columns.
    pub fn sqlite3_column_count(stmt: *mut sqlite3_stmt) -> c_int;
    pub fn sqlite3_column_type(stmt: *mut sqlite3_stmt, i: c_int) -> c_int;
    pub fn sqlite3_column_int(stmt: *mut sqlite3_stmt, i: c_int) -> c_int;
    pub fn sqlite3_column_int64(stmt: *mut sqlite3_stmt, i: c_int) -> sqlite3_int64;
    pub fn sqlite3_column_double(stmt: *mut sqlite3_stmt, i: c_int) -> f64;
    pub fn sqlite3_column_text(stmt: *mut sqlite3_stmt, i: c_int) -> *const u8;
    pub fn sqlite3_column_blob(stmt: *mut sqlite3_stmt, i: c_int) -> *const c_void;
    pub fn sqlite3_column_bytes(stmt: *mut sqlite3_stmt, i: c_int) -> c_int;
    pub fn sqlite3_column_name(stmt: *mut sqlite3_stmt, i: c_int) -> *const c_char;
    pub fn sqlite3_column_decltype(stmt: *mut sqlite3_stmt, i: c_int) -> *const c_char;

    // Connection state.
    pub fn sqlite3_changes(db: *const sqlite3) -> c_int;
    pub fn sqlite3_total_changes(db: *const sqlite3) -> c_int;
    pub fn sqlite3_last_insert_rowid(db: *const sqlite3) -> sqlite3_int64;
    pub fn sqlite3_get_autocommit(db: *const sqlite3) -> c_int;
    pub fn sqlite3_db_filename(db: *const sqlite3, db_name: *const c_char) -> *const c_char;
}

unsafe fn cstr(p: *const c_char) -> String {
    if p.is_null() {
        return String::new();
    }
    std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned()
}

/// Anchor the shim into this link.
///
/// The crate is built as an rlib, and an rlib is a collection of code
/// generation units that the linker only pulls in when something references
/// them. Every symbol below is `#[no_mangle]`, so nothing in this file
/// references a Rust item in the shim and the whole library would be dropped,
/// leaving every `sqlite3_*` unresolved. Taking the address of one exported
/// function pulls in its CGU, and the linker resolves the rest of the ABI from
/// the same object.
fn anchor_the_shim() -> *const c_void {
    nsqlite_capi::sqlite3_libversion as *const c_void
}

/// The whole surface, driven the way a C caller drives it.
#[test]
fn the_abi_matches_what_a_c_caller_expects() {
    // Must happen before anything is called, or the first call is the one that
    // fails to link.
    let _ = anchor_the_shim();
    unsafe {
        // Library level.
        let version = cstr(sqlite3_libversion());
        assert!(!version.is_empty());
        assert_eq!(
            sqlite3_libversion_number(),
            nsqlite::version_number(0, 1, 0) as c_int
        );
        assert!(!cstr(sqlite3_sourceid()).is_empty());

        // Open.
        let mut db: *mut sqlite3 = std::ptr::null_mut();
        assert_eq!(
            sqlite3_open(c":memory:".as_ptr() as *const c_char, &mut db),
            SQLITE_OK
        );
        assert!(!db.is_null());
        assert_eq!(sqlite3_get_autocommit(db), 1);

        // Errors.
        assert_eq!(cstr(sqlite3_errstr(SQLITE_OK)), "not an error");
        assert_eq!(cstr(sqlite3_errstr(SQLITE_ERROR)), "SQL logic error");
        assert_eq!(cstr(sqlite3_errstr(999)), "unknown error");
        assert_eq!(cstr(sqlite3_errmsg(std::ptr::null())), "out of memory");
        assert_eq!(sqlite3_errcode(std::ptr::null()), 7);

        // Exec.
        assert_eq!(
            sqlite3_exec(
                db,
                c"CREATE TABLE t(i INTEGER, r REAL, s TEXT, b BLOB, n)".as_ptr(),
                None,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            ),
            SQLITE_OK
        );
        assert_eq!(
            sqlite3_exec(
                db,
                c"INSERT INTO t VALUES(42, 1.5, 'hello', x'0102ff', NULL)".as_ptr(),
                None,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            ),
            SQLITE_OK
        );
        assert_eq!(sqlite3_changes(db), 1);
        assert_eq!(sqlite3_total_changes(db), 1);
        assert_eq!(sqlite3_last_insert_rowid(db), 1);

        // An error, and the message that goes with it.
        let rc = sqlite3_exec(
            db,
            c"SELECT * FROM nope".as_ptr(),
            None,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        );
        assert_eq!(rc, SQLITE_ERROR);
        assert_eq!(cstr(sqlite3_errmsg(db)), "no such table: nope");
        assert_eq!(sqlite3_errcode(db), SQLITE_ERROR);
        assert_eq!(sqlite3_extended_errcode(db), SQLITE_ERROR);

        // Prepare and step.
        let mut st: *mut sqlite3_stmt = std::ptr::null_mut();
        assert_eq!(
            sqlite3_prepare_v2(
                db,
                c"SELECT i, r, s, b, n FROM t".as_ptr(),
                -1,
                &mut st,
                std::ptr::null_mut(),
            ),
            SQLITE_OK
        );
        assert_eq!(cstr(sqlite3_sql(st)), "SELECT i, r, s, b, n FROM t");
        assert_eq!(sqlite3_column_count(st), 5);
        assert_eq!(cstr(sqlite3_column_name(st, 0)), "i");
        // A column index past the end is not a name, it is a null pointer --
        // which is what the real sqlite3 3.53.4 returns, checked by calling
        // sqlite3_column_name and sqlite3_column_name16 through the installed
        // libsqlite3. A caller that got "" back would read an empty string
        // where it should have checked for null.
        assert!(sqlite3_column_name(st, 9).is_null());
        assert_eq!(sqlite3_step(st), SQLITE_ROW);

        // Every column type.
        assert_eq!(sqlite3_column_type(st, 0), SQLITE_INTEGER);
        assert_eq!(sqlite3_column_int64(st, 0), 42);
        assert_eq!(sqlite3_column_type(st, 1), SQLITE_FLOAT);
        assert_eq!(sqlite3_column_double(st, 1), 1.5);
        assert_eq!(sqlite3_column_type(st, 2), SQLITE_TEXT);
        assert_eq!(cstr(sqlite3_column_text(st, 2) as *const c_char), "hello");
        assert_eq!(sqlite3_column_bytes(st, 2), 5);
        assert_eq!(sqlite3_column_type(st, 3), SQLITE_BLOB);
        let blob = sqlite3_column_blob(st, 3) as *const u8;
        assert_eq!(sqlite3_column_bytes(st, 3), 3);
        assert_eq!(*blob, 0x01);
        assert_eq!(*blob.add(2), 0xff);
        assert_eq!(sqlite3_column_type(st, 4), SQLITE_NULL);
        assert!(sqlite3_column_text(st, 4).is_null());
        assert_eq!(sqlite3_column_bytes(st, 4), 0);

        // The last step is DONE, and so is the one after it. Reset is OK in
        // every non-error case -- probed against sqlite3 3.53.4, which answers
        // 0 after a DONE, after a ROW, and on a query with no rows -- so it is
        // not the last step's code.
        assert_eq!(sqlite3_step(st), SQLITE_DONE);
        assert_eq!(sqlite3_step(st), SQLITE_DONE);
        assert_eq!(sqlite3_reset(st), SQLITE_OK);
        assert_eq!(sqlite3_step(st), SQLITE_ROW);
        assert_eq!(sqlite3_reset(st), SQLITE_OK);
        assert_eq!(sqlite3_step(st), SQLITE_ROW);
        assert_eq!(sqlite3_finalize(st), SQLITE_OK);

        // prepare_v3 takes a flags word.
        let mut st2: *mut sqlite3_stmt = std::ptr::null_mut();
        assert_eq!(
            sqlite3_prepare_v3(
                db,
                c"SELECT 1".as_ptr(),
                -1,
                0,
                &mut st2,
                std::ptr::null_mut(),
            ),
            SQLITE_OK
        );
        sqlite3_finalize(st2);

        // Binding, including the 64-bit forms and the destructor argument.
        assert_eq!(
            sqlite3_exec(
                db,
                c"CREATE TABLE p(a,b,c,d,e)".as_ptr(),
                None,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            ),
            SQLITE_OK
        );
        let mut ins: *mut sqlite3_stmt = std::ptr::null_mut();
        assert_eq!(
            sqlite3_prepare_v2(
                db,
                c"INSERT INTO p VALUES(?,?,?,?,?)".as_ptr(),
                -1,
                &mut ins,
                std::ptr::null_mut(),
            ),
            SQLITE_OK
        );
        assert_eq!(sqlite3_bind_parameter_count(ins), 5);
        assert_eq!(sqlite3_bind_int64(ins, 1, 7), SQLITE_OK);
        assert_eq!(sqlite3_bind_double(ins, 2, 2.5), SQLITE_OK);
        assert_eq!(
            sqlite3_bind_text(ins, 3, c"t".as_ptr(), -1, sqlite3_transient()),
            SQLITE_OK
        );
        assert_eq!(
            sqlite3_bind_blob64(
                ins,
                4,
                [0xffu8].as_ptr() as *const c_void,
                1,
                sqlite3_transient()
            ),
            SQLITE_OK
        );
        assert_eq!(sqlite3_bind_null(ins, 5), SQLITE_OK);
        // An index that does not exist is a RANGE error.
        assert_eq!(sqlite3_bind_int(ins, 99, 1), SQLITE_RANGE);
        assert_eq!(sqlite3_step(ins), SQLITE_DONE);
        sqlite3_finalize(ins);

        // Parameter names, against what the real library reports for the same
        // statement: two parameters, and the bare ? yields to the later ?2.
        let mut pn: *mut sqlite3_stmt = std::ptr::null_mut();
        assert_eq!(
            sqlite3_prepare_v2(
                db,
                c"SELECT :aa, :aa, ?, ?2".as_ptr(),
                -1,
                &mut pn,
                std::ptr::null_mut(),
            ),
            SQLITE_OK
        );
        assert_eq!(sqlite3_bind_parameter_count(pn), 2);
        assert_eq!(cstr(sqlite3_bind_parameter_name(pn, 1)), ":aa");
        assert_eq!(cstr(sqlite3_bind_parameter_name(pn, 2)), "?2");
        assert!(sqlite3_bind_parameter_name(pn, 3).is_null());
        assert_eq!(sqlite3_bind_parameter_index(pn, c":aa".as_ptr()), 1);
        assert_eq!(sqlite3_bind_parameter_index(pn, c"?2".as_ptr()), 2);
        assert_eq!(sqlite3_bind_parameter_index(pn, c"zz".as_ptr()), 0);
        sqlite3_finalize(pn);

        // Autocommit tracks the transaction.
        assert_eq!(sqlite3_get_autocommit(db), 1);
        sqlite3_exec(
            db,
            c"BEGIN".as_ptr(),
            None,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        );
        assert_eq!(sqlite3_get_autocommit(db), 0);
        sqlite3_exec(
            db,
            c"COMMIT".as_ptr(),
            None,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        );
        assert_eq!(sqlite3_get_autocommit(db), 1);

        // Probed: a memory database's filename is the empty string, not NULL.
        let name = sqlite3_db_filename(db, c"main".as_ptr());
        assert!(!name.is_null());
        assert_eq!(cstr(name), "");

        // close refuses while a statement is open.
        let mut live: *mut sqlite3_stmt = std::ptr::null_mut();
        sqlite3_prepare_v2(
            db,
            c"SELECT 1".as_ptr(),
            -1,
            &mut live,
            std::ptr::null_mut(),
        );
        assert_eq!(sqlite3_close(db), 5);
        assert_eq!(
            cstr(sqlite3_errmsg(db)),
            "unable to close due to unfinalized statements or unfinished backups"
        );
        sqlite3_finalize(live);
        assert_eq!(sqlite3_close(db), SQLITE_OK);
        assert_eq!(sqlite3_close_v2(std::ptr::null_mut()), SQLITE_OK);
        assert_eq!(sqlite3_finalize(std::ptr::null_mut()), SQLITE_OK);
        sqlite3_free(std::ptr::null_mut());
    }
}

/// `sqlite3_open_v2` with its flags, which the other test does not reach
/// because a read/write open is the only one a memory database needs.
#[test]
fn open_v2_honours_its_flags() {
    unsafe {
        let mut db: *mut sqlite3 = std::ptr::null_mut();
        let rc = sqlite3_open_v2(
            c":memory:".as_ptr(),
            &mut db,
            SQLITE_OPEN_READWRITE | SQLITE_OPEN_CREATE | SQLITE_OPEN_URI,
            std::ptr::null(),
        );
        assert_eq!(rc, SQLITE_OK);
        assert!(!db.is_null());
        assert_eq!(sqlite3_close(db), SQLITE_OK);

        // Probed against sqlite3 3.53.4: a read-only open of a *memory*
        // database succeeds, because there is no file for the flag to apply
        // to. The shim matches that rather than inventing a difference.
        let mut ro: *mut sqlite3 = std::ptr::null_mut();
        let rc = sqlite3_open_v2(
            c":memory:".as_ptr(),
            &mut ro,
            SQLITE_OPEN_READONLY,
            std::ptr::null(),
        );
        assert_eq!(rc, SQLITE_OK);
        assert!(!ro.is_null());
        sqlite3_close_v2(ro);

        // Conflicting access bits are a MISUSE, and that one is refused.
        let mut both: *mut sqlite3 = std::ptr::null_mut();
        let rc = sqlite3_open_v2(
            c":memory:".as_ptr(),
            &mut both,
            SQLITE_OPEN_READONLY | SQLITE_OPEN_READWRITE,
            std::ptr::null(),
        );
        assert_eq!(rc, SQLITE_MISUSE);
        assert!(!both.is_null());
        assert_eq!(
            cstr(sqlite3_errmsg(both)),
            "bad parameter or other API misuse"
        );
        sqlite3_close_v2(both);

        // A read-only open of an existing file succeeds, serves reads, and
        // refuses writes with SQLITE_READONLY. Probed against sqlite3 3.53.4:
        // the open is SQLITE_OK, the read is SQLITE_OK, and the write is 8
        // "attempt to write a readonly database".
        //
        // The file name carries this process's id, because cargo runs the test
        // binaries in parallel and a fixed name in the temp directory lets two
        // of them delete each other's database mid-test.
        let path = std::env::temp_dir().join(format!(
            "nsqlite_capi_abi_readonly_{}.db",
            std::process::id()
        ));
        let path_str = path.to_str().expect("a path").replace('\\', "/");
        let _ = std::fs::remove_file(&path);
        let mut rw: *mut sqlite3 = std::ptr::null_mut();
        assert_eq!(
            sqlite3_open_v2(
                path_str.as_ptr() as *const c_char,
                &mut rw,
                SQLITE_OPEN_READWRITE | SQLITE_OPEN_CREATE,
                std::ptr::null(),
            ),
            SQLITE_OK
        );
        sqlite3_close(rw);

        let mut file_ro: *mut sqlite3 = std::ptr::null_mut();
        let rc = sqlite3_open_v2(
            path_str.as_ptr() as *const c_char,
            &mut file_ro,
            SQLITE_OPEN_READONLY,
            std::ptr::null(),
        );
        assert_eq!(rc, SQLITE_OK);
        // DDL writes the main file, so it is refused too. Probed against
        // sqlite3 3.53.4: CREATE TABLE on a read-only handle is 8.
        assert_eq!(
            sqlite3_exec(
                file_ro,
                c"CREATE TABLE t(a)".as_ptr(),
                None,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            ),
            SQLITE_READONLY
        );
        assert_eq!(
            sqlite3_exec(
                file_ro,
                c"INSERT INTO t VALUES(1)".as_ptr(),
                None,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            ),
            SQLITE_READONLY
        );
        let mut st: *mut sqlite3_stmt = std::ptr::null_mut();
        assert_eq!(
            sqlite3_prepare_v2(
                file_ro,
                c"INSERT INTO t VALUES(2)".as_ptr(),
                -1,
                &mut st,
                std::ptr::null_mut(),
            ),
            SQLITE_OK
        );
        // The write is refused at step time, not at prepare time.
        assert_eq!(sqlite3_step(st), SQLITE_READONLY);
        // Reset reports the failure, and the finalize after it is OK.
        assert_eq!(sqlite3_reset(st), SQLITE_READONLY);
        assert_eq!(sqlite3_finalize(st), SQLITE_OK);
        sqlite3_close_v2(file_ro);

        // A read-only open of a path that does not exist is CANTOPEN, because
        // the open must not create the file. Probed: sqlite3 3.53.4 answers
        // 14 "unable to open database file".
        let absent =
            std::env::temp_dir().join(format!("nsqlite_capi_abi_absent_{}.db", std::process::id()));
        let _ = std::fs::remove_file(&absent);
        let absent_str = absent.to_str().expect("a path").replace('\\', "/");
        let mut absent_ro: *mut sqlite3 = std::ptr::null_mut();
        assert_eq!(
            sqlite3_open_v2(
                absent_str.as_ptr() as *const c_char,
                &mut absent_ro,
                SQLITE_OPEN_READONLY,
                std::ptr::null(),
            ),
            SQLITE_CANTOPEN
        );
        assert!(!absent.exists(), "a read-only open must not create a file");
        sqlite3_close_v2(absent_ro);
        let _ = std::fs::remove_file(&path);
    }
}

/// `sqlite3_exec` with a callback, which the other test drives without one.
#[test]
fn exec_calls_back_per_row() {
    unsafe extern "C" fn cb(
        _arg: *mut c_void,
        n_col: c_int,
        values: *mut *mut c_char,
        _names: *mut *mut c_char,
    ) -> c_int {
        assert_eq!(n_col, 2);
        // A NULL column arrives as a null pointer, not as an empty string.
        let second = *values.add(1);
        if second.is_null() {
            1
        } else {
            0
        }
    }

    unsafe {
        let mut db: *mut sqlite3 = std::ptr::null_mut();
        assert_eq!(sqlite3_open(c":memory:".as_ptr(), &mut db), SQLITE_OK);
        assert_eq!(
            sqlite3_exec(
                db,
                c"CREATE TABLE t(a,b)".as_ptr(),
                None,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            ),
            SQLITE_OK
        );
        assert_eq!(
            sqlite3_exec(
                db,
                c"INSERT INTO t VALUES(1,'x'),(2,NULL)".as_ptr(),
                None,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            ),
            SQLITE_OK
        );
        // The callback returns 1 for the row with a NULL, which aborts.
        assert_eq!(
            sqlite3_exec(
                db,
                c"SELECT a,b FROM t ORDER BY a".as_ptr(),
                Some(cb),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            ),
            SQLITE_ABORT
        );
        sqlite3_close(db);
    }
}
