#!/usr/bin/env bash
# Differential runner, FIFTH generation: nsqlited against the real sqlite3,
# compared by VALUE, over the areas the first four corpora never reached.
#
# WHY A FIFTH RUNNER
#
# Four runners exist and all four work. `tools/difftest.sh` (with
# `crates/nsqlite/tests/differential.rs`), `tools/difftest2.sh` (with
# `differential2.rs` and `differential4.rs`), and `tools/difftest3.py` (with
# `differential3.rs`). This is not a wider version of any of them. It is built
# around the gaps a survey of all four corpora actually found, and one of those
# gaps is about the RUNNERS rather than the engine:
#
#   * the date and time functions have ZERO occurrences across all four
#     corpora. `grep -c '\bdate(\|\btime(\|\bdatetime(\|\bjulianday(\|\bunixepoch(
#     \|\bstrftime(' tools/difftest2-cases.sql tools/gen2.sql tools/gen4.sql
#     tools/difftest3-cases.sql` returns 0 for every one of them. They are the
#     largest block of untested code in the engine (`func_math.rs`).
#   * `format`/`concat`/`concat_ws` have zero occurrences. `printf` has three.
#
# So the shape here is deliberately different: rather than generating random
# SQL and hoping, the corpus is a list of hand-chosen QUESTIONS whose answers
# are already known, and the runner's job is to find the ones the engine gets
# wrong. The comparison had to get stricter to make that worth anything, which
# is what the rest of this header is about.
#
# WHAT IS COMPARED, AND WHY IT IS NOT THE PRINTED TEXT
#
# By value, always. The two shells format differently -- nsqlited prints the
# real 1.0 as `1` and a blob as bare hex, sqlite3 prints `1.0` and `x'00FF'`
# -- so comparing their renderings would report differences that are only about
# printing. It would also miss the ones that matter: NULL against the empty
# string, and a real against an integer whose digits are identical.
#
# Every query is rewritten into a TYPED PROJECTION the engine evaluates itself.
# Each output column `<expr>` becomes
#
#     hex(typeof(<expr>) || '~' || quote(<expr>)) AS c<n>
#
# so each value arrives as a hex string, and both engines are asked the SAME
# question. The class is carried three times over, which is the point:
#
#   * `typeof` gives the storage class, so a real never matches an integer and
#     a blob never matches text even when the bytes are the same characters.
#   * `quote` gives NULL the four letters `NULL` and the empty string the
#     two-character literal `''`, so the two can never be conflated. It
#     renders a real as `1.0` and an integer as `1`, and a blob as `X'00FF'`,
#     so the class is visible in the payload too and not only in the tag.
#   * hex() means no byte of a value -- a newline, a `|`, a quote, a NUL -- can
#     be mistaken for a field or row boundary. The split is exact because
#     every field is pure hex.
#
# Measured, both engines, same seven questions, byte-identical:
#
#     $ sqlite3 -batch :memory: "SELECT hex(typeof(x'414243')||'~'||quote(x'414243'));"
#     626C6F627E582734313432343327
#     $ printf 'SELECT hex(typeof(x'"'"'414243'"'"')||'"'"'~'"'"'||quote(x'"'"'414243'"'"'));\n' \
#         | nsqlited --testsuite :memory:
#     C 1 T68657828747970656F6628...29
#     R T36323643364636323745353832373334333133343332333433333237
#
# Rows are compared as TUPLES, never as `|`-joined text, so a rendering that
# itself contains `|` cannot forge a field boundary. Rows are NOT sorted,
# columns are NOT reordered, nothing is trimmed or case-folded, and a
# difference in row count or row order IS a difference: ORDER BY is part of the
# answer wherever it is under test.
#
# ERROR TEXT IS COMPARED BYTE FOR BYTE, AND THAT IS A FINDING CLASS
#
# The previous runners count an error message that differs in wording as a
# separate, weaker statistic, so a systematic wording difference never becomes
# a line item anyone fixes. Here it is a first-class result. The two engines
# hand the message over in different envelopes:
#
#     sqlite3:   Parse error near line 1: no such function: nosuchfunc
#     nsqlited:  E no such function: nosuchfunc
#
# The envelope is the SHELL's, not the engine's: `Parse error near line N:` is
# added by the sqlite3 CLI and says nothing about which engine produced the
# message, so it is stripped and the REMAINDER is compared exactly. So is the
# caret line and the echoed statement, which the CLI also adds.
#
# This matters for the class this whole exercise is about. An error that is
# merely reworded is cosmetic. An error that is right in one engine and ABSENT
# in the other is a wrong answer wearing a costume: the statement that should
# have been refused succeeded, and everything downstream trusts the result.
# Both are reported; the second is ranked as the defect.
#
# THE STATE IS HELD IN STEP, OR EVERY FINDING AFTER A REFUSAL IS FAKE
#
# A corpus is a sequence of statements against a database, and the two engines
# must be looking at the same database at every step. When one engine refuses
# a statement the other accepts, the two files diverge, and every statement
# after that point is comparing two different databases. The result is a long
# tail of confident nonsense.
#
# So the runner checkpoints both files before every statement and restores
# BOTH whenever EITHER engine reported an error. The databases are then
# identical in content at the start of every step, and a finding is a fact
# about one statement rather than about a statement plus everything that
# happened to follow it. This is why a section can contain a statement that is
# KNOWN to be refused without poisoning the statements that follow it.
#
# IT IS A SHELL SCRIPT BECAUSE THE NAME SAYS SO; THE BODY IS PYTHON
#
# The comparison decodes hex in both directions and reduces record streams to
# tuples. Doing that in bash means a subshell per field, which the second
# runner's own header measured at 96% of its wall clock, or reading values as
# text, which is the exact defect the second runner was rewritten to fix. The
# script locates a Python 3 and hands it the program on stdin. Nothing else
# about it is unusual and the corpus format is the same `###`-delimited shape
# every other corpus in tools/ uses.
#
# USAGE:
#   tools/difftest3.sh corpus.sql              # run every section
#   tools/difftest3.sh corpus.sql --list       # list the sections, run none
#   tools/difftest3.sh corpus.sql --section 7  # run one section by number
#   tools/difftest3.sh --sql "SELECT 1"        # one ad-hoc statement
#   tools/difftest3.sh --self-test             # check the runner itself
#   tools/difftest3.sh corpus.sql --weak       # also show weak comparisons
#   tools/difftest3.sh corpus.sql --only wrong # only wrong-answer findings
#
# Environment:
#   NSQLITED   this engine's CLI  (default: target/debug/nsqlited.exe)
#   SQLITE3    the real sqlite3   (default: sqlite3 on PATH)
#   WORKDIR    scratch location   (default: a directory beside the repo's tmp/)
#
# Exit status: 0 when the engines agreed everywhere, 1 otherwise.
#
# THE CLASSIFICATION, AND WHY IT IS THREE WAYS AND NOT TWO
#
#   wrong    both engines answered, with DIFFERENT values, or one answered and
#            the other returned no rows. The engine returned a wrong answer
#            rather than an error. This is the class worth fixing: nothing
#            about it fails loudly.
#   refuse   one engine refused a statement the other answered. A missing
#            capability. Real, but it is visible, and it is a different kind
#            of work from a wrong answer.
#   wording  both engines refused and the messages differ. Cosmetic compared to
#            the two above, but it is a real difference and it is listed.
###

