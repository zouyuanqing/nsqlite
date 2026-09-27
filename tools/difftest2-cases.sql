-- The corpus for tools/difftest2.sh, covering what tools/difftest.sh does not.
--
-- A case file is a sequence of sections. Each starts with `### <name>` and runs
-- from an empty pair of databases, so one section's schema never answers
-- another's query. Read with:
--
--     tools/difftest2.sh tools/difftest2-cases.sql
--     tools/difftest2.sh tools/difftest2-cases.sql --list
--     tools/difftest2.sh tools/difftest2-cases.sql --section 12
--
-- Every statement here is valid SQL that real sqlite3 accepts. That is not an
-- assumption: each was run against the real shell while this corpus was being
-- written, and a case that real sqlite3 refuses would be testing the runner
-- rather than the engine. Where an engine's refusal IS the thing under test --
-- a constraint that should fire part way through a multi-row INSERT -- the case
-- is written so that the refusal is the correct behaviour and only one engine
-- produces it.
--
-- The corpus is deliberately biased towards the answers that are WRONG rather
-- than absent. A statement the parser refuses fails loudly and is cheap to
-- find; the defects worth hunting return a plausible value. So the bias is
-- towards shapes where a value is computable and an implementation can get it
-- subtly, and wrongly, right: operator precedence, storage-class boundaries,
-- the empty-set behaviour that differs per aggregate, and float rendering.

-- ===========================================================================
-- 1. Expression grammar: precedence, associativity, and the integer boundary
-- ===========================================================================
--
-- Concatenation binds tighter than every arithmetic operator in SQLite, which
-- is the opposite of what most languages do and the opposite of what reading
-- the grammar suggests. `2||3*4` is `(2||3)*4` = 92, not `2||(3*4)` = 212.
-- This was measured against the real engine before it was written down, which
-- is how it is known to be 92 and not merely suspected to be.
### 1a arithmetic precedence, all pairs
SELECT 2+3*4 AS a, 2*3+4 AS b, 2+3-4 AS c, 2-3+4 AS d, 10-2-3 AS e, 2-3-4 AS f;
SELECT 2*3*4 AS a, 100/5/2 AS b, 100/20/5 AS c, 2-2-2 AS d, 2/2/2 AS e;
SELECT 2+3%4 AS a, 2%3+4 AS b, 2*3%4 AS c, 2%3*4 AS d, 100%7%3 AS e;
SELECT 2*3/4 AS a, 2/3*4 AS b, 2/3%4 AS c, 2%3/4 AS d, 1+2*3-4/2 AS e;
SELECT 1+2*3+4*5-6/3 AS a, 20%7*3+1 AS b, 100/10/2%3 AS c, 2*3%4/2 AS d;
### 1b parentheses move the grouping
SELECT 2*(3+4) AS a, (2+3)*4 AS b, 2+3*4-1 AS c, ((1)) AS d, (1+2)*(3+4) AS e;
SELECT (2+3)*(4-1) AS a, 2*(3*(4+1)) AS b, ((2+3)*4)*5 AS c;
### 1c concatenation binds tighter than arithmetic
SELECT 2||3*4 AS a, 2*3||4 AS b, 1||2*3 AS c, 2||3+4 AS d, 2+3||4 AS e;
SELECT 1+1||1*1 AS a, 5||5 AS b, 5||'5' AS c, 'a'||1+2 AS d, 1+2||'a' AS e;
SELECT 2||3*4-1 AS a, 10-2||3 AS b, 2*2||2*2 AS c;
### 1d unary minus: precedence, chaining, and the literal boundary
-- A leading `-` on a numeric literal is part of the literal, which is why
-- -9223372036854775808 is an integer at all: 9223372036854775808 on its own
-- would not fit in an i64. An engine that parses the minus as an operator over
-- a literal it has already overflowed gets this wrong, and gets it wrong
-- quietly, by returning a real.
SELECT -9223372036854775808 AS min_int, 9223372036854775807 AS max_int, -0 AS neg_zero;
SELECT -9223372036854775807-1 AS underflow, 9223372036854775807+1 AS overflow;
SELECT typeof(9223372036854775807+1) AS overflow_type, typeof(-9223372036854775808) AS minint_type;
SELECT -2+3 AS a, -2*3 AS b, -2-3 AS c, 3*-2 AS d, -(3)-4 AS e, - - -1 AS f;
SELECT -0 AS a, - -0 AS b, -(-0) AS c, typeof(-0) AS t1, typeof(- -0) AS t2;
SELECT -1.5 AS a, -'3' AS b, -'abc' AS c, -NULL AS d, typeof(-NULL) AS e;
SELECT -9223372036854775808+0 AS a, -9223372036854775808*1 AS b, -9223372036854775808/1 AS c;
### 1e integer division and modulo truncate toward zero
-- The sign of the result follows the DIVIDEND, not the divisor and not the
-- sign of the quotient: -7/2 is -3, 7/-2 is -3 and -7%-2 is 1. An
-- implementation using Rust's `/` and `%` gets -7%2 right by accident, because
-- Rust truncates too, but a float-based or floor-based one does not.
SELECT 7/2 AS a, -7/2 AS b, 7/-2 AS c, -7/-2 AS d, 7%2 AS e, -7%2 AS f, 7%-2 AS g, -7%-2 AS h;
SELECT 0/1 AS a, 1/0 AS b, 0%1 AS c, 1%0 AS d, typeof(1/0) AS e, typeof(1%0) AS f;
SELECT 1/3 AS a, 2/3 AS b, -1/3 AS c, 1.0/3 AS d, 7/2.0 AS e, 10/4 AS f;
SELECT 7.0%2 AS a, typeof(7.0%2) AS b, 7%2.0 AS c, typeof(7%2.0) AS d, -7.0%2 AS e;
### 1f string and numeric mixing
-- A string that does not look like a number is coerced to 0 rather than being
-- an error, so 'abc'+1 is 1 and not a failure. A string that does look like one
-- is converted, and '1e2'+0 is 100.0 -- a real, which is the case an integer
-- implementation is most likely to get wrong.
SELECT '1'+1 AS a, 1+'2' AS b, '2'*'3' AS c, 'abc'+1 AS d, '0x10'+0 AS e, '1e2'+0 AS f;
SELECT typeof('1'+1) AS a, typeof('1e2'+0) AS b, typeof('abc'+1) AS c, typeof('0x10'+0) AS d;
SELECT '1.5'+1 AS a, ' 2 '+1 AS b, '.5'+1 AS c, '5.'+1 AS d, '1e2'+0 AS e;
SELECT 'a'||NULL AS a, NULL||'a' AS b, 'a'||1.5 AS c, typeof('a'||1.5) AS d;
SELECT 1+NULL AS a, NULL-1 AS b, NULL*1 AS c, NULL/1 AS d, 1%NULL AS e;
### 1g comparison chains and cross-class comparison
-- SQLite has no chained comparison, so `1=1=1` is `(1=1)=1`: 1, not a
-- conjunction. An engine that implements chaining as conjunction would return
-- the same 1 here and be caught by the case that separates them.
SELECT 1=1=1 AS a, 2>1>0 AS b, 1<2<3 AS c, 1=2=1 AS d, 3>=3>=1 AS e;
SELECT 'a'<'b' AS a, 'A'<'a' AS b, 'a'='A' AS c, ''<'a' AS d, 'a'||''='a' AS e;
SELECT x'31'='1' AS a, x'31'=1 AS b, 1='1' AS c, '1'=1.0 AS d, '1'=1 AS e;
SELECT 1<>2 AS a, 1!=2 AS b, 1==1 AS c, NULL=NULL AS d, NULL<>NULL AS e, NULL<NULL AS f;
SELECT '2'<'10' AS a, 2<10 AS b, '2'<'10' AS c, typeof('2'<'10') AS d;
SELECT 1 IS 1 AS a, 'a' IS 'a' AS b, 1 IS NOT 2 AS c, NULL IS NULL AS d, 1 ISNULL AS e;
### 1h three-valued logic, NULL on either side
-- The empty fields here are real NULLs, not the empty string. The three
-- columns are chosen so that the FALSE, TRUE and UNKNOWN cases of each operator
-- all appear, and a two-valued implementation gets at least one wrong.
SELECT NULL AND 0 AS a, NULL AND 1 AS b, 0 AND NULL AS c, 1 AND NULL AS d, NULL AND NULL AS e;
SELECT NULL OR 0 AS a, NULL OR 1 AS b, 0 OR NULL AS c, 1 OR NULL AS d, NULL OR NULL AS e;
SELECT NOT NULL AS a, NOT 0 AS b, NOT 1 AS c, 0 AND NOT NULL AS d, NULL AND NOT 0 AS e;
SELECT 0 OR 1 AND 0 AS a, (0 OR 1) AND 0 AS b, 1 OR 0 AND 0 AS c, NOT 1=1 AS d;
SELECT 1 AND 0 OR 1 AS a, 0 OR 0 AND NULL AS b, NULL OR NULL AND NULL AS c;
SELECT (NULL AND 0) IS 0 AS a, (NULL OR 1) IS 1 AS b, (NULL=NULL) IS NULL AS c;
SELECT CASE WHEN NULL THEN 1 ELSE 2 END AS a, CASE WHEN NULL OR 0 THEN 1 ELSE 2 END AS b;
SELECT coalesce(NULL,NULL,3) AS a, coalesce(NULL,NULL) AS b, ifnull(NULL,'d') AS c, nullif(1,1) AS d;
### 1i CAST, and the type boundary it exposes
SELECT CAST(1.9 AS INTEGER) AS a, CAST(-1.9 AS INTEGER) AS b, CAST('12abc' AS INTEGER) AS c;
SELECT typeof(CAST(1.0 AS INTEGER)) AS a, typeof(CAST(1.5 AS INTEGER)) AS b, typeof(CAST('1' AS INTEGER)) AS c;
SELECT CAST(1 AS TEXT) AS a, typeof(CAST(1 AS TEXT)) AS b, CAST(1.5 AS TEXT) AS c;
SELECT CAST('7' AS REAL) AS a, typeof(CAST('7' AS REAL)) AS b, CAST('x' AS REAL) AS c;
SELECT CAST(x'414243' AS TEXT) AS a, typeof(CAST(x'414243' AS TEXT)) AS b, CAST(0 AS BLOB) AS c;
SELECT CAST(1e300 AS INTEGER) AS a, typeof(CAST(1e300 AS INTEGER)) AS b, CAST(-1.9 AS INTEGER) AS c;

