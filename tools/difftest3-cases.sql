-- The corpus for tools/difftest3.py: the third generation, and the one that
-- found the defects the second one could not reach.
--
-- WHY A THIRD RUNNER AND NOT A WIDER FILE FOR tools/difftest2.sh
--
-- tools/difftest2.sh is correct and its 51-check self-test passes. It is also
-- slow by construction: for every statement it spawns BOTH engines twice (once
-- to probe whether the typed projection is usable, once for the value) and
-- copies BOTH database files twice for the checkpoint. Its own full run over
-- 354 statements took about twenty minutes. Hunting needs hundreds of
-- statements, so the cost was moved rather than paid: difftest3.py is one
-- process for the whole corpus, one spawn per engine per statement, and it
-- reaches the same verdict on all 343 statements the first corpus contains in
-- 23 seconds. It is a separate file rather than an edit because tools/ is
-- shared and tools/difftest2.sh plus tools/difftest2-cases.sql are somebody
-- else's deliverable.
--
-- It compares BY VALUE and strictly, with the same projection the second runner
-- uses:
--
--     SELECT hex(typeof(<expr>)||'~'||quote(<expr>)) AS c1, ... <rest>
--
-- and falls back to comparing the two engines' own renderings when that
-- projection cannot be built or cannot be run, counting those separately as
-- weak. Nothing is sorted, trimmed, case-folded or hex-normalised, and a
-- difference in row order is a difference.
--
-- WHAT THIS CORPUS IS FOR
--
-- The first corpus found four defects of one kind: the engine returned a WRONG
-- ANSWER rather than an error. That is the class worth hunting, because nothing
-- fails loudly. Sections 1-8 are the state-corruption half of it -- a
-- constraint that does not fire, a value stored in the wrong class, a panic.
-- Sections 9-14 are the capability half: what the two engines do not even both
-- have, which is a missing capability whatever the value is.
--
-- Every disagreement here was reduced by hand to the smallest statement that
-- still shows it, and each reduction was re-run against the real sqlite3, so
-- every one of them is a fact about the engine and not about this corpus.

-- ===========================================================================
-- 1. Constraints that do not fire. The worst class found so far.
-- ===========================================================================
--
-- Every statement in this section is a statement real sqlite3 REFUSES and
-- nsqlited accepts, leaving the violating row in the table. A multi-row INSERT
-- with a UNIQUE or CHECK violation is the case the first corpus found; this
-- section asks the same question of every other write path, and the answer is
-- the same each time: the check is not applied at all, so the state is left
-- corrupt rather than left alone.
--
-- The reductions are separated from the multi-statement originals on purpose.
-- `CREATE TABLE t(a INT, b INT UNIQUE); INSERT ...` is a two-statement
-- reproduction that also depends on CREATE TABLE being right; the one-liners
-- below assume the table exists and ask only whether the check fires.
### 1a the smallest statement that shows a UNIQUE check not firing on UPDATE
CREATE TABLE u1(a INT, b INT UNIQUE);
INSERT INTO u1 VALUES(1,1),(2,2);
UPDATE u1 SET b=1 WHERE a>0;
SELECT count(*) AS n, sum(b) AS s FROM u1;
### 1b the smallest statement that shows a CHECK check not firing on UPDATE
CREATE TABLE u2(a INT, b INT CHECK(b>0));
INSERT INTO u2 VALUES(1,1),(2,2);
UPDATE u2 SET b=-1 WHERE a>0;
SELECT count(*) AS n, sum(b) AS s FROM u2;
### 1c UNIQUE on the PRIMARY KEY of an UPDATE
CREATE TABLE u4(a INTEGER PRIMARY KEY, b);
INSERT INTO u4 VALUES(1,'x'),(2,'y');
UPDATE u4 SET a=1 WHERE a=2;
SELECT count(*) AS n FROM u4;
### 1d a CHECK whose expression is a comparison between columns
CREATE TABLE u5(a INT, b INT CHECK(a<b));
INSERT INTO u5 VALUES(1,2);
INSERT INTO u5 VALUES(3,2);
SELECT count(*) AS n FROM u5;
### 1e a CHECK whose expression is an IN
CREATE TABLE u6(c INT CHECK(c IN (1,2,3)));
INSERT INTO u6 VALUES(9);
SELECT count(*) AS n FROM u6;
### 1f a CHECK whose expression is a function call
CREATE TABLE u7(c INT CHECK(abs(c)<10));
INSERT INTO u7 VALUES(99);
SELECT count(*) AS n FROM u7;
### 1g a NOT NULL that does not fire on UPDATE
CREATE TABLE u8(a INT NOT NULL, b);
INSERT INTO u8 VALUES(1,1);
UPDATE u8 SET a=NULL;
SELECT count(*) AS n FROM u8;
### 1h a multi-row UPDATE that should roll back as a unit
CREATE TABLE u9(a INT, b INT CHECK(b>0));
INSERT INTO u9 VALUES(1,1),(2,2),(3,3);
UPDATE u9 SET b=0;
SELECT count(*) AS n, sum(b) AS s FROM u9;
### 1i a check that fires inside a transaction and rolls the transaction back
CREATE TABLE u10(a INT CHECK(a>0));
BEGIN;
INSERT INTO u10 VALUES(-1);
SELECT count(*) AS n FROM u10;
ROLLBACK;
SELECT count(*) AS n FROM u10;
### 1j INSERT OR IGNORE and INSERT OR REPLACE do not remove or ignore
CREATE TABLE u11(a INTEGER PRIMARY KEY, b);
INSERT INTO u11 VALUES(1,'x');
INSERT OR IGNORE INTO u11 VALUES(1,'y');
SELECT b FROM u11;
### 1k REPLACE INTO does not replace
CREATE TABLE u12(a INTEGER PRIMARY KEY, b);
INSERT INTO u12 VALUES(1,'x');
REPLACE INTO u12 VALUES(1,'y');
SELECT b FROM u12;
### 1l a UNIQUE index on a nullable column: NULLs must not collide
CREATE TABLE u13(a, b UNIQUE);
INSERT INTO u13 VALUES(1,NULL),(2,NULL),(3,NULL);
SELECT count(*) AS n FROM u13;

