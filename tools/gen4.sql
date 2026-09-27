# THIRD differential corpus: the shapes where a wrong answer hides, in the
# places the first two corpora did not reach.
#
# The first corpus (tools/difftest2-cases.sql) covers expression grammar,
# affinity, aggregates and the schema. The second (tools/gen2.sql) covers the
# same ground again plus the storage classes. This one is a THIRD, independent
# set, and every section in it exists because a defect of the class this runner
# is for was found there and reduced to the smallest statement that still shows
# it.
#
# THE CLASS: the engine returns a WRONG ANSWER rather than an error. Nothing
# fails loudly. A refused statement is a disagreement a run reports by name; a
# statement that answers `0` where SQLite answers `NULL` is scored an agreement
# by any test that only checks it ran, and is a silently corrupted answer to
# anything that reads it.
#
# WHAT IS IN HERE, and why each section is here:
#
#   a  NULL propagation through ||, +, -, *, / and %
#   b  BETWEEN and IN with a NULL bound
#   c  the integer literal boundary, where the unary minus is the whole story
#   d  a text value that looks like a number, on the way into a REAL
#   e  a REAL that overflows, on the way out
#   f  a BLOB handed to a string function
#   g  GLOB, which is a crash rather than a wrong answer
#   h  round() with a negative or NULL precision
#   i  a chained comparison, which is a parse, not an evaluation
#   j  PRAGMA table_info and the declared type it prints
#   k  PRAGMA table_list, index_list, index_info
#   l  a declared type is preserved, not folded to lower case
#   m  indexes: insert, update, delete, and the index against the table
#   m2 a UNIQUE index, which is built and then not enforced
#   n  transactions: BEGIN, a change, ROLLBACK, and what the file holds
#   o  the two column-name settings, which are connection state
#   p  a multi-statement script, so a failure part way is visible
#
# RULES THE RUNNER ENFORCES, restated because a corpus that breaks them
# produces reports that are about the corpus:
#
#   * one statement per `;`, and a section runs as a session against a pair of
#     fresh databases, so a section must CREATE everything it reads;
#   * no string literal may contain a newline -- the runner joins a statement
#     that spans lines with a space, so use char(10) where a newline is the
#     point;
#   * a value that must be told from another by its storage class is compared
#     through the engine's own quote(), so the class is part of the answer.
#
#   bash tools/difftest2.sh tools/gen3.sql
#   bash tools/difftest2.sh tools/gen3.sql --list
#   bash tools/difftest2.sh tools/gen3.sql --section 3