-- ===========================================================================
-- 2. Type and affinity across the whole grid
-- ===========================================================================
--
-- Every value is inserted into all five column affinities and read back with
-- typeof, so the answer being checked is the STORED class and the stored value,
-- not the value that was written. The five are INTEGER, REAL, TEXT, BLOB and
-- NUMERIC, which between them cover the three conversion rules: nothing is
-- stored in a BLOB column, a TEXT column stores what it is given as text unless
-- it is numeric, and INTEGER and NUMERIC both try to make a number.
### 2a the affinity grid, one table, every value into every affinity
CREATE TABLE aff(i INTEGER, r REAL, s TEXT, b BLOB, n NUMERIC);
INSERT INTO aff VALUES(1, 1.0, '1', x'31', 1);
INSERT INTO aff VALUES('2', '2.5', 2, '2', '2.5');
INSERT INTO aff VALUES(3.0, 'abc', 'abc', 'abc', 'abc');
INSERT INTO aff VALUES(NULL, NULL, NULL, NULL, NULL);
INSERT INTO aff VALUES(0.5, 0.5, 0.5, 0.5, 0.5);
SELECT i, typeof(i) AS ti, r, typeof(r) AS tr, s, typeof(s) AS ts, b, typeof(b) AS tb, n, typeof(n) AS tn FROM aff;
SELECT i, typeof(i) AS ti, r, typeof(r) AS tr, s, typeof(s) AS ts, b, typeof(b) AS tb, n, typeof(n) AS tn FROM aff;
### 2b affinity, the values that decide which rule applies
CREATE TABLE aff2(i INTEGER, r REAL, s TEXT, b BLOB, n NUMERIC);
INSERT INTO aff2 VALUES(1.0, 1.0, 1.0, 1.0, 1.0);
INSERT INTO aff2 VALUES('2', '2', '2', '2', '2');
INSERT INTO aff2 VALUES('2.5', '2.5', '2.5', '2.5', '2.5');
INSERT INTO aff2 VALUES(' 3 ', ' 3 ', ' 3 ', ' 3 ', ' 3 ');
INSERT INTO aff2 VALUES(4e2, 4e2, 4e2, 4e2, 4e2);
SELECT i, typeof(i) AS ti, r, typeof(r) AS tr, s, typeof(s) AS ts, b, typeof(b) AS tb, n, typeof(n) AS tn FROM aff2;
### 2c affinity, the declared types that carry a rule rather than a spelling
-- INT, INT2, INT8, INTEGER and the CHAR/CLOB/TEXT family all pick a rule by
-- substring match, so their case does not matter: InTeGeR and integer have the
-- same affinity. UNSIGNED BIG INT and Native Character carry a rule in the
-- middle of the name, which is where a prefix-only implementation diverges.
CREATE TABLE aff3(a INT, b INT2, c INT8, d INTEGER, e CHAR(20), f CLOB, g TEXT, h 'bigint', i 'UNSIGNED BIG INT', j 'Native Character(70)', k 'VARYING CHARACTER(255)');
INSERT INTO aff3(a,b,c,d,e,f,g,h,i,j,k) VALUES('2','2','2','2','2','2','2','2','2','2','2');
INSERT INTO aff3(a,b,c,d,e,f,g,h,i,j,k) VALUES(2.5,2.5,2.5,2.5,2.5,2.5,2.5,2.5,2.5,2.5,2.5);
INSERT INTO aff3(a,b,c,d,e,f,g,h,i,j,k) VALUES('abc','abc','abc','abc','abc','abc','abc','abc','abc','abc','abc');
SELECT typeof(a) AS a1, typeof(b) AS b1, typeof(c) AS c1, typeof(d) AS d1 FROM aff3 WHERE a IS NOT NULL AND b IS NOT NULL;
SELECT typeof(a) AS a2, typeof(b) AS b2, typeof(c) AS c2, typeof(d) AS d2 FROM aff3 WHERE b IS NOT NULL;
SELECT typeof(e) AS e1, typeof(f) AS f1, typeof(g) AS g1 FROM aff3 WHERE e IS NOT NULL;
SELECT typeof(h) AS h1, typeof(i) AS i1, typeof(j) AS j1, typeof(k) AS k1 FROM aff3 WHERE h IS NOT NULL;
SELECT a, typeof(a) AS t1, j, typeof(j) AS t2 FROM aff3 ORDER BY 1;
### 2d affinity in a comparison: the value is converted, not the column
CREATE TABLE aff4(x INTEGER);
INSERT INTO aff4 VALUES(2);
SELECT x='2' AS a, '2'=x AS b, x='2.0' AS c, x='abc' AS d, 'abc'=x AS e FROM aff4;
SELECT x=2.0 AS a, 2.0=x AS b, x+1 AS c, '2'+1 AS d, x||'' AS e FROM aff4;
SELECT 2='2' AS a, 2.0='2' AS b, 2='2.0' AS c, x'32'=2 AS d, '2'=x'32' AS e;
### 2e text affinity stores the rendering of a real
-- A real written into a TEXT column is converted to a STRING, and the string
-- becomes the value. The conversion is the shortest text that reads back as the
-- same double, so 1.0 is `1.0` and 1e300 is `1.0e+300` -- not 300 digits. The
-- `.0` matters: `1.0` and `1` are different strings, and length() separates them.
CREATE TABLE txt(x TEXT);
INSERT INTO txt VALUES(1.5), (1.0), (0.1), (1e20), (1e17), (1e-300);
SELECT x, length(x) AS len, typeof(x) AS t FROM txt;
SELECT x, length(x) AS len, typeof(x) AS t FROM txt;
### 2f numeric affinity on a value that is not representable as an integer
CREATE TABLE num(n NUMERIC);
INSERT INTO num VALUES(2.5), ('2.5'), (3), ('3'), ('abc'), (NULL);
SELECT n, typeof(n) AS t FROM num;
CREATE TABLE num2(i INTEGER);
INSERT INTO num2 VALUES(2.5), ('2.5'), (3), ('abc');
SELECT i, typeof(i) AS t FROM num2;

