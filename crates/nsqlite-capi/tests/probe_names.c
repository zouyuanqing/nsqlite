/*
** The last open questions, batched:
**   - does `?abc` name or number, and is it reusable?
**   - the `?N` bounds and the message outside them.
**   - a genuine double close of a connection that was freed immediately.
**   - whether last_insert_rowid survives a SELECT.
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

static void probe_qmark_name(void) {
  sqlite3 *db = NULL;
  sqlite3_stmt *st = NULL;
  int i, n;
  const char *cases[] = {
      "SELECT ?abc", "SELECT ?abc, ?abc", "SELECT ?abc, :abc, ?abc",
      "SELECT ?abc, ?", "SELECT ?abc, ?1", "SELECT ?1, ?abc",
      "SELECT :a, ?a", "SELECT @a, ?a", "SELECT $a, ?a",
      "SELECT :a, :a, @a, $a, ?a", NULL
  };
  int c;
  printf("== ?name ==\n");
  sqlite3_open(":memory:", &db);
  for (c = 0; cases[c]; c++) {
    printf("-- %s\n", cases[c]);
    fflush(stdout);
    if (sqlite3_prepare_v2(db, cases[c], -1, &st, 0) != SQLITE_OK || !st) {
      printf("   prepare failed: %s\n", sqlite3_errmsg(db)); fflush(stdout); continue;
    }
    n = sqlite3_bind_parameter_count(st);
    P("   count", n);
    for (i = 1; i <= n; i++) {
      char tag[80];
      sprintf(tag, "   name[%d]", i);
      PS(tag, sqlite3_bind_parameter_name(st, i));
    }
    P("   index(\"?abc\")", sqlite3_bind_parameter_index(st, "?abc"));
    P("   index(\":abc\")", sqlite3_bind_parameter_index(st, ":abc"));
    sqlite3_finalize(st);
    st = 0;
  }
  sqlite3_close(db);
}

static void probe_qmark_bounds(void) {
  sqlite3 *db = NULL;
  sqlite3_stmt *st = NULL;
  const char *cases[] = {
      "SELECT ?0", "SELECT ?1", "SELECT ?32765", "SELECT ?32766",
      "SELECT ?32767", "SELECT ?32768", "SELECT ?99999999999999999999",
      "SELECT ?-1", "SELECT ?+1", NULL
  };
  int c;
  printf("== ?N bounds ==\n");
  sqlite3_open(":memory:", &db);
  for (c = 0; cases[c]; c++) {
    int rc;
    printf("-- %s\n", cases[c]); fflush(stdout);
    rc = sqlite3_prepare_v2(db, cases[c], -1, &st, 0);
    P("   prepare rc", rc);
    PS("   errmsg", sqlite3_errmsg(db));
    if (st) {
      P("   count", sqlite3_bind_parameter_count(st));
      PS("   name[1]", sqlite3_bind_parameter_name(st, 1));
      sqlite3_finalize(st);
      st = 0;
    }
  }
  sqlite3_close(db);
}

static void probe_double_close(void) {
  sqlite3 *db = NULL;
  int rc;
  printf("== double close ==\n");
  sqlite3_open(":memory:", &db);
  P("close (frees)", sqlite3_close(db));
  rc = sqlite3_close(db);
  P("close again", rc);
  fflush(stdout);
  printf("(survived the second close)\n");
  fflush(stdout);
}

static void probe_rowid_after_select(void) {
  sqlite3 *db = NULL;
  sqlite3_stmt *st = NULL;
  printf("== rowid after select ==\n");
  sqlite3_open(":memory:", &db);
  sqlite3_exec(db, "CREATE TABLE t(a);", 0, 0, 0);
  sqlite3_exec(db, "INSERT INTO t VALUES(1),(2),(3);", 0, 0, 0);
  P("rowid after inserts", sqlite3_last_insert_rowid(db));
  sqlite3_exec(db, "SELECT * FROM t", 0, 0, 0);
  P("rowid after a SELECT via exec", sqlite3_last_insert_rowid(db));
  sqlite3_prepare_v2(db, "SELECT a FROM t", -1, &st, 0);
  P("rowid after preparing a SELECT", sqlite3_last_insert_rowid(db));
  sqlite3_step(st);
  P("rowid after stepping a SELECT", sqlite3_last_insert_rowid(db));
  sqlite3_finalize(st);
  sqlite3_exec(db, "UPDATE t SET a=a", 0, 0, 0);
  P("rowid after an UPDATE", sqlite3_last_insert_rowid(db));
  sqlite3_exec(db, "CREATE TABLE u(a)", 0, 0, 0);
  P("rowid after a CREATE", sqlite3_last_insert_rowid(db));
  sqlite3_close(db);
}

/* Is a connection usable after a close that returned BUSY, and can a
** transaction be started on a read-only handle? */
static void probe_reuse_after_busy(void) {
  sqlite3 *db = NULL;
  sqlite3_stmt *st = NULL;
  printf("== reuse after BUSY ==\n");
  sqlite3_open(":memory:", &db);
  sqlite3_prepare_v2(db, "SELECT 1", -1, &st, 0);
  P("close with a live stmt", sqlite3_close(db));
  P("exec after the BUSY close", sqlite3_exec(db, "CREATE TABLE t(a)", 0, 0, 0));
  P("prepare after the BUSY close", sqlite3_prepare_v2(db, "SELECT 1", -1, &st == 0 ? 0 : &st, 0));
  printf("(still usable)\n");
  fflush(stdout);
}

/* What a non-UTF-8 bound text looks like when asked for as text, in a form a
** lossless conversion would match. */
static void probe_nonutf8_shape(void) {
  sqlite3 *db = NULL;
  sqlite3_stmt *st = NULL;
  int rc, i;
  static const unsigned char bad[] = {0xff, 0xfe, 0x41, 0xc3, 0x28, 0x00};
  printf("== non-utf8 shape ==\n");
  sqlite3_open(":memory:", &db);
  sqlite3_prepare_v2(db, "SELECT ?1", -1, &st, 0);
  rc = sqlite3_bind_text(st, 1, (const char *)bad, 5, SQLITE_TRANSIENT);
  P("bind rc", rc);
  rc = sqlite3_step(st);
  P("step rc", rc);
  if (rc == SQLITE_ROW) {
    const unsigned char *p = sqlite3_column_text(st, 0);
    int n = sqlite3_column_bytes(st, 0);
    P("bytes", n);
    for (i = 0; i < n; i++) printf("   byte[%d] = 0x%02x\n", i, p[i]);
    fflush(stdout);
  }
  sqlite3_finalize(st);
  sqlite3_close(db);
}

int main(void) {
  printf("probe5 against %s\n", sqlite3_libversion());
  probe_qmark_name();
  probe_qmark_bounds();
  probe_double_close();
  probe_rowid_after_select();
  probe_reuse_after_busy();
  probe_nonutf8_shape();
  return 0;
}