### a NULL propagation: every arithmetic operator, with NULL on either side
SELECT typeof('a'||NULL), ('a'||NULL) IS NULL;
SELECT typeof(NULL||'a'), (NULL||'a') IS NULL;
SELECT typeof('a'||NULL||'b'), ('a'||NULL||'b') IS NULL;
SELECT typeof(1+NULL), (1+NULL) IS NULL;
SELECT typeof(1-NULL), (1-NULL) IS NULL;
SELECT typeof(1*NULL), (1*NULL) IS NULL;
SELECT typeof(1/NULL), (1/NULL) IS NULL;
SELECT typeof(1%NULL), (1%NULL) IS NULL;
SELECT typeof(NULL-NULL), (NULL-NULL) IS NULL;
SELECT typeof(''||NULL), (''||NULL) IS NULL;
SELECT typeof(x'41'||NULL), (x'41'||NULL) IS NULL;
SELECT length('a'||NULL), length(NULL||'a');
### b BETWEEN and IN, where a NULL bound makes the answer NULL and not zero
SELECT NULL BETWEEN 1 AND 2 IS NULL;
SELECT 1 BETWEEN 0 AND 2 IS NULL, 1 BETWEEN 0 AND 2;
SELECT 5 BETWEEN NULL AND 2 IS NULL;
SELECT NULL NOT BETWEEN 1 AND 2 IS NULL;
SELECT 1 NOT BETWEEN 0 AND 2 IS NULL, 1 NOT BETWEEN 0 AND 2;
SELECT NULL IN (1,2) IS NULL;
SELECT 1 IN (1,2), 3 IN (1,2);
SELECT NULL NOT IN (1,2) IS NULL;
SELECT 1 NOT IN (1,2);
SELECT 1 IN (NULL,1), 2 IN (NULL,1);
SELECT 1 IN (), 1 NOT IN ();
### c the integer literal boundary, where the unary minus is the whole story
SELECT typeof(-9223372036854775808), quote(-9223372036854775808);
SELECT typeof(9223372036854775807), quote(9223372036854775807);
SELECT typeof(-9223372036854775807), quote(-9223372036854775807);
SELECT typeof(-9223372036854775808+0), quote(-9223372036854775808+0);
SELECT typeof(-0), quote(-0);
SELECT typeof(- 9223372036854775808);
### d a text value that looks like a number, on the way into a REAL
SELECT typeof('100.0'+0), quote('100.0'+0);
SELECT typeof('1.0'+0), quote('1.0'+0);
SELECT typeof('1.5'+0), quote('1.5'+0);
SELECT typeof('0.0'+0), quote('0.0'+0);
SELECT typeof('-1.0'+0), quote('-1.0'+0);
SELECT typeof('100'+0), quote('100'+0);
SELECT typeof('1e2'+0), quote('1e2'+0);
SELECT typeof(' 1.0 '+0), quote(' 1.0 '+0);
SELECT typeof('1.0'||''), quote('1.0'||'');
### e a REAL that overflows, on the way out
SELECT typeof(1e308*10), quote(1e308*10);
SELECT typeof(1e308+1e308), quote(1e308+1e308);
SELECT typeof(-1e308*10), quote(-1e308*10);
SELECT typeof(1e308/1e-308), quote(1e308/1e-308);
SELECT typeof(1e-308/10), quote(1e-308/10);
SELECT typeof(0.0/0.0);
SELECT typeof(1.0/0.0), typeof(-1.0/0.0);
### f a BLOB handed to a string function
SELECT typeof(upper(x'414243')), quote(upper(x'414243'));
SELECT typeof(lower(x'414243')), quote(lower(x'414243'));
SELECT typeof(CAST(x'414243' AS TEXT)), quote(CAST(x'414243' AS TEXT));
SELECT typeof(hex(x'414243')), quote(hex(x'414243'));
SELECT typeof(length(x'414243')), quote(length(x'414243'));
SELECT typeof(substr(x'414243',1,2)), quote(substr(x'414243',1,2));
SELECT typeof(trim(x'20414243')), quote(trim(x'20414243'));
### g GLOB, which is a crash rather than a wrong answer
SELECT 'abc' GLOB 'a*';
SELECT 'abc' GLOB 'A*';
SELECT 'abc' GLOB '*b*';
SELECT 'abc' GLOB '?';
SELECT 'a.c' GLOB 'a?c';
SELECT 'a.c' GLOB 'a.c';
SELECT 'abc' GLOB 'a*c';
SELECT 'ABC' GLOB 'abc';
SELECT 'abc' NOT GLOB 'x*';
SELECT 'abc' LIKE 'a%', 'abc' LIKE 'A%';
### h round(), where the precision argument is the whole story
SELECT quote(round(1.0, 2)), quote(round(2.675, 2));
SELECT quote(round(1.5, -1));
SELECT quote(round(1.45, 1)), quote(round(1.45, 0));
SELECT quote(round(NULL)), quote(round(1, NULL));
SELECT typeof(round(1.5)), quote(round(1.5));
SELECT typeof(round(2.5)), quote(round(2.5));
SELECT typeof(round(-1.5)), quote(round(-1.5));
### i a chained comparison, which is a parse and not an evaluation
SELECT 1=1=1;
SELECT 1<2<3;
SELECT 1=2=1;
SELECT 2>1>0;
SELECT 1<>1<>1;
SELECT 'a'='a'='a';
### j PRAGMA table_info, and the declared type it prints
CREATE TABLE ti(a INTEGER, b TEXT, c REAL, d BLOB, e NUMERIC, f);
PRAGMA table_info(ti);
SELECT typeof(a), typeof(b), typeof(c), typeof(d), typeof(e), typeof(f) FROM ti;
### k the other table and index pragmas
CREATE TABLE tk(a, b);
CREATE INDEX tk_a ON tk(a);
SELECT type, name, tbl_name FROM sqlite_master ORDER BY type, name;
PRAGMA index_list(tk);
PRAGMA index_info(tk_a);
SELECT sql FROM sqlite_master WHERE type='index' ORDER BY name;
### l a declared type is preserved, not folded to lower case
CREATE TABLE lt(a INTEGER, b VARCHAR(10), c "Weird Type", d);
PRAGMA table_info(lt);
### m indexes: the index checked against the table, not against an expectation
CREATE TABLE ix(a INTEGER, b TEXT, c REAL);
INSERT INTO ix VALUES(1,'one',1.5),(2,'two',2.5),(3,'three',3.5);
CREATE INDEX ix_a ON ix(a);
SELECT a, b FROM ix WHERE a=2;
SELECT a, b FROM ix WHERE a>2 ORDER BY a;
SELECT a, b FROM ix WHERE a BETWEEN 1 AND 2 ORDER BY a;
UPDATE ix SET a=9 WHERE a=1;
SELECT a, b FROM ix WHERE a=1;
SELECT a, b FROM ix WHERE a=9;
DELETE FROM ix WHERE a=2;
SELECT a, b FROM ix WHERE a=2;
SELECT a, b FROM ix ORDER BY a;
SELECT count(*) FROM ix;
CREATE UNIQUE INDEX ix_b ON ix(b);
SELECT a, b FROM ix WHERE b='one' ORDER BY a;
### m2 a UNIQUE index, which is built and then not enforced
CREATE TABLE uq(a, b);
INSERT INTO uq VALUES(1,'one');
CREATE UNIQUE INDEX uq_b ON uq(b);
INSERT INTO uq VALUES(2,'one');
SELECT count(*) FROM uq;
SELECT a, b FROM uq WHERE b='one' ORDER BY a;
UPDATE uq SET b='two' WHERE a=1;
INSERT INTO uq VALUES(3,'two');
SELECT count(*) FROM uq;
### n transactions: BEGIN, a change, ROLLBACK, and what is left
CREATE TABLE tx(a);
BEGIN;
INSERT INTO tx VALUES(1);
INSERT INTO tx VALUES(2);
SELECT a FROM tx ORDER BY a;
ROLLBACK;
SELECT a FROM tx ORDER BY a;
SELECT count(*) FROM tx;
BEGIN;
INSERT INTO tx VALUES(3);
COMMIT;
SELECT a FROM tx ORDER BY a;
### o the two column-name settings, which are connection state
CREATE TABLE cs(a, b);
SELECT a AS x, b AS y FROM cs;
SELECT a AS "Z z", b AS "Q q" FROM cs;
### p a multi-statement script, so a failure part way is visible
CREATE TABLE ms(a);
INSERT INTO ms VALUES(1);
INSERT INTO ms VALUES('two');
INSERT INTO ms VALUES(3.5);
INSERT INTO ms VALUES(NULL);
INSERT INTO ms VALUES(x'414243');
SELECT typeof(a), quote(a) FROM ms ORDER BY rowid;
SELECT count(*), count(a) FROM ms;