set -uo pipefail

DIR="$(cd "$(dirname "$0")/.." && pwd)"

PY=""
for cand in python3 python py; do
    if command -v "$cand" >/dev/null 2>&1; then PY="$(command -v "$cand")"; break; fi
done
if [ -z "$PY" ]; then
    echo "error: no python3 on PATH; this runner locates one to decode hex" >&2
    exit 3
fi

DIFFTEST_ROOT="$DIR" exec "$PY" - "$@" <<'PYEOF'
import os
import re
import shutil
import subprocess
import sys

# The program is fed on stdin, so __file__ is "<stdin>" and there is nothing
# to derive the repo root from. The shell passes it in as an argument; the
# default keeps a bare `python3 - ...` invocation working from the repo root.
ROOT = os.environ.get("DIFFTEST_ROOT") or os.getcwd()
NSQLITED = os.environ.get("NSQLITED", os.path.join(ROOT, "target", "debug", "nsqlited.exe"))
SQLITE3 = os.environ.get("SQLITE3", "sqlite3")
NULLMARK = "~NULL~"

VERBOSE = False
SHOW_WEAK = False
ONLY_WRONG = False
LISTONLY = False
SELFTEST = False
WANT_SECTION = None
CORPUS = None
ONE_SQL = None

# The result lists of the pragmas this corpus asks about. A PRAGMA has no
# SELECT to rewrite, and comparing its raw output is exactly the weak
# rendering comparison this runner exists to avoid, so the columns are named
# here and the pragma is projected like anything else. The names are SQLite's
# own, which is the whole reason they are spelled out rather than guessed.
PRAGMA_COLS = {
    "table_info": ["cid", "name", "type", "notnull", "dflt_value", "pk"],
    "table_xinfo": ["cid", "name", "type", "notnull", "dflt_value", "pk", "hidden"],
    "index_list": ["seq", "name", "unique", "origin", "partial"],
    "index_info": ["seqno", "cid", "name"],
    "index_xinfo": ["seqno", "cid", "name", "desc", "coll", "key"],
    "foreign_key_list": ["id", "seq", "table", "from", "to", "on_update", "on_delete", "match"],
    "table_list": ["schema", "name", "type", "ncol", "wr", "strict"],
    "database_list": ["seq", "name", "file"],
    "compile_options": ["compile_options"],
    "function_list": ["name", "builtin", "type", "enc", "narg", "flags"],
}


def die(msg, code=2):
    sys.stderr.write("error: %s\n" % msg)
    sys.exit(code)


# --- statement splitting ----------------------------------------------------
#
# One statement per `;`. A `;` inside a string literal, a quoted or bracketed
# identifier, or a comment does not end a statement, because a corpus that
# puts one there is asking two different questions and the runner would report
# the difference between them as a finding.


def split_statements(text):
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
        if c == "[":
            quote = "]"
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


# --- the typed projection ---------------------------------------------------


def _scan(text, on_top_from):
    """Walk `text` once, tracking paren depth and quoting.

    `on_top_from` is called with the index of a top-level FROM. Returning True
    from it stops the walk, which is how the result list is separated from the
    rest of the query.
    """
    depth, i, n = 0, 0, len(text)
    quote = None
    while i < n:
        c = text[i]
        if quote:
            if c == quote:
                if i + 1 < n and text[i + 1] == quote:
                    i += 2
                    continue
                quote = None
            i += 1
            continue
        if c in "'\"`":
            quote = c
            i += 1
            continue
        if c == "[":
            quote = "]"
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
        if depth == 0 and c in "Ff" and text[i : i + 4].upper() == "FROM":
            nxt = text[i + 4 : i + 5]
            if not (nxt.isalnum() or nxt == "_"):
                if on_top_from(i):
                    return i
                return None
        i += 1
    return None


def _split_top_level(head):
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
        if c == "[":
            quote = "]"
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
    return items


# Words that can follow an expression and mean the expression CONTINUES, so
# the thing after them is not an alias. Without this list a bare-alias rule
# reads `a IS NULL` as column `a` aliased `IS`.
CONTINUATION_WORDS = frozenset(
    """AND OR NOT IS NULL IN LIKE GLOB MATCH REGEXP BETWEEN COLLATE ESCAPE
    ISNULL NOTNULL XOR DIV MOD""".split()
)

# A token that is a complete expression in itself. If one of these is what a
# trailing "alias" turned out to be, it is not an alias.
OPERATOR_TOKENS = frozenset("""+ - * / % || < > <= >= = <> != & | ~ << >>""".split())


def _tokenize_tail(item):
    """Split `item` into (token, start) pairs, respecting quoting.

    Quoted runs and bracketed identifiers come back as single tokens whatever
    is inside them, so `'a b'` is one token and not two.
    """
    toks, i, n = [], 0, len(item)
    QUOTES = "'\"`"
    PUNCT = QUOTES + "[]."
    while i < n:
        c = item[i]
        if c.isspace():
            i += 1
            continue
        start = i
        if c in QUOTES:
            j = i + 1
            while j < n:
                if item[j] == c:
                    if j + 1 < n and item[j + 1] == c:
                        j += 2
                        continue
                    j += 1
                    break
                j += 1
            toks.append((item[i:j], start))
            i = j
            continue
        if c == "[":
            j = item.find("]", i + 1)
            j = n if j < 0 else j + 1
            toks.append((item[i:j], start))
            i = j
            continue
        if c.isalnum() or c == "_":
            j = i
            while j < n and (item[j].isalnum() or item[j] in "_$"):
                j += 1
            toks.append((item[i:j], start))
            i = j
            continue
        if c == ".":
            j = i + 1
            while j < n and (item[j].isalnum() or item[j] == "_"):
                j += 1
            toks.append((item[i:j], start))
            i = j
            continue
        # Any run of punctuation is one token, so `<=` and `||` stay whole.
        j = i
        while (
            j < n
            and not item[j].isspace()
            and not item[j].isalnum()
            and item[j] not in "_$"
            and item[j] not in PUNCT
        ):
            j += 1
        toks.append((item[i:j], start))
        i = j
    return toks