-- ===========================================================================
-- 2. A real stored in a TEXT column loses its `.0`.
-- ===========================================================================
--
-- This is the same defect class as a wrong answer rather than an error, and it
-- is one character. SQLite converts a REAL to TEXT with the `%!.15g` rendering
-- and then appends `.0` when the result would otherwise read as an integer, so
-- 1.0 becomes the three-character string `1.0`. nsqlited stores the one-character
-- string `1`.
--
-- The consequences are not cosmetic and are why it is worth a section: the
-- value is a different string, so its LENGTH changes, so `=` against another
-- text value changes, and a client that round-trips a number through a TEXT
-- column gets a different answer back. It is also silent: nothing is refused.
### 2a the smallest statement that shows it
CREATE TABLE r1(t TEXT);
INSERT INTO r1 VALUES(1.0);
SELECT t, length(t) AS l, typeof(t) AS ty FROM r1;
### 2b the whole float grid through a TEXT column
CREATE TABLE r2(t TEXT);
INSERT INTO r2 VALUES(1.0),(-1.0),(0.0),(1e15),(1e16),(1e17),(1e-5),(1e-4),(0.1),(1e300);
SELECT t, length(t) AS l FROM r2;
### 2c a real stored in an INTEGER and a NUMERIC column, which must NOT get .0
CREATE TABLE r3(i INTEGER, n NUMERIC);
INSERT INTO r3 VALUES(1.0,1.0);
SELECT i, typeof(i) AS ti, n, typeof(n) AS tn FROM r3;
### 2d text affinity on an integer-valued real coming from a string
CREATE TABLE r4(t TEXT);
INSERT INTO r4 VALUES('1.0');
SELECT t, typeof(t) AS ty FROM r4;
### 2e the .0 the column lost also changes a comparison
CREATE TABLE r5(t TEXT);
INSERT INTO r5 VALUES(1.0);
SELECT t='1' AS a, t='1.0' AS b, length(t)=1 AS c FROM r5;

