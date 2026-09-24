/*
** A differential probe for the nsqlite C ABI shim.
**
** This program is compiled twice: once against the real SQLite (3.53.4) and once
** against `nsqlite_capi.dll`. Both builds run the same sequences and print the
** same lines, so `diff` between the two outputs says exactly where the shim
** diverges from the library it is standing in for. Every expectation quoted in
** the shim's source came from the "real" side of that diff, not from recall.
**
** Build:
**   cl /nologo -I<sqlite3 include> probe_diff.c <real sqlite3 link> /Fe:probe_real.exe
**   cl /nologo -I<sqlite3 include> probe_diff.c nsqlite_capi.dll.lib /Fe:probe_shim.exe
*/

#include <stdio.h>
#include <string.h>
#include <stdlib.h>
#include "sqlite3.h"

/* Each print flushes, so a library that crashes mid-probe is located exactly
** rather than four kilobytes back in a block buffer. */
static void P(const char *tag, long v) {
  printf("%-46s = %ld\n", tag, v); fflush(stdout);
}
static void PS(const char *tag, const char *v) {
  printf("%-46s = %s\n", tag, v ? v : "(null)"); fflush(stdout);
}

/* ---- 1. reset()'s return value ------------------------------------- */
static void probe_reset(void) {
  sqlite3 *db = NULL;
  sqlite3_stmt *st = NULL;
  int rc;
  printf("== reset ==\n");
  sqlite3_open(":memory:", &db);
  sqlite3_exec(db, "CREATE TABLE t(a); INSERT INTO t VALUES(1),(2),(3);", 0, 0, 0);

  /* reset after DONE on an empty result */
  sqlite3_prepare_v2(db, "SELECT a FROM t WHERE a=999", -1, &st, 0);
  rc = sqlite3_step(st);
  P("step(empty)->DONE then reset", sqlite3_reset(st));
  P("step again after reset", sqlite3_step(st));
  sqlite3_reset(st);
  sqlite3_finalize(st);

  /* reset after a ROW */
  sqlite3_prepare_v2(db, "SELECT a FROM t", -1, &st, 0);
  sqlite3_step(st);
  P("step->ROW then reset", sqlite3_reset(st));
  P("step again after reset", sqlite3_step(st));
  sqlite3_finalize(st);

  /* reset on a statement that has never been stepped */
  sqlite3_prepare_v2(db, "SELECT a FROM t", -1, &st, 0);
  P("reset on a never-stepped stmt", sqlite3_reset(st));
  sqlite3_finalize(st);

  /* reset twice in a row */
  sqlite3_prepare_v2(db, "SELECT a FROM t", -1, &st, 0);
  sqlite3_step(st);
  sqlite3_reset(st);
  P("reset a second time", sqlite3_reset(st));
  sqlite3_finalize(st);

  /* reset after an error */
  sqlite3_prepare_v2(db, "SELECT * FROM no_such_table_xyz", -1, &st, 0);
  P("prepare(bad table) rc", st ? 1 : 1);
  if (st) {
    P("step(bad table)", sqlite3_step(st));
    P("reset after error", sqlite3_reset(st));
    P("errmsg after reset", 0);
    sqlite3_finalize(st);
  } else {
    printf("%-46s = stmt was NULL\n", "prepare(bad table)");
  }
  sqlite3_close(db);
}

/* ---- 2. changes()/total_changes() around a prepare ------------------ */
static void probe_changes(void) {
  sqlite3 *db = NULL;
  sqlite3_stmt *st = NULL;
  printf("== counters ==\n");
  sqlite3_open(":memory:", &db);
  sqlite3_exec(db, "CREATE TABLE t(a);", 0, 0, 0);
  sqlite3_exec(db, "INSERT INTO t VALUES(1),(2),(3),(4),(5);", 0, 0, 0);
  P("changes after 5-row insert", sqlite3_changes(db));
  P("total_changes after 5-row insert", sqlite3_total_changes(db));
  sqlite3_prepare_v2(db, "SELECT a FROM t", -1, &st, 0);
  P("changes after PREPARE (not stepped)", sqlite3_changes(db));
  P("total_changes after PREPARE", sqlite3_total_changes(db));
  sqlite3_step(st);
  P("changes after stepping the SELECT", sqlite3_changes(db));
  P("total_changes after stepping the SELECT", sqlite3_total_changes(db));
  sqlite3_finalize(st);

  /* A DML statement prepared but not stepped must not move the counters. */
  sqlite3_prepare_v2(db, "INSERT INTO t VALUES(6)", -1, &st, 0);
  P("changes after preparing an INSERT", sqlite3_changes(db));
  sqlite3_step(st);
  P("changes after stepping the INSERT", sqlite3_changes(db));
  P("total_changes after stepping the INSERT", sqlite3_total_changes(db));
  sqlite3_finalize(st);
  sqlite3_close(db);
}