-- ===========================================================================
-- 3. GROUP BY, HAVING, the aggregates, and DISTINCT
-- ===========================================================================
--
-- The empty-set row is the part of this section worth having. It differs per
-- aggregate and the differences are easy to get uniformly wrong: count is 0,
-- total is 0.0 and is a REAL, sum is NULL, avg is NULL, min and max are NULL,
-- and group_concat is NULL. An implementation that returns 0 for sum, or 0.0
-- for avg, or an empty string for group_concat, is wrong on every one of those
-- and right on none.
### 3a the empty-set results, per aggregate
CREATE TABLE empty_t(a);
SELECT count(*) AS c1, count(a) AS c2, sum(a) AS s, total(a) AS t, avg(a) AS av, min(a) AS mn, max(a) AS mx FROM empty_t;
-- The aliases here avoid repeating the column's own name, because `sum(a) AS a`
-- is refused by this engine and would make the whole statement unrunnable
-- before it reached the part it exists to check. The refusal itself is kept as
-- a case in section 9a.
SELECT typeof(sum(a)) AS ts, typeof(total(a)) AS tt, typeof(avg(a)) AS ta, typeof(count(a)) AS tc, typeof(min(a)) AS tm FROM empty_t;
SELECT count(*) AS c1, count(a) AS c2, count(DISTINCT a) AS c3, sum(a) AS c4, group_concat(a) AS c5 FROM empty_t;
### 3b aggregates over a group
CREATE TABLE grp(a, b);
INSERT INTO grp VALUES(1,10),(1,20),(2,30),(2,NULL),(3,40);
SELECT a, count(*) AS c1, count(b) AS c2, sum(b) AS s, avg(b) AS av, min(b) AS mn, max(b) AS mx FROM grp GROUP BY a;
SELECT a, sum(b) AS s, total(b) AS t FROM grp GROUP BY a;
SELECT a, group_concat(b) AS g FROM grp GROUP BY a;
SELECT sum(b) AS s, count(b) AS c FROM grp;
SELECT sum(b) AS s, count(b) AS c FROM grp WHERE a > 99;
### 3c GROUP BY with a HAVING that filters, and one that does not
SELECT a, sum(b) AS s FROM grp GROUP BY a HAVING count(*) > 1;
SELECT a, sum(b) AS s FROM grp GROUP BY a HAVING sum(b) > 20;
SELECT a, sum(b) AS s FROM grp GROUP BY a HAVING sum(b) IS NULL;
SELECT a, sum(b) AS s FROM grp GROUP BY a HAVING a > 1 AND sum(b) IS NOT NULL;
SELECT a, sum(b) AS s FROM grp GROUP BY a HAVING count(*) > 0;
SELECT a, sum(b) AS s FROM grp GROUP BY a HAVING 0;
SELECT count(*) AS c FROM grp HAVING count(*) > 0;
SELECT count(*) AS c FROM grp HAVING count(*) > 99;
SELECT sum(a) AS s FROM grp HAVING sum(a) > 0;
### 3d an aggregate with no GROUP BY is one group, and an empty one is no row
SELECT sum(a) AS s, count(*) AS c FROM empty_t;
SELECT count(*) AS c FROM grp HAVING sum(a) > 1000;
### 3e DISTINCT, where the storage class decides which rows survive
-- 1 and 1.0 are ONE group because they compare equal, while NULL and '' are two,
-- because a NULL never equals anything. DISTINCT therefore keeps 1, 1.0, NULL
-- and '' -- four groups from five rows. Sorting them afterwards is what makes
-- the NULL placement observable.
CREATE TABLE dis(v);
INSERT INTO dis VALUES(1),(1.0),('1'),(NULL),(''),(2),(2.0);
SELECT DISTINCT v, typeof(v) AS t FROM dis;
SELECT DISTINCT v, typeof(v) AS t FROM dis ORDER BY v;
SELECT DISTINCT v FROM dis ORDER BY v DESC;
SELECT DISTINCT typeof(v) AS t FROM dis;
SELECT DISTINCT v FROM dis ORDER BY v IS NULL, v;
### 3f DISTINCT over a whole row, and with an aggregate
CREATE TABLE dis2(a, b);
INSERT INTO dis2 VALUES(1,1),(1,1),(1,'1'),(NULL,NULL),(NULL,NULL);
SELECT DISTINCT a, b FROM dis2;
SELECT DISTINCT a FROM dis2;
SELECT DISTINCT count(*) AS c FROM dis2;
SELECT count(DISTINCT a) AS c, count(DISTINCT b) AS d, count(*) AS e FROM dis2;
### 3g group_concat, the separator and DISTINCT argument
SELECT group_concat(a) AS g1, group_concat(a,'-') AS g2, group_concat(DISTINCT a) AS g3 FROM grp;
SELECT group_concat(b,'|') AS g FROM grp;
SELECT a, group_concat(b,'-') AS g FROM grp GROUP BY a;
SELECT group_concat(NULL) AS g, group_concat('x') AS h FROM empty_t;
### 3h the scalar min and max take an argument list, and all-NULL is NULL
SELECT max(1,2,3) AS a, max(NULL,2) AS b, max(1,NULL) AS c, min(1,2) AS d, min('a','b') AS e;
SELECT typeof(max(1,2)) AS a, typeof(max(1,2.0)) AS b, typeof(min(1,2.0)) AS c, typeof(max(1,NULL)) AS d;
-- The zero-argument form is a parse error in the real engine, not a NULL: it is
-- included so the refusal itself is compared, and so an engine that returns
-- NULL where the real one refuses is caught. It is in its own statement so that
-- the rest of the section still runs when it is refused.
SELECT max() AS a;
### 3i the aggregates over values that are not numbers
CREATE TABLE mixed(v);
INSERT INTO mixed VALUES(1),('2'),('abc'),(NULL),(3.5),(x'31');
SELECT sum(v) AS s, total(v) AS t, avg(v) AS av, count(v) AS c, count(*) AS c2 FROM mixed;
SELECT min(v) AS mn, max(v) AS mx, typeof(min(v)) AS t1, typeof(max(v)) AS t2 FROM mixed;
SELECT group_concat(v) AS g FROM mixed;
SELECT typeof(sum(v)) AS a, typeof(total(v)) AS b, typeof(avg(v)) AS c FROM mixed;

