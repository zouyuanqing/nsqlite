# UNIQUE is not enforced: the duplicate row is written

**This is the most serious open defect in the engine, and it is data
corruption rather than a wrong answer.** Measured 2026-09-28 against the real
`sqlite3` 3.53.4.

```sh
$ printf 'CREATE TABLE uq1(a UNIQUE);
INSERT INTO uq1 VALUES(1);
INSERT INTO uq1 VALUES(1);
'     | nsqlited --testsuite :memory:
X
X                              <- the second INSERT succeeded

$ printf 'CREATE TABLE uq1(a UNIQUE);
INSERT INTO uq1 VALUES(1);
INSERT INTO uq1 VALUES(1);
'     | sqlite3 :memory:
Error near line 3: UNIQUE constraint failed: uq1.a
```

A caller that relies on the constraint gets a database containing the row it
was promised could not be there. Every other finding in
`docs/differential-findings.md` produces a wrong *answer*; this one produces a
wrong *file*.

## Scope, measured

Three distinct paths, and they do not agree with each other:

| declaration | reference | this engine |
| --- | --- | --- |
| `a INTEGER PRIMARY KEY` | refuses the duplicate | **refuses it** — enforced |
| `a UNIQUE` (column constraint) | `UNIQUE constraint failed: uq1.a` | **accepts the duplicate** |
| `CREATE UNIQUE INDEX i ON u(a)` | `UNIQUE constraint failed: u.a` | **accepts the duplicate** |

The first works only because a single `INTEGER PRIMARY KEY` is the **rowid
alias**, so the duplicate is caught by the b-tree's own key check and never
reaches a constraint check. That is why the third row of the table looks like
the first working: it is the same mechanism, not a working constraint system.

So: uniqueness is enforced for exactly one column shape, and not for the two
shapes a caller would actually reach for.

## The root cause is stated in the code

`crates/nsqlite/src/catalog.rs:238`:

```
/// A composite or non-integer primary key is an ordinary uniqueness
/// constraint, and this engine does not yet enforce uniqueness at all.
```

The catalog records the `unique` flag (`catalog.rs:83`, and
`connection.rs:60`/`71` carry it through from the `CREATE TABLE`), and nothing
reads it on the write path. `connection.rs:1386` can emit a unique-constraint
message, but only for the rowid-alias case.

## The reference semantics to implement

```sh
sqlite3 :memory: "CREATE TABLE uq1(a UNIQUE);
                  INSERT INTO uq1 VALUES(1); INSERT INTO uq1 VALUES(1);"
# UNIQUE constraint failed: uq1.a          <- names TABLE.COLUMN

sqlite3 :memory: "CREATE TABLE u(a);
                  CREATE UNIQUE INDEX i ON u(a);
                  INSERT INTO u VALUES(1); INSERT INTO u VALUES(1);"
# UNIQUE constraint failed: u.a           <- same shape, via the index

sqlite3 :memory: "CREATE TABLE u(a UNIQUE);
                  INSERT INTO u VALUES(NULL); INSERT INTO u VALUES(NULL);
                  SELECT count(*) FROM u;"
# 2                                       <- NULLs are EXEMPT
```

Three things that a first implementation gets wrong:

1. **The message names `table.column`**, not the constraint or the index, in
   both the column-constraint and the `CREATE UNIQUE INDEX` case. The suite
   compares it verbatim.
2. **NULL is exempt.** Any number of NULLs is legal under UNIQUE, because NULL
   is not equal to itself for this purpose. A test for "have I seen this value
   before?" that treats two NULLs as equal rejects a row SQLite accepts.
3. **A `UNIQUE` column and a `CREATE UNIQUE INDEX` over the same column must
   produce the same message.** They are the same constraint expressed two ways,
   and the reference does not tell them apart.

## What to check, and what not to trust

Check by **value**, and check the row count after the duplicate — a fix that
makes the INSERT fail for an unrelated reason (a parse error, an unsupported
feature) can lower a failure count without enforcing anything:

```sh
printf 'CREATE TABLE u(a UNIQUE);
INSERT INTO u VALUES(1);
INSERT INTO u VALUES(1);
SELECT count(*) FROM u;
'   | nsqlited --testsuite :memory:      # must be: E ..., then R I31
```

The two measurement traps that have bitten this work before are in
`docs/substr-multibyte.md`: do not read a byte-level result off the record
stream, and do not measure a binary older than its source.