/* ---- 3. read-only opens --------------------------------------------- */
static void probe_readonly(void) {
  sqlite3 *db = NULL;
  sqlite3_stmt *st = NULL;
  int rc;
  const char *path = "probe_ro.db";
  const char *uri = "file:probe_ro.db?mode=ro";
  printf("== readonly ==\n");
  remove(path);
  sqlite3_open(path, &db);
  sqlite3_exec(db, "CREATE TABLE t(a); INSERT INTO t VALUES(7);", 0, 0, 0);
  sqlite3_close(db);

  rc = sqlite3_open_v2(path, &db, SQLITE_OPEN_READONLY, 0);
  P("open_v2 READONLY rc", rc);
  PS("  errmsg", db ? sqlite3_errmsg(db) : "(no db)");
  PS("  db_filename", db ? sqlite3_db_filename(db, "main") : "(no db)");
  if (rc == SQLITE_OK && db) {
    sqlite3_prepare_v2(db, "SELECT a FROM t", -1, &st, 0);
    P("  read on a readonly db: step", sqlite3_step(st));
    P("  read value", st ? sqlite3_column_int(st, 0) : -1);
    sqlite3_finalize(st);
    st = 0;
    rc = sqlite3_exec(db, "INSERT INTO t VALUES(8)", 0, 0, 0);
    P("  write on a readonly db: exec rc", rc);
    PS("  write errmsg", sqlite3_errmsg(db));
    PS("  write errstr", sqlite3_errstr(sqlite3_errcode(db)));
    sqlite3_close(db);
    db = 0;
  } else if (db) {
    sqlite3_close_v2(db);
    db = 0;
  }

  /* The URI spelling. */
  rc = sqlite3_open_v2(uri, &db, SQLITE_OPEN_READONLY | SQLITE_OPEN_URI, 0);
  P("open_v2 uri mode=ro rc", rc);
  PS("  errmsg", db ? sqlite3_errmsg(db) : "(no db)");
  if (rc == SQLITE_OK && db) {
    PS("  db_filename", sqlite3_db_filename(db, "main"));
    sqlite3_close(db);
  } else if (db) {
    sqlite3_close_v2(db);
    db = 0;
  }

  /* A readonly open of a path that does not exist. */
  rc = sqlite3_open_v2("no_such_dir_xyz/foo.db", &db, SQLITE_OPEN_READONLY, 0);
  P("open_v2 READONLY missing path rc", rc);
  PS("  errmsg", db ? sqlite3_errmsg(db) : "(no db)");
  PS("  errstr", sqlite3_errstr(rc));
  if (db) sqlite3_close_v2(db);
  db = 0;

  /* A read/write open of a path that does not exist, no CREATE. */
  rc = sqlite3_open_v2("no_such_dir_xyz/foo.db", &db, SQLITE_OPEN_READWRITE, 0);
  P("open_v2 READWRITE missing path rc", rc);
  PS("  errmsg", db ? sqlite3_errmsg(db) : "(no db)");
  if (db) sqlite3_close_v2(db);
  db = 0;

  /* :memory: under READONLY. */
  rc = sqlite3_open_v2(":memory:", &db, SQLITE_OPEN_READONLY, 0);
  P("open_v2 :memory: READONLY rc", rc);
  if (db) sqlite3_close(db);
  db = 0;
  remove(path);
}