-- ===========================================================================
-- 4. ORDER BY: ordinals, aliases, expressions, NULL placement, LIMIT/OFFSET
-- ===========================================================================
--
-- SQLite sorts NULL first in ASC and last in DESC, which falls out of NULL
-- being smaller than everything rather than being a special case. An
-- implementation that sorts NULLs last in both directions gets half of these
-- wrong, and an implementation that treats an ORDER BY key as a column index
-- rather than an alias gets the first two wrong.
CREATE TABLE ord(a, b, v);
INSERT INTO ord VALUES(1,2,NULL),(1,1,10),(2,1,20),(2,2,NULL),(NULL,1,30);
### 4a ordinals and aliases as sort keys
SELECT a AS k, b AS m FROM ord ORDER BY 1;
SELECT a AS k, b AS m FROM ord ORDER BY k;
SELECT a AS k, b AS m FROM ord ORDER BY 2, 1;
SELECT a, b FROM ord ORDER BY 1 DESC, 2 ASC;
### 4b an expression as the sort key
SELECT a, b FROM ord ORDER BY a+b;
SELECT a, b FROM ord ORDER BY a+b DESC;
SELECT a, b FROM ord ORDER BY b*2 ASC, a ASC;
SELECT a, b FROM ord ORDER BY abs(a) ASC, b DESC;
### 4c multiple keys with mixed directions
SELECT a, b, v FROM ord ORDER BY a ASC, b DESC;
SELECT a, b, v FROM ord ORDER BY a DESC, b ASC;
SELECT a, b, v FROM ord ORDER BY b DESC, a ASC, v ASC;
SELECT a, b FROM ord ORDER BY a, b;
### 4d NULL placement, each direction
SELECT a FROM ord ORDER BY a ASC;
SELECT a FROM ord ORDER BY a DESC;
SELECT v FROM ord ORDER BY v ASC;
SELECT v FROM ord ORDER BY v DESC;
SELECT a, b FROM ord ORDER BY a IS NULL DESC, a;
### 4e LIMIT with OFFSET, and the comma form
SELECT a, b FROM ord ORDER BY a, b LIMIT 2;
SELECT a, b FROM ord ORDER BY a, b LIMIT 2 OFFSET 1;
SELECT a, b FROM ord ORDER BY a, b LIMIT 1, 2;
SELECT a, b FROM ord ORDER BY a, b LIMIT 0;
SELECT a, b FROM ord ORDER BY a, b LIMIT 100 OFFSET 3;
SELECT a, b FROM ord ORDER BY a, b LIMIT -1 OFFSET 2;
SELECT a, b FROM ord ORDER BY a, b LIMIT 2 OFFSET 99;
SELECT a, b FROM ord ORDER BY a, b LIMIT 1 OFFSET 1;
### 4f an expression in the select list that is not the sort key
SELECT a+b AS s, a, b FROM ord ORDER BY s DESC;
SELECT a, b AS k FROM ord ORDER BY k, a;
### 4g ORDER BY over a group, and over a value that is not in the result
SELECT a, sum(b) AS s FROM ord GROUP BY a ORDER BY a DESC;
SELECT a, count(*) AS c FROM ord GROUP BY a ORDER BY 2 DESC;
SELECT a, sum(b) AS s FROM ord GROUP BY a ORDER BY sum(b) DESC;
SELECT a FROM ord ORDER BY v;

