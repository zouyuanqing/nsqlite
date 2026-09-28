# Compound SELECT: the reference semantics, measured

Every row here was measured against the real `sqlite3` 3.53.4 on 2026-09-28, not
reasoned about. The point of the table is the rows that a straightforward
implementation gets **wrong**, which is most of them.

Reproduce any row with:

```sh
printf '%s\n' "<statement>" | sqlite3 :memory:
```

## The four operators

| Statement | Result |
| --- | --- |
| `SELECT 1 UNION SELECT 2` | `1`, `2` |
| `SELECT 1 UNION ALL SELECT 1` | `1`, `1` — **not** deduplicated |
| `SELECT 1 UNION SELECT 1` | `1` — one row |
| `SELECT 1 EXCEPT SELECT 2` | `1` |
| `SELECT 1 INTERSECT SELECT 2` | *empty* |

`UNION` deduplicates, `UNION ALL` does not, and the other two are the set
operations. Note `UNION` also **sorts** its output; `UNION ALL` preserves the
left arm's order.

## Deduplication is by storage class, not by value

This is the rule that decides whether an implementation is right, and the one
most likely to be got wrong:

| Statement | Result | Why |
| --- | --- | --- |
| `SELECT 1 UNION SELECT '1'` | **two** rows: `1`, `1` | integer 1 and text `'1'` are different values |
| `SELECT 1 UNION SELECT 1.0` | **one** row: `1.0` | integer 1 and real 1.0 compare equal |
| `SELECT NULL UNION SELECT NULL` | **one** row | NULL equals NULL for `UNION` |

Verified through the typed projection so the class is visible rather than
inferred from the printed digits:

```sh
sqlite3 :memory: "SELECT typeof(a) FROM (SELECT 1 AS a UNION SELECT '1');"
# integer
# text          <- two rows, so the dedup did not collapse them
sqlite3 :memory: "SELECT count(*) FROM (SELECT NULL UNION SELECT NULL);"
# 1
```

Note the third row. `NULL` is **not** distinct from `NULL` under `UNION`, which
is the opposite of what SQL's `IS NOT DISTINCT FROM` intuition suggests and the
opposite of what a set-of-distinct-values implementation gives. A dedup that
skips NULLs produces two rows and fails.

The second row is subtler than it looks: `1` and `1.0` collapse, but the
**surviving value is the real** `1.0`, not the integer `1`. So dedup is by
comparison semantics, and the winner is the right-hand row — a left-biased
dedup that keeps the first occurrence would return `1` and be wrong.

## Precedence: INTERSECT binds tighter than UNION

```sh
sqlite3 :memory: "SELECT a FROM (SELECT 1 AS a INTERSECT SELECT 1 UNION SELECT 2);"
# 1
# 2
```

That is `(1 INTERSECT 1) UNION 2` = `1 UNION 2` = `1, 2`. Reading it as
`1 INTERSECT (1 UNION 2)` would give `1` alone. `UNION` and `EXCEPT` are
left-associative with each other; `INTERSECT` groups tighter than both.

## Trailing ORDER BY and LIMIT bind to the whole compound

```sh
sqlite3 :memory: "SELECT 1 UNION SELECT 2 ORDER BY 1 DESC;"   # 2, then 1
```

Not to the last arm. `ORDER BY 1` is a column ordinal here, resolved against
the compound's result columns, and it is the one case where an `ORDER BY`
ordinal is legal after a compound.

## Column count mismatch

```sh
sqlite3 :memory: "SELECT 1,2 UNION SELECT 3,4;"   # 1|2 and 3|4: fine
sqlite3 :memory: "SELECT 1 UNION SELECT 3,4;"
# Parse error near line 1: SELECTs to the left and right of UNION do not have
# the same number of result columns
```

The message names the operator that was used, so it is not one fixed string —
`EXCEPT` and `INTERSECT` produce their own. Generating it from the operator
matters, because the suite compares the text verbatim.

## What this engine does today

`SELECT 1 UNION SELECT 2;` → `E Union is not supported yet`, from the refusal
arm at `crates/nsqlite/src/connection.rs:1827` inside `Connection::select`.
The **parse** half is already done — `SelectBody::Compound` exists and
`connection.rs:1611` can inspect it — so this is an execution gap, not a
grammar one.