/* ---- 4. named parameters and their sigils --------------------------- */
static void probe_params(void) {
  sqlite3 *db = NULL;
  sqlite3_stmt *st = NULL;
  int i, n;
  const char *cases[] = {
      "SELECT @x, @x, :x, $x",
      "SELECT :x, @x",
      "SELECT @x, :x, @x, :x",
      "SELECT @a, @a, :a, $a",
      "SELECT $x, $x, :x",
      "SELECT :x",
      "SELECT @x",
      "SELECT $x",
      "SELECT :x, :y, @z",
      "SELECT :abc, :ABC",
      "SELECT :a1, :a_1",
      NULL
  };
  int c;
  printf("== parameters ==\n");
  sqlite3_open(":memory:", &db);
  for (c = 0; cases[c]; c++) {
    printf("-- %s\n", cases[c]);
    if (sqlite3_prepare_v2(db, cases[c], -1, &st, 0) != SQLITE_OK || !st) {
      printf("   prepare failed: %s\n", sqlite3_errmsg(db));
      continue;
    }
    n = sqlite3_bind_parameter_count(st);
    P("   count", n);
    for (i = 1; i <= n; i++) {
      char tag[80];
      const char *nm = sqlite3_bind_parameter_name(st, i);
      sprintf(tag, "   name[%d]", i);
      PS(tag, nm);
    }
    /* Lookup by the other sigils. */
    P("   index(\":x\")", sqlite3_bind_parameter_index(st, ":x"));
    P("   index(\"@x\")", sqlite3_bind_parameter_index(st, "@x"));
    P("   index(\"$x\")", sqlite3_bind_parameter_index(st, "$x"));
    P("   index(\"x\")", sqlite3_bind_parameter_index(st, "x"));
    sqlite3_finalize(st);
    st = 0;
  }
  sqlite3_close(db);
}

/* ---- 5. column_int64 / column_double on TEXT ----------------------- */
static void probe_text_numbers(void) {
  sqlite3 *db = NULL;
  sqlite3_stmt *st = NULL;
  int i;
  const char *cases[] = {
      "1e3", "1.9e2", "inf", "nan", "1e400", "9223372036854775808",
      "42abc", "abc", " 7 ", "3.9", "1.5e2xyz", "-12.5rest", ".5",
      "1e", "1e+", "0x10", "1d3", "  -12  ", "+5", "1.7976931348623157e309",
      "9223372036854775807", "9223372036854775809", "1e-400", "1E3",
      "-1e3", "3e", NULL
  };
  printf("== text prefixes ==\n");
  sqlite3_open(":memory:", &db);
  sqlite3_prepare_v2(db, "SELECT ?1", -1, &st, 0);
  for (i = 0; cases[i]; i++) {
    int rc;
    char tag[80];
    sqlite3_reset(st);
    rc = sqlite3_bind_text(st, 1, cases[i], -1, SQLITE_TRANSIENT);
    if (rc != SQLITE_OK) {
      sprintf(tag, "'%s' bind rc", cases[i]);
      P(tag, rc);
      continue;
    }
    rc = sqlite3_step(st);
    if (rc != SQLITE_ROW) {
      sprintf(tag, "'%s' step rc", cases[i]);
      P(tag, rc);
      continue;
    }
    printf("-- '%s'\n", cases[i]);
    P("   int64", sqlite3_column_int64(st, 0));
    P("   double_is_0", sqlite3_column_double(st, 0) == 0.0);
    P("   double_neg1", sqlite3_column_double(st, 0) == -1.0);
    P("   bytes", sqlite3_column_bytes(st, 0));
  }
  sqlite3_finalize(st);
  sqlite3_close(db);
}

/* ---- 6. bind_text with bytes that are not UTF-8 -------------------- */
static void probe_bind_nonutf8(void) {
  sqlite3 *db = NULL;
  sqlite3_stmt *st = NULL;
  int rc;
  static const char bad[] = {'\xff', '\xfe'};
  printf("== bind non-utf8 ==\n");
  sqlite3_open(":memory:", &db);
  sqlite3_prepare_v2(db, "SELECT ?1", -1, &st, 0);
  rc = sqlite3_bind_text(st, 1, bad, 2, SQLITE_TRANSIENT);
  P("bind_text rc", rc);
  PS("errmsg", sqlite3_errmsg(db));
  P("errcode", sqlite3_errcode(db));
  rc = sqlite3_step(st);
  P("step rc", rc);
  if (rc == SQLITE_ROW) {
    P("column_bytes", sqlite3_column_bytes(st, 0));
    P("column_type", sqlite3_column_type(st, 0));
  }
  sqlite3_finalize(st);
  sqlite3_close(db);
}

