#!/usr/bin/env python3
"""Fast differential hunter: nsqlited vs the real sqlite3, one statement at a
time, reporting every statement where the two engines do not return the same
typed values.

Why this exists at all: tools/difftest2.sh is correct but slow -- for every
statement it spawns both engines twice (once for the probe of the projection,
once for the value) and copies both database files twice for the checkpoint.
Hunting needs hundreds of statements, so the cost is paid in a different place:
one python process for the whole corpus, one spawn per engine per statement, no
checkpoint copies.

The comparison is still BY VALUE, and strictly: every SELECT is re-run on both
sides as

    SELECT hex(typeof(<expr>)||'~'||quote(<expr>)) AS c1, ... <rest of query>

which is the same projection difftest2.sh builds. When either engine refuses
that projection the statement is re-compared as each engine renders it, and the
summary counts those separately as weak, exactly as difftest2.sh does.

Usage:
    python tools/difftest3.py corpus.sql
    python tools/difftest3.py corpus.sql --section 12
    python tools/difftest3.py corpus.sql --sql "SELECT 1+1"
    python tools/difftest3.py corpus.sql --weak     # show weak comparisons too
"""
import os
import re
import shutil
import subprocess
import sys
import tempfile

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
NSQLITED = os.environ.get("NSQLITED", os.path.join(ROOT, "target", "debug", "nsqlited.exe"))
SQLITE3 = os.environ.get(
    "SQLITE3", r"C:\Users\zyq\scoop\apps\msys2\current\ucrt64\bin\sqlite3.exe"
)

NULLMARK = "~NULL~"
VERBOSE_RAW = False


# --- statement splitting ----------------------------------------------------


def split_statements(text):
    """Split a SQL body on top-level semicolons, respecting string literals,
    double-quoted and backtick identifiers, and -- line comments.  Block
    comments are handled too: a `;` inside one would otherwise split a
    statement in half."""
    out, buf, i, n = [], [], 0, len(text)
    quote = None
    while i < n:
        c = text[i]
        if quote:
            buf.append(c)
            if c == quote:
                if i + 1 < n and text[i + 1] == quote:
                    buf.append(text[i + 1])
                    i += 2
                    continue
                quote = None
            i += 1
            continue
        if c in "'\"`":
            quote = c
            buf.append(c)
            i += 1
            continue
        if text.startswith("--", i):
            j = text.find("\n", i)
            i = n if j < 0 else j
            continue
        if text.startswith("/*", i):
            j = text.find("*/", i + 2)
            i = n if j < 0 else j + 2
            continue
        if c == ";":
            s = "".join(buf).strip()
            if s:
                out.append(s)
            buf = []
            i += 1
            continue
        buf.append(c)
        i += 1
    s = "".join(buf).strip()
    if s:
        out.append(s)
    return out


def parse_sections(path):
    secs, name, body = [], None, []
    with open(path, encoding="utf-8") as f:
        for line in f:
            if line.startswith("###"):
                if name is not None:
                    secs.append((name, "\n".join(body)))
                name, body = line[3:].strip(), []
            elif name is not None:
                body.append(line.rstrip("\n"))
    if name is not None:
        secs.append((name, "\n".join(body)))
    return [(n, b) for n, b in secs if b.strip()]


# --- the typed projection ----------------------------------------------------