-- ===========================================================================
-- 5. The text and math functions, each with its edge cases
-- ===========================================================================
--
-- Each function is called with the arguments that decide its behaviour: an
-- empty string, a NULL, a negative index, a value at a type boundary, and a
-- precision where the function takes one. The boundary is usually about what
-- the function returns rather than what it computes -- a negative starting
-- position counts from the end, and a start position of 0 behaves as 1.
### 5a length, upper, lower, trim
SELECT length(NULL) AS a, length('') AS b, length('abc') AS c, length('héllo') AS d, length(x'00ff') AS e;
SELECT upper('abc') AS a, upper(NULL) AS b, lower('ABC') AS c, lower('') AS d, upper('héllo') AS e;
SELECT trim('  ab  ') AS a, trim('xxaxx','x') AS b, ltrim('  a') AS c, rtrim('a  ') AS d, trim('') AS e;
SELECT trim('xyx','xy') AS a, ltrim('xay','x') AS b, rtrim('ayx','x') AS c, typeof(trim(123)) AS d;
SELECT replace('aaa','a','b') AS a, replace('abc','z','y') AS b, replace('ab','ab','') AS c, replace('ab','','X') AS d;
SELECT replace(NULL,'a','b') AS a, replace('a',NULL,'b') AS b, replace('a','a',NULL) AS c;
### 5b substr, where the index conventions hide errors
-- A negative start counts back from the end; a start of 0 is treated as 1; a
-- negative length runs to just before the start; and a start past the end gives
-- the empty string rather than an error. All four are conventions rather than
-- arithmetic, so an implementation that computes them directly gets some right
-- by luck.
SELECT substr('abcdef',2,3) AS a, substr('abcdef',-2,3) AS b, substr('abcdef',2) AS c, substr('abcdef',0,3) AS d;
SELECT substr('abcdef',-10,3) AS a, substr('abcdef',2,-1) AS b, substr('abcdef',-3,-1) AS c, substr('abcdef',7,1) AS d;
SELECT substr('',1,1) AS a, substr('abc',1,0) AS b, substr('abc',1,-5) AS c, substr(NULL,1,1) AS d;
SELECT substr('abcdef',1,1) AS a, substr('abcdef',2,0) AS b, substr('abcdef',0,0) AS c, typeof(substr(123,1,1)) AS d;
SELECT instr('abcabc','c') AS a, instr('abc','z') AS b, instr(NULL,'a') AS c, instr('a','') AS d;
SELECT instr('abc','a') AS a, instr('a','a') AS b, instr('','a') AS c, typeof(instr(123,'2')) AS d;
### 5c the type boundary a function coerces to
SELECT abs(-0) AS a, abs(-0.0) AS b, abs(-1.5) AS c, abs('3') AS d, abs(NULL) AS e, abs('abc') AS f;
SELECT typeof(abs(-0)) AS a, typeof(abs(-0.0)) AS b, typeof(abs(-2)) AS c, typeof(abs('3')) AS d;
SELECT length(123) AS a, typeof(length(123)) AS b, upper(123) AS c, typeof(upper(123)) AS d;
-- abs() of a value too large for an i64 is an ERROR in the real engine, not a
-- real: SQLite's abs() is defined only over integers that fit. Included so the
-- refusal is compared, and because returning a real here would be exactly the
-- silent wrong answer this hunt is for. Its own statement, so the section
-- continues when it is refused.
SELECT abs(-9223372036854775807-1) AS a;
### 5d round, and the precision argument
-- A negative precision is legal and rounds to tens, hundreds and thousands. An
-- implementation that rejects a negative precision reports an ERROR where the
-- real engine returns a value, which is a loud failure -- but it is a refusal
-- of a valid statement, and it is the kind that a test using only
-- round(x, 2) never reaches.
SELECT round(2.5) AS a, round(-2.5) AS b, round(2.4) AS c, round(0.5) AS d, round(1.0) AS e;
SELECT round(2.4567,2) AS a, round(-2.4567,2) AS b, round(2.5,0) AS c, round(2.55,1) AS d;
SELECT round(123.456,-1) AS a, round(25,-1) AS b, round(15,-1) AS c, round(125,-2) AS d;
SELECT round('2.5',1) AS a, typeof(round('2.5',1)) AS b, round(NULL,1) AS c, round(1,NULL) AS d;
SELECT typeof(round(2.0)) AS a, typeof(round(2)) AS b, typeof(round(2.5,0)) AS c, typeof(round(2.5,1)) AS d;
### 5e coalesce, ifnull, nullif and the CASE forms
SELECT coalesce(NULL,NULL,3) AS a, coalesce(NULL,NULL) AS b, ifnull(NULL,'d') AS c, nullif(1,1) AS d;
SELECT nullif(1,2) AS a, nullif('a','a') AS b, nullif(NULL,NULL) AS c, typeof(nullif(1,2)) AS d;
SELECT iif(1,2,3) AS a, iif(0,2,3) AS b, iif(NULL,2,3) AS c, typeof(iif(1,2,3)) AS d;
### 5f the functions this engine does not implement
-- printf is a scalar function in the real engine, not a PRAGMA. If it is
-- missing here this is reported as a one-sided refusal, which is a real
-- finding: a scalar function the real engine has and this one does not. The
-- statement is split so the rest of the section still runs if it is refused.
SELECT printf('%d',5) AS a;
SELECT coalesce(1,2) AS sanity;

