# FOURTH differential corpus: the shapes where a wrong answer hides, in the
# places the earlier corpora could not reach.
#
# Run it with the runner that already exists:
#
#   bash tools/difftest2.sh tools/difftest2-finds.sql
#   bash tools/difftest2.sh tools/difftest2-finds.sql --list
#   bash tools/difftest2.sh tools/difftest2-finds.sql --section 3
#
# WHAT IS IN HERE, and why each section is here
#
# Every section exists because a statement was run against the real sqlite3 and
# against this engine and the two disagreed, and in every case the disagreement
# was reduced to the smallest statement that still shows it. The list of those
# statements, with both engines' answers, is the deliverable; this file is the
# part of it that can be re-run.
#
# THE CLASS: the engine returns a WRONG ANSWER rather than an error, or refuses
# something that is valid SQL. Nothing here fails loudly.
#
#   a  WHERE on a SELECT with no FROM is parsed and then never applied
#   b  LIMIT and OFFSET are coerced three ways at once
#   c  a non-finite real is stored rather than folded to NULL
#   d  a string function returns a value of the storage class it was given
#   e  rowid, _rowid_ and oid resolve in no position at all
#   f  a qualified star in a join is refused, and the message names a
#      function that is not in the statement
#   g  PRAGMA table_list, table_xinfo and index_xinfo return no rows, and the
#      two column-name settings are a no-op
#   script h  transactions, which AGREE. They are here because they are the
#      section that proves the runner can see a transaction at all: run one
#      statement at a time, BEGIN and ROLLBACK are two connections and both
#      engines correctly refuse both, so the same section passes while proving
#      nothing. ROLLBACK, COMMIT, a rolled-back UPDATE, a rolled-back DELETE
#      and a rolled-back CREATE TABLE all agree, and the one thing that does
#      differ after a rollback is the file's change counter, which is a physical
#      detail and not an answer
#   i  an index changes the order a table with no ORDER BY is read in
#   j  an UPDATE that rewrites a row, in a table with no ORDER BY. It AGREES,
#      and it is the control for section i: an UPDATE reorders a table here
#      only when an index is present
#   k  a WHERE with a FROM-less projection, and a no-FROM query that was never
#      compared at all (the runner defect this corpus was written to expose)
#
# RULES THE RUNNER ENFORCES, restated because a corpus that breaks them
# produces reports that are about the corpus:
#
#   * one statement per `;`, and a section runs as a session against a pair of
#     fresh databases, so a section must CREATE everything it reads -- and the
#     runner now FAILS a section in which nothing was verified, so a section
#     that forgets cannot report PASS;
#   * a section whose name begins with `script` is run as ONE script per engine,
#     one connection each, and is compared by the last result plus the row count
#     of every table it created. That is the only shape in which a transaction
#     is visible at all: run one statement at a time, BEGIN and ROLLBACK become
#     two connections, and both engines correctly refuse both of them, so the
#     section passes for a transaction that is not a transaction. Measured on
#     the statement-at-a-time form of section h:
#         PASS: 27/27 statements agreed, 0 disagreed, 5 refusals worded differently
#     with the database holding 0 rows on sqlite3 and 1 row here;
#   * no string literal may contain a newline -- the runner joins a statement
#     that spans lines with a space, so use char(10) where a newline is the
#     point;
#   * a value that must be told from another by its storage class is compared
#     through the engine's own quote(), so the class is part of the answer;
#   * an ORDER BY is not optional in this corpus unless the row order is itself
#     the finding. A `SELECT a FROM t` with no ORDER BY comes back `3|5` from
#     sqlite3 and `5|3` from this engine after an UPDATE, and that is a real
#     difference -- but it is only a difference because sqlite3 itself rewrote
#     the third table in section j, so the row order after the third statement
#     is where the finding is and nowhere else in that section.

### a a WHERE on a SELECT with no FROM is parsed and then never applied
SELECT 1 WHERE 0;
SELECT 1 WHERE 1;
SELECT 1 WHERE 0.0;
SELECT 1 WHERE NULL;
SELECT 1 WHERE 'a';
SELECT 1 WHERE 0.5;
SELECT 1 WHERE x'00';
SELECT 1 WHERE NOT 0;
SELECT 1 WHERE 1=0;
SELECT 1 WHERE 0 AND 1;
SELECT 1 WHERE 0 OR 1;
SELECT count(*) WHERE 0;
SELECT count(*) WHERE 1;
SELECT sum(1) WHERE 0;
SELECT min(1) WHERE 0;
SELECT max(1) WHERE 0;
SELECT 1, 2 WHERE 0;
SELECT 'a' WHERE 0;
SELECT 1 WHERE 0 ORDER BY 1;
SELECT 1 WHERE 0 LIMIT 1;

