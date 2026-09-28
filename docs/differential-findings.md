# Differential findings, fifth pass

What `tools/difftest3.sh` over `tools/difftest5-cases.sql` found, measured
against the real `sqlite3` (3.53.4) on this machine, with every claim below
produced by running a command rather than by reasoning about the code.

Ordered smallest repro first. Each entry gives the statement, what each engine
answered, and which one is right. **Where they differ, `sqlite3` is right** —
every entry was checked against the real shell and none of the disagreements
is a matter of taste or formatting.

The class worth chasing is the one the four earlier passes found: the engine
returns a **wrong answer rather than an error**, so nothing fails loudly. That
is why the first two entries are first.

---

## The two that corrupt data

### 1. A UNIQUE column is not enforced, and the bad row is written

```sql
CREATE TABLE uq1(a UNIQUE);
INSERT INTO uq1 VALUES(1);
INSERT INTO uq1 VALUES(1);
```

| | |
|---|---|
| sqlite3 | `Error: UNIQUE constraint failed: uq1.a` — the second row is not written |
| nsqlited | accepted, exit status 0 |

```sql
SELECT count(*) FROM uq1;
```

| | |
|---|---|
| sqlite3 | `1` |
| nsqlited | `2` |

This is the worst shape in this document: the row is written, the file holds
it, and **the real sqlite3 agrees that the file holds two rows**. A database
produced here is not merely read differently by another engine — it is
permanently corrupt, and nothing in the file records that it should not be.
The `differential3.rs` corpus already found "a UNIQUE index, which is built
and then not enforced"; this is the same defect reached through the column
constraint rather than the index, and it is still open.

Reproduce:

```bash
rm -f /tmp/u.db
./target/debug/nsqlited.exe --testsuite /tmp/u.db "CREATE TABLE uq1(a UNIQUE); INSERT INTO uq1 VALUES(1);"
./target/debug/nsqlited.exe --testsuite /tmp/u.db "INSERT INTO uq1 VALUES(1);"   # exit 0
sqlite3 -batch /tmp/u.db "SELECT count(*) FROM uq1;"                            # 2
```

### 2. Concatenating a blob corrupts any byte that is not valid UTF-8

```sql
SELECT hex(x'FF' || x'00');
```

| | |
|---|---|
| sqlite3 | `FF00` |
| nsqlited | `EFBFBD00` |

`EFBFBD` is the UTF-8 encoding of U+FFFD REPLACEMENT CHARACTER. The byte `0xFF`
cannot appear in valid UTF-8, so somewhere in the concatenation the blob is
decoded to text with lossy error handling and the original byte is replaced by
a three-byte stand-in. **This is silent, irreversible data loss**, and it fires
on any blob holding bytes outside the Unicode range — which is most binary
data.

Note what still works: `SELECT hex(x'FF00FE')` is correct, so `hex()` itself
does not lose the byte. The loss is specific to the `||` path.

---

## A NUL byte is not a terminator, and is counted as if it were

`length()` counts **bytes** in this engine and stops counting at a NUL, while
SQLite counts bytes and does not stop. Both engines store the full value —
`hex()` agrees, `610062` on both — so the value is right and only the functions
that look at it are wrong.

### 3. `length()` counts bytes, and stops at the first NUL

Two separate rules are wrong in the same function, and they are worth keeping
apart because they have different causes.

**It counts bytes where SQLite counts characters.** This one is easy to miss,
because a multi-byte *literal* agrees and only the constructed value does not:

| statement | sqlite3 | nsqlited |
|---|---|---|
| `length('日本')` (a TEXT literal, 2 chars / 6 bytes) | `2` | `2` |
| `length(x'E697A5E69CACE8AA9E')` (a BLOB, 6 bytes) | `9` | `9` |
| `length(CAST(x'E697A5' AS TEXT))` (1 char / 3 bytes) | `1` | **`3`** |
| `length(CAST(x'C3A9' AS TEXT))` (1 char / 2 bytes) | `1` | **`2`** |
| `length(CAST(x'68C3A96C6C6F' AS TEXT))` (`héllo`, 5 chars / 6 bytes) | `5` | **`6`** |
| `length(CAST(x'E697A5E69CACE8AA9E' AS TEXT))` (3 chars / 9 bytes) | `3` | **`9`** |