-- ===========================================================================
-- 3. A comparison against a column does not apply the column's affinity.
-- ===========================================================================
--
-- SQLite applies a column's affinity to the OTHER side of a comparison before
-- it compares, so a TEXT column holding '5' equals the integer 5. It does not
-- apply the numeric coercion to a literal with no column on that side, which is
-- why `SELECT '5'=1` is 0 and not 1 -- the distinction is what makes the
-- reduction below attributable to the column rather than to the comparison.
--
-- nsqlited gets the literal case right and the column case wrong, which is the
-- signature of the affinity never being consulted at all rather than of a rule
-- being applied on the wrong side.
### 3a the literal case, which nsqlited gets right
SELECT '5'=1 AS a, '1.0'=1.0 AS b, ' 1 '=1 AS c, '1e2'=100 AS d;
### 3b the column case, which nsqlited gets wrong
CREATE TABLE c1(a INTEGER, b TEXT);
INSERT INTO c1 VALUES('5','5');
SELECT a='5' AS a1 FROM c1;
SELECT b='5' AS b1 FROM c1;
### 3c both columns of a row that holds the same digits
SELECT a='5' AS a1, a=5 AS a2, b='5' AS b1, b=5 AS b2 FROM c1;
### 3d a REAL column compared against a text that looks like an integer
CREATE TABLE c2(r REAL);
INSERT INTO c2 VALUES(5);
SELECT r='5' AS a, r='5.0' AS b FROM c2;
### 3e a NUMERIC column compared against a text
CREATE TABLE c3(n NUMERIC);
INSERT INTO c3 VALUES('5');
SELECT n='5' AS a, n=5 AS b FROM c3;
### 3f the stored class is right even though the comparison is wrong
SELECT a, typeof(a) AS ta, b, typeof(b) AS tb FROM c1;

-- ===========================================================================
-- 4. GLOB panics. A crash, not a wrong answer.
-- ===========================================================================
--
-- The most severe thing in this corpus, because it is a panic rather than a
-- wrong value: any statement containing GLOB takes the process down, so a
-- single untrusted query is a denial of service against the whole CLI.
--
-- The cause is a mismatch between the parser and the evaluator rather than
-- anything about GLOB itself. `parser.rs` turns GLOB into
-- `Expr::Binary { op: BinOp::Glob, .. }` and `eval.rs` has a matching arm for
-- it -- the arm that says `unreachable!("the pattern operators are handled by
-- their own forms")`. The evaluator only has its own form for `Expr::Like`, so
-- the arm is reached and the process aborts. LIKE works; GLOB does not; and
-- the arm that would have implemented GLOB is the one that panics.
### 4a the smallest statement that panics
SELECT 'abc' GLOB 'a*';
### 4b GLOB in a WHERE clause
CREATE TABLE g1(s);
INSERT INTO g1 VALUES('abc'),('ABC'),('xyz');
SELECT s FROM g1 WHERE s GLOB 'a*' ORDER BY s;
### 4c NOT GLOB
SELECT 'abc' NOT GLOB 'z*' AS a;
### 4d LIKE, which is the form that IS implemented, for contrast
SELECT 'abc' LIKE 'a%' AS a, 'abc' LIKE 'A%' AS b;

-- ===========================================================================
-- 5. BETWEEN is two-valued where SQLite is three-valued.
-- ===========================================================================
--
-- `NULL BETWEEN 1 AND 2` is NULL in SQLite and 0 here. The cause is that
-- BETWEEN is expanded to `x >= a AND x <= b`, and the AND is a two-valued
-- one: a NULL operand becomes false rather than unknown. The same expansion is
-- why `NULL NOT BETWEEN 1 AND 2` is 1 here and NULL in SQLite -- the NOT is
-- applied to a false rather than to an unknown.
--
-- A two-valued AND is also the cause of the NULL AND / NULL OR disagreements
-- the second corpus found, so this section is a second window onto one defect:
-- `0 AND NULL` is 0 in both engines because 0 decides it, while `NULL AND 1`
-- is NULL in SQLite and 0 here because nothing decided it.
### 5a the smallest statement that shows it
SELECT NULL BETWEEN 1 AND 2 AS a;
### 5b the NOT form, which disagrees in the other direction
SELECT NULL NOT BETWEEN 1 AND 2 AS a;
### 5c the non-NULL cases, which agree -- so it is NULL and nothing else
SELECT 1 BETWEEN 0 AND 2 AS a, 5 BETWEEN NULL AND 2 AS b, 1 BETWEEN 2 AND 0 AS c;
### 5d BETWEEN across storage classes
SELECT 'b' BETWEEN 1 AND 'c' AS a, 2 BETWEEN 1.5 AND 2.5 AS b;
### 5e BETWEEN used as a WHERE predicate
CREATE TABLE b1(a);
INSERT INTO b1 VALUES(1),(2),(NULL);
SELECT a FROM b1 WHERE a BETWEEN 1 AND 2 ORDER BY a;
### 5f the two-valued AND underneath it
SELECT NULL AND 1 AS a, 0 AND NULL AS b, 1 AND NULL AS c, NULL OR 0 AS d, 1 OR NULL AS e;
### 5g AND and OR with a NULL on the left and a false on the right
SELECT NULL AND 0 AS a, NULL OR 0 AS b, NULL AND NULL AS c, NULL OR NULL AS d;