def strip_alias(item):
    """The expression of a result-list item, with any trailing alias removed.

    An alias has to be removed because the two engines disagree on the name a
    generated column gets -- sqlite3 uses the source text, this engine renders a
    debug form -- and the runner must not compare names it cannot agree on. So
    every item is aliased to c1..cN by the projection instead.

    Three ways an item ends, and the third is the subtle one:

      `a AS z`   an explicit alias
      `a z`      a bare alias
      `a + b`    NOT an alias, and indistinguishable from `a z` by looking at
                 whitespace alone: both are two words with a space between them.
                 Splitting on whitespace makes `a + b` into column `a`, which
                 asks a different question about the same statement, gets an
                 answer both engines agree on, and therefore reports an
                 AGREEMENT for a sum that was never compared. That is the worst
                 failure this runner can have, so the decision is made on
                 TOKENS: a bare alias is a trailing word that is not an
                 operator, is not a word the expression continues with, and is
                 preceded by something that can end an expression.
    """
    toks = _tokenize_tail(item)
    if not toks:
        return item.strip()

    # An explicit AS, at depth 0, outside any string: everything from it on is
    # the alias and everything before it is the expression.
    depth, quote = 0, None
    for idx, (tok, st) in enumerate(toks):
        if quote is not None:
            if tok and tok[0] == quote:
                quote = None
            continue
        if tok and tok[0] in "'\"`":
            quote = tok[0]
            continue
        if tok == "(":
            depth += 1
        elif tok == ")":
            depth -= 1
        elif depth == 0 and tok.upper() == "AS":
            return item[:st].strip()
    if len(toks) < 2:
        return item.strip()

    last_tok, last_start = toks[-1]
    prev_tok, _ = toks[-2]
    if last_tok.upper() in CONTINUATION_WORDS:
        return item.strip()
    if last_tok in OPERATOR_TOKENS:
        return item.strip()
    if not re.fullmatch(r"[A-Za-z_][A-Za-z_0-9$]*", last_tok):
        return item.strip()
    # A word directly after `(` or after an operator is the operand that
    # operator needs, not an alias: `(a b)` is a syntax error and `a + b` is a
    # sum, but both end in a word whose predecessor is not a value.
    if not re.fullmatch(r"[A-Za-z_0-9$)\]]", prev_tok):
        return item.strip()
    return item[:last_start].strip()


def build_projection(sql):
    """The typed projection of a query, or None when one cannot be built.

    Returns None rather than a weaker comparison for anything the engine may
    not be able to run, so the caller can decide. A projection that this engine
    refuses is not a finding about values; it is a finding about capability and
    is reported as such by the caller.
    """
    s = sql.strip().rstrip(";").strip()
    up = s.upper()

    m = re.match(r"(?i)^PRAGMA\s+([A-Za-z_]+)\s*(\(.*\))?$", s)
    if m:
        cols = PRAGMA_COLS.get(m.group(1).lower())
        if not cols:
            return None
        arg = (m.group(2) or "").strip()
        if not (arg.startswith("(") and arg.endswith(")")):
            return None
        # Each column is named in DOUBLE QUOTES, and it has to be. Two of the
        # names in PRAGMA_COLS are words SQLite's own grammar claims --
        # `notnull` and `key` -- and an unquoted reference to either is a syntax
        # error in the projection:
        #
        #     SELECT hex(typeof(notnull)||'~'||quote(notnull)) ... -> syntax error
        #
        # which is not a disagreement about a value, it is the runner's rewrite
        # not being a statement. The projection then fails on both sides for
        # different reasons, the runner drops to comparing the two shells'
        # own renderings, and the two renderings differ -- sqlite3 prints
        # nothing for a pragma on a missing table and nsqlited prints a `C 0`
        # record -- so a statement whose two sides AGREE is reported as a
        # difference. Measured:
        #
        #     PRAGMA table_info(no_such_table)   both engines return zero rows
        #
        # and the runner reported it as a wrong answer. That is the whole class
        # of report this corpus exists to avoid, so the projection is built so
        # it does not happen.
        proj = ", ".join(
            'hex(typeof("%s")||\'~\'||quote("%s")) AS c%d' % (c, c, k + 1)
            for k, c in enumerate(cols)
        )
        # The ARGUMENT is re-quoted as a single-quoted string literal. Carried
        # through as written it is a double-quoted identifier, which SQLite
        # resolves as a column name and refuses:
        #
        #     FROM pragma_table_info("k")   ->  no such column: "k"
        #
        # An unquoted bare name is worse, because it resolves against a table
        # that happens to have a column of that name and silently reads the
        # wrong thing.
        inner = arg[1:-1].strip()
        if not re.fullmatch(r"'(?:[^']|'')*'", inner):
            # A pragma argument that is itself an expression (an index name, a
            # schema-qualified table) is not something this runner will
            # re-quote, so no projection is offered rather than a wrong one.
            return None
        # `inner` is ALREADY a quoted SQL string literal, including its quotes,
        # so it is passed through as it stands. Stripping the quotes and
        # wrapping the remainder again would give ''k'', a string holding the
        # three characters 'k' with quotes in, and a pragma on a table so named
        # returns nothing on both sides -- an empty result that looks like an
        # agreement and is one only by accident.
        return "SELECT %s FROM pragma_%s(%s)" % (proj, m.group(1).lower(), inner)

    m = re.match(r"(?i)^VALUES\s*", s)
    if m:
        return None

    if not up.startswith("SELECT ") and not up.startswith("WITH "):
        return None

    rest = s
    lead = ""
    if up.startswith("SELECT "):
        rest = s[7:].strip()
        d = re.match(r"(?i)^distinct\s+", rest)
        if d:
            lead = "DISTINCT "
            rest = rest[d.end():].strip()
    else:
        # A WITH ... SELECT cannot be rewritten by prefixing: the result list
        # is in the SELECT at the end and the CTE body comes first. Rather than
        # a rewrite that is subtly wrong, report that no projection exists.
        return None

    if not rest:
        return None

    found = []

    def on_from(i):
        found.append(i)
        return True

    pos = _scan(rest, on_from)
    head = rest if not found else rest[: found[0]]
    tail = "" if not found else " FROM" + rest[found[0] + 4:]

    items = _split_top_level(head)
    if not items:
        return None
    for it in items:
        b = strip_alias(it)
        if b == "*" or b.endswith(".*"):
            return None
    proj = ", ".join(
        "hex(typeof(%s)||'~'||quote(%s)) AS c%d" % (strip_alias(i), strip_alias(i), k + 1)
        for k, i in enumerate(items)
    )
    return "SELECT %s%s%s" % (lead, proj, tail)