The first two rows are the trap. A literal is byte-counted correctly and a blob
is byte-counted correctly, because for a **blob** SQLite counts bytes too -- so
both engines agree. The disagreement appears only for a **text** value whose
byte count differs from its character count, which is why a corpus that tests
`length('日本')` finds nothing and one that builds the text with
`CAST` finds it. `héllo` is the smallest case that shows it: five
characters, six bytes, `5` against `6`.

`substr()` **is** character-aware and agrees, which is why this is so easy to
miss -- on the same value, only `length()` is wrong:

| statement | sqlite3 | nsqlited |
|---|---|---|
| `hex(substr(CAST(x'E697A5E69CACE8AA9E' AS TEXT),1,1))` | `E697A5` | `E697A5` |
| `hex(substr(CAST(x'E697A5E69CACE8AA9E' AS TEXT),2,1))` | `E69CAC` | `E69CAC` |
| `hex(substr(CAST(x'E697A5E69CACE8AA9E' AS TEXT),1,3))` | `E697A5E69CACE8AA9E` | `E697A5E69CACE8AA9E` |

A caller that uses `length()` to slice, or to bound a `substr()`, is therefore
wrong for every non-ASCII string, in a way that a `substr()`-only test never
sees.

**It stops at the first NUL.** For a TEXT value SQLite's `length()` returns the
number of bytes before the first NUL:

```sql
SELECT length('a' || char(0) || 'b');
```

| | |
|---|---|
| sqlite3 | `1` |
| nsqlited | `3` |

`1` is right and is worth understanding, because it is not a bug in the
function: it is documented behaviour and it is deliberate — `length()` is
defined for the C interface, where a NUL-terminated string is what the caller
holds. The engine returns the whole value's byte count instead, which is the
number a BLOB gets:

| statement | sqlite3 | nsqlited |
|---|---|---|
| `length('ab')` | `2` | `2` |
| `length('a' \|\| char(0))` | `1` | `2` |
| `length('a' \|\| char(0) \|\| 'b')` | `1` | `3` |
| `length(char(0))` | `0` | `1` |
| `length(x'00')` (a BLOB) | `1` | `1` |
| `length(x'610062')` (a BLOB) | `3` | `3` |

The last two are the tell: for a **blob** both engines count the NUL and both
say 1 and 3. Only **text** differs, and only on the NUL rule. So the engine is
applying the blob rule to text.

Reduced to the smallest form of both defects:

```sql
SELECT length(CAST(x'E697A5' AS TEXT));   -- sqlite3: 1   nsqlited: 3  (bytes, not characters)
SELECT length(char(0));                  -- sqlite3: 0   nsqlited: 1  (stops at the NUL)
```

`char(0)` itself is **not** wrong: `hex(char(0))` is `00` on both engines and
`typeof` is `text` on both. The runner reported it as a difference only
because `quote()` renders a NUL as a space and the two renderings are then
compared — which is a limitation of that comparison, not a defect, and it is
the reason every other entry here is checked with `hex()`.

### 4. Every other function over the same value truncates too

```sql
SELECT substr('a' || char(0) || 'b', 1, 3);
```

| | |
|---|---|
| sqlite3 | `'a'` |
| nsqlited | `'a b'` (three characters, NUL rendered as a space by `quote()`) |

```sql
SELECT upper('a' || char(0) || 'b');
```

| | |
|---|---|
| sqlite3 | `'A'` |
| nsqlited | `'A B'` |

Same for `trim()`, `replace()`, and `instr()`. The consistent pattern: the
engine carries the full value but treats the text routines as if they end at
the NUL **and** pads what follows. `SELECT hex('a'||char(0)||'b')` is `610062`
on both engines, which is the proof that the value is intact and only the text
functions are wrong.