def find_result_list(rest):
    """Return (items, tail) for a SELECT, or (None, None).

    `tail` is everything from the top-level FROM onwards; `items` is the
    top-level comma-separated result list.  Quotes and parentheses are tracked
    so a comma inside `max(a,b)` or inside a string does not split an item.
    """
    depth, i, n = 0, 0, len(rest)
    quote = None
    head, tail = rest, ""
    while i < n:
        c = rest[i]
        if quote:
            if c == quote:
                if i + 1 < n and rest[i + 1] == quote:
                    i += 2
                    continue
                quote = None
            i += 1
            continue
        if c in "'\"`":
            quote = c
            i += 1
            continue
        if c == "(":
            depth += 1
            i += 1
            continue
        if c == ")":
            depth -= 1
            i += 1
            continue
        if c == "[":
            # A bracketed identifier, `[not null]`.  The closing bracket ends it
            # whatever it is, so the quote is carried but its VALUE is not
            # compared: `"notnull"` and `notnull` are the same column name and
            # the real engine accepts either spelling.
            quote = "]"
            i += 1
            continue
        if depth == 0 and rest[i : i + 4].upper() == "FROM" and (
            i + 4 >= n or not (rest[i + 4].isalnum() or rest[i + 4] == "_")
        ):
            head, tail = rest[:i], " FROM" + rest[i + 4 :]
            break
        i += 1
    items, depth, cur, quote = [], 0, [], None
    i = 0
    while i < len(head):
        c = head[i]
        if quote:
            cur.append(c)
            if c == quote:
                if i + 1 < len(head) and head[i + 1] == quote:
                    cur.append(head[i + 1])
                    i += 2
                    continue
                quote = None
            i += 1
            continue
        if c in "'\"`":
            quote = c
            cur.append(c)
            i += 1
            continue
        if c == "(":
            depth += 1
            cur.append(c)
            i += 1
            continue
        if c == ")":
            depth -= 1
            cur.append(c)
            i += 1
            continue
        if c == "," and depth == 0:
            items.append("".join(cur).strip())
            cur = []
            i += 1
            continue
        cur.append(c)
        i += 1
    cur = "".join(cur).strip()
    if cur:
        items.append(cur)
    return (items, tail) if items else (None, None)


ALIAS_RE = re.compile(r"\s+AS\s+$", re.IGNORECASE)


def strip_alias(item):
    """Remove a trailing `AS name` or a bare trailing alias, at paren depth 0
    and outside any string."""
    depth, i, n = 0, 0, len(item)
    quote = None
    cut = -1
    while i < n:
        c = item[i]
        if quote:
            if c == quote:
                if i + 1 < n and item[i + 1] == quote:
                    i += 2
                    continue
                quote = None
            i += 1
            continue
        if c in "'\"`":
            quote = c
            i += 1
            continue
        if c == "(":
            depth += 1
            i += 1
            continue
        if c == ")":
            depth -= 1
            i += 1
            continue
        if depth == 0 and item[i : i + 4].upper() == " AS ":
            cut = i
            break
        i += 1
    return item if cut < 0 else item[:cut].strip()


def build_projection(sql):
    """The typed projection of a SELECT, or None when one cannot be built."""
    s = sql.strip().rstrip(";")
    if not s.upper().startswith("SELECT "):
        return None
    rest = s[7:].strip()
    if not rest:
        return None
    lead = ""
    m = re.match(r"(?i)^distinct\s+", rest)
    if m:
        lead = "DISTINCT "
        rest = rest[m.end() :].strip()
    items, tail = find_result_list(rest)
    if not items or any(i.strip() == "*" for i in items):
        return None
    proj = ", ".join(
        "hex(typeof(%s)||'~'||quote(%s)) AS c%d" % (strip_alias(i), strip_alias(i), k + 1)
        for k, i in enumerate(items)
    )
    return "SELECT %s%s%s" % (lead, proj, tail)


# --- running one engine ------------------------------------------------------


def run_sqlite(db, sql, typed=False):
    """Run one statement through the real sqlite3 in list mode.

    `typed` says the statement IS a projection, so each field is a hex string
    the engine produced and is decoded here.  That is what puts the two sides
    into the same shape: without it the real side is raw hex text and the
    nsqlited side is a decoded tagged field, and they can never be equal.  The
    decode is the projection's own inverse -- `hex(x)` is a hex encoding of
    `typeof(e) || '~' || quote(e)` -- so it is lossless.
    """
    script = ".mode list\n.headers off\n.separator |\n.nullvalue %s\n%s;\n" % (NULLMARK, sql)
    p = subprocess.run(
        [SQLITE3, "-batch", db], input=script, capture_output=True, text=True,
        encoding="utf-8", errors="replace",
    )
    out = p.stdout.replace("\r\n", "\n").replace("\r", "")
    err = p.stderr
    if err:
        m = re.search(r": (.*)$", err.strip().splitlines()[-1])
        err = m.group(1) if m else err.strip()
    lines = [l for l in out.split("\n") if l != ""]
    if not typed:
        return lines, err.strip(), lines
    rows = []
    for l in lines:
        if l == NULLMARK or not l:
            # A NULL projection: `typeof(NULL)||'~'||quote(NULL)` is the
            # literal text `null~NULL`, which hex() renders as digits, so a
            # NULL here is already a value and not a missing one.  A line that
            # IS the nullvalue is a whole row of one NULL column.
            rows.append(l)
            continue
        rows.append("|".join(decode_projection_field(f) for f in l.split("|")))
    return rows, err.strip(), lines


