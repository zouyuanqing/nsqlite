# Second differential corpus: the shapes a wrong answer hides in.
#
# The first corpus (tools/difftest2-cases.sql) covers expression grammar,
# affinity, aggregates, ORDER BY, LIMIT, the functions, multi-row statements and
# the schema. This one is a SECOND, independent set over the same ground,
# written to be run by the same runner, and it exists because the runner is
# only as good as the questions it is asked. Every section below is aimed at a
# shape where the two engines can return the same number of rows in the same
# order with a different VALUE, which is the only kind of defect nothing else
# finds.
#
# RULES THE RUNNER ENFORCES, restated here because a corpus that breaks them
# produces reports that are about the corpus:
#
#   * one statement per `;`, and a section runs as a session against a pair of
#     fresh databases;
#   * NO string literal may contain a newline. The runner joins a statement
#     that spans lines with a space, so `SELECT 'a<newline>b'` would be sent to
#     the engines as `SELECT 'a b'` -- two different questions, one answer.
#     Where a newline is the point, use char(10), which the engines agree on;
#   * no statement depends on a statement that was expected to fail, because a
#     statement that disagreed puts the databases back;
#   * a value that must be told apart from another by its storage class is
#     compared through the engine's own quote(), so the class is part of the
#     comparison rather than something the runner reconstructs.
#
#   bash tools/difftest2.sh tools/gen2.sql
#   bash tools/difftest2.sh tools/gen2.sql --list
#   bash tools/difftest2.sh tools/gen2.sql --section 5