### b LIMIT and OFFSET are coerced three ways at once
CREATE TABLE lim(a);
INSERT INTO lim VALUES(1),(2),(3),(4),(5);
SELECT a FROM lim ORDER BY a LIMIT -1;
SELECT a FROM lim ORDER BY a LIMIT -1 OFFSET 2;
SELECT a FROM lim ORDER BY a LIMIT 0;
SELECT a FROM lim ORDER BY a LIMIT 2;
SELECT a FROM lim ORDER BY a LIMIT 2.0;
SELECT a FROM lim ORDER BY a LIMIT 1e0;
SELECT a FROM lim ORDER BY a LIMIT '2';
SELECT a FROM lim ORDER BY a LIMIT 'x';
SELECT a FROM lim ORDER BY a LIMIT NULL;
SELECT a FROM lim ORDER BY a LIMIT 1.7;
SELECT a FROM lim ORDER BY a LIMIT x'D2';
SELECT a FROM lim ORDER BY a LIMIT -9223372036854775808;
SELECT a FROM lim ORDER BY a LIMIT 2 OFFSET 99;
SELECT a FROM lim ORDER BY a LIMIT 2 OFFSET -1;
SELECT a FROM lim ORDER BY a LIMIT 1 OFFSET 1.5;
SELECT a FROM lim ORDER BY a LIMIT 1, 2;

### c a non-finite real is stored rather than folded to NULL
SELECT quote(1e300*1e300);
SELECT quote(1e300*1e300 - 1e300*1e300);
SELECT typeof(1e300*1e300 - 1e300*1e300);
SELECT (1e300*1e300 - 1e300*1e300) IS NULL;
SELECT quote(1e300*1e300*0.0);
SELECT (1e300*1e300*0.0) IS NULL;
SELECT quote(0.0/0.0);
SELECT typeof(0.0/0.0);
SELECT (0.0/0.0) IS NULL;
SELECT quote(-1e300*1e300);
SELECT quote(1e300/1e-300);
SELECT quote(1.0e+999+0);
SELECT typeof(9.0e+999);
SELECT quote(1e300+1e300);

### d a string function returns a value of the storage class it was given
SELECT quote(upper(1)), typeof(upper(1));
SELECT quote(lower(1)), typeof(lower(1));
SELECT quote(upper(1.5)), typeof(upper(1.5));
SELECT quote(upper(-1)), typeof(upper(-1));
SELECT quote(upper(0)), typeof(upper(0));
SELECT quote(upper(NULL)), typeof(upper(NULL));
SELECT quote(upper(x'414243')), typeof(upper(x'414243'));
SELECT quote(lower(x'414243')), typeof(lower(x'414243'));
SELECT upper(1)='1';
SELECT quote(trim(x'2041')), typeof(trim(x'2041'));
SELECT quote(trim(12)), typeof(trim(12));
SELECT quote(upper('abc')), typeof(upper('abc'));

### e rowid, _rowid_ and oid resolve in no position at all
CREATE TABLE rid(a);
INSERT INTO rid VALUES(1),(2),(3);
SELECT rowid FROM rid;
SELECT rid.rowid FROM rid;
SELECT rid._rowid_ FROM rid;
SELECT rid.oid FROM rid;
SELECT _rowid_ FROM rid;
SELECT oid FROM rid;
SELECT a FROM rid ORDER BY rowid;
SELECT a FROM rid WHERE rowid=2;
SELECT max(rowid) FROM rid;
SELECT count(*) FROM rid WHERE rowid>1;
UPDATE rid SET a=9 WHERE rowid=1;
SELECT a FROM rid ORDER BY a;
DELETE FROM rid WHERE rowid=2;
SELECT a FROM rid ORDER BY a;
SELECT a FROM rid ORDER BY rowid;

### f a qualified star in a join is refused, and names a function that is absent
CREATE TABLE jl(a);
CREATE TABLE jr(a, c);
INSERT INTO jl VALUES(1),(2);
INSERT INTO jr VALUES(1,'p'),(2,'q');
SELECT jl.*, jr.c FROM jl LEFT JOIN jr ON jl.a=jr.a ORDER BY jl.a;
SELECT jl.a, jl.* FROM jl LEFT JOIN jr ON jl.a=jr.a ORDER BY jl.a;
SELECT jl.a, jr.c FROM jl LEFT JOIN jr ON jl.a=jr.a ORDER BY jl.a;
SELECT a, c FROM jl LEFT JOIN jr ON jl.a=jr.a ORDER BY a;
SELECT *, jr.c FROM jl LEFT JOIN jr ON jl.a=jr.a ORDER BY jl.a;

### g the schema pragmas return no rows, and the two column-name settings do nothing
CREATE TABLE gi(a, b);
CREATE INDEX gi_a ON gi(a);
SELECT count(*) FROM pragma_table_list;
SELECT count(*) FROM pragma_index_list('gi');
SELECT count(*) FROM pragma_table_info('gi');
CREATE TABLE gk(a INTEGER PRIMARY KEY, b);
SELECT pk FROM pragma_table_info('gk') ORDER BY cid;
SELECT name, type FROM pragma_table_info('gk') ORDER BY cid;
PRAGMA short_column_names;
PRAGMA full_column_names;
CREATE TABLE gc(a, b);
INSERT INTO gc VALUES(1,2);
PRAGMA full_column_names=1;
SELECT a+1, a, a AS k FROM gc;