def decode_projection_field(f):
    """One typed field, on either engine, back to `class~rendering`.

    Applied last, after each side's own transport decode, so both sides arrive
    here as the same kind of thing and the same function undoes the projection
    for both.  The `~` is what keeps a value containing arbitrary bytes -- a
    blob, a string with a newline in it -- from being ambiguous: the class is
    always one of the five fixed words, so the FIRST `~` is the boundary.
    """
    if f == NULLMARK:
        return NULLMARK
    if not re.fullmatch(r"[0-9A-Fa-f]*", f) or not f or len(f) % 2:
        return f
    try:
        text = bytes.fromhex(f).decode("utf-8", "replace")
    except ValueError:
        return f
    if "~" not in text:
        return text
    cls, _, rest = text.partition("~")
    if cls in ("null", "integer", "real", "text", "blob"):
        return "<%s> %s" % (cls, rest)
    return text


def unhex_field(f):
    """A record-stream field back into a comparable token.

    T/B -> the bytes, quoted so a NULL and an empty string stay distinct and a
    separator inside a value cannot be mistaken for a field boundary.
    I/F -> the rendered text, quoted, so a real and an integer never match.

    Both sides are given the SAME treatment, which is the part that matters and
    the part a first draft of this got wrong.  Each engine's typed projection is
    the text `typeof(e)||'~'||quote(e)`, and each engine hex-encodes it on the
    way out -- the real one because `hex()` is in the projection, this one
    because the record stream encodes every payload as hex after a type tag. So
    the same two decodes apply to both, and the tag byte is dropped only on the
    record-stream side where it exists:

        record stream   T 696E...  ->  tag dropped, hex decoded
        list mode         696E...  ->  hex decoded

    NULL becomes the sentinel on both sides and a `~` inside a value can never
    be mistaken for the `~` the projection puts between the class and the
    rendering.
    """
    if f == "-":
        return NULLMARK
    tag, payload = f[0], f[1:]
    if tag not in "TBIF":
        return f
    if not re.fullmatch(r"[0-9A-Fa-f]*", payload):
        return f
    try:
        return bytes.fromhex(payload).decode("utf-8", "replace")
    except ValueError:
        return f


def run_nsqlite(db, sql, typed=False):
    p = subprocess.run(
        [NSQLITED, "--testsuite", db, sql], capture_output=True, text=True,
        encoding="utf-8", errors="replace",
    )
    rows, err = [], ""
    raw = []
    for line in p.stdout.split("\n"):
        if not line.strip():
            continue
        raw.append(line)
        if line.startswith("C "):
            continue
        if line.startswith("R "):
            f = line[2:].split()
            if typed:
                # A typed projection.  The projection is
                # `hex(typeof(e)||'~'||quote(e))`, and the record stream hex
                # encodes that on the way out, so this field is hex TWICE:
                # once by `hex()` and once by the stream.  The same is true of
                # the real engine's list-mode output, once.  So the order is
                # stream-decode then projection-decode on BOTH sides, and
                # getting it wrong shows up as every value disagreeing while
                # the two raw streams are visibly identical -- which is how it
                # was found.
                fields = [
                    decode_projection_field(unhex_field(x)) for x in f
                ]
            else:
                fields = [unhex_field(x) for x in f]
            rows.append("|".join(fields))
        elif line.startswith("E "):
            err = line[2:]
            break
    if not err and p.stderr.strip():
        m = re.search(r"Error: (?:[A-Z]+: )?(.*)$", p.stderr.strip().splitlines()[-1])
        err = m.group(1) if m else p.stderr.strip()
    return rows, err, raw