Reduced further, the smallest form of all:

```sql
SELECT length(char(0));    -- sqlite3: 0   nsqlited: 1
SELECT char(0);            -- sqlite3: ''  nsqlited: ' '
```

---

## A blob is a value, and these functions treat its source text as its value

Four separate defects, one shape. When a blob reaches `trim()` or `replace()`,
the result is the **literal source text of the literal** — `x'616263'` — not
the bytes and not the string.

### 5. `trim()` and `replace()` return the blob literal's own text

```sql
SELECT trim(x'616263');
```

| | |
|---|---|
| sqlite3 | `'abc'` |
| nsqlited | `x'616263'` |

```sql
SELECT replace(x'616263', x'62', x'58');
```

| | |
|---|---|
| sqlite3 | `'aXc'` |
| nsqlited | `x'616263'` |

The blob is being re-rendered to its SQL source form and returned unchanged.
`trim('  ab  ')` is correct, so this is specific to a blob argument.

### 6. `instr()` with a blob needle does not find it

```sql
SELECT instr(x'616263', x'62');
```

| | |
|---|---|
| sqlite3 | `2` |
| nsqlited | `0` |

### 7. A blob is not converted to a number before arithmetic

```sql
SELECT x'31' + x'32';
```

| | |
|---|---|
| sqlite3 | `3` |
| nsqlited | `0` |

```sql
SELECT x'31' + 1;        -- sqlite3: 2   nsqlited: 1
SELECT abs(x'2D33');     -- sqlite3: 3.0 nsqlited: 0
SELECT round(x'332E35'); -- sqlite3: 4.0 nsqlited: 0.0
```

`x'31'` is the byte `0x31`, which is the character `1`. SQLite applies the usual
numeric conversion and adds one and one. The engine treats a blob as zero in
arithmetic. `typeof(x'31'+x'32')` is `integer` on both and `typeof(abs(...))` is
`real` on both, so the answer is the right *class* and the wrong *value* — it
is a coercion that is not happening, not a type confusion.

### 8. `upper()`/`lower()` change the wrong thing, and `substr()` changes the other one

For these the **bytes are right and only the storage class is wrong**, which is
the opposite of the two defects above and matters for a different reason: a
caller comparing with `=` still gets the right answer, and a caller reading
`typeof()` does not.

```sql
SELECT hex(upper(x'616263'));      -- sqlite3: 414243   nsqlited: 414243   (bytes agree)
SELECT typeof(upper(x'616263'));   -- sqlite3: text     nsqlited: blob     (class differs)
```

SQLite converts a blob to text by its bytes before applying the function, so
the result is text. The engine does the conversion and then keeps the blob
class — the transformation ran and the label did not follow it.
`lower(x'414243')` is the same.

```sql
SELECT typeof(substr(x'616263', 2, 1));   -- sqlite3: blob   nsqlited: text
SELECT hex(substr(x'616263', 2, 1));      -- sqlite3: 62     nsqlited: 62
```

Here SQLite keeps the blob class and the engine converts to text — the
**opposite** error. So the engine's blob handling is not "blobs are left
alone"; it is inconsistent in both directions, which is worth knowing before
anyone writes a fix. Each of these two is the smallest statement that shows its
case: `typeof(upper(x'616263'))` and `typeof(substr(x'616263',2,1))`.

The same pattern, one step further on, in the numeric functions: the class
agrees and the value does not, so this is a coercion that is not happening
rather than a type confusion.

```sql
SELECT typeof(abs(x'2D33'));     -- sqlite3: real     nsqlited: real     (class agrees)
SELECT abs(x'2D33');             -- sqlite3: 3.0      nsqlited: 0
SELECT typeof(round(x'332E35')); -- sqlite3: real     nsqlited: real     (class agrees)
SELECT round(x'332E35');         -- sqlite3: 4.0      nsqlited: 0.0
```