# --- decoding one side ------------------------------------------------------


def decode_projection_field(f):
    """One typed field on either engine, back to `class~rendering`.

    Applied last, after each side's own transport decode, so both sides arrive
    as the same kind of thing and one function undoes the projection for both.
    The FIRST `~` is the boundary because the class is always one of five
    fixed words, so a value containing arbitrary bytes cannot make it ambiguous.
    """
    if not f or len(f) % 2 or not re.fullmatch(r"[0-9A-Fa-f]*", f):
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


def parse_record_stream(text):
    """nsqlited --testsuite output -> one outcome per statement.

        C <n> <hex>...   n column names, opening a statement
        R <field>...     one row of that statement
        E <text>         the statement failed; the text is the engine's message
        X                a statement that reported a row count
        N                a statement that returned nothing

    A field is `T<hex>` or `B<hex>` for text or blob, `I<decimal>` or `F<text>`
    for an integer or a real, and a lone `-` for NULL. Two properties make this
    the right transport: a NULL is a lone `-` and an empty string is a tag with
    no payload, so the one distinction a print mode cannot make is made; and an
    empty result is UNAMBIGUOUS, being a C record with no R record after it,
    where a print mode writes a Rust Vec debug of the column list with no
    trailing newline and a harness that maps lines into rows counts it as a
    row.

    TWO STATEMENTS, ONE C RECORD. The stream has no end-of-statement marker of
    its own for a statement that produced no rows, so a script of several
    statements runs together and the boundaries have to come from the shape.
    Measured:

        CREATE TABLE t(a); CREATE TABLE t(a);
        X
        E table t already exists

    is two statements, and a parser that attaches the E to the X reports the
    FIRST statement as having failed, which is false and is a wrong answer.
    Every C, X, N and E therefore CLOSES whatever statement is open and opens
    its own, and only R extends the one that is open. That is what makes
    `SELECT 1; SELECT 2;` two records rather than one with two rows.
    """
    outs, cur, closed = [], None, True
    for raw in text.replace("\r\n", "\n").replace("\r", "").split("\n"):
        line = raw.rstrip()
        if not line:
            continue
        tag, _, rest = line.partition(" ")
        if tag == "R":
            if cur is not None and cur["kind"] == "query":
                cur["rows"].append(_decode_fields(rest))
            continue
        # Anything that is not R ends the statement before it.
        if tag == "C":
            cur = {"cols": rest.split(), "rows": [], "err": None, "kind": "query"}
        elif tag == "E":
            cur = {"cols": [], "rows": [], "err": rest, "kind": "error"}
        elif tag in ("X", "N"):
            # A statement with no result set opens no C record: CREATE TABLE,
            # INSERT, BEGIN all report only a row count. It is still a
            # statement and still the one the caller asked about, so it opens a
            # record of its own rather than being dropped -- otherwise
            # `CREATE TABLE t(a)` produces an EMPTY list of outcomes and the
            # caller reads that as "the engine produced no output at all",
            # which is a far louder claim than what happened.
            if cur is None or closed:
                cur = {"cols": [], "rows": [], "err": None, "kind": "exec"}
            else:
                cur["kind"] = "exec"
        else:
            continue
        outs.append(cur)
        closed = True
    return outs


def _decode_fields(rest):
    """A record stream's row -> a tuple of self-describing tokens."""
    fields, i, n = [], 0, len(rest)
    while i < n:
        if rest[i] == " ":
            i += 1
            continue
        t = rest[i]
        j = i + 1
        while j < n and rest[j] != " ":
            j += 1
        payload = rest[i + 1:j]
        i = j
        if t == "-":
            fields.append("null")
        elif t == "T":
            fields.append("t'" + _hex_show(payload) + "'")
        elif t == "B":
            fields.append("b" + payload.upper())
        elif t == "I":
            fields.append("i" + payload)
        elif t == "F":
            fields.append("r" + payload)
        else:
            fields.append("?" + t + payload)
    return tuple(fields)


def _hex_show(h):
    """Hex bytes back to text with `'` doubled, the way quote() would."""
    try:
        b = bytes.fromhex(h)
    except ValueError:
        return h
    s = b.decode("utf-8", "replace")
    return s.replace("'", "''")


# --- error messages ---------------------------------------------------------

ENVELOPE = re.compile(r"^.*?error near line \d+: ?(.*)$", re.IGNORECASE)


def strip_envelope(stderr):
    """sqlite3's CLI envelope off an error message, keeping the rest exact.

    `Parse error near line 1: no such table: t` and `no such table: t` are the
    same message in two wrappings; the wrapping is the shell's and is not part
    of what the engine said. Everything after the envelope is compared byte
    for byte.
    """
    msgs = []
    for line in stderr.replace("\r\n", "\n").split("\n"):
        if not line.strip():
            continue
        m = ENVELOPE.match(line.strip())
        if m:
            msgs.append(m.group(1))
    if msgs:
        return msgs[-1].strip()
    txt = [l for l in stderr.replace("\r\n", "\n").split("\n") if l.strip()]
    return txt[-1].strip() if txt else ""


# --- running one engine -----------------------------------------------------


