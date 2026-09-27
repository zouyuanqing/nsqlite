#!/bin/bash
# Differential EXPLAIN QUERY PLAN harness for the explain module.
#
# Runs each query through the probe (a hand-built catalog, so no reopen and no
# pager) and through the real sqlite3 over the same schema, and compares the
# `detail` lines joined with " ~ ".
#
# One invocation per engine per query: the schema is set up in the sqlite3 file
# once up front, and the probe rebuilds the identical catalog every time.
set -u
ROOT=D:/Prj-SQLite-Rust
PROBE=/tmp/blt/debug/examples/probe_explain.exe
DB=/tmp/eqp.db

sqlite3 "$DB" < "$ROOT/tools/eqp_schema.sql" > /dev/null 2>&1

ok=0
bad=0
while IFS= read -r q; do
  [ -z "$q" ] && continue
  n=$("$PROBE" "$q" 2>&1 | tr -d '\r')
  s=$(sqlite3 "$DB" "EXPLAIN QUERY PLAN $q" 2>&1 | sed -n '/^[|`: ]*--/p' \
        | sed 's/^[|`: ]*--//' | sed ':a;N;$!ba;s/\n/ ~ /g')
  if [ "$n" = "$s" ]; then
    ok=$((ok + 1))
  else
    bad=$((bad + 1))
    printf 'DIFF  %s\n  nsqlite: %s\n  sqlite3: %s\n' "$q" "$n" "$s"
  fi
done < "$1"
echo "OK=$ok DIFF=$bad"
