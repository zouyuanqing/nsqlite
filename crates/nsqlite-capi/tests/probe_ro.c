/*
** Probes that need a specific page size, plus the read-only enforcement
** question: does a read-only handle on a NON-EMPTY file still refuse writes?
**
** The answer decides whether a shim can implement SQLITE_OPEN_READONLY at all:
** `Connection::open` creates page 1 and writes a header, so a database opened
** that way needs a page size, and a handle the caller believes is read-only has
** already had a write happen to the file.
**
** Built twice, exactly like probe_diff.c.
*/

#include <stdio.h>
#include <string.h>
#include <stdlib.h>
#include "sqlite3.h"

static void P(const char *tag, long v) {
  printf("%-50s = %ld\n", tag, v); fflush(stdout);
}
static void PS(const char *tag, const char *v) {
  printf("%-50s = %s\n", tag, v ? v : "(null)"); fflush(stdout);
}

static void make_db(const char *path, int pageSize, int rows) {
  sqlite3 *db = NULL;
  char sql[256];
  int i;
  remove(path);
  sqlite3_open(path, &db);
  sqlite3_exec(db, "PRAGMA page_size=4096;", 0, 0, 0);
  if (pageSize >= 512 && pageSize <= 65536 && (pageSize & (pageSize - 1)) == 0) {
    sprintf(sql, "PRAGMA page_size=%d;", pageSize);
    sqlite3_exec(db, sql, 0, 0, 0);
  }
  sqlite3_exec(db, "CREATE TABLE t(a);", 0, 0, 0);
  sqlite3_exec(db, "BEGIN;", 0, 0, 0);
  for (i = 1; i <= rows; i++) {
    sprintf(sql, "INSERT INTO t VALUES(%d);", i);
    sqlite3_exec(db, sql, 0, 0, 0);
  }
  sqlite3_exec(db, "COMMIT;", 0, 0, 0);
  sqlite3_close(db);
}

static long file_size(const char *path) {
  FILE *f = fopen(path, "rb");
  long n;
  if (!f) return -1;
  fseek(f, 0, SEEK_END);
  n = ftell(f);
  fclose(f);
  return n;
}

static void probe_pagesize(void) {
  const char *path = "probe_ps.db";
  static const int sizes[] = {512, 1024, 2048, 4096, 8192, 16384, 32768, 65536, 700, 100};
  size_t i;
  printf("== page size ==\n");
  for (i = 0; i < sizeof(sizes) / sizeof(sizes[0]); i++) {
    sqlite3 *db = NULL;
    sqlite3_stmt *st = NULL;
    char tag[80];
    char sql[128];
    sprintf(tag, "declared %d: prepare page_size", sizes[i]);
    make_db(path, sizes[i], 1);
    if (sqlite3_open_v2(path, &db, SQLITE_OPEN_READONLY, 0) == SQLITE_OK) {
      sqlite3_prepare_v2(db, "PRAGMA page_size;", -1, &st, 0);
      if (st && sqlite3_step(st) == SQLITE_ROW) {
        P(tag, sqlite3_column_int(st, 0));
        sqlite3_finalize(st);
        st = 0;
      } else {
        PS(tag, "prepare/step failed");
      }
      sqlite3_close(db);
    } else {
      P(tag, -999);
      PS("  errmsg", sqlite3_errmsg(db));
      sqlite3_close_v2(db);
    }
    /* And the round trip: a new database declared at that size. */
    sprintf(sql, "PRAGMA page_size=%d; CREATE TABLE u(a);", sizes[i]);
    remove("probe_ps2.db");
    if (sqlite3_open("probe_ps2.db", &db) == SQLITE_OK) {
      sqlite3_exec(db, sql, 0, 0, 0);
      sqlite3_exec(db, "PRAGMA page_size;", 0, 0, 0);
      sprintf(tag, "declared %d: db filename", sizes[i]);
      PS(tag, sqlite3_db_filename(db, "main"));
      sqlite3_close(db);
    }
  }
  remove(path);
  remove("probe_ps2.db");
}

