/*
** Closed-handle accessors, and `mode=ro` under a READWRITE flag.
**
** A fix that adds a liveness registry has to answer these the same way the real
** library does, so they are measured rather than assumed.
**
** Built against the real library only.
*/

#include <stdio.h>
#include <string.h>
#include "sqlite3.h"

static void P(const char *tag, long v) {
  printf("%-52s = %ld\n", tag, v); fflush(stdout);
}
static void PS(const char *tag, const char *v) {
  printf("%-52s = %s\n", tag, v ? v : "(null)"); fflush(stdout);
}

static void probe_closed_handle(void) {
  sqlite3 *db = 0;
  printf("== accessors on a closed handle ==\n");
  sqlite3_open(":memory:", &db);
  sqlite3_close(db);
  PS("errmsg on a closed handle", sqlite3_errmsg(db));
  P("errcode on a closed handle", sqlite3_errcode(db));
  P("extended on a closed handle", sqlite3_extended_errcode(db));
  P("changes on a closed handle", sqlite3_changes(db));
  P("total_changes on a closed handle", sqlite3_total_changes(db));
  P("last_insert_rowid on a closed handle", sqlite3_last_insert_rowid(db));
  P("get_autocommit on a closed handle", sqlite3_get_autocommit(db));
  PS("db_filename on a closed handle", sqlite3_db_filename(db, "main"));
  P("prepare on a closed handle", sqlite3_prepare_v2(db, "SELECT 1", -1, 0, 0));
  PS("  errmsg", sqlite3_errmsg(db));
  P("close_v2 on a closed handle", sqlite3_close_v2(db));
  fflush(stdout);
  printf("(survived every accessor on a closed handle)\n");
  fflush(stdout);
}

static void probe_zombie_handle(void) {
  sqlite3 *db = 0;
  sqlite3_stmt *st = 0;
  printf("== accessors on a zombie (close_v2'd, statement live) ==\n");
  sqlite3_open(":memory:", &db);
  sqlite3_prepare_v2(db, "SELECT 1", -1, &st, 0);
  P("close_v2", sqlite3_close_v2(db));
  PS("errmsg on a zombie", sqlite3_errmsg(db));
  P("changes on a zombie", sqlite3_changes(db));
  P("get_autocommit on a zombie", sqlite3_get_autocommit(db));
  PS("db_filename on a zombie", sqlite3_db_filename(db, "main"));
  P("prepare on a zombie", sqlite3_prepare_v2(db, "SELECT 2", -1, &st == 0 ? 0 : &st, 0));
  PS("  errmsg", sqlite3_errmsg(db));
  fflush(stdout);
  printf("(survived every accessor on a zombie)\n");
  fflush(stdout);
}

static void probe_uri_modes(void) {
  sqlite3 *db = 0;
  const char *path = "probe_uri.db";
  int rc;
  const char *uris[] = {
      "file:probe_uri.db?mode=ro", "file:probe_uri.db?mode=rw",
      "file:probe_uri.db", "file:probe_uri.db?mode=ro&cache=shared",
      "file:probe_uri.db?mode=rwc", "file:probe_uri.db?mode=memory",
      "file:missing_dir_xyz/a.db?mode=ro", NULL
  };
  int i;
  int flag_sets[] = {
      SQLITE_OPEN_READWRITE, SQLITE_OPEN_READWRITE | SQLITE_OPEN_CREATE,
      SQLITE_OPEN_READWRITE | SQLITE_OPEN_URI, SQLITE_OPEN_READONLY | SQLITE_OPEN_URI,
      SQLITE_OPEN_READWRITE | SQLITE_OPEN_CREATE | SQLITE_OPEN_URI };
  const char *flag_names[] = {"RW", "RW|CREATE", "RW|URI", "RO|URI", "RW|CREATE|URI"};
  size_t f;
  printf("== uri modes ==\n");
  remove(path);
  sqlite3_open(path, &db);
  sqlite3_exec(db, "CREATE TABLE t(a); INSERT INTO t VALUES(1);", 0, 0, 0);
  sqlite3_close(db);
  for (f = 0; f < sizeof(flag_sets) / sizeof(flag_sets[0]); f++) {
    for (i = 0; uris[i]; i++) {
      char tag[128];
      db = 0;
      sprintf(tag, "open_v2 %-16s %s", flag_names[f], uris[i]);
      rc = sqlite3_open_v2(uris[i], &db, flag_sets[f], 0);
      printf("-- %-64s rc=%d msg=%s\n", tag, rc, db ? sqlite3_errmsg(db) : "(no db)");
      fflush(stdout);
      if (rc == SQLITE_OK && db) {
        char *e = 0;
        int w = sqlite3_exec(db, "INSERT INTO t VALUES(2)", 0, 0, &e);
        printf("   write rc=%d msg=%s\n", w, e ? e : "(none)");
        fflush(stdout);
        if (e) sqlite3_free(e);
        sqlite3_close(db);
      } else if (db) {
        sqlite3_close_v2(db);
      }
    }
  }
  remove(path);
}

int main(void) {
  printf("probe7 against %s\n", sqlite3_libversion());
  probe_closed_handle();
  probe_zombie_handle();
  probe_uri_modes();
  return 0;
}