### script h1 a rolled-back INSERT, the one thing a transaction has to get right
CREATE TABLE tx(a);
INSERT INTO tx VALUES(1);
BEGIN;
INSERT INTO tx VALUES(2);
ROLLBACK;
SELECT a FROM tx ORDER BY a;
SELECT count(*) FROM tx;

### script h2 a rolled-back UPDATE
CREATE TABLE ty(a);
INSERT INTO ty VALUES(1);
BEGIN;
UPDATE ty SET a=9;
ROLLBACK;
SELECT a FROM ty;

### script h3 a rolled-back DELETE
CREATE TABLE tz(a);
INSERT INTO tz VALUES(1);
BEGIN;
DELETE FROM tz;
ROLLBACK;
SELECT a FROM tz;

### script h4 a committed transaction is committed
CREATE TABLE tc(a);
INSERT INTO tc VALUES(1);
BEGIN;
INSERT INTO tc VALUES(2);
COMMIT;
SELECT a FROM tc ORDER BY a;

### script h5 a rolled-back CREATE TABLE does not leave the table behind
CREATE TABLE hu(a);
BEGIN;
CREATE TABLE ht(a);
ROLLBACK;
SELECT count(*) FROM sqlite_master WHERE name='ht';
SELECT count(*) FROM hu;

### i an index changes the order a table with no ORDER BY is read in
CREATE TABLE ia(a, b, c);
INSERT INTO ia VALUES(1,'x','p'),(2,'y','q'),(3,'x','r');
CREATE INDEX ia_a ON ia(a);
CREATE INDEX ia_ab ON ia(a,b);
SELECT a FROM ia ORDER BY a;
UPDATE ia SET a=5 WHERE a=1;
SELECT a FROM ia;
SELECT a FROM ia ORDER BY a;
SELECT a FROM ia WHERE a=1;
SELECT a FROM ia WHERE a=5;
CREATE TABLE ib(a);
INSERT INTO ib VALUES(1),(2),(3);
SELECT a FROM ib;
CREATE INDEX ib_a ON ib(a);
SELECT a FROM ib;
SELECT a FROM ib ORDER BY a;

### j an UPDATE that rewrites a row, in a table with no ORDER BY
-- This section AGREES, and it is here because it is the control for section i.
-- An UPDATE that rewrites a row moves that row to the end of an unordered scan
-- here, and does not on sqlite3 -- but on THIS table sqlite3 does not reorder
-- either, so the two agree. The difference in section i appears only once an
-- index is present. Both facts are worth having: the first says an UPDATE is
-- not the cause, the second says an index is.
--
-- The prose is `--` and not `#` because this runner's statement splitter strips
-- `--` and nothing else, so a `#` line inside a section body is handed to both
-- engines as SQL. That is a runner limitation and it is also an engine gap --
-- sqlite3 accepts a `#` line anywhere a `--` line is accepted, and this engine
-- refuses it with `unrecognized token: "#"`.
--
--     $ printf 'SELECT 1;\n# a comment line\nSELECT 2;\n' | sqlite3 -batch :memory:
--     1
--     2
--     $ printf 'SELECT 1;\n# a comment line\nSELECT 2;\n' | nsqlited --testsuite :memory:
--     E unrecognized token: "#"
--
-- so the finding is real and it is written down here rather than in a section,
-- where the runner would report it about a sentence of prose.
CREATE TABLE ua(a);
INSERT INTO ua VALUES(1),(2),(3);
SELECT a FROM ua;
UPDATE ua SET a=a+10;
SELECT a FROM ua;
UPDATE ua SET a=a+10 WHERE a=12;
SELECT a FROM ua;
CREATE TABLE ub(a);
INSERT INTO ub VALUES(1),(2),(3);
UPDATE ub SET a=a+10;
SELECT a FROM ub ORDER BY a;
CREATE TABLE uc(a, b);
INSERT INTO uc VALUES(1,'x'),(2,'y'),(3,'z');
UPDATE uc SET b='q';
SELECT a, b FROM uc;

### k a WHERE beside a FROM-less projection, and what a runner defect left uncompared
SELECT 1 WHERE 0 LIMIT 1;
SELECT 1 WHERE 1 LIMIT -1;
SELECT 1 WHERE 0 LIMIT '2';
SELECT 1 WHERE 0 LIMIT 1.7;
SELECT 1 WHERE 0 LIMIT NULL;
SELECT 1 WHERE 0 LIMIT 1,1;
SELECT 1 WHERE 0 OFFSET 2;
SELECT 1 WHERE 0 ORDER BY 1;
SELECT count(*) WHERE 0 LIMIT 1;
SELECT 'a' WHERE 0 ORDER BY 1;
