/*
** Follow-up probes: the precedence SQLite uses when a write is attempted on a
** read-only handle, and the double-close behaviour the first probe tripped
** over.
**
** Built twice like probe_diff.c.
*/

#include <stdio.h>
#include <string.h>
#include <stdlib.h>
#include "sqlite3.h"

static void P(const char *tag, long v) {
  printf("%-52s = %ld\n", tag, v); fflush(stdout);
}
static void PS(const char *tag, const char *v) {
  printf("%-52s = %s\n", tag, v ? v : "(null)"); fflush(stdout);
}

/* Which error wins on a read-only handle: the read-only one, or whatever the
** statement would have failed with anyway? SQLite resolves the statement first,
** so a write to a table that does not exist should report "no such table" and
** only a statement that would otherwise have succeeded should report
** SQLITE_READONLY. */
static void probe_ro_precedence(void) {
  const char *path = "probe_rop.db";
  sqlite3 *db = NULL;
  char *e = 0;
  int rc;
  const char *writes[] = {
      "INSERT INTO nosuchtable VALUES(1)",
      "INSERT INTO t VALUES(1)",
      "UPDATE nosuchtable SET a=1",
      "UPDATE t SET a=1",
      "DELETE FROM nosuchtable",
      "DELETE FROM t",
      "CREATE TABLE t(a)",
      "CREATE TABLE brandnew(a)",
      "CREATE TABLE IF NOT EXISTS t(a)",
      "DROP TABLE nosuchtable",
      "DROP TABLE t",
      "ALTER TABLE t RENAME TO t2",
      "ALTER TABLE t ADD COLUMN b",
      "REINDEX t",
      "ANALYZE t",
      "VACUUM",
      "PRAGMA page_size=8192",
      "PRAGMA user_version=7",
      "SELECT * FROM nosuchtable",
      "SELECT a FROM t",
      "BEGIN",
      "COMMIT",
      "ROLLBACK",
      NULL
  };
  int i;
  printf("== ro precedence ==\n");
  remove(path);
  sqlite3_open(path, &db);
  sqlite3_exec(db, "CREATE TABLE t(a); INSERT INTO t VALUES(1);", 0, 0, 0);
  sqlite3_close(db);

  for (i = 0; writes[i]; i++) {
    rc = sqlite3_open_v2(path, &db, SQLITE_OPEN_READONLY, 0);
    if (rc != SQLITE_OK) { printf("-- %s : OPEN FAILED %d\n", writes[i], rc); continue; }
    rc = sqlite3_exec(db, writes[i], 0, 0, &e);
    printf("-- %-42s rc=%-3d msg=%s\n", writes[i], rc, e ? e : "(none)");
    fflush(stdout);
    if (e) { sqlite3_free(e); e = 0; }
    sqlite3_close(db);
    db = 0;
  }
  remove(path);
}

/* close_v2 with a live statement, then a second close_v2 on the same handle. */
static void probe_close_v2_twice(void) {
  sqlite3 *db = NULL;
  sqlite3_stmt *st = NULL;
  int rc;
  printf("== close_v2 twice ==\n");
  sqlite3_open(":memory:", &db);
  sqlite3_prepare_v2(db, "SELECT 1", -1, &st, 0);
  P("close_v2 with a live stmt", sqlite3_close_v2(db));
  P("finalize", sqlite3_finalize(st));
  fflush(stdout);
  P("close_v2 again after the last finalize", sqlite3_close_v2(db));
}

/* close (not v2) with a live statement, then again. */
static void probe_close_twice(void) {
  sqlite3 *db = NULL;
  sqlite3_stmt *st = NULL;
  int rc;
  printf("== close twice ==\n");
  sqlite3_open(":memory:", &db);
  sqlite3_prepare_v2(db, "SELECT 1", -1, &st, 0);
  P("close with a live stmt", sqlite3_close(db));
  P("finalize", sqlite3_finalize(st));
  fflush(stdout);
  P("close again after the last finalize", sqlite3_close(db));
  fflush(stdout);
}

/* A statement prepared before the close, used after it. */
static void probe_use_after_close(void) {
  sqlite3 *db = NULL;
  sqlite3_stmt *st = NULL;
  int rc;
  printf("== use after close ==\n");
  sqlite3_open(":memory:", &db);
  sqlite3_prepare_v2(db, "SELECT 1", -1, &st, 0);
  P("close_v2 with a live stmt", sqlite3_close_v2(db));
  P("step the statement after close_v2", sqlite3_step(st));
  P("column_count after close_v2", sqlite3_column_count(st));
  P("finalize", sqlite3_finalize(st));
  fflush(stdout);
  P("close_v2 after", sqlite3_close_v2(db));
}

