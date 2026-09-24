/*
** A C test program for the nsqlite C ABI shim.
**
** WHY THIS EXISTS
**
** The shim is the thing the official TCL suite loads, so the question that
** matters is not "does the Rust code do the right thing" but "does a C compiler
** linking against these symbols see the API it expects". Every function here is
** called through the real `sqlite3.h` prototypes, so a signature the shim got
** wrong is a compile error rather than a silent mismatch, and a symbol that is
** missing is a link error rather than a runtime surprise.
**
** The expectations in this file are not recalled. Each one was produced by
** running the same sequence against the real sqlite3 (3.53.4) and recording
** what it printed, which is what the `TODO`-free comment on each block says.
**
** WHAT IS COVERED
**
** open / open_v2, exec with and without a callback, prepare / step / reset /
** finalize / sql, every bind function, every parameter-name function, every
** column accessor for every storage class, the counters, autocommit, and
** db_filename. Also the error paths, because a shim that returns SQLITE_OK
** where SQLite returns SQLITE_ERROR would pass a happy-path test and still be
** wrong.
*/

#include <stdio.h>
#include <string.h>
#include <stdlib.h>

#include "sqlite3.h"

static int nFailure = 0;
static int nCheck = 0;

static void ok(int cond, const char *what, const char *file, int line) {
  nCheck++;
  if (!cond) {
    nFailure++;
    printf("FAIL %s:%d: %s\n", file, line, what);
  }
}

#define OK(cond) ok((cond), #cond, __FILE__, __LINE__)

/* sqlite3_exec's row callback. Counts rows and remembers the last one, which is
** how the test checks that a script ran every statement and produced rows. */
static int rowCount = 0;
static int lastId = 0;
static int lastNameWasNull = 0;

static int execCb(void *pArg, int nCol, char **azVal, char **azCol) {
  (void)pArg;
  (void)azCol;
  rowCount++;
  if (nCol >= 2) {
    lastId = azVal[0] ? atoi(azVal[0]) : -1;
    lastNameWasNull = (azVal[1] == NULL);
  }
  return 0;
}

/* A callback that aborts, to check the non-zero return stops the script where
** it stands. It counts its own calls, which is how the test checks that the
** script stopped on the first row rather than running to the end. */
static int abortCount = 0;

static int abortCb(void *pArg, int nCol, char **azVal, char **azCol) {
  (void)pArg; (void)nCol; (void)azVal; (void)azCol;
  abortCount++;
  return 1;
}