class Engine:
    def __init__(self, workdir, tag):
        self.db = os.path.join(workdir, tag + ".db")
        self.tag = tag
        for suffix in ("", "-journal", "-wal", "-shm"):
            try:
                os.remove(self.db + suffix)
            except OSError:
                pass

    def snapshot(self):
        snap = {}
        for suffix in ("", "-journal", "-wal", "-shm"):
            p = self.db + suffix
            try:
                with open(p, "rb") as f:
                    snap[suffix] = f.read()
            except OSError:
                snap[suffix] = None
        return snap

    def restore(self, snap):
        for suffix in ("", "-journal", "-wal", "-shm"):
            p = self.db + suffix
            if snap.get(suffix) is None:
                try:
                    os.remove(p)
                except OSError:
                    pass
            else:
                with open(p, "wb") as f:
                    f.write(snap[suffix])

    def _run(self, script, timeout=60):
        # The script goes in as BYTES. Passing a str makes subprocess pick a
        # text wrapper, and on Windows that wrapper blocks forever on a
        # program that does not read stdin the way the wrapper expects -- the
        # run simply hangs until the timeout. Measured, same command, same
        # engine, same database:
        #
        #     input=b'SELECT 1 AS a;\n'   ->  rc 0, b'C 1 T61\nR I31\n'
        #     input='SELECT 1 AS a;\n'    ->  TimeoutExpired after 60s
        #
        # Bytes also mean no encoding choice is made for the corpus, so a
        # statement holding a byte that is not valid UTF-8 in the console's
        # code page still reaches the engine byte for byte.
        p = subprocess.run(
            [self.exe] + self.args + [self.db],
            input=script.encode("utf-8", "surrogateescape"),
            capture_output=True,
            timeout=timeout,
        )
        return p.stdout, p.stderr.decode("utf-8", "replace")

    def run_raw(self, sql, timeout=60):
        return self._run(sql.rstrip().rstrip(";") + ";\n", timeout)


class Nsqlite(Engine):
    exe = NSQLITED
    args = ["--testsuite"]

    def query(self, sql, timeout=60):
        """-> (rows|None, err) where rows is a tuple of tuples of tokens."""
        out, err = self.run_raw(sql, timeout)
        outs = parse_record_stream(out.decode("utf-8", "surrogateescape"))
        if not outs:
            return None, strip_envelope(err) or "no output"
        rec = outs[-1]
        if rec["err"]:
            return None, rec["err"]
        if rec["kind"] == "query":
            return rec["rows"], None
        return None, None

    def run(self, sql, timeout=60):
        """-> (ok, err). A statement that is not a query has no value."""
        out, err = self.run_raw(sql, timeout)
        outs = parse_record_stream(out.decode("utf-8", "surrogateescape"))
        if not outs:
            return False, strip_envelope(err) or "no output"
        rec = outs[-1]
        return (rec["err"] is None), rec["err"]


class Sqlite(Engine):
    exe = SQLITE3
    args = ["-batch"]

    PREFIX = (
        ".mode list\n.headers off\n.separator |\n.nullvalue %s\n"
        "PRAGMA trusted_schema=OFF;\n" % NULLMARK
    )

    def _lines(self, script, timeout=60):
        p = subprocess.run(
            [SQLITE3, "-batch", self.db],
            input=(self.PREFIX + script).encode("utf-8", "surrogateescape"),
            capture_output=True, timeout=timeout,
        )
        out = p.stdout.decode("utf-8", "surrogateescape")
        out = out.replace("\r\n", "\n").replace("\r", "")
        return [l for l in out.split("\n")], p.stderr.decode("utf-8", "replace")

    def query(self, sql, timeout=60):
        lines, err = self._lines(sql.rstrip().rstrip(";") + ";\n", timeout)
        if err.strip():
            return None, strip_envelope(err)
        rows = []
        for l in lines:
            if l == "":
                continue
            if l == NULLMARK:
                rows.append((NULLMARK,))
                continue
            rows.append(tuple(l.split("|")))
        return rows, None

    def run(self, sql, timeout=60):
        lines, err = self._lines(sql.rstrip().rstrip(";") + ";\n", timeout)
        if err.strip():
            return False, strip_envelope(err)
        return True, None


# --- the comparison ---------------------------------------------------------


class Finding(object):
    def __init__(self, kind, section, stmt, note, r, n, detail=""):
        self.kind = kind
        self.section = section
        self.stmt = stmt
        self.note = note
        self.r = r
        self.n = n
        self.detail = detail


def as_tokens(rows, mode):
    """Rows from one transport -> rows of comparable tokens.

    The two transports hand the runner different things and this is where they
    are made the same, which is the one place the two shapes are allowed to
    differ.

      "sqlite"  sqlite3 printed the projection's own output, which IS hex
                 text, so the field is decoded here.

      "nsqlite" nsqlited's record stream tags every field by class and
                 hex-encodes the bytes: the projection's own hex text arrives
                 as T36393645..., i.e. a TEXT field whose bytes are the ASCII
                 of that hex. So the tag is stripped and the bytes are handed
                 on, and the SAME decoder as the other side finishes the job.

    The tag has to come off first. `t'36393645'` is not hex, so a decoder
    given the tagged form finds no hex, gives up, and returns the tagged form
    unchanged -- and the two sides then disagree on every statement of the
    whole corpus while each is in fact correct. That is the shape of runner
    defect the previous generation of this runner already had once (a decoder
    that could not tell a blob from a text value), and it is why this one
    decodes once, at the end, for both sides.
    """
    out = []
    for row in rows:
        cells = []
        for f in row:
            if f == NULLMARK:
                cells.append(NULLMARK)
            elif (
                mode == "nsqlite"
                and len(f) > 3
                and f[0] == "t"
                and f[1] == "'"
                and f[-1] == "'"
            ):
                # f[2:-1] is already the hex of the field's bytes -- the
                # record stream carries it hex-encoded -- so it is handed
                # straight to the same decoder the other side uses. It is NOT
                # run through _hex_show: that function goes the other way,
                # turning bytes into text, and applying it here would produce
                # the ASCII of the hex rather than the bytes the hex stands
                # for, which compares `696E...` against `integer~2`.
                cells.append(decode_projection_field(f[2:-1]))
            elif mode == "sqlite":
                cells.append(decode_projection_field(f))
            else:
                cells.append(f)
        out.append(tuple(cells))
    return out


def compare(rq, nq):
    """Both sides answered. Are the values the same?

    The hex is decoded on both sides AFTER each transport has delivered it, so
    a value that is NULL on one side and the empty string on the other cannot
    be conflated, and a real cannot match an integer.
    """
    r_rows, n_rows = rq, nq
    if (r_rows is None) != (n_rows is None):
        return "wrong", "one engine returned no rows and the other did"
    if r_rows is None:
        return "agree", ""
    if len(r_rows) != len(n_rows):
        return "wrong", "row counts differ"
    for i, (a, b) in enumerate(zip(r_rows, n_rows)):
        if len(a) != len(b):
            return "wrong", "row %d: column counts differ" % (i + 1)
        for j, (x, y) in enumerate(zip(a, b)):
            if x != y:
                return "wrong", "row %d column %d" % (i + 1, j + 1)
    return "agree", ""


