/*
** The last questions, batched:
**   - sqlite3_changes() after DDL and after a no-op write.
**   - the exact doubles and int64s sqlite3 reports for text prefixes.
**   - `?name` and `:1` / `@1` / `$1` parameter spellings.
**   - what sqlite3_column_bytes / _blob do to a BLOB's reported type.
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

static void probe_changes_semantics(void) {
  sqlite3 *db = NULL;
  sqlite3_stmt *st = NULL;
  printf("== changes semantics ==\n");
  sqlite3_open(":memory:", &db);
  sqlite3_exec(db, "CREATE TABLE t(a);", 0, 0, 0);
  P("after CREATE", sqlite3_changes(db));
  sqlite3_exec(db, "INSERT INTO t VALUES(1),(2),(3);", 0, 0, 0);
  P("after 3-row INSERT", sqlite3_changes(db));
  P("total", sqlite3_total_changes(db));
  sqlite3_exec(db, "CREATE TABLE u(a);", 0, 0, 0);
  P("after a second CREATE", sqlite3_changes(db));
  P("total", sqlite3_total_changes(db));
  sqlite3_exec(db, "SELECT * FROM t", 0, 0, 0);
  P("after a SELECT via exec", sqlite3_changes(db));
  P("total", sqlite3_total_changes(db));
  sqlite3_exec(db, "UPDATE t SET a=a WHERE 0", 0, 0, 0);
  P("after a 0-row UPDATE", sqlite3_changes(db));
  P("total", sqlite3_total_changes(db));
  sqlite3_exec(db, "DELETE FROM t WHERE 0", 0, 0, 0);
  P("after a 0-row DELETE", sqlite3_changes(db));
  P("total", sqlite3_total_changes(db));
  sqlite3_exec(db, "BEGIN", 0, 0, 0);
  sqlite3_exec(db, "INSERT INTO t VALUES(4),(5);", 0, 0, 0);
  P("inside a txn, after a 2-row INSERT", sqlite3_changes(db));
  P("total", sqlite3_total_changes(db));
  sqlite3_exec(db, "ROLLBACK", 0, 0, 0);
  P("after a ROLLBACK", sqlite3_changes(db));
  P("total", sqlite3_total_changes(db));
  sqlite3_exec(db, "PRAGMA user_version=1", 0, 0, 0);
  P("after a PRAGMA", sqlite3_changes(db));
  /* A stepped statement, which is where the shim differs. */
  sqlite3_prepare_v2(db, "INSERT INTO t VALUES(6)", -1, &st, 0);
  sqlite3_step(st);
  P("after a stepped INSERT: changes", sqlite3_changes(db));
  P("after a stepped INSERT: total", sqlite3_total_changes(db));
  sqlite3_finalize(st);
  st = 0;
  sqlite3_prepare_v2(db, "SELECT a FROM t", -1, &st, 0);
  sqlite3_step(st);
  P("after a stepped SELECT: changes", sqlite3_changes(db));
  P("after a stepped SELECT: total", sqlite3_total_changes(db));
  sqlite3_finalize(st);
  st = 0;
  sqlite3_prepare_v2(db, "CREATE TABLE v(a)", -1, &st, 0);
  sqlite3_step(st);
  P("after a stepped CREATE: changes", sqlite3_changes(db));
  P("after a stepped CREATE: total", sqlite3_total_changes(db));
  sqlite3_finalize(st);
  /* A DDL prepared but not stepped must not move total. */
  sqlite3_prepare_v2(db, "CREATE TABLE w(a)", -1, &st, 0);
  P("after preparing a CREATE: changes", sqlite3_changes(db));
  P("after preparing a CREATE: total", sqlite3_total_changes(db));
  sqlite3_step(st);
  P("after stepping it: total", sqlite3_total_changes(db));
  sqlite3_finalize(st);
  sqlite3_close(db);
}

static void probe_text_values(void) {
  sqlite3 *db = NULL;
  sqlite3_stmt *st = NULL;
  int i;
  const char *cases[] = {
      "1e3", "1.9e2", "inf", "nan", "INF", "NaN", "1e400", "-1e400",
      "9223372036854775808", "9223372036854775809", "-9223372036854775809",
      "9223372036854775807", "42abc", "abc", " 7 ", "3.9", "1.5e2xyz",
      "-12.5rest", ".5", "1e", "1e+", "0x10", "1d3", "  -12  ", "+5",
      "1.7976931348623157e309", "1e-400", "1E3", "3e", "5.", "1..2",
      "  0x", "0b101", "1_000", "1.2.3", "e5", "--5", "00012", "1.0e",
      "1.0e+", "9223372036854775807abc", "  +0x10", "\t\n 42 \t", "-0",
      NULL
  };
  printf("== text values ==\n");
  sqlite3_open(":memory:", &db);
  sqlite3_prepare_v2(db, "SELECT ?1", -1, &st, 0);
  for (i = 0; cases[i]; i++) {
    int rc, ti;
    double d;
    long long n;
    char tag[80];
    printf("-- '%s'\n", cases[i]);
    fflush(stdout);
    sqlite3_reset(st);
    rc = sqlite3_bind_text(st, 1, cases[i], -1, SQLITE_TRANSIENT);
    if (rc != SQLITE_OK) { P("   bind rc", rc); continue; }
    rc = sqlite3_step(st);
    if (rc != SQLITE_ROW) { P("   step rc", rc); continue; }
    ti = sqlite3_column_type(st, 0);
    n = sqlite3_column_int64(st, 0);
    d = sqlite3_column_double(st, 0);
    P("   type", ti);
    sprintf(tag, "   int64 %lld", n);
    PS(tag, "see below");
    printf("   int64 = %lld\n", n);
    if (d == 0.0) PS("   double", "0.0");
    else if (d == 1.0) PS("   double", "1.0");
    else if (d == -1.0) PS("   double", "-1.0");
    else if (d != d) PS("   double", "NaN");
    else if (d > 1.7976931348623157e308) PS("   double", "+Inf");
    else if (d < -1.7976931348623157e308) PS("   double", "-Inf");
    else {
      sprintf(tag, "   double %.17g", d);
      PS(tag, "?");
    }
    sprintf(tag, "   bytes %d text '%s'", sqlite3_column_bytes(st, 0),
            sqlite3_column_text(st, 0) ? (const char *)sqlite3_column_text(st, 0) : "(null)");
    PS(tag, "?");
    fflush(stdout);
  }
  sqlite3_finalize(st);
  sqlite3_close(db);
}