-- ===========================================================================
-- 6. Multi-row statements: an error part way through must be visible
-- ===========================================================================
--
-- This is the section the first runner cannot have. A runner that sends one
-- statement per invocation and reads only the final state sees the state both
-- engines agree on after a correctly rolled-back statement, so a statement that
-- applied the rows it should have refused is invisible to it. Here each
-- multi-row INSERT is one statement and the table is read back on the next
-- line, so a partly applied insert is reported against the state it left.
--
-- The correct behaviour for all three cases is the same: refuse the statement
-- and leave the table EMPTY, because SQLite's multi-row VALUES insert runs
-- inside one implicit transaction and rolls back as a unit. An engine that
-- applies the first and third rows and refuses the second has produced a wrong
-- answer that no final-state comparison would ever reach.
### 6a NOT NULL failing on the second row
CREATE TABLE nn(a INT NOT NULL, b INT);
INSERT INTO nn VALUES(1,1),(NULL,2),(3,3);
SELECT a, b FROM nn;
SELECT count(*) AS rows_left, sum(a) AS total FROM nn;
### 6b UNIQUE failing on the second row
CREATE TABLE uq(a INT, b INT UNIQUE);
INSERT INTO uq VALUES(1,1),(2,2),(3,2);
SELECT a, b FROM uq;
SELECT count(*) AS rows_left FROM uq;
### 6c CHECK failing on the first row
CREATE TABLE ck(a INT, b INT CHECK(b>0));
INSERT INTO ck VALUES(1,-1),(2,2);
SELECT a, b FROM ck;
### 6d CHECK failing on the second row
CREATE TABLE ck2(a INT, b INT CHECK(b>0));
INSERT INTO ck2 VALUES(1,1),(2,-2),(3,3);
SELECT a, b FROM ck2;
### 6e a multi-row insert that should succeed entirely
CREATE TABLE ok(a, b);
INSERT INTO ok VALUES(1,1),(2,2),(3,3),(4,4),(5,5);
SELECT a, b FROM ok;
SELECT count(*) AS c, sum(a) AS s, avg(a) AS av FROM ok;
### 6f the row count of a multi-row insert, then the state
CREATE TABLE cnt(a);
INSERT INTO cnt VALUES(1),(2),(3),(4),(5),(6),(7);
SELECT count(*) AS c FROM cnt;
### 6g a multi-row insert where the failure is in the VALUES list itself
CREATE TABLE vt(a NOT NULL, b);
INSERT INTO vt VALUES(1,1),(2);
SELECT a, b FROM vt;
### 6h a multi-row insert into a table that does not exist
INSERT INTO no_such_table_at_all VALUES(1),(2);
### 6i a multi-row insert after a failing one, to show the table is still usable
INSERT INTO ok VALUES(6,6),(7,7);
SELECT a, b FROM ok;
SELECT count(*) AS c FROM ok;
### 6j a multi-row UPDATE that should be checked against the same rule
CREATE TABLE up(a INT, b INT);
INSERT INTO up VALUES(1,1),(2,2),(3,3);
UPDATE up SET b=0;
SELECT a, b FROM up;
UPDATE up SET a=a*10;
SELECT a, b FROM up;
UPDATE up SET a=a/10 WHERE a>0;
SELECT a, b FROM up;
### 6k a multi-row DELETE, and the state it leaves
-- This uses its own table name rather than 6j's. 6j is expected to disagree --
-- the UPDATE that sets a=0 on every row breaks its UNIQUE constraint -- and the
-- runner rolls the pair of databases back to the state before the offending
-- statement, so 6j's rows are still there when 6k starts. A 6k that reused
-- `up` would then be comparing a DELETE against a table that both engines have
-- half of, and the report would be about the rollback rather than about DELETE.
--
-- That rollback is what makes each disagreement attributable to its own
-- statement, and the cost is that a section cannot depend on a table an earlier
-- disagreeing section populated. So nothing here is shared.
CREATE TABLE del_t(a INT, b INT);
INSERT INTO del_t VALUES(1,1),(2,2),(3,3);
DELETE FROM del_t WHERE a=2;
SELECT a, b FROM del_t;
DELETE FROM del_t;
SELECT count(*) AS c FROM del_t;
SELECT a, b FROM del_t;

-- ===========================================================================
-- 7. PRAGMA output, the schema, and what the file looks like afterwards
-- ===========================================================================
--
-- The pragma table-valued functions are written as `SELECT * FROM
-- pragma_xxx(...)` rather than as `PRAGMA xxx`, because this engine's parser
-- accepts the table-valued form and refuses the statement form with a syntax
-- error -- verified: `PRAGMA table_xinfo(s)` is `near "(": syntax error` here.
-- That is a parse gap, not a semantic one, and it is reported as such in the
-- findings rather than papered over. The table-valued form is the one that
-- composes with the rest of the query grammar, so it is what the corpus uses.
### 7a table_info: the declared type, the constraints, and the default
CREATE TABLE pi(a INTEGER PRIMARY KEY, b TEXT NOT NULL DEFAULT 'd', c REAL, d INT DEFAULT 5, e);
SELECT cid, name, type, "notnull", dflt_value, pk FROM pragma_table_info('pi');
SELECT cid, name, type, "notnull", dflt_value, pk FROM pragma_table_info('pi');
### 7b the declared type is reported as written, case and all
-- A declared type is TEXT, not a rule, and it comes back exactly as it was
-- spelled. Upper-casing it, or normalising a quoted type name by keeping the
-- quotes, changes what a client sees and what a later CREATE would copy.
CREATE TABLE dt(a InTeGeR, b VarChar(20), c 'MyType', d DECIMAL(10,2), e BOOLEAN);
SELECT cid, name, type, "notnull", dflt_value, pk FROM pragma_table_info('dt');
SELECT name, type FROM pragma_table_info('dt') ORDER BY cid;
### 7c index_list and index_info
CREATE TABLE ix(a INTEGER, b TEXT, c REAL);
CREATE INDEX ix_c ON ix(c);
CREATE UNIQUE INDEX ix_b ON ix(b);
SELECT name, "unique", origin, partial FROM pragma_index_list('ix');
SELECT seqno, cid, name FROM pragma_index_info('ix_c');
SELECT seqno, cid, name FROM pragma_index_info('ix_b');
### 7d the schema table, which is what a client reads to rebuild the database
SELECT type, name, tbl_name FROM sqlite_schema ORDER BY type, name;
SELECT type, name, sql FROM sqlite_schema WHERE type='table';
SELECT count(*) AS n_tables FROM sqlite_schema WHERE type='table';
SELECT count(*) AS n_indexes FROM sqlite_schema WHERE type='index';
### 7e the table-valued list form, and a table that does not exist
SELECT name FROM pragma_table_list;
SELECT count(*) AS n FROM pragma_table_list;
SELECT count(*) AS n FROM pragma_table_info('no_such_table');
### 7f the pragmas that are a value rather than a table
PRAGMA page_size;
PRAGMA encoding;
PRAGMA journal_mode;
PRAGMA user_version;
PRAGMA schema_version;
PRAGMA foreign_keys;
PRAGMA integrity_check;
PRAGMA application_id;
### 7g user_version is writable and is read back
PRAGMA user_version=7;
PRAGMA user_version;
PRAGMA user_version=0;
PRAGMA user_version;
### 7h a table read back after it was written, on a fresh connection
-- The file was written by the statements above and this re-opens it, so a
-- schema that was never flushed, or a value that was not written through, is
-- visible here rather than only in the same session.
SELECT name, type FROM pragma_table_info('pi');
SELECT name FROM pragma_table_list;
SELECT type, name FROM sqlite_schema ORDER BY type, name;