/* reset's value when the statement itself was never run, plus a statement that
** returns an error mid-iteration. */
static void probe_reset_errors(void) {
  sqlite3 *db = NULL;
  sqlite3_stmt *st = NULL;
  int rc;
  printf("== reset and errors ==\n");
  sqlite3_open(":memory:", &db);
  sqlite3_exec(db, "CREATE TABLE t(a PRIMARY KEY);", 0, 0, 0);
  sqlite3_exec(db, "INSERT INTO t VALUES(1),(2);", 0, 0, 0);

  /* A statement that errors part way through its rows. */
  if (sqlite3_prepare_v2(db, "SELECT a FROM t UNION ALL SELECT 1", -1, &st, 0) == SQLITE_OK) {
    while ((rc = sqlite3_step(st)) == SQLITE_ROW) { }
    P("step loop ended with", rc);
    P("errmsg", 0); PS("  errmsg", sqlite3_errmsg(db));
    P("reset after the error", sqlite3_reset(st));
    P("errcode after reset", sqlite3_errcode(db));
    PS("errmsg after reset", sqlite3_errmsg(db));
    sqlite3_finalize(st);
    st = 0;
  }
  /* A statement that never ran at all. */
  if (sqlite3_prepare_v2(db, "SELECT a FROM t", -1, &st, 0) == SQLITE_OK) {
    P("reset on an unstepped stmt", sqlite3_reset(st));
    sqlite3_finalize(st);
    st = 0;
  }
  /* reset clears the connection's error. */
  sqlite3_exec(db, "SELECT * FROM nosuchtable", 0, 0, 0);
  PS("errmsg after a failed exec", sqlite3_errmsg(db));
  if (sqlite3_prepare_v2(db, "SELECT 1", -1, &st, 0) == SQLITE_OK) {
    P("reset on a fresh stmt clears the error", sqlite3_reset(st));
    PS("  errmsg", sqlite3_errmsg(db));
    P("  errcode", sqlite3_errcode(db));
    sqlite3_finalize(st);
  }
  sqlite3_close(db);
}

/* Does an UPDATE/DELETE that matches nothing change sqlite3_changes on a
** read-only handle, and what does a *stepped* write do to the counter? */
static void probe_ro_counters(void) {
  const char *path = "probe_roc.db";
  sqlite3 *db = NULL;
  sqlite3_stmt *st = NULL;
  int rc;
  printf("== ro counters ==\n");
  remove(path);
  sqlite3_open(path, &db);
  sqlite3_exec(db, "CREATE TABLE t(a);", 0, 0, 0);
  sqlite3_exec(db, "INSERT INTO t VALUES(1),(2),(3);", 0, 0, 0);
  sqlite3_close(db);
  rc = sqlite3_open_v2(path, &db, SQLITE_OPEN_READONLY, 0);
  if (rc != SQLITE_OK) { printf("open failed %d\n", rc); return; }
  P("changes on a fresh RO handle", sqlite3_changes(db));
  P("total_changes on a fresh RO handle", sqlite3_total_changes(db));
  P("last_insert_rowid on a fresh RO handle", sqlite3_last_insert_rowid(db));
  sqlite3_prepare_v2(db, "UPDATE t SET a=a WHERE 0", -1, &st, 0);
  P("step a no-op UPDATE", sqlite3_step(st));
  P("changes after it", sqlite3_changes(db));
  sqlite3_finalize(st);
  st = 0;
  sqlite3_prepare_v2(db, "SELECT count(*) FROM t", -1, &st, 0);
  sqlite3_step(st);
  P("row count read through RO", sqlite3_column_int(st, 0));
  sqlite3_finalize(st);
  sqlite3_close(db);
  remove(path);
}

/* What does a read-only handle report for db_filename when the URI had a
** mode=ro on it? */
static void probe_ro_filename(void) {
  const char *path = "probe_rof.db";
  sqlite3 *db = NULL;
  int rc;
  printf("== ro filename ==\n");
  remove(path);
  sqlite3_open(path, &db);
  sqlite3_exec(db, "CREATE TABLE t(a);", 0, 0, 0);
  sqlite3_close(db);
  rc = sqlite3_open_v2("file:probe_rof.db?mode=ro", &db,
                       SQLITE_OPEN_READONLY | SQLITE_OPEN_URI, 0);
  P("uri ro open rc", rc);
  PS("  db_filename", db ? sqlite3_db_filename(db, "main") : "(no db)");
  P("  autocommit", db ? sqlite3_get_autocommit(db) : -1);
  if (db) sqlite3_close(db);
  remove(path);
}

int main(void) {
  printf("probe3 against %s\n", sqlite3_libversion());
  probe_ro_precedence();
  probe_close_twice();
  probe_close_v2_twice();
  probe_use_after_close();
  probe_reset_errors();
  probe_ro_counters();
  probe_ro_filename();
  return 0;
}