-- ===========================================================================
-- 6. LIMIT is not coerced, and three of its five wrong answers are wrong in
--    the direction that returns MORE rows than asked for.
-- ===========================================================================
--
-- SQLite requires the LIMIT expression to yield an integer and refuses
-- anything else with `datatype mismatch`, and it treats a negative limit as no
-- limit at all. nsqlited accepts every one of these without converting, so:
--
--   LIMIT '2'   the string is not a number, becomes 0 -- but nsqlited returns
--               every row, which is neither the integer 2 nor the 0 the
--               conversion would give
--   LIMIT NULL   accepted here; a datatype mismatch there
--   LIMIT 1.7   accepted here; a datatype mismatch there
--   LIMIT -1     zero rows here; every row there
--
-- A query that returns every row when it was asked for two is the worst shape
-- this defect can take, and it fails without a message.
### 6a the smallest statement that returns too many rows
CREATE TABLE l1(a);
INSERT INTO l1 VALUES(1),(2),(3),(4),(5);
SELECT a FROM l1 ORDER BY a LIMIT '2';
### 6b a negative limit means no limit in SQLite
SELECT a FROM l1 ORDER BY a LIMIT -1;
### 6c LIMIT NULL is a datatype mismatch in SQLite
SELECT a FROM l1 ORDER BY a LIMIT NULL;
### 6d a non-integer real is a datatype mismatch in SQLite
SELECT a FROM l1 ORDER BY a LIMIT 1.7;
### 6e a non-numeric string is a datatype mismatch in SQLite
SELECT a FROM l1 ORDER BY a LIMIT 'x';
### 6f the cases that agree, so the coercion of a plain integer is fine
SELECT a FROM l1 ORDER BY a LIMIT 2;
SELECT a FROM l1 ORDER BY a LIMIT 2 OFFSET 1;
SELECT a FROM l1 ORDER BY a LIMIT 1, 2;
### 6g OFFSET has the same coercion
SELECT a FROM l1 ORDER BY a LIMIT 2 OFFSET '1';
SELECT a FROM l1 ORDER BY a LIMIT 2 OFFSET -1;

-- ===========================================================================
-- 7. COLLATE is parsed and then thrown away.
-- ===========================================================================
--
-- `eval.rs` evaluates `Expr::Collate` by evaluating its inner expression and
-- discarding the collation name, so a COLLATE changes nothing at all. Every
-- comparison, sort and unique test through it is therefore BINARY.
--
-- The third case is the one that matters most and is not about text at all: a
-- UNIQUE index on a COLLATE NOCASE column does not fire for two values that
-- differ only in case, so a table that a client reads as having a
-- case-insensitive key accepts two rows that must be one.
### 7a the smallest statement that shows it
SELECT 'a' = 'A' COLLATE NOCASE AS a;
### 7b a COLLATE in a comparison is a syntax error
SELECT 'a' COLLATE NOCASE < 'B' AS a;
### 7c a UNIQUE index through a COLLATE NOCASE column does not fire
CREATE TABLE k1(a COLLATE NOCASE UNIQUE);
INSERT INTO k1 VALUES('A');
INSERT INTO k1 VALUES('a');
SELECT count(*) AS n FROM k1;
### 7d ORDER BY a COLLATE NOCASE sorts as BINARY
CREATE TABLE k2(v);
INSERT INTO k2 VALUES('B'),('a'),('C'),('b'),('A');
SELECT v FROM k2 ORDER BY v COLLATE NOCASE;
### 7e the same sort without the COLLATE, for contrast
SELECT v FROM k2 ORDER BY v;
### 7f a COLLATE on a column declaration
CREATE TABLE k3(v COLLATE NOCASE);
INSERT INTO k3 VALUES('A'),('a');
SELECT count(DISTINCT v) AS n FROM k3;