/* ---- 7. sqlite3_errstr and extended codes -------------------------- */
static void probe_errstr(void) {
  int codes[] = {0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16,
                 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 100, 101,
                 787, 1555, 9999, -1, 256, 517, 787 - 256, 9999 - 9984};
  size_t i;
  printf("== errstr ==\n");
  for (i = 0; i < sizeof(codes) / sizeof(codes[0]); i++) {
    char tag[80];
    sprintf(tag, "errstr(%d)", codes[i]);
    PS(tag, sqlite3_errstr(codes[i]));
  }
  sqlite3_close(0);
}

/* ---- 8. close with unfinalized statements -------------------------- */
static void probe_close(void) {
  sqlite3 *db = NULL;
  sqlite3_stmt *a = NULL, *b = NULL;
  int rc;
  printf("== close ==\n");

  sqlite3_open(":memory:", &db);
  sqlite3_prepare_v2(db, "SELECT 1", -1, &a, 0);
  sqlite3_prepare_v2(db, "SELECT 1", -1, &b, 0);
  rc = sqlite3_close(db);
  P("close, two live stmts", rc);
  /* Probing the handle after a close that did not free it is only safe if it
  ** really was not freed; skip errmsg and go straight to finalizing. */
  sqlite3_finalize(a);
  P("close, one live stmt", sqlite3_close(db));
  sqlite3_finalize(b);
  P("close, no live stmts", sqlite3_close(db));

  /* close_v2 with a live statement. */
  sqlite3_open(":memory:", &db);
  sqlite3_prepare_v2(db, "SELECT 1", -1, &a, 0);
  P("close_v2, one live stmt", sqlite3_close_v2(db));
  sqlite3_finalize(a);
  P("close_v2 again after finalize", sqlite3_close_v2(db));
}

/* ---- 9. column_type after a sibling accessor ----------------------- */
static void probe_column_type(void) {
  sqlite3 *db = NULL;
  sqlite3_stmt *st = NULL;
  int rc;
  printf("== column_type ==\n");
  sqlite3_open(":memory:", &db);

  /* BLOB read as text. */
  sqlite3_prepare_v2(db, "SELECT x'0102ff'", -1, &st, 0);
  rc = sqlite3_step(st);
  P("blob step", rc);
  P("blob type before column_text", sqlite3_column_type(st, 0));
  sqlite3_column_text(st, 0);
  P("blob type after column_text", sqlite3_column_type(st, 0));
  P("blob bytes after column_text", sqlite3_column_bytes(st, 0));
  P("blob type after column_bytes", sqlite3_column_type(st, 0));
  sqlite3_finalize(st);

  /* REAL read as text. */
  sqlite3_prepare_v2(db, "SELECT 1.5", -1, &st, 0);
  sqlite3_step(st);
  P("real type before column_text", sqlite3_column_type(st, 0));
  sqlite3_column_text(st, 0);
  P("real type after column_text", sqlite3_column_type(st, 0));
  sqlite3_finalize(st);

  /* INTEGER read as text. */
  sqlite3_prepare_v2(db, "SELECT 42", -1, &st, 0);
  sqlite3_step(st);
  P("int type before column_text", sqlite3_column_type(st, 0));
  sqlite3_column_text(st, 0);
  P("int type after column_text", sqlite3_column_type(st, 0));
  sqlite3_finalize(st);

  /* TEXT read as blob. */
  sqlite3_prepare_v2(db, "SELECT 'abc'", -1, &st, 0);
  sqlite3_step(st);
  P("text type before column_blob", sqlite3_column_type(st, 0));
  sqlite3_column_blob(st, 0);
  P("text type after column_blob", sqlite3_column_type(st, 0));
  sqlite3_finalize(st);

  /* column_int64 then column_type on a TEXT value. */
  sqlite3_prepare_v2(db, "SELECT '123'", -1, &st, 0);
  sqlite3_step(st);
  P("text'123' type before column_int64", sqlite3_column_type(st, 0));
  sqlite3_column_int64(st, 0);
  P("text'123' type after column_int64", sqlite3_column_type(st, 0));
  sqlite3_finalize(st);

  sqlite3_close(db);
}