-- ===========================================================================
-- 8. Cross-engine interop: what the file looks like to the OTHER engine
-- ===========================================================================
--
-- A section whose name begins with `interop` is a round trip rather than a
-- sequence. Both engines write their OWN copy of the same body, and every
-- statement marked `> ` is then run by sqlite3 against the file nsqlited wrote
-- and by nsqlited against the file sqlite3 wrote. The two answers are compared.
--
-- This is the only section that can see a defect where an engine agrees with
-- ITSELF about the file it produced and the other engine cannot see it at all.
-- Comparing each engine against its own database misses that completely: both
-- would report their own index, and the file on disk would be wrong.
--
-- The read-backs are the ones that matter. A schema entry that was never
-- flushed, a declared type stored with its quotes still on it, and a value
-- written through as something other than what was stored are all invisible
-- from inside the writing session and visible from the other engine.
### interop 8a a table written by each engine, read by the other
CREATE TABLE w(a INTEGER, b TEXT, c REAL);
INSERT INTO w VALUES(1,'x',1.5),(2,'y',2.5),(NULL,NULL,NULL);
> SELECT a, b, c FROM w ORDER BY a;
> SELECT count(*) AS n, sum(a) AS s, typeof(sum(a)) AS t FROM w;
> SELECT type, name, tbl_name FROM sqlite_schema ORDER BY type, name;
> SELECT name FROM pragma_table_info('w');
> SELECT cid, name, type, "notnull", dflt_value, pk FROM pragma_table_info('w');
### interop 8b the declared type survives the round trip
CREATE TABLE d(a InTeGeR, b VarChar(20), c 'MyType', d DECIMAL(10,2), e BOOLEAN);
> SELECT cid, name, type FROM pragma_table_info('d') ORDER BY cid;
> SELECT sql FROM sqlite_schema WHERE name='d';
### interop 8c an index written by one engine, seen by the other
CREATE TABLE i(a INTEGER, b TEXT);
INSERT INTO i VALUES(1,'p'),(2,'q');
CREATE INDEX i_a ON i(a);
CREATE UNIQUE INDEX i_b ON i(b);
> SELECT name, "unique", origin, partial FROM pragma_index_list('i');
> SELECT seqno, cid, name FROM pragma_index_info('i_a');
> SELECT type, name, tbl_name, sql FROM sqlite_schema ORDER BY type, name;
> SELECT a, b FROM i ORDER BY a;
### interop 8d a constraint written by one engine, enforced by the other
CREATE TABLE k(a INTEGER PRIMARY KEY, b TEXT NOT NULL, c INT CHECK(c>0));
INSERT INTO k VALUES(1,'p',1);
> INSERT INTO k VALUES(1,'q',1);
> INSERT INTO k VALUES(2,NULL,1);
> INSERT INTO k VALUES(3,'r',-1);
> SELECT a, b, c FROM k ORDER BY a;
### interop 8e every storage class survives the round trip byte for byte
CREATE TABLE v(i INTEGER, r REAL, s TEXT, b BLOB, n NUMERIC);
INSERT INTO v VALUES(1, 1.0, '1', x'31', 1);
INSERT INTO v VALUES(-1, -1.5, '', x'', -1.5);
INSERT INTO v VALUES(0, 0.0, 'a b|c
d', x'00FF', 0.5);
INSERT INTO v VALUES(NULL, NULL, NULL, NULL, NULL);
> SELECT i, typeof(i) AS ti, r, typeof(r) AS tr, s, typeof(s) AS ts, b, typeof(b) AS tb, n, typeof(n) AS tn FROM v;
> SELECT quote(i) AS qi, quote(r) AS qr, quote(s) AS qs, quote(b) AS qb, quote(n) AS qn FROM v;
> SELECT count(*) AS n FROM v;
> SELECT count(DISTINCT i) AS n FROM v;
### interop 8f a view and a user_version written by one engine, read by the other
CREATE TABLE base(a, b);
INSERT INTO base VALUES(1,10),(2,20);
CREATE VIEW bv AS SELECT a, b*2 AS dbl FROM base;
PRAGMA user_version=42;
> SELECT a, dbl FROM bv ORDER BY a;
> SELECT type, name FROM sqlite_schema ORDER BY type, name;
> PRAGMA user_version;
> SELECT page_size, encoding FROM pragma_page_size(), pragma_encoding();
### interop 8g the schema text a client would read back to rebuild the table
CREATE TABLE full(a INTEGER PRIMARY KEY, b TEXT NOT NULL DEFAULT 'z', c REAL DEFAULT 1.5, d INT, CHECK(d>=0));
> SELECT sql FROM sqlite_schema WHERE name='full';
> SELECT cid, name, type, "notnull", dflt_value, pk FROM pragma_table_info('full');
> SELECT count(*) AS n FROM sqlite_schema WHERE type='table';
### interop 8h a table emptied, then read back by the other engine
CREATE TABLE e(a, b);
INSERT INTO e VALUES(1,'x'),(2,'y');
DELETE FROM e;
> SELECT count(*) AS n FROM e;
> SELECT type, name FROM sqlite_schema ORDER BY type, name;
> SELECT a, b FROM e;

-- ===========================================================================
-- 9. Cases added after the first run found the patterns below
-- ===========================================================================
--
-- Each of these was found by a disagreement in the sections above and then
-- reduced by hand to the smallest statement that still shows it. They are
-- gathered here so the reduction is part of the corpus rather than a thing that
-- happened once, and so a fix that breaks one of them is caught immediately.
### 9a an aggregate whose alias is the name of its own argument
-- `sum(a) AS a` is legal SQL: the alias names the result column and the `a`
-- inside the call names the column being summed. This engine reads the alias as
-- a reference back to the aggregate and refuses the statement, so a query
-- written in the ordinary way -- aggregate, named after what it aggregates over
-- -- cannot be run at all. It is a parse bug rather than a wrong value, and it
-- is the kind that a test using a distinct alias never reaches.
CREATE TABLE alias_ag(a, b);
INSERT INTO alias_ag VALUES(1,2),(3,4);
SELECT sum(a) AS a FROM alias_ag;
SELECT count(DISTINCT a) AS a FROM alias_ag;
SELECT max(a) AS a FROM alias_ag;
SELECT avg(a) AS a, sum(b) AS b FROM alias_ag;
### 9b the empty-set results restated one aggregate at a time
-- The same facts as 3a, but with no other aggregate in the statement, so a
-- failure in one of them is not masked by a passing one beside it.
CREATE TABLE e9(a);
SELECT count(DISTINCT a) FROM e9;
SELECT group_concat(a) FROM e9;
SELECT sum(a) FROM e9;
SELECT total(a) FROM e9;
SELECT avg(a) FROM e9;
SELECT min(a) FROM e9;
SELECT max(a) FROM e9;
### 9c sum and avg over values that are not all numbers
-- A non-numeric value is treated as 0 by sum, so the total here is 1+2+0+3.5+0
-- = 6.5, and the average divides by the count of NON-NULL rows including the
-- one that contributed nothing. A SUM that skips non-numeric values, or an AVG
-- that divides by the count of the ones that were, is right on the total and
-- wrong on the average -- or the reverse.
CREATE TABLE nonnum(v);
INSERT INTO nonnum VALUES(1),('2'),('abc'),(NULL),(3.5),(x'31');
SELECT sum(v) AS s, avg(v) AS av, count(v) AS c, count(*) AS c2 FROM nonnum;
SELECT sum(v) AS s, total(v) AS t, avg(v) AS av FROM nonnum;
SELECT typeof(sum(v)) AS ts, typeof(avg(v)) AS ta, typeof(total(v)) AS tt FROM nonnum;
### 9d the minimum and maximum of mixed classes
-- Ordering across storage classes is NOT numeric: NULL is smallest, then
-- numbers, then text, then blobs. With 1, '2', 'abc', NULL, 3.5 and x'31' the
-- minimum is NULL and the maximum is the blob, not the 3.5.
SELECT min(v) AS mn, max(v) AS mx, typeof(min(v)) AS t1, typeof(max(v)) AS t2 FROM nonnum;
SELECT min(v) AS mn FROM nonnum WHERE v IS NOT NULL;
SELECT max(v) AS mx FROM nonnum WHERE v IS NOT NULL;
### 9e count(DISTINCT) over a column holding NULL and equal values
-- count(DISTINCT a) counts the DISTINCT non-NULL values, so NULL contributes
-- nothing however many rows hold it. Here a is 1,1,1,NULL,NULL: one distinct
-- value. b is 1,1,'1',NULL,NULL: two, because 1 and '1' are equal under
-- comparison but the count is over distinct VALUES, so a text and an integer
-- that compare equal are one group.
CREATE TABLE cd(a, b);
INSERT INTO cd VALUES(1,1),(1,1),(1,'1'),(NULL,NULL),(NULL,NULL);
SELECT count(DISTINCT a) AS c1, count(DISTINCT b) AS c2, count(*) AS c3 FROM cd;
SELECT DISTINCT a, typeof(a) AS t FROM cd;

-- ===========================================================================
-- 10. Reductions of the findings in section 9, kept as the smallest statements
-- ===========================================================================
--
-- Every case here is the smallest statement that still shows the disagreement
-- above it, verified by hand against the real engine. Keeping the reduction in
-- the corpus means the reduction itself is re-checked: a fix that makes the
-- large case pass while breaking the small one shows up here, and the roadmap
-- entry stays honest about the size of the bug.
### 10a a comparison operator cannot be the left operand of another
-- SQLite has no chained comparison. `1=1=1` is `(1=1)=1` -- the comparison
-- yields the integer 1, and that 1 is compared with 1 -- so the answer is 1,
-- and 1<2<3 is `(1<2)<3` = `1<3` = 1. An implementation that reads the chain as
-- a conjunction happens to agree, which is why 1<2<3 is not the discriminating
-- case; `1=2=1` is, because a conjunction would read it as `1=2 AND 2=1` =
-- false and the real reading `(1=2)=1` is `0=1` = false too. They agree there
-- as well, so the point is the PARSE, not a value: this engine refuses the
-- statement, and a parse gap is a missing capability whatever the value is.
--
-- The discriminating case for a wrong VALUE is `1 IN (1,2)=1`: SQLite parses it
-- as `(1 IN (1,2)) = 1`, which is `1 = 1` = 1, while a chain-as-conjunction
-- reading has no sensible parse at all.
SELECT 1=1=1;
SELECT 1<2<3;
SELECT 1=2=1;
SELECT 1 IN (1,2)=1;
SELECT (1=1)=1;
SELECT 1=1 IS 1;
### 10b min and max over mixed storage classes
-- Ordering across classes is NULL, then numbers, then text, then blobs, so
-- over these six values max is the blob and min is the integer 1. A max that
-- compares only within the first class it saw, or that orders text by numeric
-- coercion, returns a different value -- and returns it plausibly.
CREATE TABLE mx(v);
INSERT INTO mx VALUES(1),('2'),('abc'),(NULL),(3.5),(x'31');
SELECT max(v) AS mx, min(v) AS mn FROM mx;
SELECT typeof(max(v)) AS t1, typeof(min(v)) AS t2 FROM mx;
SELECT v FROM mx ORDER BY v;
SELECT quote(max(v)) AS q1, quote(min(v)) AS q2 FROM mx;
### 10c trim and replace with a set or an empty string
-- trim's second argument is a SET of characters to remove, not a prefix or a
-- substring: trim('xyx','xy') is empty, not 'x'. And replace with an empty
-- search string returns the subject unchanged, where a loop that replaces the
-- empty string between every pair of characters would double it.
SELECT trim('xyx','xy') AS a, ltrim('xay','x') AS b, rtrim('ayx','x') AS c;
SELECT replace('ab','ab','') AS a, replace('ab','','X') AS b, replace('abc','b','') AS c;
SELECT replace(NULL,'a','b') AS a, replace('a',NULL,'b') AS b, replace('a','a',NULL) AS c;
### 10d the multi-row insert state, restated as a count
-- The disagreement in 6b and 6c is on the STATEMENT; these are on the STATE it
-- leaves. Both engines should refuse and leave the table empty. An engine that
-- refuses nothing and keeps every row is wrong in a way no final-state read of
-- a correctly rolled-back statement would ever reach.
CREATE TABLE st(a INT, b INT UNIQUE);
INSERT INTO st VALUES(1,1),(2,2),(3,2);
SELECT count(*) AS n FROM st;
SELECT a, b FROM st;
CREATE TABLE st2(a INT, b INT CHECK(b>0));
INSERT INTO st2 VALUES(1,-1),(2,2);
SELECT count(*) AS n FROM st2;
SELECT a, b FROM st2;
### 10e a CAST that changes the storage class
-- CAST(blob AS TEXT) yields TEXT holding the bytes, not a blob still labelled
-- text: a client that reads the value back with typeof gets a different answer
-- depending on which engine wrote the row.
SELECT CAST(x'414243' AS TEXT) AS a, typeof(CAST(x'414243' AS TEXT)) AS b;
SELECT CAST(0 AS BLOB) AS a, typeof(CAST(0 AS BLOB)) AS b;
SELECT quote(CAST(x'414243' AS TEXT)) AS q;