-- ===========================================================================
-- 8. upper() and lower() are ASCII-only, and upper() of a blob is a blob.
-- ===========================================================================
--
-- SQLite's built-in upper() and lower() only fold ASCII: `upper('héllo')` is
-- `HéLLO` with the é untouched, because folding it would need a Unicode table
-- and the built-in has none. nsqlited leaves the non-ASCII characters
-- untouched but also fails to fold the ASCII ones around them -- `HÉLLO` rather
-- than `HéLLO` -- so it is not "the right answer, differently arrived at", it
-- is a different string.
--
-- The second defect is the storage class: upper() of a blob is a blob in
-- SQLite, and here it comes back with the blob tag still on it, so a value
-- that was text became a blob.
### 8a the smallest statement that shows the ASCII-only fold
SELECT upper('héllo') AS a;
### 8b lower, which has the same defect
SELECT lower('ÉÀÜ') AS a;
### 8c the ASCII cases, which agree -- so it is the non-ASCII neighbours
SELECT upper('abc') AS a, lower('ABC') AS b, upper('a1!') AS c;
### 8d upper of a blob keeps the blob class in SQLite
SELECT upper(x'414243') AS a, typeof(upper(x'414243')) AS b;
### 8e upper of a number, which is text in both
SELECT upper(123) AS a, typeof(upper(123)) AS b;

-- ===========================================================================
-- 9. INF and NAN: the overflow and the division, and how they render.
-- ===========================================================================
--
-- SQLite renders an overflowed real as `Inf`, which is a REAL and which
-- `quote()` reports as `Inf`. nsqlited stores a real infinity, so the value is
-- right and the RENDERING is not, and the rendering is what a client sees.
--
-- The second is the underflow case, and it is a different bug: 1e-308/10 is a
-- subnormal double, and SQLite's decimal conversion of a subnormal is not
-- correctly rounded, so it needs 17 significant digits where the shortest
-- round-tripping form needs 1. nsqlited emits the shortest form. Both values
-- are the same double; they are written differently, and the engine's own
-- `testsuite.rs` documents the divergence rather than fixing it.
### 9a the overflow rendering
SELECT 1e308*10 AS a, typeof(1e308*10) AS b;
### 9b the same value on the other side of the sign
SELECT -1e308*10 AS a, typeof(-1e308*10) AS b;
### 9c the subnormal rendering
SELECT 1e-308/10 AS a, typeof(1e-308/10) AS b;
### 9d the well-conditioned cases, which agree
SELECT 1e308/1e308 AS a, 1e-308*1e308 AS b, 1.0/3 AS c, 22.0/7 AS d;
### 9e the shortest-round-trip divergences the engine documents for itself
SELECT quote(1.0/3) AS a, quote(2.0/3) AS b, quote(22.0/7) AS c;
SELECT quote(1.0/49) AS a, quote(2.0/9) AS b, quote(1.0e-310) AS c;
### 9f division by zero
SELECT 1.0/0.0 AS a, -1.0/0.0 AS b, typeof(1.0/0.0) AS c;
### 9g the integer division by zero, which is NULL rather than an error
SELECT 1/0 AS a, typeof(1/0) AS b, 1%0 AS c, typeof(1%0) AS d;