static void probe_ro_enforcement(void) {
  const char *path = "probe_roe.db";
  sqlite3 *db = NULL;
  sqlite3_stmt *st = NULL;
  int rc;
  long before, after;
  printf("== readonly enforcement ==\n");

  /* 1. An EMPTY (zero-length) file opened READONLY. */
  remove(path);
  fclose(fopen(path, "wb"));
  before = file_size(path);
  rc = sqlite3_open_v2(path, &db, SQLITE_OPEN_READONLY, 0);
  P("empty file, READONLY: open rc", rc);
  PS("  errmsg", db ? sqlite3_errmsg(db) : "(no db)");
  PS("  db_filename", db ? sqlite3_db_filename(db, "main") : "(no db)");
  if (db) {
    P("  changes", sqlite3_changes(db));
    rc = sqlite3_exec(db, "SELECT 1", 0, 0, 0);
    P("  SELECT 1 rc", rc);
    rc = sqlite3_exec(db, "CREATE TABLE z(a)", 0, 0, 0);
    P("  CREATE rc", rc);
    PS("  CREATE errmsg", sqlite3_errmsg(db));
    if (rc == SQLITE_OK && db) {
      P("  sqlite3_changes after the CREATE", sqlite3_changes(db));
      P("  total_changes", sqlite3_total_changes(db));
    }
    P("  close rc", sqlite3_close(db));
    db = 0;
  }
  after = file_size(path);
  P("  file size before open", before);
  P("  file size after open+close", after);

  /* 2. A file that does not exist at all, READONLY. */
  remove("no_such_file_xyz.db");
  rc = sqlite3_open_v2("no_such_file_xyz.db", &db, SQLITE_OPEN_READONLY, 0);
  P("missing file, READONLY: open rc", rc);
  PS("  errmsg", db ? sqlite3_errmsg(db) : "(no db)");
  P("  file exists after open", file_size("no_such_file_xyz.db") >= 0);
  if (db) sqlite3_close_v2(db);
  db = 0;

  /* 3. A real database, READONLY, with a write attempted. Does the
  **   zero-changes counter stay put, and does the file change? */
  make_db(path, 4096, 20);
  before = file_size(path);
  rc = sqlite3_open_v2(path, &db, SQLITE_OPEN_READONLY, 0);
  P("real db, READONLY: open rc", rc);
  if (rc == SQLITE_OK && db) {
    P("  changes after open", sqlite3_changes(db));
    rc = sqlite3_exec(db, "SELECT count(*) FROM t", 0, 0, 0);
    P("  SELECT rc", rc);
    rc = sqlite3_exec(db, "INSERT INTO t VALUES(999)", 0, 0, 0);
    P("  INSERT rc", rc);
    PS("  INSERT errmsg", sqlite3_errmsg(db));
    P("  errcode", sqlite3_errcode(db));
    P("  extended errcode", sqlite3_extended_errcode(db));
    rc = sqlite3_exec(db, "DROP TABLE t", 0, 0, 0);
    P("  DROP rc", rc);
    PS("  DROP errmsg", sqlite3_errmsg(db));
    rc = sqlite3_exec(db, "CREATE TABLE t(a)", 0, 0, 0);
    P("  CREATE rc", rc);
    rc = sqlite3_exec(db, "BEGIN; INSERT INTO t VALUES(1); COMMIT;", 0, 0, 0);
    P("  BEGIN;INSERT;COMMIT rc", rc);
    P("  changes after the failed writes", sqlite3_changes(db));
    P("  total_changes after the failed writes", sqlite3_total_changes(db));
    P("  autocommit", sqlite3_get_autocommit(db));
    /* A prepared write statement, stepped. */
    if (sqlite3_prepare_v2(db, "INSERT INTO t VALUES(555)", -1, &st, 0) == SQLITE_OK) {
      P("  prepare INSERT rc", SQLITE_OK);
      P("  step INSERT rc", sqlite3_step(st));
      PS("  step errmsg", sqlite3_errmsg(db));
      sqlite3_finalize(st);
      st = 0;
    } else {
      P("  prepare INSERT rc", sqlite3_errcode(db));
    }
    sqlite3_close(db);
    db = 0;
  } else if (db) {
    sqlite3_close_v2(db);
    db = 0;
  }
  after = file_size(path);
  P("  file size before", before);
  P("  file size after", after);

  /* 4. A write statement, PREPARED but never stepped, on a readonly handle.
  **    This is the case a probe-run shim cannot avoid. */
  rc = sqlite3_open_v2(path, &db, SQLITE_OPEN_READONLY, 0);
  if (rc == SQLITE_OK && db) {
    rc = sqlite3_prepare_v2(db, "INSERT INTO t VALUES(777)", -1, &st, 0);
    P("  prepare a write on RO rc", rc);
    PS("  prepare errmsg", db ? sqlite3_errmsg(db) : "(no db)");
    P("  stmt is null", st == 0);
    if (st) sqlite3_finalize(st);
    sqlite3_close(db);
    db = 0;
  } else if (db) {
    sqlite3_close_v2(db);
    db = 0;
  }

  /* 5. A SELECT on a readonly handle, prepared, for the page size. */
  rc = sqlite3_open_v2(path, &db, SQLITE_OPEN_READONLY, 0);
  if (rc == SQLITE_OK && db) {
    if (sqlite3_prepare_v2(db, "PRAGMA page_size", -1, &st, 0) == SQLITE_OK) {
      if (sqlite3_step(st) == SQLITE_ROW) P("  page_size read back", sqlite3_column_int(st, 0));
      sqlite3_finalize(st);
    }
    sqlite3_close(db);
    db = 0;
  }
  remove(path);
}

/* Does a file opened READWRITE without CREATE still get created? */
static void probe_nocreate(void) {
  const char *path = "probe_nc.db";
  sqlite3 *db = NULL;
  int rc;
  printf("== READWRITE without CREATE ==\n");
  remove(path);
  rc = sqlite3_open_v2(path, &db, SQLITE_OPEN_READWRITE, 0);
  P("missing file, READWRITE: open rc", rc);
  P("  file exists after open", file_size(path) >= 0);
  if (db) sqlite3_close_v2(db);
  db = 0;
  remove(path);

  /* A READONLY file on disk, opened READWRITE. */
  fclose(fopen("probe_ro_only.db", "wb"));
  rc = sqlite3_open_v2("probe_ro_only.db", &db, SQLITE_OPEN_READWRITE, 0);
  P("unwritable file, READWRITE: open rc", rc);
  PS("  errmsg", db ? sqlite3_errmsg(db) : "(no db)");
  if (db) sqlite3_close_v2(db);
  db = 0;
  remove("probe_ro_only.db");

  /* A directory, opened as a database. */
  rc = sqlite3_open_v2("no_such_dir_xyz", &db, SQLITE_OPEN_READWRITE | SQLITE_OPEN_CREATE, 0);
  P("missing dir as db rc", rc);
  PS("  errmsg", db ? sqlite3_errmsg(db) : "(no db)");
  if (db) sqlite3_close_v2(db);
}

int main(void) {
  printf("probe2 against %s\n", sqlite3_libversion());
  probe_pagesize();
  probe_ro_enforcement();
  probe_nocreate();
  return 0;
}