---

### 9. `lower()` of text containing a non-ASCII character returns it unchanged, as a blob

```sql
SELECT hex(lower(CAST(x'48C3A94C4C4F' AS TEXT)));   -- the text  HÉLLO
```

| | |
|---|---|
| sqlite3 | `68C3A96C6C6F` (`héllo`) |
| nsqlited | `48C3A94C4C4F` (unchanged) |

```sql
SELECT typeof(lower(CAST(x'48C3A94C4C4F' AS TEXT)));
```

| | |
|---|---|
| sqlite3 | `text` |
| nsqlited | `blob` |

Both the value **and** the class are wrong, and nothing failed: the string
comes back the way it went in. It is specific to the case where a fold is
actually needed — `lower('ABC')` is correct on both, and
`lower(CAST(x'CEB1' AS TEXT))` (the byte for `α`) is correct on both,
because there is no ASCII-case character to fold. So the defect only shows on a
string that is *partly* non-ASCII.

`upper()` is **correct** on the same value, which is what makes this easy to
miss:

| statement | sqlite3 | nsqlited |
|---|---|---|
| `hex(upper(CAST(x'68C3A96C6C6F' AS TEXT)))` (`héllo`) | `48C3A94C4C4F` | `48C3A94C4C4F` |
| `hex(lower(CAST(x'48C383894C4C4F' AS TEXT)))` (`HÉLLO`) | `68C383896C6C6F` | `48C383894C4C4F` |

The `differential3.rs` corpus already found "upper() and lower() are ASCII-only";
this is the same area from the other side — `upper()` is ASCII-only and *that
is correct*, because SQLite's own `upper()` only folds ASCII. The bug is
`lower()`'s, and it is not a wrong fold but a refused one.

---

## Numeric defects

### 10. ~~Negating the most negative integer does not overflow~~ — FIXED

```sql
SELECT -(-9223372036854775808);
```

| | when first measured | in the current tree |
|---|---|---|
| sqlite3 | `9.2233720368547758e+18` (real) | `9.2233720368547758e+18` (real) |
| nsqlited | `-9223372036854775808` (integer) | `9.2233720368547758e+18` (real) — **agrees** |

Negating `i64::MIN` overflows, and SQLite's answer is to produce a real. The
engine used to return the operand unchanged, which is not the negation of
anything.

**This one is closed.** It was found by the first run of this corpus against a
build of 08:14, and gone by the time the corpus was re-run against the tree as
it stands — another agent's work fixed it in between, not this one. It is kept
in the list because it is the shape worth having a regression test for, and
because the corpus statement
`SELECT -(-9223372036854775808)` is what caught it.

Everything else at the integer boundary is correct and stayed correct: the
arithmetic that leaves the range (`9223372036854775807+1`,
`3000000000*3000000000`, `4611686018427387904*2`), the unary minus on
`-9223372036854775807-1` and `-9223372036854775808-1`, division and modulo at
both ends, the boundary values stored in a column and read back, and the
boundary passed through `abs()`, `round()`, `ceil()` and `floor()`. The whole
of section 9a-9f agrees.

### 11. Float overflow produces a real infinity, not SQLite's rendering

```sql
SELECT 1e308 * 10;
```

| | |
|---|---|
| sqlite3 | `9.0e+999` |
| nsqlited | `Inf` |

Both are the same value in IEEE terms, and `typeof` agrees (`real` on both), so
this is a **rendering** difference rather than a wrong value — the first entry
in this document that is one. It is listed because `quote()` and `printf()` of
an infinite real will both differ, so anything that turns the value into text
is affected. `SELECT 1e400` returns `Inf` on both.

---

## Capabilities that are missing (refusals, not wrong answers)

These are visible failures rather than silent ones, and are listed separately
because they are a different kind of work.

### 12. `ALTER TABLE ... ADD COLUMN` is not implemented

```sql
CREATE TABLE addcol(a);
ALTER TABLE addcol ADD COLUMN b;
```