### a arithmetic: the five operators, pairings with and without parens
CREATE TABLE n(a,b);
INSERT INTO n VALUES(7,3);
SELECT a+b, a-b, a*b, a/b, a%b FROM n;
SELECT (a+b)*2, a+b*2, 2*(a+b), (a-b)/2, a-(b-2) FROM n;
SELECT a/b, a%b, -a/b, -a%b, a/-b FROM n;
SELECT 2+3*4-6/2, (2+3)*(4-6)/2, 2*3%4+5 FROM n;
SELECT -a, -(-a), - -a, -(a) FROM n;
SELECT -9223372036854775807-1, -9223372036854775807-2 FROM n;
SELECT 9223372036854775807+1, 3000000000*3000000000 FROM n;
SELECT -'3', -'3.5', -x'31', -NULL FROM n;
SELECT a, b FROM n;
### b string and numeric mixing, and the concat precedence
SELECT 'a'||1, 1||2, 'a'||'b'||'c', 2||'x'||3;
SELECT 'a'+1, '1'+1, '1.5'+1, '1e2'+0, ' 1 '+1, 'abc'+1, ''+1;
SELECT 1||'a'+1, 1+1||'a';
SELECT 'a'||NULL, NULL||'a', length('a'||NULL);
SELECT '5'+'5', '5'||'5', '5'+5, '5'||5;
### c comparison chains and cross-class comparison
SELECT 1=1, 1=2, 1<2, 2<=2, 3>4, 3>=3, 1<>2, 1==1;
SELECT 1=1=1, 1<2<3, 1=2=1, 2>1>0;
SELECT '1'=1, 1='1', '1.0'=1.0, ' 1 '=1, '1e2'=100, 'abc'='abc';
SELECT NULL=NULL, NULL<>NULL, NULL<1, 1<NULL, NULL IS NULL, NULL IS NOT NULL;
SELECT 1<2 AND 2<3, 1<2 AND 2<1, 1>2 OR 2>1, 1>2 OR 2<1;
### d three-valued logic, NULL on either side
SELECT 0 AND NULL, 1 AND NULL, NULL AND 1, NULL AND 0, NULL AND NULL;
SELECT 0 OR NULL, 1 OR NULL, NULL OR 1, NULL OR 0, NULL OR NULL;
SELECT NOT NULL, NOT 0, NOT 1, NOT (NULL AND 1), NOT (NULL OR 0);
SELECT 1 AND 0, 1 OR 0, 0 OR 1, 0 AND 1, NOT (1 AND 0);
SELECT NULL BETWEEN 1 AND 2, 1 BETWEEN 0 AND 2, 5 BETWEEN NULL AND 2;
SELECT NULL NOT BETWEEN 1 AND 2, 1 NOT BETWEEN 0 AND 2;
SELECT NULL IN (1,2), 1 IN (1,2), 3 IN (1,2), NULL NOT IN (1,2);
### e type and affinity grid: the same values into every affinity
CREATE TABLE g(ia INTEGER, ta TEXT, na NUMERIC, ra REAL, ba BLOB, ua);
INSERT INTO g VALUES(1,1,1,1,1,1);
INSERT INTO g VALUES(-1,-1,-1,-1,-1,-1);
INSERT INTO g VALUES(0,0,0,0,0,0);
INSERT INTO g VALUES(1.0,1.0,1.0,1.0,1.0,1.0);
INSERT INTO g VALUES(-1.5,-1.5,-1.5,-1.5,-1.5,-1.5);
INSERT INTO g VALUES('abc','abc','abc','abc','abc','abc');
INSERT INTO g VALUES('','','','','','');
INSERT INTO g VALUES('123abc','123abc','123abc','123abc','123abc','123abc');
INSERT INTO g VALUES('  12  ','  12  ','  12  ','  12  ','  12  ','  12  ');
INSERT INTO g VALUES('1e3','1e3','1e3','1e3','1e3','1e3');
INSERT INTO g VALUES('0x10','0x10','0x10','0x10','0x10','0x10');
INSERT INTO g VALUES(9223372036854775807,9223372036854775807,9223372036854775807,9223372036854775807,9223372036854775807,9223372036854775807);
INSERT INTO g VALUES(9223372036854775808,9223372036854775808,9223372036854775808,9223372036854775808,9223372036854775808,9223372036854775808);
INSERT INTO g VALUES(NULL,NULL,NULL,NULL,NULL,NULL);
INSERT INTO g VALUES(x'414243',x'414243',x'414243',x'414243',x'414243',x'414243');
SELECT typeof(ia), ia, typeof(ta), ta, typeof(na), na, typeof(ra), ra, typeof(ba), ba, typeof(ua), ua FROM g;
SELECT count(*) FROM g;
### f the declared types that carry an affinity rule rather than a spelling
CREATE TABLE ty(i INT, ii INTEGER, t TEXT, c CHAR, v VARCHAR(10), cl CLOB, n NUMERIC, d DECIMAL(10,2), r REAL, dbl DOUBLE, b BLOB, iv INT4, i8 INT8);
INSERT INTO ty VALUES(1,1,1,1,1,1,1,1,1,1,1,1,1);
INSERT INTO ty VALUES(1.0,1.0,1.0,1.0,1.0,1.0,1.0,1.0,1.0,1.0,1.0,1.0,1.0);
INSERT INTO ty VALUES('abc','abc','abc','abc','abc','abc','abc','abc','abc','abc','abc','abc','abc');
INSERT INTO ty VALUES(NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL);
SELECT typeof(i), typeof(ii), typeof(t), typeof(c), typeof(v), typeof(cl), typeof(n), typeof(d), typeof(r), typeof(dbl), typeof(b), typeof(iv), typeof(i8) FROM ty;
SELECT typeof(1.0), typeof('1'), typeof(x'31'), typeof(NULL);
### g affinity applied in a COMPARISON against a column
CREATE TABLE c1(a INTEGER, b TEXT, n NUMERIC, r REAL);
INSERT INTO c1 VALUES('5','5','5','5');
SELECT a='5', b='5', n='5', r='5' FROM c1;
SELECT a=5, b=5, n=5, r=5 FROM c1;
SELECT a='5.0', b='5.0', n='5.0', r='5.0' FROM c1;
SELECT a=' 5 ', b=' 5 ', n=' 5 ', r=' 5 ' FROM c1;
SELECT a='abc', b='abc' FROM c1;
SELECT typeof(a), typeof(b), typeof(n), typeof(r) FROM c1;
SELECT '5'=1, '1.0'=1.0, ' 1 '=1, '1e2'=100;
### h a real stored in a TEXT column: the rendered string IS the value
CREATE TABLE txt(t TEXT);
INSERT INTO txt VALUES(1.0);
INSERT INTO txt VALUES(-1.0);
INSERT INTO txt VALUES(0.0);
INSERT INTO txt VALUES(1e15);
INSERT INTO txt VALUES(1e16);
INSERT INTO txt VALUES(1e17);
INSERT INTO txt VALUES(1e-5);
INSERT INTO txt VALUES(1e300);
INSERT INTO txt VALUES(0.1);
SELECT t, length(t), typeof(t) FROM txt;
SELECT t='1', t='1.0', length(t)=1 FROM txt;
SELECT count(*) FROM txt;
### i aggregates over an EMPTY set: the result differs per aggregate
CREATE TABLE e0(v);
SELECT count(v), count(*), sum(v), total(v), avg(v), min(v), max(v), group_concat(v) FROM e0;
SELECT typeof(sum(v)), typeof(total(v)), typeof(avg(v)), typeof(min(v)), typeof(max(v)) FROM e0;
### j aggregates over a group, and over values that are not all numbers
CREATE TABLE m(v);
INSERT INTO m VALUES(1),('2'),('abc'),(NULL),(3.5),(x'31');
SELECT sum(v), total(v), avg(v), count(v), count(*), min(v), max(v) FROM m;
CREATE TABLE g2(k, v);
INSERT INTO g2 VALUES('a',1),('a',2),('b',10),('b',20),('c',NULL);
SELECT k, count(*), sum(v), avg(v), min(v), max(v) FROM g2 GROUP BY k;
SELECT k, count(*) FROM g2 GROUP BY k HAVING count(*)>1;
SELECT k, count(*) FROM g2 GROUP BY k HAVING sum(v) IS NULL;
SELECT k FROM g2 GROUP BY k ORDER BY k;
SELECT count(DISTINCT k), count(DISTINCT v) FROM g2;
### k DISTINCT, where the storage class decides which rows survive
CREATE TABLE d(v);
INSERT INTO d VALUES(1),(1.0),('1'),(NULL),(NULL),(x'31'),(2);
SELECT DISTINCT v FROM d;
SELECT count(DISTINCT v) FROM d;
SELECT DISTINCT v FROM d ORDER BY v;
SELECT DISTINCT v+0 FROM d;
SELECT DISTINCT typeof(v), v FROM d;
### l ORDER BY: ordinals, aliases, expressions, mixed directions, NULL placement
CREATE TABLE ord(a,b,v);
INSERT INTO ord VALUES(1,'x',NULL),(2,'y',1),(3,'x',2.5),(4,NULL,3),(5,'y',-1);
SELECT a FROM ord ORDER BY 1;
SELECT a, b AS k FROM ord ORDER BY k;
SELECT a+b AS s FROM ord ORDER BY s DESC;
SELECT a, b FROM ord ORDER BY a DESC, b ASC;
SELECT a, b FROM ord ORDER BY b, a;
SELECT a, b, v FROM ord ORDER BY v ASC;
SELECT a, b, v FROM ord ORDER BY v DESC;
SELECT a, b FROM ord ORDER BY b IS NULL, a;
SELECT a, b FROM ord ORDER BY b IS NULL DESC, a;
### m LIMIT with OFFSET, the comma form, and the shapes that are refused
SELECT a FROM ord ORDER BY a LIMIT 2;
SELECT a FROM ord ORDER BY a LIMIT 2 OFFSET 1;
SELECT a FROM ord ORDER BY a LIMIT 1, 2;
SELECT a FROM ord ORDER BY a LIMIT 0;
SELECT a FROM ord ORDER BY a LIMIT 100 OFFSET 3;
SELECT a FROM ord ORDER BY a LIMIT -1 OFFSET 2;
SELECT a FROM ord ORDER BY a LIMIT 2 OFFSET 99;
SELECT a FROM ord ORDER BY a LIMIT '2';
SELECT a FROM ord ORDER BY a LIMIT NULL;
SELECT a FROM ord ORDER BY a LIMIT 1.7;
SELECT a FROM ord ORDER BY a LIMIT 'x';
SELECT count(*) FROM ord;
### n text functions, each against empty, NULL, negative and boundary input
SELECT length(''), length(NULL), length('abc'), length('a b'), length(123), length(1.5);
SELECT length(CAST(x'414243' AS TEXT)), length('a');
SELECT upper(''), upper(NULL), upper('abc'), upper('a1!');
SELECT lower(''), lower(NULL), lower('ABC'), lower('A1!');
SELECT trim('  a  '), trim('a'), trim(''), trim(NULL), trim(' a b ');
SELECT trim('xxaxx', 'x'), trim('xax', 'xa');
SELECT substr('abcdef', 2), substr('abcdef', 2, 3), substr('abcdef', -1, 3);
SELECT substr('abcdef', 0), substr('abcdef', 7), substr('abcdef', 2, 0);
SELECT substr('abcdef', 2, 100), substr('abcdef', -100, 3);
SELECT replace('aaaa', 'a', 'b'), replace('abc', 'z', 'y');
SELECT instr('abcdef', 'cd'), instr('abcdef', 'zz'), instr('abc', '');
SELECT hex(1), hex('a'), hex(x'414243'), hex(NULL), hex('');
SELECT quote('a'), quote(''), quote(NULL), quote(1), quote(1.0), quote(x'414243');
SELECT typeof(''), typeof('1'), typeof(1), typeof(1.0), typeof(NULL), typeof(x'31');
### o math functions, the precision argument, and the boundaries
SELECT abs(1), abs(-1), abs(1.5), abs(-1.5), abs(NULL);
SELECT abs(-9223372036854775808);
SELECT round(1.4), round(1.5), round(2.5), round(-1.5), round(1.45, 1), round(1.45, 0);
SELECT round(1.0, 2), round(1.5, -1), round(2.675, 2), round(NULL), round(1, NULL);
SELECT max(1,2), max('a','b'), max(NULL, 1), max(1, NULL), min(1,2), min(NULL,1);
SELECT coalesce(NULL, NULL, 3), ifnull(NULL, 4), ifnull('a', 4), nullif(1,1), nullif(1,2);
SELECT CASE WHEN 1 THEN 'a' ELSE 'b' END, CASE WHEN 0 THEN 'a' END, CASE 1 WHEN 1 THEN 'x' END;
### p multi-row statements, and the state they leave part way through
CREATE TABLE mr(a INTEGER, b INTEGER);
INSERT INTO mr VALUES(1,1),(2,2),(3,3);
SELECT count(*), sum(a) FROM mr;
INSERT INTO mr VALUES(4,4),(5,1);
SELECT count(*), sum(a) FROM mr;
INSERT INTO mr VALUES(6,NULL),(7,7);
SELECT count(*), count(b) FROM mr;
UPDATE mr SET b=1 WHERE a>0;
SELECT count(*), sum(b) FROM mr;
DELETE FROM mr WHERE a>2;
SELECT count(*) FROM mr;
INSERT INTO mr SELECT a+10, b FROM mr;
SELECT count(*), sum(a) FROM mr;
### q the write statements one at a time, and the state each one leaves
CREATE TABLE w(a,b);
INSERT INTO w VALUES(1,1);
INSERT INTO w VALUES(2,2);
SELECT count(*) FROM w;
INSERT INTO w VALUES(3,1);
SELECT count(*) FROM w;
INSERT INTO w VALUES(1,1);
SELECT count(*) FROM w;
UPDATE w SET b=b+1;
SELECT sum(b) FROM w;
UPDATE w SET a=a+10;
SELECT sum(a) FROM w;
DELETE FROM w;
SELECT count(*) FROM w;
### r a NUL byte, which both engines truncate at the literal
CREATE TABLE nn(t, b);
INSERT INTO nn VALUES(CAST(x'610062' AS TEXT), x'610062');
SELECT length(t), hex(t), typeof(t), length(b), hex(b), typeof(b) FROM nn;
SELECT quote(t), quote(b) FROM nn;
### s the storage classes, side by side
SELECT typeof(NULL), typeof(0), typeof(1), typeof(1.0), typeof(''), typeof('a'), typeof(x'');
SELECT quote(NULL), quote(0), quote(1), quote(1.0), quote(''), quote('a'), quote(x'');
SELECT length(NULL), length(0), length(''), length(x'');
### t PRAGMA output and the schema
CREATE TABLE p(a INTEGER, b TEXT, c REAL, PRIMARY KEY(a));
CREATE INDEX p_i ON p(b);
PRAGMA table_info(p);
PRAGMA index_list(p);
PRAGMA index_info(p_i);
PRAGMA user_version;
PRAGMA user_version=7;
PRAGMA user_version;
PRAGMA table_list;
PRAGMA foreign_keys;
PRAGMA foreign_key_list(p);
PRAGMA integrity_check;
SELECT type, name FROM sqlite_master ORDER BY type, name;
### u the schema text a client would read back to rebuild the database
CREATE TABLE sch(a INTEGER PRIMARY KEY, b TEXT NOT NULL DEFAULT 'x', c REAL, d BLOB);
CREATE INDEX sch_b ON sch(b);
CREATE UNIQUE INDEX sch_c ON sch(c);
SELECT type, name, tbl_name FROM sqlite_master ORDER BY type, name;
SELECT sql FROM sqlite_master WHERE type='table' ORDER BY name;
### v GLOB, LIKE, REGEXP and MATCH, the pattern operators
SELECT 'abc' GLOB 'a*', 'abc' GLOB 'A*', 'abc' LIKE 'a%', 'abc' LIKE 'A%';
SELECT 'abc' GLOB '*b*', 'abc' GLOB '?', 'a.c' GLOB 'a?c', 'abc' GLOB 'a*c';
SELECT 'ABC' GLOB 'abc', 'abc' GLOB '[a-c]*', 'a1c' GLOB '[a-z][0-9][a-z]';
SELECT 'abc' NOT GLOB 'x*', 'abc' NOT LIKE 'x%';
### w COLLATE, which is parsed and then discarded
SELECT 'a' = 'A' COLLATE NOCASE, 'a' = 'A', 'a' < 'B' COLLATE NOCASE;
CREATE TABLE k1(a COLLATE NOCASE);
INSERT INTO k1 VALUES('A'),('a'),('B');
SELECT a, a='a' FROM k1 ORDER BY a;
SELECT count(DISTINCT a) FROM k1;
### x the integer literal boundary and the overflow shapes
SELECT -9223372036854775808, typeof(-9223372036854775808);
SELECT -9223372036854775808+0, typeof(-9223372036854775808+0);
SELECT -(9223372036854775807+1), typeof(-(9223372036854775807+1));
SELECT 9223372036854775807, 9223372036854775808, typeof(9223372036854775808);
SELECT 1e308*10, 1e308+1e308, -1e308*10;
SELECT 1e-308/10, 1.0/3, 22.0/7, 2.0/3;
SELECT quote(1.0/3), quote(22.0/7), quote(1e300), quote(1e-300), quote(-0.0);
### y CAST, and the storage class it leaves behind
SELECT CAST(x'414243' AS TEXT), typeof(CAST(x'414243' AS TEXT));
SELECT CAST(0 AS BLOB), typeof(CAST(0 AS BLOB));
SELECT CAST(1.9 AS INTEGER), CAST(-1.9 AS INTEGER), CAST('12abc' AS INTEGER);
SELECT CAST(1 AS TEXT), typeof(CAST(1 AS TEXT)), CAST(1.5 AS TEXT), typeof(CAST(1.5 AS TEXT));
SELECT CAST('1.5' AS REAL), typeof(CAST('1.5' AS REAL)), CAST(NULL AS INTEGER), typeof(CAST(NULL AS INTEGER));
SELECT CAST(x'41' AS BLOB), typeof(CAST(x'41' AS BLOB));
SELECT upper(x'414243'), typeof(upper(x'414243'));
### z a value read back from a table, so a stored class cannot hide
CREATE TABLE rt(v);
INSERT INTO rt VALUES(1),(1.0),('1'),(x'31'),(NULL),(''),(x'');
SELECT quote(v), typeof(v), length(v) FROM rt;
SELECT count(*) FROM rt;
SELECT DISTINCT quote(v) FROM rt;