def quote_projection(sql):
    """`SELECT quote(e) AS c1, ...` for a query, or None.

    Weaker than the typed projection in one way only: quote() renders a real
    as `1.0` and an integer as `1`, so the class is visible in the payload but
    not as a separate tag, and two values that quote identically but have
    different classes would be missed. Everything else -- NULL against '', a
    blob against text -- is still carried.
    """
    s = sql.strip().rstrip(";")
    if not s.upper().startswith("SELECT "):
        return None
    rest = s[7:].strip()
    d = re.match(r"(?i)^distinct\s+", rest)
    lead = ""
    if d:
        lead = "DISTINCT "
        rest = rest[d.end():].strip()
    if not rest:
        return None
    found = []

    def on_from(i):
        found.append(i)
        return True

    _scan(rest, on_from)
    head = rest if not found else rest[: found[0]]
    tail = "" if not found else " FROM" + rest[found[0] + 4:]
    items = _split_top_level(head)
    if not items or any(strip_alias(i) in ("*",) for i in items):
        return None
    return "SELECT %s%s" % (
        lead,
        ", ".join("quote(%s) AS c%d" % (strip_alias(i), k + 1) for k, i in enumerate(items)),
    ) + tail


# --- running a corpus -------------------------------------------------------


def report(f, n_only):
    if f.kind == "agree":
        return
    if ONLY_WRONG and f.kind != "wrong":
        return
    print("")
    print("  %s: %s" % (f.kind.upper(), f.stmt))
    if f.note:
        print("    %s" % f.note)
    if f.r is not None:
        print("    sqlite3:  %s" % f.r)
    if f.n is not None:
        print("    nsqlited: %s" % f.n)
    if f.detail:
        for line in f.detail.splitlines():
            print("      %s" % line)