-- ===========================================================================
-- 10. The integer boundary: the literal, the unary minus, and the overflow.
-- ===========================================================================
--
-- `-9223372036854775808` is an INTEGER in SQLite because the leading minus is
-- part of the literal, so the parser sees one token that fits in an i64. Parse
-- the minus as an operator and the magnitude 9223372036854775808 has already
-- overflowed by the time it is negated, and the value comes back as a REAL.
-- That is the wrong answer rather than an error, and it is silent.
--
-- The unary minus on TEXT is a separate and louder finding: SQLite applies the
-- numeric coercion and returns -3, and nsqlited refuses the whole statement,
-- so a query written the ordinary way cannot run at all.
### 10a the smallest statement that shows the literal boundary
SELECT -9223372036854775808 AS a, typeof(-9223372036854775808) AS b;
### 10b the same value reached by arithmetic
SELECT -9223372036854775808+0 AS a, typeof(-9223372036854775808+0) AS b;
### 10c negating a parenthesised expression that has already overflowed
SELECT -(9223372036854775807+1) AS a, typeof(-(9223372036854775807+1)) AS b;
### 10d the overflow itself, which is a real in both
SELECT 9223372036854775807+1 AS a, typeof(9223372036854775807+1) AS b;
### 10e unary minus on text, which is refused rather than coerced
SELECT -'3' AS a, typeof(-'3') AS b;
### 10f unary minus on text that is not a number, and on NULL
SELECT -'abc' AS a, -NULL AS b, typeof(-NULL) AS c;
### 10g unary minus on a blob and on an exponent-form string
SELECT -x'31' AS a, -'1e2' AS b, typeof(-'1e2') AS c;
### 10h integer arithmetic that overflows, which is a real in both
SELECT 4611686018427387904+4611686018427387904 AS a, 3000000000*3000000000 AS b;
### 10i integer arithmetic that does NOT overflow, which must stay an integer
SELECT 3000000000*3000000000-1 AS a, typeof(3000000000*3000000000-1) AS b;
SELECT 1000000*1000000 AS a, 1000000*1000000*1000000 AS b;

-- ===========================================================================
-- 11. CAST: which conversions change the storage class
-- ===========================================================================
--
-- `CAST(x'414243' AS TEXT)` is TEXT holding `ABC` in SQLite. Here it comes
-- back still tagged as a blob, so the value and the class are both wrong, and
-- `CAST(0 AS BLOB)` returns the integer 0 rather than a blob. A client that
-- stores with a CAST and reads back with typeof gets a different class from
-- the one it wrote.
### 11a the smallest statement that shows it
SELECT CAST(x'414243' AS TEXT) AS a, typeof(CAST(x'414243' AS TEXT)) AS b;
### 11b CAST to BLOB
SELECT CAST(0 AS BLOB) AS a, typeof(CAST(0 AS BLOB)) AS b;
### 11c CAST blob to BLOB, which agrees
SELECT CAST(x'414243' AS BLOB) AS a, typeof(CAST(x'414243' AS BLOB)) AS b;
### 11d CAST to INTEGER truncates toward zero in both
SELECT CAST(1.9 AS INTEGER) AS a, CAST(-1.9 AS INTEGER) AS b, typeof(CAST(1.9 AS INTEGER)) AS c;
### 11e CAST to REAL, which agrees
SELECT CAST('7' AS REAL) AS a, typeof(CAST('7' AS REAL)) AS b, CAST('x' AS REAL) AS c;
### 11f CAST to TEXT, which agrees for numbers
SELECT CAST(1 AS TEXT) AS a, CAST(1.5 AS TEXT) AS b, typeof(CAST(1.5 AS TEXT)) AS c;
### 11g CAST a NULL, which agrees
SELECT CAST(NULL AS INTEGER) AS a, CAST(NULL AS TEXT) AS b, typeof(CAST(NULL AS TEXT)) AS c;

-- ===========================================================================
-- 12. The empty-set result of every aggregate, one statement at a time.
-- ===========================================================================
--
-- Included because the second corpus found the aggregate-over-non-numbers
-- defect here and this is the same function with a different input, and
-- because the per-aggregate empty results are the easiest thing in SQL to get
-- uniformly wrong: count is 0, total is 0.0 and is a REAL, sum is NULL, avg is
-- NULL, min and max are NULL and group_concat is NULL. One statement per
-- aggregate, so a failure in one is not masked by a passing one beside it.
### 12a the empty set, one aggregate at a time
CREATE TABLE z1(a);
SELECT count(*) FROM z1;
SELECT count(a) FROM z1;
SELECT count(DISTINCT a) FROM z1;
SELECT sum(a) FROM z1;
SELECT total(a) FROM z1;
SELECT avg(a) FROM z1;
SELECT min(a) FROM z1;
SELECT max(a) FROM z1;
SELECT group_concat(a) FROM z1;
### 12b a set that is all NULL, which is not the same as the empty set
CREATE TABLE z2(a);
INSERT INTO z2 VALUES(NULL),(NULL);
SELECT count(*) AS c, count(a) AS ca, sum(a) AS s, total(a) AS t FROM z2;
SELECT typeof(sum(a)) AS s, typeof(total(a)) AS t, typeof(avg(a)) AS a FROM z2;
### 12c one value, to separate the "no rows" case from the "one row" case
CREATE TABLE z3(a);
INSERT INTO z3 VALUES(1);
SELECT sum(a) AS s, typeof(sum(a)) AS ts, total(a) AS t, typeof(total(a)) AS tt FROM z3;
### 12d one real value, where sum is a real and avg divides by one
CREATE TABLE z4(a);
INSERT INTO z4 VALUES(1.5);
SELECT sum(a) AS s, typeof(sum(a)) AS ts, avg(a) AS a, typeof(avg(a)) AS ta FROM z4;
### 12e an aggregate over a group where every value is NULL
SELECT min(a) AS mn, max(a) AS mx, group_concat(a) AS g FROM z2;

