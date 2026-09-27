#!/usr/bin/env python3
"""Sweep the affinity comparison rules against the real sqlite3.

Two sweeps:
  * two columns:  every ordered pair of column affinities, every value pair
  * one column:   every column affinity against the same values as literals

Both emit one result row per case, so the whole sweep is one script and one
diff. The values are chosen to distinguish the rules from each other: 2^53+1 is
an exact i64 and not an exact f64, so REAL-on-both and NUMERIC-on-the-text
disagree there, and '5' against 5 is the asymmetric case from the spec.
"""
import sys

sys.path.insert(0, r"D:\Prj-SQLite-Rust\tools")
from affdiff import diff  # noqa: E402

AFFS = ["TEXT", "NUMERIC", "INTEGER", "REAL", "BLOB"]
# (label, SQL literal) -- the value written into the column or used as the
# literal side of the comparison.
VALUES = [
    ("int5", "5"),
    ("real5", "5.0"),
    ("str5", "'5'"),
    ("str50", "'5.0'"),
    ("frac", "1.5"),
    ("nonum", "'12abc'"),
    ("big", "9007199254740993"),
    ("bigreal", "9007199254740992.0"),
    ("padded", "'  7.5  '"),
    ("blob35", "x'35'"),
]


def two_column_sql(only_a=None, only_b=None):
    lines = []
    for i, a in enumerate(AFFS):
        for b in AFFS:
            if only_a and a != only_a:
                continue
            if only_b and b != only_b:
                continue
            t = "c%d_%s_%s" % (i, a, b)
            lines.append("CREATE TABLE %s(x %s, y %s);" % (t, a, b))
            for vn, vl in VALUES:
                for wn, wl in VALUES:
                    lines.append("INSERT INTO %s VALUES(%s, %s);" % (t, vl, wl))
            lines.append(
                "SELECT '%s|%s', x, y, (x = y), (x < y), (x > y) FROM %s;"
                % (a, b, t)
            )
    return "\n".join(lines)


def one_column_sql():
    lines = []
    for a in AFFS:
        t = "l_%s" % a
        lines.append("CREATE TABLE %s(x %s);" % (t, a))
        for vn, vl in VALUES:
            lines.append("INSERT INTO %s VALUES(%s);" % (t, vl))
        # x against each value spelled as a literal, both written orders.
        for vn, vl in VALUES:
            lines.append(
                "SELECT '%s|lit|%s', x, (x = %s) FROM %s;"
                % (a, vn, vl, t)
            )
            lines.append(
                "SELECT '%s|lit|%s|rev', x, (%s = x) FROM %s;"
                % (a, vn, vl, t)
            )
    return "\n".join(lines)


def cast_sql():
    """CAST through this engine against the same CAST through sqlite3."""
    lines = []
    targets = ["INTEGER", "TEXT", "REAL", "NUMERIC", "BLOB"]
    srcs = [
        ("int5", "5"),
        ("real5", "5.0"),
        ("str5", "'5'"),
        ("str50", "'5.0'"),
        ("strfrac", "'1.5'"),
        ("nonum", "'12abc'"),
        ("padded", "'  7.5  '"),
        ("big", "9007199254740993"),
        ("bigneg", "-9223372036854775808"),
        ("blob35", "x'35'"),
        ("null", "NULL"),
        ("empty", "''"),
        ("exp", "'1e3'"),
    ]
    for sn, sv in srcs:
        for tg in targets:
            lines.append(
                "SELECT 'cast|%s|%s', quote(CAST(%s AS %s));"
                % (sn, tg.lower(), sv, tg)
            )
    return "\n".join(lines)


if __name__ == "__main__":
    which = sys.argv[1] if len(sys.argv) > 1 else "all"
    fns = {
        "two": two_column_sql,
        "one": one_column_sql,
        "cast": cast_sql,
    }
    if which == "two":
        # The two-column sweep is 25 tables x 100 rows; as one script it is
        # past the Windows 32767-byte command line the CLI takes its script
        # on, so it is chunked one affinity pair at a time.
        total = 0
        for i, a in enumerate(AFFS):
            for b in AFFS:
                bad = diff(two_column_sql(a, b), quiet=True)
                if bad:
                    print("pair %s/%s: %d differing lines" % (a, b, len(bad)))
                    for x, y in bad[:6]:
                        print("  sqlite3: %s" % x)
                        print("  nsqlite: %s" % y)
                total += len(bad)
        print("total differing lines: %d" % total)
    elif which == "all":
        for name, fn in fns.items():
            print("=== %s ===" % name)
            diff(fn())
    else:
        diff(fns[which]())