def main():
    global VERBOSE, SHOW_WEAK, ONLY_WRONG, LISTONLY, SELFTEST, WANT_SECTION, CORPUS, ONE_SQL

    # The report prints VALUES, and a value is arbitrary bytes. On a machine
    # whose console code page is GBK a lone replacement character cannot be
    # encoded and the report dies halfway through with a UnicodeEncodeError --
    # which loses every finding after the one that crashed it, and makes the
    # crash look like the last finding. So the report is written as UTF-8 with
    # a replacement for anything that will not survive, and nothing is
    # truncated: the point of printing a value is to show what it was.
    try:
        sys.stdout.reconfigure(encoding="utf-8", errors="replace")
    except Exception:
        pass

    args = sys.argv[1:]
    i = 0
    while i < len(args):
        a = args[i]
        if a in ("-v", "--verbose"):
            VERBOSE = True
        elif a == "--weak":
            SHOW_WEAK = True
        elif a == "--only":
            a2 = args[i + 1] if i + 1 < len(args) else ""
            if a2 == "wrong":
                ONLY_WRONG = True
            else:
                die("--only takes one of: wrong, refuse, wording")
            i += 1
        elif a == "--only-wrong":
            ONLY_WRONG = True
        elif a == "--list":
            LISTONLY = True
        elif a == "--self-test":
            SELFTEST = True
        elif a == "--section":
            WANT_SECTION = args[i + 1] if i + 1 < len(args) else ""
            i += 1
        elif a == "--sql":
            ONE_SQL = args[i + 1] if i + 1 < len(args) else ""
            i += 1
        elif a.startswith("-"):
            die("unknown option %r" % a)
        else:
            CORPUS = a
        i += 1

    if SELFTEST:
        return self_test()
    if ONE_SQL is not None and CORPUS is None:
        CORPUS = "<sql>"
    if not CORPUS:
        die("no corpus file and no --sql; try --help")
    if not os.path.isfile(CORPUS) and CORPUS != "<sql>":
        die("no such corpus: %s" % CORPUS)

    if not os.path.isfile(NSQLITED):
        die("engine not built: %s" % NSQLITED)
    if shutil.which(SQLITE3) is None and not os.path.isfile(SQLITE3):
        die("no such sqlite3: %s" % SQLITE3)

    if CORPUS == "<sql>":
        sections = [("ad-hoc", ONE_SQL)]
    else:
        sections = parse_sections(CORPUS)

    if LISTONLY:
        print("%d sections:" % len(sections))
        for k, (n, _) in enumerate(sections, 1):
            print("  %3d  %s" % (k, n))
        return 0

    workdir = os.environ.get("WORKDIR") or os.path.join(ROOT, "tmp", "difftest3-work")
    os.makedirs(workdir, exist_ok=True)

    counts = {"agree": 0, "wrong": 0, "refuse": 0, "wording": 0}
    weak_total = 0
    unverified = 0
    findings = []
    total = 0

    for idx, (name, body) in enumerate(sections, 1):
        if WANT_SECTION is not None and str(idx) != str(WANT_SECTION):
            continue
        stmts = split_statements(body)
        if not stmts:
            continue
        ns = Nsqlite(workdir, "n")
        sq = Sqlite(workdir, "r")
        print("")
        print("=== section %d: %s" % (idx, name))
        for sql in stmts:
            total += 1
            if VERBOSE:
                print("  . %s" % sql)
            snap_n, snap_r = ns.snapshot(), sq.snapshot()
            proj = build_projection(sql)
            strict = proj is not None

            if strict:
                r_rows, r_err = sq.query(proj)
                n_rows, n_err = ns.query(proj)
                if r_err or n_err:
                    r_ok, r_err2 = sq.run(sql)
                    n_ok, n_err2 = ns.run(sql)
                    if r_ok and not n_ok:
                        f = Finding("refuse", name, sql, "nsqlited refused a statement sqlite3 answered",
                                    "ok", "Error: " + (n_err2 or ""))
                    elif n_ok and not r_ok:
                        f = Finding("refuse", name, sql, "sqlite3 refused a statement nsqlited answered",
                                    "Error: " + (r_err2 or ""), "ok")
                    elif r_ok and n_ok:
                        # Both engines ran the ORIGINAL, so the statement is
                        # answerable and the only reason the projection was not
                        # is that the rewrite is not runnable here. The rows are
                        # read from the record stream and the two are shown, so
                        # the reader can see which one is wrong rather than
                        # being told there is a difference.
                        r_raw, _ = sq.query(sql)
                        n_raw, _ = ns.query(sql)
                        f = Finding(
                            "wrong", name, sql,
                            "the typed projection could not run here, so the two "
                            "renderings are compared instead (weaker)",
                            " / ".join(" | ".join(x) for x in (r_raw or [])),
                            " / ".join(" | ".join(x) for x in (n_raw or [])),
                        )
                    elif (r_err2 or "") == (n_err2 or ""):
                        counts["agree"] += 1
                        f = None
                    else:
                        f = Finding("wording", name, sql, "both refused, differently",
                                    r_err2 or "", n_err2 or "")
                else:
                    # The two sides arrive in DIFFERENT shapes and each is put
                    # into the same one before they are compared.
                    #
                    # sqlite3 delivered the projection's own hex text, so it is
                    # decoded here. nsqlited delivered a record stream whose
                    # fields are already tagged by the engine itself -- a text
                    # field arrives as t'<bytes>' -- so decoding it again would
                    # look for hex in a string that is not hex, fail, and hand
                    # back the tagged form unchanged. That mismatch is why both
                    # sides go through as_tokens with the flag each needs: a
                    # blind decode on both sides reports a disagreement on
                    # every single statement, which is how a runner ends up
                    # finding everything wrong and therefore saying nothing.
                    ra = as_tokens(r_rows, "sqlite")
                    na = as_tokens(n_rows, "nsqlite")
                    verdict, detail = compare(ra, na)
                    if verdict == "agree":
                        counts["agree"] += 1
                        f = None
                    else:
                        f = Finding("wrong", name, sql, detail,
                                    " / ".join(" | ".join(r) for r in ra),
                                    " / ".join(" | ".join(r) for r in na))
            else:
                # No typed projection could be built for this statement. The
                # fallback is the engine's own quote(), which still carries the
                # storage class, and the comparison stays a VALUE comparison --
                # never a comparison of the two shells' printed text.
                #
                # The reason that is said out loud: an earlier version of this
                # branch fell back to the raw renderings whenever the quoted
                # projection could not run, and two engines that returned the
                # same zero rows were reported as disagreeing, because one of
                # them printed nothing and the other printed a record-stream
                # header. A fallback that reports a difference the engines do
                # not have is worse than no fallback, so the fallback asks the
                # same question of both engines and, if it still cannot, says
                # the statement was not compared rather than inventing a
                # verdict.
                weak_total += 1
                f = None
                q = quote_projection(sql)
                if q:
                    r_rows, r_err = sq.query(q)
                    n_rows, n_err = ns.query(q)
                    if not r_err and not n_err:
                        verdict, detail = compare(r_rows, n_rows)
                        if verdict == "agree":
                            counts["agree"] += 1
                        else:
                            f = Finding("wrong", name, sql, detail + "  (weak comparison)",
                                        " / ".join(" | ".join(r) for r in r_rows),
                                        " / ".join(" | ".join(n) for n in n_rows))
                if f is None:
                    r_ok, r_err2 = sq.run(sql)
                    n_ok, n_err2 = ns.run(sql)
                    if r_ok and not n_ok:
                        f = Finding("refuse", name, sql, "nsqlited refused a statement sqlite3 answered",
                                    "ok", "Error: " + (n_err2 or ""))
                    elif n_ok and not r_ok:
                        f = Finding("refuse", name, sql, "sqlite3 refused a statement nsqlited answered",
                                    "Error: " + (r_err2 or ""), "ok")
                    elif (r_err2 or "") == (n_err2 or ""):
                        counts["agree"] += 1
                    elif not r_ok and not n_ok:
                        f = Finding("wording", name, sql, "both refused, differently",
                                    r_err2 or "", n_err2 or "")
                    else:
                        # Both engines ran the statement and the quoted
                        # projection is not available, so there is nothing to
                        # compare. It is counted as unverified rather than as
                        # agreement, because reporting it as agreement would
                        # claim a comparison that was not made.
                        unverified += 1

            if f is not None:
                if f.kind == "wrong" and strict and f.detail:
                    pass
                counts[f.kind] += 1
                findings.append(f)
                report(f, True)

            # Keep the two databases identical in content for the next
            # statement. If either engine refused, the files have just diverged
            # and every statement after this one would be comparing two
            # different databases.
            if (f is not None and f.kind in ("refuse", "wording")) or (
                f is None and False
            ):
                ns.restore(snap_n)
                sq.restore(snap_r)

    print("")
    print("========================================")
    print("%d statements: %d agreed, %d wrong answers, %d refusals, %d worded differently"
          % (total, counts["agree"], counts["wrong"], counts["refuse"], counts["wording"]))
    if weak_total:
        print("%d statements had no usable typed projection and were compared weakly" % weak_total)
    if unverified:
        print("%d statements were NOT compared at all: no projection was runnable on either side" % unverified)
    print("========================================")
    return 0 if counts["wrong"] == 0 and counts["refuse"] == 0 and counts["wording"] == 0 else 1


# --- the runner's own self-test ---------------------------------------------