static void probe_param_spellings(void) {
  sqlite3 *db = NULL;
  sqlite3_stmt *st = NULL;
  int i, n;
  const char *cases[] = {
      "SELECT ?abc", "SELECT :1", "SELECT @1", "SELECT $1", "SELECT ?0",
      "SELECT ::a", "SELECT :\"a\"", "SELECT `a`", "SELECT :a:b",
      "SELECT $$", "SELECT :$", "SELECT :0", "SELECT @0", "SELECT $0",
      "SELECT ?99999999999999999999", "SELECT :1, ?1, ?",
      NULL
  };
  int c;
  printf("== param spellings ==\n");
  sqlite3_open(":memory:", &db);
  for (c = 0; cases[c]; c++) {
    printf("-- %s\n", cases[c]);
    fflush(stdout);
    if (sqlite3_prepare_v2(db, cases[c], -1, &st, 0) != SQLITE_OK || !st) {
      printf("   prepare failed: %s (rc=%d)\n", sqlite3_errmsg(db), sqlite3_errcode(db));
      fflush(stdout);
      continue;
    }
    n = sqlite3_bind_parameter_count(st);
    P("   count", n);
    for (i = 1; i <= n; i++) {
      char tag[80];
      sprintf(tag, "   name[%d]", i);
      PS(tag, sqlite3_bind_parameter_name(st, i));
    }
    P("   index(\":1\")", sqlite3_bind_parameter_index(st, ":1"));
    P("   index(\"?abc\")", sqlite3_bind_parameter_index(st, "?abc"));
    P("   index(\"abc\")", sqlite3_bind_parameter_index(st, "abc"));
    sqlite3_finalize(st);
    st = 0;
  }
  sqlite3_close(db);
}

static void probe_blob_bytes(void) {
  sqlite3 *db = NULL;
  sqlite3_stmt *st = NULL;
  printf("== blob and bytes ==\n");
  sqlite3_open(":memory:", &db);
  sqlite3_prepare_v2(db, "SELECT x'0102ff'", -1, &st, 0);
  sqlite3_step(st);
  P("type first", sqlite3_column_type(st, 0));
  P("bytes", sqlite3_column_bytes(st, 0));
  P("type after column_bytes", sqlite3_column_type(st, 0));
  sqlite3_finalize(st);

  sqlite3_prepare_v2(db, "SELECT x'0102ff'", -1, &st, 0);
  sqlite3_step(st);
  P("type first", sqlite3_column_type(st, 0));
  sqlite3_column_blob(st, 0);
  P("type after column_blob", sqlite3_column_type(st, 0));
  sqlite3_finalize(st);

  /* Does the conversion survive to the next step? */
  sqlite3_prepare_v2(db, "SELECT x'0102ff' UNION ALL SELECT x'aabb'", -1, &st, 0);
  sqlite3_step(st);
  P("row 1 type", sqlite3_column_type(st, 0));
  sqlite3_column_text(st, 0);
  P("row 1 type after text", sqlite3_column_type(st, 0));
  sqlite3_step(st);
  P("row 2 type after a new step", sqlite3_column_type(st, 0));
  sqlite3_finalize(st);

  /* An INTEGER and a REAL, to see the stored-vs-viewed split. */
  sqlite3_prepare_v2(db, "SELECT 42, 1.5", -1, &st, 0);
  sqlite3_step(st);
  P("int type", sqlite3_column_type(st, 0));
  sqlite3_column_text(st, 0);
  P("int type after text", sqlite3_column_type(st, 0));
  P("int bytes after text", sqlite3_column_bytes(st, 0));
  P("real type", sqlite3_column_type(st, 1));
  sqlite3_column_text(st, 1);
  P("real type after text", sqlite3_column_type(st, 1));
  P("real bytes after text", sqlite3_column_bytes(st, 1));
  sqlite3_finalize(st);
  sqlite3_close(db);
}

int main(void) {
  printf("probe4 against %s\n", sqlite3_libversion());
  probe_changes_semantics();
  probe_text_values();
  probe_param_spellings();
  probe_blob_bytes();
  return 0;
}