# --- one statement -----------------------------------------------------------


def norm_err(e):
    e = e.strip()
    for pre in ("Parse error in ", "Error in ", "Parse error near line ", "Error near line "):
        if e.startswith(pre):
            e = e.split(":", 2)[-1].strip() if e.count(":") >= 2 else e
    e = re.sub(r"^[A-Z_]+: ", "", e)
    return e.strip()


def same_refusal(a, b):
    if not a or not b:
        return False
    a, b = norm_err(a), norm_err(b)
    if a == b:
        return True
    return b.startswith(a + ":") or a.startswith(b + ":")


class Case:
    def __init__(self, db_r, db_n, total):
        self.db_r, self.db_n, self.total = db_r, db_n, total
        self.agree = self.diff = self.weak = self.wording = 0
        self.diffs = []

    def snapshot(self):
        for src, dst in ((self.db_r, "r.snap"), (self.db_n, "n.snap")):
            try:
                shutil.copyfile(src, os.path.join(os.path.dirname(src), dst))
            except OSError:
                pass

    def restore(self):
        for snap, db in (("r.snap", self.db_r), ("n.snap", self.db_n)):
            p = os.path.join(os.path.dirname(self.db_r), snap)
            if os.path.exists(p):
                shutil.copyfile(p, db)
        for j in ("-journal", "-wal", "-shm"):
            for db in (self.db_r, self.db_n):
                if os.path.exists(db + j):
                    os.remove(db + j)

    def run(self, sql):
        self.total += 1
        self.snapshot()
        rows_n, err_n, raw_n = run_nsqlite(self.db_n, sql)

        def report(reason, r, n, detail="", raw_r="", raw_n=""):
            self.diff += 1
            self.diffs.append((reason, r, n, detail, sql, raw_r, raw_n))
            self.restore()

        if not rows_n and err_n:
            rows_r, err_r, _ = run_sqlite(self.db_r, sql)
            if err_r:
                if same_refusal(err_n, err_r):
                    self.wording += 1
                    self.agree += 1
                else:
                    report("both refused, for different reasons", "Error: " + err_r, "Error: " + err_n)
            else:
                report("nsqlited refused a statement sqlite3 accepted", "\n".join(rows_r), "Error: " + err_n)
            return

        proj = build_projection(sql)
        if proj:
            pr, perr, raw_pr = run_sqlite(self.db_r, proj, typed=True)
            pn, pnerr, raw_pn = run_nsqlite(self.db_n, proj, typed=True)
            if not perr and pn:
                if pr == pn:
                    self.agree += 1
                    return
                report(
                    "values differ (typed)", "\n".join(pr), "\n".join(pn),
                    first_diff(pr, pn), "\n".join(raw_pr), "\n".join(raw_pn),
                )
                return
            # projection unusable: fall through to the weak comparison

        rows_r, err_r, raw_r = run_sqlite(self.db_r, sql)
        if err_r:
            if same_refusal(err_n or "", err_r):
                self.wording += 1
                self.agree += 1
            else:
                report("sqlite3 refused a statement nsqlited accepted", "Error: " + err_r, "ok")
            return
        if rows_r == rows_n:
            self.agree += 1
            self.weak += 1
            return
        report(
            "values differ (WEAK: no usable typed projection)",
            "\n".join(rows_r), "\n".join(rows_n), first_diff(rows_r, rows_n),
            "\n".join(rows_r), "\n".join(raw_n),
        )