| | |
|---|---|
| sqlite3 | accepted |
| nsqlited | `Error: near "ALTER": syntax error` |

### 13. There is no `now` time value

```sql
SELECT date('now');
```

| | |
|---|---|
| sqlite3 | `2026-09-28` |
| nsqlited | `NULL` |

This one is a **missing capability, not a clock race**, which matters because a
clock-dependent value cannot be compared between two processes. Here the
engine's answer is stably `NULL` on every run (measured three times), so the
disagreement is that one engine has the feature and the other does not. The
deterministic form of the same defect:

```sql
SELECT typeof(date('now'));    -- sqlite3: 'text'   nsqlited: 'null'
```

Everything else in the date and time group — `date()`, `time()`,
`datetime()`, `julianday()`, `unixepoch()`, `strftime()` and all their
modifiers, including the month-length rules (`date('2024-01-31','+1 month')` is
`2024-03-02` on both), the weekday and day-of-year specifiers, and the
round trips through `julianday()` — **agrees on every statement in the
corpus**. The one exception is `date(julianday('now')-0.5)`, which is the same
`now` gap seen from the other side.

### 14. Table-valued pragma functions are not supported

```sql
CREATE TABLE k(a);
SELECT cid, name FROM pragma_table_info('k');
```

| | |
|---|---|
| sqlite3 | `0\|a` |
| nsqlited | `Error: near "(": syntax error` |

---

## Error text, compared byte for byte

The earlier runners count a wording difference as a weaker statistic so it never
becomes a line item. These were compared exactly, after stripping the
sqlite3 CLI's own `Parse error near line N:` envelope — which is the shell's
wording, not the engine's.

**The unknown-table, unknown-function and constraint messages agree exactly.**
`no such table:`, `no such function:`, `wrong number of arguments to function
abs()`, `UNIQUE constraint failed: uq1.a` — all byte-identical. That is a good
result and worth recording as one.

Three do not:

| statement | sqlite3 | nsqlited |
|---|---|---|
| `SELECT FROM t` | `near "FROM": syntax error` | `near "from": syntax error` |
| `SELECT 1 FROM t GROUP BY` | `near ";": syntax error` | (agrees) |
| `CREATE INDEX ON t(a)` | `near "ON": syntax error` | `object name reserved for internal use: sqlite_autoindex_t_1` |

The first is a token case difference. The third is more interesting: with no
index name given, nsqlited invents `sqlite_autoindex_t_1` and then complains
that **its own generated name** is reserved, where sqlite3 reports the actual
problem — the missing name.

### A tokenizer message differs by a quote character

```sql
SELECT hex(upper(x'616263));
```

| | |
|---|---|
| sqlite3 | `unrecognized token: "x'616263));"` |
| nsqlited | `unrecognized token: "x'616263));` |

Both engines refuse it (the corpus splitter left a stray `;` inside the
literal) and both name the same span; the difference is whether the closing
double quote is repeated. Cosmetic, and listed only because the suite compares
text exactly.

---

## One finding that was the runner's fault, not the engine's

Recorded because it is the class of report this whole exercise exists to
avoid, and because the fix belongs to the runner.

`PRAGMA table_info(no_such_table)` was reported as a wrong answer. Both engines
return **zero rows** for it. The cause was in the runner's own rewrite: the
typed projection named the pragma's columns unquoted, and two of them —
`notnull` and `key` — are words SQLite's grammar claims, so

```sql
SELECT hex(typeof(notnull)||'~'||quote(notnull)) ...
```

is a syntax error. The projection therefore failed on both sides, the runner
dropped to comparing the two shells' **renderings**, and the renderings differ:
sqlite3 prints nothing and nsqlited prints a `C 0` record. Two engines that
agree were reported as disagreeing, because the fallback compared how they
print rather than what they return.