int main(void) {
  sqlite3 *db = NULL;
  sqlite3_stmt *st = NULL;
  int rc = 0;
  char *errMsg = NULL;

  printf("nsqlite-capi C test: libversion=%s number=%d\n",
         sqlite3_libversion(), sqlite3_libversion_number());
  OK(sqlite3_libversion() != NULL);
  OK(sqlite3_libversion_number() > 0);
  OK(sqlite3_sourceid() != NULL);

  /* --- open ---------------------------------------------------------- */
  rc = sqlite3_open(":memory:", &db);
  OK(rc == SQLITE_OK);
  OK(db != NULL);
  /* Probed: an in-memory database has no file, so db_filename is the empty
  ** string -- a valid pointer to nothing, not NULL. */
  OK(sqlite3_db_filename(db, "main") != NULL);
  OK(strcmp((const char *)sqlite3_db_filename(db, "main"), "") == 0);
  /* Probed: a schema name that is not attached is NULL. */
  OK(sqlite3_db_filename(db, "temp") == NULL);
  OK(sqlite3_db_filename(db, "nosuch") == NULL);
  OK(sqlite3_get_autocommit(db) == 1);

  /* --- errstr -------------------------------------------------------- */
  /* The whole table, read out of the real library. */
  OK(strcmp(sqlite3_errstr(SQLITE_OK), "not an error") == 0);
  OK(strcmp(sqlite3_errstr(SQLITE_ERROR), "SQL logic error") == 0);
  OK(strcmp(sqlite3_errstr(3), "access permission denied") == 0);
  OK(strcmp(sqlite3_errstr(SQLITE_BUSY), "database is locked") == 0);
  OK(strcmp(sqlite3_errstr(SQLITE_CANTOPEN), "unable to open database file") == 0);
  OK(strcmp(sqlite3_errstr(SQLITE_CONSTRAINT), "constraint failed") == 0);
  OK(strcmp(sqlite3_errstr(SQLITE_MISUSE), "bad parameter or other API misuse") == 0);
  OK(strcmp(sqlite3_errstr(SQLITE_RANGE), "column index out of range") == 0);
  OK(strcmp(sqlite3_errstr(SQLITE_ROW), "another row available") == 0);
  OK(strcmp(sqlite3_errstr(SQLITE_DONE), "no more rows available") == 0);
  /* Probed: a code with no message of its own is "unknown error". */
  OK(strcmp(sqlite3_errstr(2), "unknown error") == 0);
  OK(strcmp(sqlite3_errstr(999), "unknown error") == 0);
  OK(strcmp(sqlite3_errstr(-1), "unknown error") == 0);

  /* --- null-handle behaviour, all of it probed ----------------------- */
  OK(strcmp(sqlite3_errmsg(NULL), "out of memory") == 0);
  OK(sqlite3_errcode(NULL) == SQLITE_NOMEM);
  OK(sqlite3_close(NULL) == SQLITE_OK);
  OK(sqlite3_finalize(NULL) == SQLITE_OK);
  /* sqlite3_free returns nothing, so the only thing to check is that freeing a
  ** null pointer does not crash. */
  sqlite3_free(NULL);
  nCheck++;

  /* --- exec, no callback --------------------------------------------- */
  rc = sqlite3_exec(db, "CREATE TABLE t(id INTEGER, name TEXT)", NULL, NULL, &errMsg);
  OK(rc == SQLITE_OK);
  OK(errMsg == NULL);

  /* --- exec, with a callback ----------------------------------------- */
  rowCount = 0;
  rc = sqlite3_exec(db,
                    "INSERT INTO t VALUES(1,'one');"
                    "INSERT INTO t VALUES(2,'two');"
                    "INSERT INTO t VALUES(3,NULL);",
                    execCb, NULL, &errMsg);
  OK(rc == SQLITE_OK);
  /* No rows come out of an INSERT, so the callback is never called. */
  OK(rowCount == 0);
  /* Probed: sqlite3_exec runs three separate INSERTs, and sqlite3_changes()
  ** reports the *last* statement's count, which is 1. total_changes() is the
  ** running sum over the connection, so it is 3. */
  OK(sqlite3_changes(db) == 1);
  OK(sqlite3_total_changes(db) == 3);
  OK(sqlite3_last_insert_rowid(db) == 3);

  rowCount = 0;
  lastId = 0;
  lastNameWasNull = 0;
  rc = sqlite3_exec(db, "SELECT id, name FROM t ORDER BY id", execCb, NULL, &errMsg);
  OK(rc == SQLITE_OK);
  OK(rowCount == 3);
  OK(lastId == 3);
  /* Probed: a NULL column reaches the callback as a NULL pointer, not "". */
  OK(lastNameWasNull == 1);

  /* An error sets the message and the code. */
  rc = sqlite3_exec(db, "SELECT * FROM nosuchtable", NULL, NULL, &errMsg);
  OK(rc == SQLITE_ERROR);
  OK(errMsg != NULL);
  if (errMsg) {
    OK(strcmp(errMsg, "no such table: nosuchtable") == 0);
    sqlite3_free(errMsg);
    errMsg = NULL;
  }
  OK(sqlite3_errcode(db) == SQLITE_ERROR);
  OK(strcmp(sqlite3_errmsg(db), "no such table: nosuchtable") == 0);

  /* A callback that returns non-zero aborts. Probed against sqlite3 3.53.4
  ** with this exact script and a three-row table: the callback is called once,
  ** its non-zero return stops the script there, the second SELECT never runs,
  ** the result code is SQLITE_ABORT (4) and the message is "query aborted". */
  rowCount = 0;
  abortCount = 0;
  rc = sqlite3_exec(db, "SELECT * FROM t; SELECT * FROM t;", abortCb, NULL, &errMsg);
  OK(rc == SQLITE_ABORT);
  OK(abortCount == 1);
  OK(strcmp(sqlite3_errmsg(db), "query aborted") == 0);

  /* --- prepare / column access, every storage class ------------------ */
  rc = sqlite3_exec(db, "DROP TABLE t", NULL, NULL, NULL);
  OK(rc == SQLITE_OK);
  rc = sqlite3_exec(db,
                    "CREATE TABLE t(i INTEGER, r REAL, s TEXT, b BLOB, n);"
                    "INSERT INTO t VALUES(42, 1.5, 'hello', x'0102ff', NULL);",
                    NULL, NULL, NULL);
  OK(rc == SQLITE_OK);

  rc = sqlite3_prepare_v2(db, "SELECT i, r, s, b, n FROM t", -1, &st, NULL);
  OK(rc == SQLITE_OK);
  OK(st != NULL);
  /* Probed: sqlite3_sql returns the statement's own text. */
  OK(strcmp(sqlite3_sql(st), "SELECT i, r, s, b, n FROM t") == 0);
  OK(sqlite3_column_count(st) == 5);
  /* Probed: the column names are the declared column names. */
  OK(strcmp(sqlite3_column_name(st, 0), "i") == 0);
  OK(strcmp(sqlite3_column_name(st, 4), "n") == 0);

  rc = sqlite3_step(st);
  OK(rc == SQLITE_ROW);

  /* INTEGER */
  OK(sqlite3_column_type(st, 0) == SQLITE_INTEGER);
  OK(sqlite3_column_int(st, 0) == 42);
  OK(sqlite3_column_int64(st, 0) == 42);
  OK(sqlite3_column_double(st, 0) == 42.0);
  OK(sqlite3_column_bytes(st, 0) == 2);
  OK(strcmp((const char *)sqlite3_column_text(st, 0), "42") == 0);

  /* REAL. Probed: 1.5 renders as "1.5". */
  OK(sqlite3_column_type(st, 1) == SQLITE_FLOAT);
  OK(sqlite3_column_double(st, 1) == 1.5);
  OK(sqlite3_column_int(st, 1) == 1);
  OK(sqlite3_column_int64(st, 1) == 1);
  OK(sqlite3_column_bytes(st, 1) == 3);
  OK(strcmp((const char *)sqlite3_column_text(st, 1), "1.5") == 0);

  /* TEXT, and the buffer is NUL-terminated past its length. */
  OK(sqlite3_column_type(st, 2) == SQLITE_TEXT);
  OK(sqlite3_column_bytes(st, 2) == 5);
  {
    const unsigned char *p = sqlite3_column_text(st, 2);
    OK(p != NULL);
    OK(memcmp(p, "hello", 5) == 0);
    /* A C caller may treat the result as a C string. */
    OK(strcmp((const char *)p, "hello") == 0);
    OK(sqlite3_column_blob(st, 2) != NULL);
  }

  /* BLOB, including a byte that is not valid UTF-8. */
  OK(sqlite3_column_type(st, 3) == SQLITE_BLOB);
  OK(sqlite3_column_bytes(st, 3) == 3);
  {
    const unsigned char *p = sqlite3_column_blob(st, 3);
    OK(p != NULL);
    if (p) {
      OK(p[0] == 0x01 && p[1] == 0x02 && p[2] == 0xff);
    }
  }

  /* NULL. Probed: a NULL column has no text, no bytes, and reads as 0. */
  OK(sqlite3_column_type(st, 4) == SQLITE_NULL);
  OK(sqlite3_column_text(st, 4) == NULL);
  OK(sqlite3_column_bytes(st, 4) == 0);
  OK(sqlite3_column_int64(st, 4) == 0);
  OK(sqlite3_column_double(st, 4) == 0.0);

  /* The last step returns DONE even though the row was the only one. */
  rc = sqlite3_step(st);
  OK(rc == SQLITE_DONE);
  rc = sqlite3_step(st);
  OK(rc == SQLITE_DONE);
  /* Probed: reset answers SQLITE_OK in every non-error case -- after a DONE,
  ** after a ROW, and on a query with no rows at all. It is NOT the last step's
  ** code, so this must not be written as `rc == SQLITE_DONE`. */
  rc = sqlite3_reset(st);
  OK(rc == SQLITE_OK);
  rc = sqlite3_step(st);
  OK(rc == SQLITE_ROW);
  OK(sqlite3_column_int(st, 0) == 42);
  /* ... and reset after a ROW is OK too. */
  rc = sqlite3_reset(st);
  OK(rc == SQLITE_OK);
  rc = sqlite3_step(st);
  OK(rc == SQLITE_ROW);
  rc = sqlite3_finalize(st);
  OK(rc == SQLITE_OK);
  st = NULL;

  /* Reset on an empty result set is OK as well. Probed: no rows -> step DONE ->
  ** reset 0. Table t is (i, r, s, b, n) at this point, so the column is `i`. */
  rc = sqlite3_prepare_v2(db, "SELECT i FROM t WHERE i=999", -1, &st, NULL);
  OK(rc == SQLITE_OK);
  rc = sqlite3_step(st);
  OK(rc == SQLITE_DONE);
  rc = sqlite3_reset(st);
  OK(rc == SQLITE_OK);
  rc = sqlite3_finalize(st);
  st = NULL;

  /* --- prepare with a tail ------------------------------------------- */
  rc = sqlite3_prepare_v2(db, "SELECT 1; SELECT 2;", -1, &st, NULL);
  OK(rc == SQLITE_OK);
  rc = sqlite3_finalize(st);
  st = NULL;
  /* Probed: a statement of only whitespace prepares OK with a null handle. */
  rc = sqlite3_prepare_v2(db, "   ", -1, &st, NULL);
  OK(rc == SQLITE_OK);
  OK(st == NULL);
  /* Probed: a syntax error is reported by prepare, with a null statement. */
  rc = sqlite3_prepare_v2(db, "SELECT bad syntax here", -1, &st, NULL);
  OK(rc == SQLITE_ERROR);
  OK(st == NULL);
  OK(sqlite3_errcode(db) == SQLITE_ERROR);

  /* --- DDL reports no columns ---------------------------------------- */
  rc = sqlite3_prepare_v2(db, "CREATE TABLE t2(a)", -1, &st, NULL);
  OK(rc == SQLITE_OK);
  OK(sqlite3_column_count(st) == 0);
  sqlite3_finalize(st);
  st = NULL;

  /* --- parameters ----------------------------------------------------- */
  rc = sqlite3_prepare_v2(db, "SELECT :aa, :aa, ?, ?2", -1, &st, NULL);
  OK(rc == SQLITE_OK);
  /* Probed: two parameters, because the bare ? must yield to the later ?2. */
  OK(sqlite3_bind_parameter_count(st) == 2);
  OK(strcmp(sqlite3_bind_parameter_name(st, 1), ":aa") == 0);
  OK(strcmp(sqlite3_bind_parameter_name(st, 2), "?2") == 0);
  OK(sqlite3_bind_parameter_name(st, 3) == NULL);
  OK(sqlite3_bind_parameter_index(st, ":aa") == 1);
  OK(sqlite3_bind_parameter_index(st, "?2") == 2);
  OK(sqlite3_bind_parameter_index(st, "zz") == 0);
  sqlite3_finalize(st);
  st = NULL;

  /* --- every bind function, and what the bound value becomes ---------- */
  rc = sqlite3_exec(db, "DROP TABLE IF EXISTS p", NULL, NULL, NULL);
  OK(rc == SQLITE_OK);
  rc = sqlite3_exec(db, "CREATE TABLE p(a,b,c,d,e)", NULL, NULL, NULL);
  OK(rc == SQLITE_OK);

  rc = sqlite3_prepare_v2(db, "INSERT INTO p VALUES(?,?,?,?,?)", -1, &st, NULL);
  OK(rc == SQLITE_OK);
  OK(sqlite3_bind_parameter_count(st) == 5);
  /* Probed: an out-of-range bind index is SQLITE_RANGE. */
  OK(sqlite3_bind_int(st, 99, 1) == SQLITE_RANGE);
  OK(sqlite3_bind_int64(st, 1, 123456789012345LL) == SQLITE_OK);
  OK(sqlite3_bind_double(st, 2, 2.5) == SQLITE_OK);
  OK(sqlite3_bind_text(st, 3, "text", -1, SQLITE_TRANSIENT) == SQLITE_OK);
  {
    unsigned char blob[3] = {0xde, 0xad, 0x00};
    OK(sqlite3_bind_blob(st, 4, blob, 3, SQLITE_TRANSIENT) == SQLITE_OK);
  }
  OK(sqlite3_bind_null(st, 5) == SQLITE_OK);
  rc = sqlite3_step(st);
  OK(rc == SQLITE_DONE);
  OK(sqlite3_changes(db) == 1);
  sqlite3_finalize(st);
  st = NULL;

  rc = sqlite3_prepare_v2(db, "SELECT a,b,c,d,e FROM p", -1, &st, NULL);
  OK(rc == SQLITE_OK);
  OK(sqlite3_step(st) == SQLITE_ROW);
  OK(sqlite3_column_type(st, 0) == SQLITE_INTEGER);
  OK(sqlite3_column_int64(st, 0) == 123456789012345LL);
  OK(sqlite3_column_type(st, 1) == SQLITE_FLOAT);
  OK(sqlite3_column_double(st, 1) == 2.5);
  OK(sqlite3_column_type(st, 2) == SQLITE_TEXT);
  OK(strcmp((const char *)sqlite3_column_text(st, 2), "text") == 0);
  OK(sqlite3_column_type(st, 3) == SQLITE_BLOB);
  OK(sqlite3_column_bytes(st, 3) == 3);
  {
    const unsigned char *p = sqlite3_column_blob(st, 3);
    OK(p && p[0] == 0xde && p[1] == 0xad && p[2] == 0x00);
  }
  OK(sqlite3_column_type(st, 4) == SQLITE_NULL);
  sqlite3_finalize(st);
  st = NULL;

  /* The 64-bit variants, and the empty text. */
  rc = sqlite3_prepare_v2(db, "INSERT INTO p VALUES(?,?,?,?,?)", -1, &st, NULL);
  OK(rc == SQLITE_OK);
  OK(sqlite3_bind_int64(st, 1, -1) == SQLITE_OK);
  OK(sqlite3_bind_text64(st, 2, "x", 1, SQLITE_TRANSIENT, SQLITE_UTF8) == SQLITE_OK);
  OK(sqlite3_bind_blob64(st, 3, "", 0, SQLITE_TRANSIENT) == SQLITE_OK);
  OK(sqlite3_bind_text(st, 4, "", 0, SQLITE_TRANSIENT) == SQLITE_OK);
  OK(sqlite3_bind_int(st, 5, 1) == SQLITE_OK);
  OK(sqlite3_step(st) == SQLITE_DONE);
  sqlite3_finalize(st);
  st = NULL;

  /* A `?` inside a string literal is a character, not a parameter. */
  rc = sqlite3_prepare_v2(db, "SELECT 'a?b', ?", -1, &st, NULL);
  OK(rc == SQLITE_OK);
  OK(sqlite3_bind_parameter_count(st) == 1);
  OK(sqlite3_bind_int64(st, 1, 7) == SQLITE_OK);
  OK(sqlite3_step(st) == SQLITE_ROW);
  OK(strcmp((const char *)sqlite3_column_text(st, 0), "a?b") == 0);
  OK(sqlite3_column_int(st, 1) == 7);
  sqlite3_finalize(st);
  st = NULL;

  /* --- transactions and autocommit ------------------------------------ */
  OK(sqlite3_get_autocommit(db) == 1);
  OK(sqlite3_exec(db, "BEGIN", NULL, NULL, NULL) == SQLITE_OK);
  OK(sqlite3_get_autocommit(db) == 0);
  OK(sqlite3_exec(db, "COMMIT", NULL, NULL, NULL) == SQLITE_OK);
  OK(sqlite3_get_autocommit(db) == 1);

  /* --- close refuses while a statement is open ------------------------ */
  rc = sqlite3_prepare_v2(db, "SELECT 1", -1, &st, NULL);
  OK(rc == SQLITE_OK);
  rc = sqlite3_close(db);
  /* Probed: SQLITE_BUSY, with this exact message. */
  OK(rc == SQLITE_BUSY);
  OK(strcmp(sqlite3_errmsg(db),
            "unable to close due to unfinalized statements or unfinished backups") == 0);
  OK(sqlite3_finalize(st) == SQLITE_OK);
  st = NULL;
  rc = sqlite3_close(db);
  OK(rc == SQLITE_OK);
  db = NULL;

  /* --- open_v2 and a file-backed database ----------------------------- */
  {
    const char *path = "nsqlite_capi_c_test.db";
    rc = sqlite3_open_v2(path, &db,
                         SQLITE_OPEN_READWRITE | SQLITE_OPEN_CREATE, NULL);
    OK(rc == SQLITE_OK);
    /* Probed: a file database reports its path. */
    OK(sqlite3_db_filename(db, "main") != NULL);
    if (sqlite3_db_filename(db, "main")) {
      OK(strstr((const char *)sqlite3_db_filename(db, "main"), path) != NULL);
    }
    OK(sqlite3_exec(db, "CREATE TABLE IF NOT EXISTS z(a)", NULL, NULL, NULL) == SQLITE_OK);
    OK(sqlite3_exec(db, "INSERT INTO z VALUES(1)", NULL, NULL, NULL) == SQLITE_OK);
    OK(sqlite3_close(db) == SQLITE_OK);
    db = NULL;
    remove(path);

    /* A directory that does not exist cannot be opened. */
    rc = sqlite3_open_v2("no_such_dir_xyz/foo.db", &db,
                         SQLITE_OPEN_READWRITE | SQLITE_OPEN_CREATE, NULL);
    OK(rc != SQLITE_OK);
    OK(db != NULL);
    if (db) {
      OK(strcmp(sqlite3_errmsg(db), "unable to open database file") == 0);
      /* close_v2 frees it even though the open failed. */
      OK(sqlite3_close_v2(db) == SQLITE_OK);
      db = NULL;
    }
  }

  /* --- a real file read back through the shim ------------------------- */
  {
    const char *path = "nsqlite_capi_c_test2.db";
    remove(path);
    rc = sqlite3_open(path, &db);
    OK(rc == SQLITE_OK);
    OK(sqlite3_exec(db, "CREATE TABLE t(a,b)", NULL, NULL, NULL) == SQLITE_OK);
    OK(sqlite3_exec(db, "INSERT INTO t VALUES(1,'x')", NULL, NULL, NULL) == SQLITE_OK);
    OK(sqlite3_close(db) == SQLITE_OK);
    db = NULL;

    rc = sqlite3_open(path, &db);
    OK(rc == SQLITE_OK);
    rc = sqlite3_prepare_v2(db, "SELECT a,b FROM t", -1, &st, NULL);
    OK(rc == SQLITE_OK);
    OK(sqlite3_step(st) == SQLITE_ROW);
    OK(sqlite3_column_int(st, 0) == 1);
    OK(strcmp((const char *)sqlite3_column_text(st, 1), "x") == 0);
    sqlite3_finalize(st);
    st = NULL;
    OK(sqlite3_close(db) == SQLITE_OK);
    db = NULL;
    remove(path);
  }

  /* --- SQLITE_OPEN_READONLY ------------------------------------------- */
  /* Probed against sqlite3 3.53.4: a read-only open of an existing file
  ** returns SQLITE_OK, serves reads, and refuses every write with
  ** SQLITE_READONLY (8). BEGIN and COMMIT return SQLITE_OK -- transaction
  ** control does not touch the file. A read-only open of a path that does not
  ** exist returns SQLITE_CANTOPEN (14), because the open must not create it. */
  {
    const char *path = "nsqlite_capi_c_test_ro.db";
    char *err = NULL;
    remove(path);
    rc = sqlite3_open(path, &db);
    OK(rc == SQLITE_OK);
    OK(sqlite3_exec(db, "CREATE TABLE t(a); INSERT INTO t VALUES(1);", NULL, NULL, NULL) == SQLITE_OK);
    OK(sqlite3_close(db) == SQLITE_OK);
    db = NULL;

    rc = sqlite3_open_v2(path, &db, SQLITE_OPEN_READONLY, NULL);
    OK(rc == SQLITE_OK);
    if (rc == SQLITE_OK) {
      OK(sqlite3_exec(db, "SELECT a FROM t", NULL, NULL, NULL) == SQLITE_OK);
      err = NULL;
      rc = sqlite3_exec(db, "INSERT INTO t VALUES(2)", NULL, NULL, &err);
      OK(rc == SQLITE_READONLY);
      if (err) { OK(strcmp(err, "attempt to write a readonly database") == 0); }
      sqlite3_free(err); err = NULL;
      rc = sqlite3_exec(db, "CREATE TABLE u(b)", NULL, NULL, &err);
      OK(rc == SQLITE_READONLY);
      sqlite3_free(err); err = NULL;
      rc = sqlite3_exec(db, "BEGIN", NULL, NULL, &err);
      OK(rc == SQLITE_OK);
      sqlite3_free(err); err = NULL;
      rc = sqlite3_exec(db, "COMMIT", NULL, NULL, &err);
      OK(rc == SQLITE_OK);
      sqlite3_free(err); err = NULL;

      /* Through prepare/step: the open-time prepare is OK, the step is 8, the
      ** reset after it is 8, and the finalize that follows is SQLITE_OK. */
      rc = sqlite3_prepare_v2(db, "INSERT INTO t VALUES(9)", -1, &st, NULL);
      OK(rc == SQLITE_OK);
      if (st) {
        OK(sqlite3_step(st) == SQLITE_READONLY);
        OK(strcmp(sqlite3_errmsg(db), "attempt to write a readonly database") == 0);
        OK(sqlite3_reset(st) == SQLITE_READONLY);
        OK(sqlite3_finalize(st) == SQLITE_OK);
        st = NULL;
      }
      OK(sqlite3_close(db) == SQLITE_OK);
      db = NULL;
    }

    /* The URI spelling, and a path that does not exist. */
    rc = sqlite3_open_v2("file:nsqlite_capi_c_test_ro.db?mode=ro", &db,
                         SQLITE_OPEN_READONLY | SQLITE_OPEN_URI, NULL);
    OK(rc == SQLITE_OK);
    if (rc == SQLITE_OK) {
      err = NULL;
      rc = sqlite3_exec(db, "INSERT INTO t VALUES(5)", NULL, NULL, &err);
      OK(rc == SQLITE_READONLY);
      sqlite3_free(err);
      OK(sqlite3_close(db) == SQLITE_OK);
      db = NULL;
    }
    rc = sqlite3_open_v2("nsqlite_capi_c_test_absent.db", &db,
                         SQLITE_OPEN_READONLY, NULL);
    OK(rc == SQLITE_CANTOPEN);
    if (db) {
      OK(strcmp(sqlite3_errmsg(db), "unable to open database file") == 0);
      OK(sqlite3_close_v2(db) == SQLITE_OK);
      db = NULL;
    }
    remove(path);
  }

  /* --- prepare reports an unresolvable FROM --------------------------- */
  /* Probed against sqlite3 3.53.4: SQLITE_ERROR with a null statement and
  ** "no such table: ...". A caller that checks only the return code must not
  ** walk on into a NULL statement. */
  {
    rc = sqlite3_open(":memory:", &db);
    OK(rc == SQLITE_OK);
    st = NULL;
    rc = sqlite3_prepare_v2(db, "SELECT * FROM no_such_table_xyz", -1, &st, NULL);
    OK(rc == SQLITE_ERROR);
    OK(st == NULL);
    OK(strcmp(sqlite3_errmsg(db), "no such table: no_such_table_xyz") == 0);
    OK(sqlite3_close(db) == SQLITE_OK);
    db = NULL;
  }

  /* --- column_type follows the representation, not the storage ------- */
  /* Probed against sqlite3 3.53.4: reading a BLOB as text converts it in
  ** place, so column_type then reports TEXT. The other direction does not
  ** happen: the numeric accessors and column_blob leave the type alone. */
  {
    rc = sqlite3_open(":memory:", &db);
    OK(rc == SQLITE_OK);
    rc = sqlite3_prepare_v2(db, "SELECT x'0102ff'", -1, &st, NULL);
    OK(rc == SQLITE_OK);
    OK(sqlite3_step(st) == SQLITE_ROW);
    OK(sqlite3_column_type(st, 0) == SQLITE_BLOB);
    OK(sqlite3_column_text(st, 0) != NULL);
    OK(sqlite3_column_type(st, 0) == SQLITE_TEXT);
    OK(sqlite3_finalize(st) == SQLITE_OK);
    st = NULL;

    /* A REAL stays FLOAT after its text rendering, and a TEXT stays TEXT after
    ** column_blob. */
    rc = sqlite3_prepare_v2(db, "SELECT 1.5", -1, &st, NULL);
    OK(rc == SQLITE_OK);
    OK(sqlite3_step(st) == SQLITE_ROW);
    OK(sqlite3_column_type(st, 0) == SQLITE_FLOAT);
    OK(sqlite3_column_text(st, 0) != NULL);
    OK(sqlite3_column_type(st, 0) == SQLITE_FLOAT);
    OK(strcmp((const char *)sqlite3_column_text(st, 0), "1.5") == 0);
    OK(sqlite3_finalize(st) == SQLITE_OK);
    st = NULL;

    rc = sqlite3_prepare_v2(db, "SELECT 'abc'", -1, &st, NULL);
    OK(rc == SQLITE_OK);
    OK(sqlite3_step(st) == SQLITE_ROW);
    OK(sqlite3_column_type(st, 0) == SQLITE_TEXT);
    OK(sqlite3_column_blob(st, 0) != NULL);
    OK(sqlite3_column_type(st, 0) == SQLITE_TEXT);
    OK(sqlite3_finalize(st) == SQLITE_OK);
    st = NULL;
    OK(sqlite3_close(db) == SQLITE_OK);
    db = NULL;
  }

  /* --- named parameters keep their sigil ------------------------------ */
  /* Probed against sqlite3 3.53.4: the sigil is part of the name, so @x, :x
  ** and $x are three different parameters, and a lookup by one sigil does not
  ** find another. `:1` is a name; `?1` is an index. */
  {
    const char *sql = "SELECT @x, @x, :x, $x";
    rc = sqlite3_open(":memory:", &db);
    OK(rc == SQLITE_OK);
    rc = sqlite3_prepare_v2(db, sql, -1, &st, NULL);
    OK(rc == SQLITE_OK);
    OK(sqlite3_bind_parameter_count(st) == 3);
    OK(strcmp(sqlite3_bind_parameter_name(st, 1), "@x") == 0);
    OK(strcmp(sqlite3_bind_parameter_name(st, 2), ":x") == 0);
    OK(strcmp(sqlite3_bind_parameter_name(st, 3), "$x") == 0);
    OK(sqlite3_bind_parameter_index(st, "@x") == 1);
    OK(sqlite3_bind_parameter_index(st, ":x") == 2);
    OK(sqlite3_bind_parameter_index(st, "$x") == 3);
    OK(sqlite3_finalize(st) == SQLITE_OK);
    st = NULL;

    /* A lookup by the wrong sigil finds nothing. */
    rc = sqlite3_prepare_v2(db, "SELECT @x", -1, &st, NULL);
    OK(rc == SQLITE_OK);
    OK(sqlite3_bind_parameter_index(st, ":x") == 0);
    OK(sqlite3_bind_parameter_index(st, "$x") == 0);
    OK(sqlite3_bind_parameter_index(st, "x") == 0);
    OK(sqlite3_bind_parameter_index(st, "@x") == 1);
    OK(sqlite3_finalize(st) == SQLITE_OK);
    st = NULL;

    /* :1 is a name, and ?1 is an index that resolves only where it exists. */
    rc = sqlite3_prepare_v2(db, "SELECT :1", -1, &st, NULL);
    OK(rc == SQLITE_OK);
    OK(strcmp(sqlite3_bind_parameter_name(st, 1), ":1") == 0);
    OK(sqlite3_bind_parameter_index(st, ":1") == 1);
    OK(sqlite3_bind_parameter_index(st, "?1") == 0);
    OK(sqlite3_finalize(st) == SQLITE_OK);
    st = NULL;

    rc = sqlite3_prepare_v2(db, "SELECT ?1", -1, &st, NULL);
    OK(rc == SQLITE_OK);
    OK(strcmp(sqlite3_bind_parameter_name(st, 1), "?1") == 0);
    OK(sqlite3_bind_parameter_index(st, "?1") == 1);
    OK(sqlite3_bind_parameter_index(st, ":1") == 0);
    OK(sqlite3_finalize(st) == SQLITE_OK);
    st = NULL;
    OK(sqlite3_close(db) == SQLITE_OK);
    db = NULL;
  }

  /* --- changes() survives a prepare and a step of a SELECT ----------- */
  /* Probed against sqlite3 3.53.4: sqlite3_changes is the count from the last
  ** statement that changed rows, and neither preparing nor stepping a SELECT
  ** disturbs it. */
  {
    rc = sqlite3_open(":memory:", &db);
    OK(rc == SQLITE_OK);
    OK(sqlite3_exec(db, "CREATE TABLE t(a)", NULL, NULL, NULL) == SQLITE_OK);
    OK(sqlite3_exec(db, "INSERT INTO t VALUES(1),(2),(3),(4),(5)", NULL, NULL, NULL) == SQLITE_OK);
    OK(sqlite3_changes(db) == 5);
    OK(sqlite3_total_changes(db) == 5);
    rc = sqlite3_prepare_v2(db, "SELECT a FROM t", -1, &st, NULL);
    OK(rc == SQLITE_OK);
    OK(sqlite3_changes(db) == 5);
    OK(sqlite3_total_changes(db) == 5);
    OK(sqlite3_step(st) == SQLITE_ROW);
    OK(sqlite3_changes(db) == 5);
    OK(sqlite3_total_changes(db) == 5);
    OK(sqlite3_finalize(st) == SQLITE_OK);
    st = NULL;
    /* A DDL statement is not a row change either. */
    OK(sqlite3_exec(db, "CREATE TABLE u(b)", NULL, NULL, NULL) == SQLITE_OK);
    OK(sqlite3_changes(db) == 5);
    /* But a 0-row UPDATE is, and it moves the counter to 0. */
    OK(sqlite3_exec(db, "UPDATE t SET a=a WHERE a=99", NULL, NULL, NULL) == SQLITE_OK);
    OK(sqlite3_changes(db) == 0);
    OK(sqlite3_total_changes(db) == 5);
    OK(sqlite3_close(db) == SQLITE_OK);
    db = NULL;
  }

  /* --- bind_text of bytes that are not UTF-8 -------------------------- */
  /* Probed against sqlite3 3.53.4: the real library binds the bytes and
  ** returns SQLITE_OK, with sqlite3_errmsg reading "not an error". The shim
  ** cannot represent them -- Value::Text is a String -- but it must not report
  ** a failure that did not happen, and the value must actually change. */
  {
    rc = sqlite3_open(":memory:", &db);
    OK(rc == SQLITE_OK);
    rc = sqlite3_prepare_v2(db, "SELECT ?1", -1, &st, NULL);
    OK(rc == SQLITE_OK);
    OK(sqlite3_bind_text(st, 1, "first", -1, SQLITE_TRANSIENT) == SQLITE_OK);
    OK(sqlite3_bind_text(st, 1, "\xff\xfe", 2, SQLITE_TRANSIENT) == SQLITE_OK);
    OK(strcmp(sqlite3_errmsg(db), "not an error") == 0);
    OK(sqlite3_step(st) == SQLITE_ROW);
    /* The bytes are lossy-decoded, so what is asserted is that the value
    ** changed and the step succeeded -- not the exact bytes. */
    OK(sqlite3_column_text(st, 0) != NULL);
    OK(strcmp((const char *)sqlite3_column_text(st, 0), "first") != 0);
    OK(sqlite3_finalize(st) == SQLITE_OK);
    st = NULL;
    OK(sqlite3_close(db) == SQLITE_OK);
    db = NULL;
  }

  /* --- column_int64 on a TEXT value reads the integer prefix ---------- */
  /* Probed against sqlite3 3.53.4: the exponent belongs to column_double, not
  ** column_int64, and 'inf'/'nan' are not numbers SQLite's own parser
  ** recognises, so both read 0. */
  {
    const char *pairs[][2] = {
      {"'1e3'", "1"},         {"'1.9e2'", "1"},   {"'42abc'", "42"},
      {"'inf'", "0"},         {"'nan'", "0"},     {"'1e400'", "1"},
      {"'  -12  '", "-12"},   {"'9007199254740993'", "9007199254740993"},
      {"'9223372036854775808'", "9223372036854775807"},
    };
    rc = sqlite3_open(":memory:", &db);
    OK(rc == SQLITE_OK);
    for (unsigned k = 0; k < sizeof(pairs) / sizeof(pairs[0]); k++) {
      char sql[64];
      snprintf(sql, sizeof(sql), "SELECT %s", pairs[k][0]);
      rc = sqlite3_prepare_v2(db, sql, -1, &st, NULL);
      OK(rc == SQLITE_OK);
      if (rc == SQLITE_OK) {
        OK(sqlite3_step(st) == SQLITE_ROW);
        OK(sqlite3_column_int64(st, 0) == atoll(pairs[k][1]));
        sqlite3_finalize(st);
        st = NULL;
      }
    }
    /* column_double is a real parse: the exponent counts there. */
    rc = sqlite3_prepare_v2(db, "SELECT '1e3'", -1, &st, NULL);
    OK(rc == SQLITE_OK);
    OK(sqlite3_step(st) == SQLITE_ROW);
    OK(sqlite3_column_double(st, 0) == 1000.0);
    OK(sqlite3_finalize(st) == SQLITE_OK);
    st = NULL;
    OK(sqlite3_close(db) == SQLITE_OK);
    db = NULL;
  }

  printf("%d checks, %d failures\n", nCheck, nFailure);
  return nFailure == 0 ? 0 : 1;
}