def self_test():
    checks = []

    def ck(name, got, want):
        checks.append((name, got == want, got, want))

    ck("split: semicolon in a string",
       split_statements("SELECT ';'; SELECT 2"), ["SELECT ';'", "SELECT 2"])
    ck("split: semicolon in a line comment",
       split_statements("SELECT 1 -- ;\n; SELECT 2"), ["SELECT 1", "SELECT 2"])
    ck("split: semicolon in a block comment",
       split_statements("SELECT 1 /* ; */; SELECT 2"), ["SELECT 1", "SELECT 2"])
    ck("split: escaped quote",
       split_statements("SELECT 'a'';'; SELECT 2"), ["SELECT 'a'';'", "SELECT 2"])
    ck("split: bracketed identifier",
       split_statements("SELECT [a;b] FROM t"), ["SELECT [a;b] FROM t"])

    ck("projection: simple select",
       build_projection("SELECT a, b FROM t"),
       "SELECT hex(typeof(a)||'~'||quote(a)) AS c1, hex(typeof(b)||'~'||quote(b)) AS c2 FROM t")
    ck("projection: keeps FROM",
       build_projection("SELECT a FROM t WHERE a>1 ORDER BY a"),
       "SELECT hex(typeof(a)||'~'||quote(a)) AS c1 FROM t WHERE a>1 ORDER BY a")
    ck("projection: keeps DISTINCT",
       build_projection("SELECT DISTINCT a FROM t").startswith("SELECT DISTINCT hex("), True)
    ck("projection: comma inside a call is not a split",
       build_projection("SELECT max(a,b) FROM t").count("AS c1"), 1)
    ck("projection: comma inside a string is not a split",
       build_projection("SELECT 'x,y' FROM t").count("AS c1"), 1)
    ck("projection: star is refused", build_projection("SELECT * FROM t"), None)
    ck("projection: t.* is refused", build_projection("SELECT t.* FROM t"), None)
    ck("projection: alias is stripped",
       build_projection("SELECT a AS z FROM t").count("typeof(a)||"), 1)
    ck("projection: bare trailing alias is stripped",
       build_projection("SELECT a z FROM t").count("typeof(a)||"), 1)
    # `a + b` is a SUM, not column `a` aliased `+`. If the stripper took it for
    # an alias the projection would ask about column `a` alone, both engines
    # would agree, and the sum would never be compared while the run reported
    # agreement. The projection repeats the expression twice, once for typeof
    # and once for quote.
    ck("projection: a two-word expression is not an alias",
       build_projection("SELECT a + b FROM t").count("typeof(a + b)||"), 1)
    ck("projection: a two-word expression keeps both columns",
       build_projection("SELECT a + b FROM t").count("quote(a + b)"), 1)
    ck("projection: a IS NULL is not an alias",
       build_projection("SELECT a IS NULL FROM t").count("typeof(a IS NULL)||"), 1)
    ck("projection: a bare alias is stripped, not the expression",
       build_projection("SELECT a z FROM t").count("typeof(a)||"), 1)
    ck("projection: a string with a space is one token",
       build_projection("SELECT 'a b' FROM t").count("typeof('a b')||"), 1)
    ck("projection: a FROM inside a string is not the tail",
       build_projection("SELECT ' FROM' FROM t").count(" AS c1 FROM t"), 1)
    ck("projection: non-select is refused", build_projection("INSERT INTO t VALUES(1)"), None)
    ck("projection: WITH is refused", build_projection("WITH q AS (SELECT 1) SELECT * FROM q"), None)
    ck("projection: VALUES is refused", build_projection("VALUES(1,2)"), None)
    # The column names are DOUBLE QUOTED. Two of them -- `notnull` and `key` --
    # are words the grammar claims, and an unquoted reference to either is a
    # syntax error, which made the whole projection unrunnable and pushed the
    # runner onto its weak path where two agreeing engines looked like two
    # disagreeing ones.
    ck("projection: pragma_table_info quotes its column names",
       build_projection("PRAGMA table_info('t')").startswith(
           'SELECT hex(typeof("cid")||\'~\'||quote("cid")) AS c1, '), True)
    ck("projection: a pragma column that is a keyword is quoted",
       '"notnull"' in build_projection("PRAGMA table_info('t')"), True)
    ck("projection: pragma_index_xinfo quotes key",
       '"key"' in build_projection("PRAGMA index_xinfo('t')"), True)
    # A pragma argument that is not a single-quoted literal is not re-quoted
    # into one: it would become an identifier and SQLite would read a column
    # of that name. No projection is offered rather than a wrong one.
    ck("projection: a pragma argument is taken as a string literal",
       "pragma_table_info('k')" in build_projection("PRAGMA table_info('k')"), True)
    ck("projection: an unquoted pragma argument is refused",
       build_projection("PRAGMA table_info(k)"), None)
    ck("projection: a subquery in FROM is left alone",
       build_projection("SELECT a FROM (SELECT 1 AS a)").endswith("FROM (SELECT 1 AS a)"), True)

    ck("decode: integer", decode_projection_field("696E74656765727E31"), "<integer> 1")
    ck("decode: real", decode_projection_field("7265616C7E312E30"), "<real> 1.0")
    ck("decode: null", decode_projection_field("6E756C6C7E4E554C4C"), "<null> NULL")
    ck("decode: empty text", decode_projection_field("746578747E2727"), "<text> ''")
    ck("decode: blob", decode_projection_field("626C6F627E58273030464627"), "<blob> X'00FF'")
    ck("decode: a non-hex field is left alone", decode_projection_field("NULLMARK"), "NULLMARK")

    # Four statements: a C with a row, a bare E, a C with a NULL row, a C with
    # a NULL row. The E closes the first statement, so it cannot be mistaken
    # for a failure OF the first statement.
    st = ("C 1 T61\nR T414243\nE some error\nC 1 T62\nR -\nC 1 T63\nR -\n")
    recs = parse_record_stream(st)
    ck("records: four statements", len(recs), 4)
    ck("records: text row", recs[0]["rows"], [("t'ABC'",)])
    ck("records: the first statement did not fail", recs[0]["err"], None)
    ck("records: the E is its own statement", recs[1]["err"], "some error")
    ck("records: a statement that only errored is not a query",
       recs[1]["kind"], "error")
    ck("records: null is not the empty string", recs[2]["rows"], [("null",)])
    # An error AFTER a row count is a second statement, not a failed first one.
    recs2 = parse_record_stream("X\nE table t already exists\n")
    ck("records: X then E is two statements", len(recs2), 2)
    ck("records: the X did not fail", recs2[0]["err"], None)
    ck("records: the E carries the message", recs2[1]["err"], "table t already exists")
    ck("records: an empty result has no rows", parse_record_stream("C 1 T61\n")[0]["rows"], [])
    # CREATE TABLE / INSERT open no C record, only X. Dropping that X would
    # make the engine look as though it had produced no output at all.
    recs3 = parse_record_stream("X\nX\n")
    ck("records: a bare X is still a statement", len(recs3), 2)
    ck("records: a bare X is not an error", recs3[0]["err"], None)
    ck("records: a lone X is a statement", parse_record_stream("X\n")[0]["kind"], "exec")

    ck("envelope: parse error",
       strip_envelope("Parse error near line 3: no such table: t\n  SELECT 1;\n  ^--- here\n"),
       "no such table: t")
    ck("envelope: runtime error",
       strip_envelope("Runtime error near line 1: abort"), "abort")
    ck("envelope: a message with no envelope is kept",
       strip_envelope("something went wrong"), "something went wrong")

    bad = [c for c in checks if not c[1]]
    for name, ok, got, want in checks:
        print("%-4s %s" % ("ok" if ok else "FAIL", name))
        if not ok:
            print("       got:  %r" % (got,))
            print("       want: %r" % (want,))
    print("")
    print("self-test: %d checks, %d failed" % (len(checks), len(bad)))
    return 0 if not bad else 1


if __name__ == "__main__":
    sys.exit(main())
PYEOF