Fixed in `tools/difftest3.sh` in two places: the projection quotes every
column name and refuses to build one for an argument that is not a string
literal, and the fallback no longer compares renderings at all — a statement it
cannot compare is now counted as **unverified**, which is reported, rather than
being given a verdict it did not earn. The case now reports as an agreement,
and the runner's self-test has a check that pins it.

---

## What was measured, and how

Four existing testers were run to completion first, all compared by value:

| runner | corpus | result |
|---|---|---|
| `tools/difftest.sh` | `tools/difftest2-cases.sql` | 100/372 agreed, 272 disagreed |
| `tools/difftest2.sh` | `tools/difftest2-cases.sql` | 416/568 agreed, 94 disagreed, 24 worded differently, 74 unverified |
| `tools/difftest2.sh` | `tools/gen2.sql` | 164/219 agreed, 41 disagreed, 1 worded differently, 16 unverified, 4 crashes |
| `tools/difftest2.sh` | `tools/gen4.sql` | 84/133 agreed, 50 disagreed, 2 worded differently, 9 crashes |
| `tools/difftest3.py` | `tools/difftest3-cases.sql` | 138/207 agreed, 69 disagreed, 61 weak, 21 worded differently |
| `tools/difftest3.sh` | `tools/difftest5-cases.sql` | 294/341 agreed, 40 wrong, 2 refusals, 5 worded differently |

Self-tests: `difftest.sh` all checks passed; `difftest2.sh` all 108 checks
passed; `difftest3.sh` 51 checks, 0 failed.

The fifth corpus yields **47 findings**: 40 wrong answers, 2 refusals and 5
wording differences, reduced below to 14 entries of which 13 are open. The
count is much larger than the number of defects because many are one root cause
seen from different statements -- entry 3 alone accounts for 16 of them.

Entry 10 was found against a build of 08:14 and was fixed by other work in
the tree before the final run; it is kept and marked rather than deleted,
because the corpus statement that caught it is worth keeping.

**A note on the first row.** `tools/difftest.sh` and `tools/difftest2-cases.sql`
are not a supported pairing — that corpus is written for the `###`-delimited
section format of `difftest2.sh`, and `difftest.sh` has no section support, so
it feeds each `###` header to the engines as a statement and all 74 of them
fail with `unrecognized token: "#"`. The number is reported because it was
asked for, but it measures the pairing, not the engine. The supported pairings
are the `difftest2.sh` rows and the `difftest3.py` row.

### The gap this corpus was written for

Counting occurrences of each function across all four existing corpora
(`difftest2-cases.sql`, `gen2.sql`, `gen4.sql`, `difftest3-cases.sql`):

```
date(  0   datetime(  0   time(  0   julianday(  0
unixepoch(  0   strftime(  0   timediff(  0
concat(  0   concat_ws(  0   format(  0
```

Ten functions with no occurrence at all. The date and time group was the
largest untested block in the engine, and the results above say most of it is
correct — which is worth knowing, and is the reason the two data-corruption
entries at the top are the headline rather than the date functions.

### How the comparison works

Every query is re-run on both engines as

```sql
hex(typeof(<expr>) || '~' || quote(<expr>)) AS c<n>
```

so the storage class is part of the answer, NULL cannot be confused with the
empty string, and no byte of a value can be mistaken for a field or row
boundary. Rows are compared as **tuples**, in order, with no sorting, no
trimming and no case folding. The single normalisation is the sqlite3 CLI's
`Parse error near line N:` envelope on an error message, which is the shell's
wording and not the engine's; the remainder is compared byte for byte.

### Coverage the corpus does not have

* Anything whose answer depends on the clock or on randomness: `random()`,
  `randomblob()`, `date('now')` as a *value*, and the `localtime` modifier.
  `date('now')` is listed above as a capability gap precisely because the
  engine's answer to it is stably `NULL` and therefore comparable.
* Cross-engine interop — a file written by one engine and read by the other —
  which `differential2.rs` covers and this corpus does not repeat.
* Transactions, journal and recovery, covered by `gen4.sql`.