/* ---- 10. finalize's return value ------------------------------------ */
static void probe_finalize(void) {
  sqlite3 *db = NULL;
  sqlite3_stmt *st = NULL;
  printf("== finalize ==\n");
  sqlite3_open(":memory:", &db);
  sqlite3_prepare_v2(db, "SELECT 1", -1, &st, 0);
  sqlite3_step(st);
  P("finalize after ROW", sqlite3_finalize(st));
  sqlite3_prepare_v2(db, "SELECT 1", -1, &st, 0);
  sqlite3_step(st);
  sqlite3_step(st);
  P("finalize after DONE", sqlite3_finalize(st));
  sqlite3_prepare_v2(db, "SELECT 1", -1, &st, 0);
  P("finalize without stepping", sqlite3_finalize(st));
  sqlite3_close(db);
}

/* ---- 11. misc: db_filename, autocommit, last_insert_rowid ---------- */
static void probe_misc(void) {
  sqlite3 *db = NULL;
  sqlite3_stmt *st = NULL;
  printf("== misc ==\n");
  sqlite3_open(":memory:", &db);
  P("autocommit", sqlite3_get_autocommit(db));
  sqlite3_exec(db, "CREATE TABLE t(a);", 0, 0, 0);
  P("last_insert_rowid after CREATE", sqlite3_last_insert_rowid(db));
  sqlite3_exec(db, "INSERT INTO t VALUES(1);", 0, 0, 0);
  P("last_insert_rowid after INSERT", sqlite3_last_insert_rowid(db));
  sqlite3_prepare_v2(db, "SELECT a FROM t", -1, &st, 0);
  P("last_insert_rowid after prepare SELECT", sqlite3_last_insert_rowid(db));
  sqlite3_finalize(st);

  /* A read on a rowid table, to see whether last_insert_rowid is disturbed. */
  sqlite3_exec(db, "DELETE FROM t; INSERT INTO t(rowid,a) VALUES(42,1);", 0, 0, 0);
  P("last_insert_rowid after explicit rowid", sqlite3_last_insert_rowid(db));

  /* db_filename spellings. */
  PS("db_filename(\"main\")", sqlite3_db_filename(db, "main"));
  PS("db_filename(\"\")", sqlite3_db_filename(db, ""));
  PS("db_filename(NULL)", sqlite3_db_filename(db, 0));
  PS("db_filename(\"temp\")", sqlite3_db_filename(db, "temp"));
  sqlite3_close(db);

  /* A file-backed one, to see the path it reports. */
  sqlite3_open("probe_fn.db", &db);
  PS("file db_filename", sqlite3_db_filename(db, "main"));
  sqlite3_close(db);
  remove("probe_fn.db");
}

/* ---- 12. column name and count for odd SELECTs -------------------- */
static void probe_names(void) {
  sqlite3 *db = NULL;
  sqlite3_stmt *st = NULL;
  int i, n;
  const char *cases[] = {
      "SELECT 1", "SELECT 1 AS x, 2 AS y", "SELECT 1, 2",
      "SELECT nosuchcol FROM sqlite_master", "SELECT * FROM no_such_table_xyz",
      "CREATE TABLE t(a,b)", "INSERT INTO t VALUES(1,2)",
      "SELECT ?1, ?1", "SELECT :a, :b", "SELECT a+b FROM (SELECT 1 a, 2 b)",
      NULL
  };
  int c;
  printf("== names ==\n");
  sqlite3_open(":memory:", &db);
  sqlite3_exec(db, "CREATE TABLE t(a,b);", 0, 0, 0);
  for (c = 0; cases[c]; c++) {
    int rc;
    char tag[90];
    printf("-- %s\n", cases[c]);
    rc = sqlite3_prepare_v2(db, cases[c], -1, &st, 0);
    P("   prepare rc", rc);
    PS("   errmsg", sqlite3_errmsg(db));
    P("   stmt is null", st == 0);
    if (st) {
      n = sqlite3_column_count(st);
      P("   column_count", n);
      for (i = 0; i < n; i++) {
        sprintf(tag, "   name[%d]", i);
        PS(tag, sqlite3_column_name(st, i));
      }
      rc = sqlite3_step(st);
      P("   step rc", rc);
      sqlite3_finalize(st);
      st = 0;
    }
  }
  sqlite3_close(db);
}

int main(void) {
  printf("probe against %s\n", sqlite3_libversion());
  probe_reset();
  probe_changes();
  probe_readonly();
  probe_params();
  probe_text_numbers();
  probe_bind_nonutf8();
  probe_errstr();
  probe_close();
  probe_column_type();
  probe_finalize();
  probe_misc();
  probe_names();
  return 0;
}
