/*
** The last measurements: does columnInt64 saturate from an integer parse or from
** a double conversion, and what does nsqlite's parser say for the errors
** prepare has to report.
**
** Built against the real library only.
*/

#include <stdio.h>
#include <string.h>
#include "sqlite3.h"

static void P(const char *tag, long v) {
  printf("%-44s = %ld\n", tag, v); fflush(stdout);
}

int main(void) {
  sqlite3 *db = 0;
  sqlite3_stmt *st = 0;
  const char *cases[] = {
      /* These distinguish an integer parse from a double conversion:
      ** 2^53+1 is exactly representable as an integer but not as a double. */
      "9007199254740993",   /* 2^53 + 1 */
      "9007199254740995",   /* 2^53 + 3 */
      "12345678901234567",  /* 17 digits, well under 2^63 */
      "123456789012345678", /* 18 digits, under 2^63 */
      "1234567890123456789",/* 19 digits, under 2^63 */
      "18446744073709551615", /* 2^64 - 1 */
      "18446744073709551616", /* 2^64 */
      "-9007199254740993",
      "0",
      "-0.0",
      "  9007199254740993x",
      NULL
  };
  int i;
  sqlite3_open(":memory:", &db);
  sqlite3_prepare_v2(db, "SELECT ?1", -1, &st, 0);
  for (i = 0; cases[i]; i++) {
    sqlite3_reset(st);
    sqlite3_bind_text(st, 1, cases[i], -1, SQLITE_TRANSIENT);
    if (sqlite3_step(st) == SQLITE_ROW) {
      printf("-- '%s'\n", cases[i]);
      printf("   int64 = %lld\n", sqlite3_column_int64(st, 0));
      printf("   double = %.17g\n", sqlite3_column_double(st, 0));
      fflush(stdout);
    }
  }
  sqlite3_finalize(st);

  /* The exact messages prepare reports, so the shim can match them. */
  {
    const char *sqls[] = {
        "SELECT * FROM no_such_table_xyz",
        "SELECT nosuchcol FROM sqlite_master",
        "SELECT nosuchcol FROM t",
        "SELECT a FROM t WHERE nosuchcol = 1",
        "SELECT 1 FROM t GROUP BY nosuchcol",
        "SELECT nosuchfunc(1)",
        "SELECT * FROM t, nosuchtable",
        "INSERT INTO nosuchtable VALUES(1)",
        "SELECT t.a FROM t",
        NULL
    };
    int c;
    sqlite3_exec(db, "CREATE TABLE t(a);", 0, 0, 0);
    for (c = 0; sqls[c]; c++) {
      int rc = sqlite3_prepare_v2(db, sqls[c], -1, &st, 0);
      printf("prepare rc=%d stmt=%s  %-45s  %s\n", rc, st ? "non-null" : "NULL",
             sqls[c], sqlite3_errmsg(db));
      fflush(stdout);
      if (st) sqlite3_finalize(st);
      st = 0;
    }
  }
  sqlite3_close(db);
  return 0;
}
