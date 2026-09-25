# Affinity: the conversion grid, taken from the real sqlite3

Every cell below was produced by running the statement against `sqlite3`
3.53.4, not by reasoning about the rules. The value shown is what `typeof`
reports after an insert, which is the only form of this question the official
suite asks.

```sql
CREATE TABLE t(a <AFFINITY>); INSERT INTO t VALUES(<VALUE>); SELECT typeof(a);
```

| value in | TEXT | NUMERIC | INTEGER | REAL | BLOB |
| --- | --- | --- | --- | --- | --- |
| `5` (integer) | text | integer | integer | **real** | integer |
| `5.0` (real) | text | **integer** | integer | real | real |
| `'5'` (text) | text | integer | integer | **real** | text |
| `'5.0'` (text) | text | **integer** | integer | real | text |
| `x'35'` (blob) | blob | blob | blob | blob | blob |
| `NULL` | null | null | null | null | null |

## The three things that are easy to get backwards

**REAL widens an integer.** `CREATE TABLE t(a REAL); INSERT INTO t VALUES(5)`
stores a real, not the integer 5. This is the one that catches people, because
a value that came from a literal `5` is an integer and one that came from a
`typeof` of `5.0` is not.

**NUMERIC narrows a whole real.** `CREATE TABLE t(a NUMERIC); INSERT INTO t
VALUES(5.0)` stores the integer 5. The opposite direction from REAL, on the
same input.

**A blob is never converted.** The `x'35'` row is blob under every affinity
including BLOB and TEXT. A blob that looks like a number is still a blob.

## Text that is not a number

A text value that is not entirely a number stays text, under every affinity
except BLOB, which leaves it as text anyway. `'12abc'` in an INTEGER column is
`'12abc'`, not `12`: affinity never discards the characters it cannot use.
This is the difference from a cast, and the two are easy to conflate.

## Where affinity is applied, and where it is not

Affinity is applied on the way **into** a table. It is not applied to a literal
in a comparison, and when two columns are compared, the affinity of each side
is applied to the other side's value. That asymmetry is what makes

```sql
CREATE TABLE t(a TEXT, b INTEGER);
INSERT INTO t VALUES('5', 5);
SELECT a = b FROM t;   -- text '5' is converted to integer 5, so this is 1
```

return 1 rather than 0.

## What this costs in the engine

`crates/nsqlite/src/affinity.rs` implements the conversion. The tests in
`crates/nsqlite/src/affinity_rules.rs` cover every cell of the grid above, and
the grid is the specification: if a cell here and a test disagree, the grid is
right, because it was measured.