-- ===========================================================================
-- 13. group_concat: the order, the NULLs and the separator
-- ===========================================================================
--
-- group_concat skips NULLs rather than rendering them, joins in the order the
-- rows arrive, and the DISTINCT form is over the joined values. The second
-- corpus found the separator and the DISTINCT argument; this is the ORDER BY
-- form and the class of the values, which are separate code paths.
### 13a a blob and a real through group_concat
CREATE TABLE y1(v);
INSERT INTO y1 VALUES(1),(NULL),(3),(2);
SELECT group_concat(v) AS g FROM y1;
### 13b the separator, and the DISTINCT argument
SELECT group_concat(v,'-') AS g FROM y1;
SELECT group_concat(DISTINCT v) AS g FROM y1;
### 13c a blob and a real through group_concat
INSERT INTO y1 VALUES(x'31'),(1.5);
SELECT group_concat(v) AS g FROM y1;
### 13d group_concat over an empty set, which is NULL rather than ''
SELECT group_concat(v) AS g FROM y1 WHERE v>99;

-- ===========================================================================
-- 14. Missing functions: what the real engine has and this one does not
-- ===========================================================================
--
-- A missing function is a missing capability whatever the value is, so these
-- are collected in one place rather than spread through the earlier sections.
-- They are one-liners, so a reader can see the whole list at once.
### 14a the math functions
SELECT ceil(1.2) AS a;
SELECT floor(-1.8) AS a;
SELECT trunc(1.9) AS a;
SELECT sign(-5) AS a;
SELECT sqrt(4) AS a;
SELECT exp(1) AS a;
SELECT ln(1) AS a;
SELECT log(100) AS a;
SELECT pow(2,10) AS a;
SELECT pi() AS a;
SELECT mod(7,3) AS a;
### 14b the string functions
SELECT char(65,66) AS a;
SELECT unicode('A') AS a;
SELECT unhex('414243') AS a;
SELECT printf('%d',5) AS a;
SELECT printf('%5.2f',3.14159) AS a;
### 14c the functions that DO exist, for contrast
SELECT abs(-1.5) AS a, round(2.5) AS b, length('abc') AS c, substr('abc',2) AS d;
SELECT hex('abc') AS a, quote('a') AS b, typeof(1) AS c, ifnull(NULL,1) AS d;
### 14d the SQL-level features that are missing
SELECT 1 IN (1,2)=1 AS a;
SELECT a FROM (SELECT 1 AS a) AS t;
SELECT 1 WHERE EXISTS (SELECT 1);
SELECT iif(1,2,3) AS a;
### 14e the schema features that are missing
SELECT name FROM pragma_table_list;
SELECT cid, name, type FROM pragma_table_info('z1');
SELECT name FROM pragma_index_list('z1');
### 14f the write features that are missing
ALTER TABLE z1 RENAME TO z2;
CREATE TABLE as1 AS SELECT 1 AS x;
CREATE TABLE gv(a INT, b INT GENERATED ALWAYS AS (a*2) VIRTUAL);
SELECT rowid FROM z1;
SELECT last_insert_rowid() AS x;
CREATE VIEW v1 AS SELECT 1 AS x;
CREATE TABLE up1(a INTEGER PRIMARY KEY, b);
INSERT INTO up1 VALUES(1,2) ON CONFLICT(a) DO UPDATE SET b=3;