def first_diff(r, n):
    """The first differing row and, within it, the first differing column.

    Reported as text rather than as hex, because a reader wants to see
    `integer~14` against `real~14.0` and not `696E74656765727E3134` against
    `7265616C7E31342E30`.
    """
    for i in range(max(len(r), len(n))):
        a = r[i] if i < len(r) else "(no row)"
        b = n[i] if i < len(n) else "(no row)"
        if a == b:
            continue
        if len(r) != len(n):
            return "  row counts differ: sqlite3 %d, nsqlited %d" % (len(r), len(n))
        for k, (x, y) in enumerate(zip(a.split("|"), b.split("|"))):
            if x != y:
                return "  row %d column %d: sqlite3=%s  nsqlited=%s" % (i + 1, k + 1, x, y)
        return "  row %d: field counts differ" % (i + 1)
    return ""


def section_key(name):
    """The stable name of a section, for `--section`.

    The corpus names its sections `1a the smallest statement that shows ...`,
    so the key is the leading token: `1a`.  A NAME rather than an index, because
    the corpus is meant to be widened while hunting, and an index moves every
    time a section is inserted above another -- so an index recorded in a
    roadmap entry goes stale silently and then reports the wrong section.
    """
    return name.split(" ", 1)[0].strip().lower()


def main():
    args = sys.argv[1:]
    if not args:
        print(__doc__)
        return 2
    one = None
    if "--sql" in args:
        one = args[args.index("--sql") + 1]
        args = [a for a in args if not a.startswith("--")]
    path = args[0]
    only = None
    if "--section" in args:
        only = args[args.index("--section") + 1].strip().lower()
    global VERBOSE_RAW
    VERBOSE_RAW = "--raw" in args

    work = tempfile.mkdtemp(prefix="nsqlite-diff3-")
    db_r, db_n = os.path.join(work, "r.db"), os.path.join(work, "n.db")
    totals = dict(total=0, agree=0, diff=0, weak=0, wording=0)
    reports = []

    try:
        if one is not None:
            secs = [("(ad-hoc)", one)]
        else:
            secs = parse_sections(path)
        if only:
            chosen = [(i, n, b) for i, (n, b) in enumerate(secs, 1)
                      if section_key(n) == only or str(i) == only]
            if not chosen:
                print("no section matches %r; the sections are:" % only, file=sys.stderr)
                for i, (n, _) in enumerate(secs, 1):
                    print("  %-6s %s" % (section_key(n), n), file=sys.stderr)
                return 2
            secs = [(n, b) for _, n, b in chosen]
        for idx, (name, body) in enumerate(secs, 1):
            for db in (db_r, db_n):
                for suffix in ("", "-journal", "-wal", "-shm"):
                    if os.path.exists(db + suffix):
                        os.remove(db + suffix)
            c = Case(db_r, db_n, 0)
            for s in split_statements(body):
                if s.startswith(">"):
                    continue
                c.run(s)
            for k in totals:
                totals[k] += getattr(c, k)
            for d in c.diffs:
                reports.append((idx, name) + d)
    finally:
        shutil.rmtree(work, ignore_errors=True)

    for idx, name, reason, r, n, detail, sql, raw_r, raw_n in reports:
        print("=== section %d (%s)" % (idx, name))
        print("  reason:    %s" % reason)
        print("  sqlite3:   %s" % r.replace("\n", " / "))
        print("  nsqlited:  %s" % n.replace("\n", " / "))
        if detail:
            print("  %s" % detail)
        print("  statement: %s" % sql)
        if VERBOSE_RAW or r != n:
            # The undecoded output of each engine, so a reader -- and the next
            # agent -- can tell an engine defect from a defect in this script.
            # Every disagreement found while this file was being written was
            # checked this way first, because two of them turned out to be the
            # decode and not the engine.
            print("  raw sqlite3:  %s" % raw_r.replace("\n", " / "))
            print("  raw nsqlited: %s" % raw_n.replace("\n", " / "))
        print()
    print(
        "%s: %d/%d statements agreed, %d disagreed, %d weak, %d refusals worded differently"
        % (
            "FAIL" if totals["diff"] else "PASS",
            totals["agree"], totals["total"], totals["diff"],
            totals["weak"], totals["wording"],
        )
    )
    return 1 if totals["diff"] else 0


if __name__ == "__main__":
    sys.exit(main())
