#!/usr/bin/env python3
"""Diff nsqlite against the real sqlite3 on a SQL script.

Runs the same script through both engines and reports the first line where they
disagree, with the engine's answer beside sqlite3's. Used as the oracle for
affinity work: a claim about SQLite behaviour is worth exactly as much as the
command that produced it, so nothing here is taken on trust.
"""
import os
import subprocess
import sys

NSQLITE = r"D:\Prj-SQLite-Rust\target\debug\nsqlited.exe"
TMPDB = r"D:\Prj-SQLite-Rust\_affdiff_tmp.db"
AFFS = ["TEXT", "NUMERIC", "INTEGER", "REAL", "BLOB", ""]


def run_sqlite(sql):
    p = subprocess.run(["sqlite3", ":memory:"], input=sql, capture_output=True, text=True)
    return clean(p.stdout + p.stderr)


def clean(lines):
    """Keeps only SELECT output, so the two engines are compared on results.

    nsqlited prints the affected row count after every INSERT/UPDATE/DELETE and
    the real shell prints nothing there, so the counts are not comparable. The
    lines being compared are the ones a SELECT produced, identified by the
    '|' separator both sides print.
    """
    return [ln for ln in lines.splitlines() if "|" in ln]


def run_nsqlite(sql):
    # A per-run database name, so a stale file left by an interrupted run
    # cannot make a script fail on `table t already exists` and report every
    # result as missing.
    import tempfile

    tmp = os.path.join(tempfile.gettempdir(), "nsqlite_affdiff_%d.db" % os.getpid())
    for suffix in ("", "-journal", "-wal", "-shm"):
        try:
            os.remove(tmp + suffix)
        except OSError:
            pass
    # The CLI takes its script as one command-line argument, so newlines are
    # folded to spaces; sqlite3 is fed the same folded text so both engines
    # parse literally the same statement stream.
    flat = " ".join(sql.split())
    p = subprocess.run([NSQLITE, tmp, flat], capture_output=True, text=True)
    for suffix in ("", "-journal", "-wal", "-shm"):
        try:
            os.remove(tmp + suffix)
        except OSError:
            pass
    return clean(p.stdout + p.stderr)


def diff(sql, quiet=False):
    """Returns a list of (sqlite_line, nsqlite_line) that differ."""
    a, b = run_sqlite(sql), run_nsqlite(sql)
    bad = []
    for i in range(max(len(a), len(b))):
        x = a[i] if i < len(a) else "<missing>"
        y = b[i] if i < len(b) else "<missing>"
        if x != y:
            bad.append((x, y))
    if bad and not quiet:
        print("DIFF (%d lines)" % len(bad))
        for x, y in bad[:40]:
            print("  sqlite3: %s" % x)
            print("  nsqlite: %s" % y)
    elif not bad:
        print("SAME (%d lines)" % len(a))
    return bad


def grid_sql():
    """Every cell of the affinity grid: 5 affinities x 6 input shapes."""
    values = [
        ("int5", "5"),
        ("real5", "5.0"),
        ("str5", "'5'"),
        ("str50", "'5.0'"),
        ("blob35", "x'35'"),
        ("null", "NULL"),
    ]
    lines = []
    for aff in AFFS:
        ty = aff if aff else "BLOB2"
        for name, lit in values:
            t = "g_%s_%s" % (ty, name)
            lines.append(
                "CREATE TABLE %s(a %s); INSERT INTO %s VALUES(%s); "
                "SELECT '%s/%s', typeof(a), quote(a) FROM %s;"
                % (t, aff, t, lit, ty.lower() or "blobaff", name, t)
            )
    return "\n".join(lines)


if __name__ == "__main__":
    if len(sys.argv) > 1 and sys.argv[1] == "--grid":
        diff(grid_sql())
    elif len(sys.argv) > 1:
        with open(sys.argv[1], encoding="utf-8") as f:
            diff(f.read())
    else:
        diff(sys.stdin.read())
