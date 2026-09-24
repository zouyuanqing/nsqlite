#!/usr/bin/env bash
# Generates the interoperability fixtures read by crates/nsqlite/tests/interop.rs.
#
# Every file here is written by the real sqlite3 program, so the tests that
# consume them are checking byte-level compatibility rather than self-agreement.
# Regenerating is idempotent: each file is removed first.
set -euo pipefail

DIR="${1:-$(cd "$(dirname "$0")/.." && pwd)/test/interop}"
mkdir -p "$DIR"

if ! command -v sqlite3 >/dev/null 2>&1; then
    echo "error: sqlite3 not found on PATH" >&2
    exit 1
fi

echo "sqlite3 version: $(sqlite3 --version)"

db() { echo "$DIR/$1"; }

# --- basic.db: one row per storage class, including a negative rowid --------
rm -f "$(db basic.db)"
sqlite3 "$(db basic.db)" <<'SQL'
CREATE TABLE t(id INTEGER PRIMARY KEY, name TEXT, score REAL, data BLOB);
INSERT INTO t VALUES(1,'alpha',1.5,x'AABB');
INSERT INTO t VALUES(2,'beta',2.25,NULL);
INSERT INTO t VALUES(3,'gamma',NULL,x'00FF00');
INSERT INTO t VALUES(-5,'negative',-3.5,x'01');
SQL

# --- empty.db: a table with no rows ----------------------------------------
rm -f "$(db empty.db)"
sqlite3 "$(db empty.db)" "CREATE TABLE e(x);"

# --- bigrow.db: one row far larger than a page, forcing an overflow chain ----
rm -f "$(db bigrow.db)"
# A printable run, so a mis-ordered overflow page shows up as wrong content
# rather than as a length that happens to match.
sqlite3 "$(db bigrow.db)" \
  "CREATE TABLE big(x TEXT);
   INSERT INTO big VALUES(substr(hex(zeroblob(10000)), 1, 20000));"

# --- manyrows.db: more rows than fit on one page, forcing interior pages ----
rm -f "$(db manyrows.db)"
sqlite3 "$(db manyrows.db)" \
  "CREATE TABLE many(a INTEGER, b TEXT);
   WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM r WHERE n<5000)
   INSERT INTO many SELECT n, 'row'||n FROM r;"

# --- negatives.db: rowids at the extremes of the signed range ---------------
rm -f "$(db negatives.db)"
sqlite3 "$(db negatives.db)" <<'SQL'
CREATE TABLE n(x);
INSERT INTO n(rowid, x) VALUES(-9223372036854775808, 'min');
INSERT INTO n(rowid, x) VALUES(-1000, 'neg1000');
INSERT INTO n(rowid, x) VALUES(-1, 'neg1');
INSERT INTO n(rowid, x) VALUES(0, 'zero');
INSERT INTO n(rowid, x) VALUES(1, 'one');
SQL

# --- types.db: every serial type width, plus the NaN normalisation ----------
rm -f "$(db types.db)"
sqlite3 "$(db types.db)" <<'SQL'
CREATE TABLE ty(name TEXT, v);
INSERT INTO ty VALUES('null',NULL);
INSERT INTO ty VALUES('zero',0);
INSERT INTO ty VALUES('one',1);
INSERT INTO ty VALUES('small',-128);
INSERT INTO ty VALUES('medium',140737488355327);
INSERT INTO ty VALUES('max',9223372036854775807);
INSERT INTO ty VALUES('min',-9223372036854775808);
INSERT INTO ty VALUES('real',3.14159265358979323846);
INSERT INTO ty VALUES('negreal',-0.5);
INSERT INTO ty VALUES('text','héllo wörld');
INSERT INTO ty VALUES('empty','');
INSERT INTO ty VALUES('blob',x'000102FEFF');
INSERT INTO ty VALUES('emptyblob',x'');
-- A REAL NaN is normalised to NULL on write and on read, so no file can hold
-- one. 0.0/0.0 produces the NaN that then becomes NULL; casting the *string*
-- 'nan' would not, since that parses as the number zero.
INSERT INTO ty VALUES('nan',0.0/0.0);
SQL

echo
echo "fixtures written to $DIR:"
ls -la "$DIR"
