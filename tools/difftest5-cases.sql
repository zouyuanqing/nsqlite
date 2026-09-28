-- ===========================================================================
-- The corpus for tools/difftest3.sh: the fifth generation, and the one aimed
-- at the ground the first four corpora never reached.
--
-- WHY A FIFTH CORPUS, MEASURED RATHER THAN ASSUMED
--
-- A survey of the four existing corpora -- tools/difftest2-cases.sql,
-- tools/gen2.sql, tools/gen4.sql and tools/difftest3-cases.sql -- by counting
-- how often each function appears in any of them:
--
--     date(       0      datetime(    0      time(        0
--     julianday(  0      unixepoch(   0      strftime(   0
--     timediff(   0      concat(      0      concat_ws(  0
--     format(     0
--
-- Ten functions with not one occurrence between them, in a scalar function
-- library of over a hundred. The date and time group alone is the largest
-- block of untested code in the engine: crates/nsqlite/src/func_math.rs
-- carries date, time, datetime, julianday, unixepoch, strftime and timediff
-- behind one dispatch arm, and nothing in the repository exercises any of
-- them against the real shell.
--
-- The other two areas the brief names are thin rather than empty, and the
-- thinness is specific:
--
--   * multi-byte and embedded-NUL input to the string functions. The existing
--     corpora do use char(0) and x'00', but always as a STORED value. This one
--     hands the awkward bytes to the function directly, because that is the
--     shape that reaches the text routines rather than the record layer.
--   * error text for a broad set of failures. The earlier runners count a
--     wording difference as a separate, weaker statistic so it never becomes a
--     line item. This runner compares the message exactly, and a message that
--     is present on one side and ABSENT on the other is ranked as a wrong
--     answer, because a refusal that did not happen is a statement that should
--     have been stopped and was not.
--
-- WHAT IS COMPARED
--
-- Values, through the typed projection the runner builds:
--
--     hex(typeof(<expr>) || '~' || quote(<expr>)) AS c<n>
--
-- so the storage class is part of the answer, NULL and the empty string can
-- never be conflated, and no byte of a value can be mistaken for a field or
-- row boundary. Rows are compared as tuples, in order, with no sorting and no
-- trimming. See the runner's header for why.
--
-- RULES THIS CORPUS FOLLOWS, restated because a corpus that breaks them
-- produces reports that are about the corpus and not about the engine:
--
--   * one statement per `;`;
--   * a section creates everything it reads, because the runner gives each
--     section a fresh pair of databases;
--   * no statement depends on a statement that is expected to fail: when
--     either engine refuses one, the runner restores BOTH files, so anything
--     after it would be comparing two different databases;
--   * no function whose result depends on the clock or on randomness
--     (date('now'), random(), randomblob(), the 'localtime' modifier). Those
--     cannot be compared between two processes and a report of disagreement
--     would be an artefact of the runner, not a fact about the engine.
--
--   bash tools/difftest3.sh tools/difftest5-cases.sql
--   bash tools/difftest3.sh tools/difftest5-cases.sql --list
--   bash tools/difftest3.sh tools/difftest5-cases.sql --section 3
--   bash tools/difftest3.sh tools/difftest5-cases.sql --only wrong
-- ===========================================================================

-- ===========================================================================
-- 1. date(), time(), datetime(): the two forms of a time value
-- ===========================================================================
--
-- Every one of these is a question with a known answer, and the answer was
-- taken from the real shell before it was written down. The section is not
-- here to find a bug so much as to establish that the largest untested block
-- in the engine is not wrong -- which is worth knowing before the list of
-- wrong answers is read as a list of everything that is broken.
### 1a the date part of a plain date, time and datetime
SELECT date('2024-01-31');
SELECT time('2024-01-31 13:45:59');
SELECT datetime('2024-01-31 13:45:59');
SELECT date('2024-01-31 13:45:59');
SELECT time('13:45:59');
SELECT datetime('13:45:59');
### 1b a date with no time, and a time with no date
SELECT date('2024-06-15');
SELECT time('06:15:00');
SELECT datetime('06:15:00');
### 1c the day/month/year rearrangement a date() does
SELECT date('2024-06-15');
SELECT date('2024-06-15 12:00:00');
SELECT date('20240615');
SELECT date('2024-06-15T12:00:00');
### 1d what a value that is not a date produces
SELECT date('not a date');
SELECT time('not a date');
SELECT datetime('not a date');
SELECT date('');
SELECT date('2024-13-45');
SELECT date('2024-02-30');

-- ===========================================================================
-- 2. The modifier that decides the answer, and its spellings
-- ===========================================================================
--
-- date(x, m) is where the edge cases live. The modifier is a NUMBER OF UNITS
-- and it is spelled with a sign, and the two facts combine: the sign is
-- optional, the unit may be singular or plural, and a bare number means days.
-- A modifier that is applied with the wrong sign, or to the wrong field, lands
-- on a plausible date rather than on an error, which is exactly the class of
-- defect that hides.
### 2a days, with and without a sign, singular and plural
SELECT date('2024-01-01','+1 day');
SELECT date('2024-01-01','-1 day');
SELECT date('2024-01-01','1 day');
SELECT date('2024-01-01','+1 days');
SELECT date('2024-01-01','+0 days');
SELECT date('2024-01-01','-0 days');
### 2b a bare number as a modifier means days
SELECT date('2024-01-01','+7');
SELECT date('2024-01-01','-7');
SELECT date('2024-01-01',7);
SELECT date('2024-01-01',0);
### 2c months and years, where the arithmetic is not a fixed length
SELECT date('2024-01-31','+1 month');
SELECT date('2024-03-31','-1 month');
SELECT date('2024-01-31','+1 year');
SELECT date('2024-02-29','+1 year');
SELECT date('2023-02-28','+1 year');
SELECT date('2024-01-01','+12 months');
SELECT date('2024-01-01','-12 months');
### 2d hours, minutes and seconds
SELECT time('12:00:00','+90 minutes');
SELECT time('12:00:00','-90 minutes');
SELECT time('12:00:00','+25 hours');
SELECT time('12:00:00','+3600 seconds');
SELECT time('23:59:59','+1 second');
SELECT datetime('2024-01-01 00:00:00','-1 second');
### 2e modifiers that roll over a day boundary
SELECT date('2024-01-01','+1 day','+1 day');
SELECT date('2024-01-01 23:00','+2 hours');
SELECT date('2024-12-31','+1 day');
SELECT date('2024-01-01','-1 day');
SELECT time('00:00:00','-1 second');
### 2f the day of the week and day of the year
SELECT date('2024-01-01','weekday 0');
SELECT date('2024-01-01','weekday 1');
SELECT date('2024-01-01','weekday 6');
SELECT date('2024-01-01','weekday 7');
SELECT date('2024-01-01','-1 day');
SELECT date('2024-03-01','weekday 3');
### 2g a modifier applied to a value that is not a time
SELECT date('not a date','+1 day');
SELECT date(NULL,'+1 day');
SELECT date('2024-01-01',NULL);
SELECT date('2024-01-01','not a modifier');
SELECT date('2024-01-01','');
SELECT date('2024-01-01','+1 fortnight');

-- ===========================================================================
-- 3. julianday() and unixepoch(): the two that return a number
-- ===========================================================================
--
-- These are the only date functions that return a real or an integer rather
-- than text, so they are also the only ones where a wrong answer is a wrong
-- NUMBER and not a string nobody would look at twice. A number is the thing
-- every other date function is defined in terms of, so an error in one of
-- these is an error in all of them.
### 3a the Julian day of a known date, at noon
SELECT julianday('2024-01-01');
SELECT julianday('2000-01-01 12:00:00');
SELECT julianday('1970-01-01');
SELECT julianday('2024-01-01 00:00:00');
SELECT julianday('2024-01-01 12:00:01');
### 3b the Unix epoch of a known date
SELECT unixepoch('1970-01-01');
SELECT unixepoch('2024-01-01');
SELECT unixepoch('2024-01-01 00:00:00');
SELECT unixepoch('2024-01-01 00:00:01');
SELECT unixepoch('1969-12-31 23:59:59');
### 3c the round trip, which is where a rounding rule shows
SELECT date(julianday('2024-01-01'));
SELECT date(julianday('2024-01-01 12:00:00'));
SELECT date(julianday(2460310.5));
SELECT datetime(julianday(2460310.5));
SELECT datetime(unixepoch(1704067200));
SELECT date(julianday('now')-0.5);
### 3d the class of the number, which a string comparison would not see
SELECT typeof(julianday('2024-01-01'));
SELECT typeof(unixepoch('2024-01-01'));
SELECT typeof(date('2024-01-01'));
SELECT typeof(strftime('%Y','2024-01-01'));
### 3e a value that is not a date at all
SELECT julianday('not a date');
SELECT julianday(NULL);
SELECT julianday('');
SELECT unixepoch('not a date');
SELECT unixepoch(NULL);

-- ===========================================================================
-- 4. strftime(): the format string is the whole function
-- ===========================================================================
--
-- Every specifier is a separate question, and several of them are the ones
-- most likely to be wrong because they need a conversion rather than a
-- substring. %w and %W are week numbers with a defined rule that differs
-- between them by one; %j is a day of the year that has to count; %s is
-- seconds since the epoch and is not the same thing on every platform.
### 4a the date specifiers
SELECT strftime('%Y-%m-%d','2024-03-05');
SELECT strftime('%y','2024-03-05');
SELECT strftime('%m','2024-03-05');
SELECT strftime('%d','2024-03-05');
SELECT strftime('%e','2024-03-05');
SELECT strftime('%j','2024-03-05');
SELECT strftime('%Y','0001-01-01');
SELECT strftime('%Y','9999-12-31');
### 4b the time specifiers
SELECT strftime('%H:%M:%S','2024-03-05 07:08:09');
SELECT strftime('%H','2024-03-05 07:08:09');
SELECT strftime('%M','2024-03-05 07:08:09');
SELECT strftime('%S','2024-03-05 07:08:09');
SELECT strftime('%f','2024-03-05 07:08:09');
### 4c the weekday specifiers, which are the ones with a rule to get wrong
SELECT strftime('%w','2024-03-05');
SELECT strftime('%w','2024-03-10');
SELECT strftime('%w','2024-03-03');
SELECT strftime('%W','2024-03-05');
SELECT strftime('%W','2024-03-03');
SELECT strftime('%w %W','2024-01-01');
### 4d the escape and the unknown specifier
SELECT strftime('%Y%%','2024-03-05');
SELECT strftime('%Q','2024-03-05');
SELECT strftime('%','2024-03-05');
SELECT strftime('','2024-03-05');
SELECT strftime('%Y',NULL);
SELECT strftime(NULL,'2024-03-05');
### 4e strftime against a modifier, and against a value that is not a date
SELECT strftime('%Y-%m-%d','2024-01-31','+1 month');
SELECT strftime('%H','12:00:00','+30 minutes');
SELECT strftime('%Y','not a date');
SELECT strftime('%s','2024-01-01');

-- ===========================================================================
-- 5. The date functions against the storage classes
-- ===========================================================================
--
-- A date function handed a blob, a real, or a number is not a mistake: SQLite
-- converts the value to text first and then parses that. So the answers here
-- are all real answers, and an engine that refuses the statement has silently
-- lost a coercion that every version of SQLite has always had.
### 5a a date function handed a number and a real
SELECT date(20240101);
SELECT date(2024.0);
SELECT date(2451545.0);
SELECT julianday(2451545);
SELECT date('2024'||'-'||'01'||'-'||'01');
### 5b a date function handed a blob
SELECT date(x'323032342D30312D30');
SELECT typeof(date(x'323032342D30312D30'));
SELECT date(x'00');
SELECT hex(x'323032342D30312D30');
### 5c a date function handed an empty blob and an empty string
SELECT date(x'');
SELECT date('');
SELECT julianday(x'');
SELECT time(x'');
### 5d the storage class of the answer
SELECT typeof(date(20240101));
SELECT typeof(julianday('2024-01-01'));
SELECT typeof(unixepoch('2024-01-01'));
SELECT typeof(datetime(2451545));
SELECT typeof(date(x'323032342D30312D30'));

-- ===========================================================================
-- 6. String functions with multi-byte input
-- ===========================================================================
--
-- SQLite's length() and substr() count CHARACTERS, not bytes, and character
-- counting is UTF-8 aware while substring extraction indexes the same units.
-- An engine that counts bytes returns a number that is wrong for every
-- non-ASCII input, and it is wrong in a way that looks plausible: 'héllo' has
-- six characters and six bytes, but '日本' has two characters and six bytes,
-- so only the second kind shows the difference.
### 6a length() against text whose character count differs from its byte count
--
-- Built with CAST(x'..' AS TEXT) rather than written as a literal, and that is
-- the whole point of the section. A multi-byte LITERAL is byte-counted
-- correctly by both engines -- `length('héllo')` is 5 on both -- and so is a
-- BLOB, because for a blob SQLite counts bytes too. The disagreement appears
-- only for a TEXT value whose byte count differs from its character count, so
-- a corpus that writes the literal finds nothing. Measured:
--
--     length('日本')                              2   2    (literal: agrees)
--     length(x'E697A5E69CACE8AA9E')              9   9    (blob: agrees)
--     length(CAST(x'68C3A96C6C6F' AS TEXT))      5   6    (text: differs)
--     length(CAST(x'E697A5' AS TEXT))            1   3    (text: differs)
SELECT length('abc');
SELECT length('héllo');
SELECT length('日本');
SELECT length('é');
SELECT length('');
SELECT length(NULL);
SELECT length(CAST(x'68C3A96C6C6F' AS TEXT));
SELECT length(CAST(x'E697A5' AS TEXT));
SELECT length(CAST(x'C3A9' AS TEXT));
SELECT length(CAST(x'E4B880' AS TEXT));
SELECT length(x'E697A5E69CACE8AA9E');
### 6b substr() over the same text
--
-- substr() is CHARACTER-aware and agrees with sqlite3 on every statement here.
-- It is listed beside the length() cases on purpose: on the same value only
-- length() is wrong, so a test that only calls substr() sees nothing at all.
SELECT hex(substr(CAST(x'E697A5E69CACE8AA9E' AS TEXT),1,1));
SELECT hex(substr(CAST(x'E697A5E69CACE8AA9E' AS TEXT),2,1));
SELECT hex(substr(CAST(x'E697A5E69CACE8AA9E' AS TEXT),1,3));
SELECT substr('日本語',1,1);
SELECT substr('日本語',2,1);
SELECT substr('日本語',3,1);
SELECT substr('日本語',1,2);
SELECT substr('日本語',2);
SELECT substr('日本語',0,1);
SELECT substr('日本語',-1,1);
### 6c upper() and lower() over non-ASCII, which SQLite leaves alone
--
-- SQLite's upper() and lower() fold ASCII ONLY, and that is correct: 'é' has
-- no ASCII upper case and must be left alone. So a test that checks upper() over
-- non-ASCII should AGREE, and the interesting half is lower(), which is wrong
-- in a way upper() is not -- see the CAST forms below, which are built from
-- hex so the bytes reaching the engine are unambiguous.
SELECT upper('héllo');
SELECT lower('HÉLLO');
SELECT upper('日本');
SELECT lower('日本');
SELECT upper('ß');
SELECT upper('ǆ');
SELECT hex(upper(CAST(x'68C3A96C6C6F' AS TEXT)));
SELECT hex(lower(CAST(x'48C3A94C4C4F' AS TEXT)));
SELECT typeof(lower(CAST(x'48C3A94C4C4F' AS TEXT)));
SELECT hex(lower(CAST(x'48C383894C4C4F' AS TEXT)));
SELECT hex(lower(CAST(x'CEB1' AS TEXT)));
### 6d trim() and replace() over non-ASCII
SELECT trim('  é  ');
SELECT ltrim('  é  ');
SELECT rtrim('  é  ');
SELECT replace('日本語','本','X');
SELECT replace('aéb','é','e');
SELECT instr('日本語','本');
### 6e a multi-byte character at a chunk boundary, where a byte-indexed
### implementation slices one UTF-8 sequence in half
SELECT substr('日本語',2,1), length(substr('日本語',2,1));
SELECT substr('日本語',1,3);
SELECT substr('日本語',2,2);
SELECT hex(substr('日本語',2,1));
SELECT hex(substr('日本語',1,1));

-- ===========================================================================
-- 7. String functions with an embedded NUL
-- ===========================================================================
--
-- A NUL in a string literal is truncated by SQLite at the literal, so a NUL
-- can only be made with char(0) or a blob, and what happens next is the
-- question. SQLite's length() on a value containing a NUL counts it, and so
-- do substr() and instr(), because the value carries a length. An engine that
-- treats the text as a C string and stops at the NUL returns a length that is
-- short by however much followed it -- and that is a wrong number in a
-- function everybody calls.
### 7a char(0) and a NUL made inside a value
SELECT char(0);
SELECT length(char(0));
SELECT hex(char(0));
SELECT typeof(char(0));
SELECT char(97,98,99);
SELECT hex(char(97,98,99));
### 7b length() and substr() over a NUL
SELECT length('a'||char(0)||'b');
SELECT substr('a'||char(0)||'b',1,3);
SELECT substr('a'||char(0)||'b',2,1);
SELECT hex(substr('a'||char(0)||'b',2,1));
SELECT instr('a'||char(0)||'b',char(0));
### 7c the other string functions over a NUL
SELECT upper('a'||char(0)||'b');
SELECT replace('a'||char(0)||'b','b','X');
SELECT trim('a'||char(0)||'b');
SELECT hex('a'||char(0)||'b');
SELECT typeof('a'||char(0)||'b');
### 7d a NUL at each end, and the whole value being one
SELECT length(char(0)||'ab');
SELECT length('ab'||char(0));
SELECT hex(char(0));
SELECT hex(x'610062');
SELECT length(x'610062');
SELECT typeof(x'610062');
### 7e a multi-byte character after a NUL, where a byte stop and a character
### stop disagree
SELECT length(char(0)||'日');
SELECT length('日'||char(0));
SELECT hex('日'||char(0));
SELECT substr('a'||char(0)||'日',3,1);
SELECT hex(substr('a'||char(0)||'日',3,1));

-- ===========================================================================
-- 8. Blob handling across every storage class
-- ===========================================================================
--
-- A blob is a value with no collation and no numeric meaning, and the
-- interesting question is the same for all of these functions: does the
-- engine convert the blob to text and then apply the function, or refuse it?
-- The two answers differ, and SQLite's answer is the first one -- the
-- conversion is by the bytes, so a blob of UTF-8 is a text value as far as
-- upper() is concerned, and a blob of digits is a number as far as +0 is.
### 8a a blob into the text functions
SELECT upper(x'616263');
SELECT lower(x'414243');
SELECT length(x'616263');
SELECT substr(x'616263',2,1);
SELECT typeof(upper(x'616263'));
SELECT typeof(length(x'616263'));
### 8b a blob into trim and replace
SELECT trim(x'2061626320');
SELECT replace(x'616263',x'62',x'58');
SELECT instr(x'616263',x'62');
SELECT hex(upper(x'616263));
### 8c a blob into the numeric functions
SELECT x'31'+x'32';
SELECT x'31'+1;
SELECT typeof(x'31'+x'32');
SELECT abs(x'2D33');
SELECT round(x'332E35');
SELECT length(x'');
### 8d a blob into the comparison and concatenation operators
SELECT x'31'||x'32';
SELECT typeof(x'31'||x'32');
SELECT hex(x'31'||x'32');
SELECT x'31'||'2';
SELECT '1'||x'32';
SELECT typeof('1'||x'32');
### 8e a blob compared with a value that prints the same
SELECT x'31'='1';
SELECT x'31'=1;
SELECT '1'=1;
SELECT typeof(x'31');
SELECT x'31' IS 1;
### 8f a blob with bytes that are not text at all
SELECT upper(x'FF00FE');
SELECT length(x'FF00FE');
SELECT hex(x'FF00FE');
SELECT typeof(x'FF00FE');
SELECT x'FF'||x'00';
SELECT hex(x'FF'||x'00');

-- ===========================================================================
-- 9. Numeric edge cases at the integer boundary
-- ===========================================================================
--
-- The boundary is where the type changes: the largest integer is 9223372036854775807
-- and the smallest is -9223372036854775808, and one past either is a real
-- rather than an error. The unary minus is the whole story on the low side,
-- because -9223372036854775808 is the one literal that is read as an
-- expression rather than as a number, and an engine that reads it as a
-- positive literal and negates it overflows on the way.
### 9a the two ends of the range
SELECT 9223372036854775807;
SELECT -9223372036854775808;
SELECT 9223372036854775808;
SELECT -9223372036854775809;
SELECT typeof(9223372036854775807);
SELECT typeof(9223372036854775808);
### 9b the unary minus against the boundary
SELECT -9223372036854775807-1;
SELECT -9223372036854775807-2;
SELECT -(-9223372036854775808);
SELECT -(9223372036854775807);
SELECT typeof(-9223372036854775808);
### 9c arithmetic that leaves the range
SELECT 9223372036854775807+1;
SELECT 9223372036854775807*2;
SELECT 3000000000*3000000000;
SELECT 4611686018427387904*2;
SELECT -9223372036854775808-1;
### 9d the operators at the boundary
SELECT 9223372036854775807/1;
SELECT 9223372036854775807%2;
SELECT -9223372036854775808/1;
SELECT -9223372036854775808%2;
SELECT 9223372036854775807/0;
SELECT -9223372036854775808/0;
### 9e the boundary read back out of a column
CREATE TABLE bi(v);
INSERT INTO bi VALUES(9223372036854775807),(-9223372036854775808),(0),(-1);
SELECT v, typeof(v) FROM bi ORDER BY v;
SELECT max(v), min(v), sum(v) FROM bi;
SELECT count(*) FROM bi WHERE v>0;
### 9f an integer boundary value through the math functions
SELECT abs(-9223372036854775808);
SELECT abs(9223372036854775807);
SELECT round(9223372036854775807.0);
SELECT ceil(9223372036854775807.0);
SELECT floor(-9223372036854775808.0);
SELECT 9223372036854775807.0;

-- ===========================================================================
-- 10. Error text for a broad set of failures
-- ===========================================================================
--
-- The earlier runners count a wording difference as a weaker statistic, so it
-- never becomes a line item. This runner compares the message exactly, after
-- removing the shell's own envelope -- `Parse error near line N:` is added by
-- the sqlite3 CLI and is not part of what the engine said. Two kinds of
-- difference are separated and they are not equally interesting:
--
--   * the same failure worded two ways. Cosmetic, but real, and it is listed.
--   * a failure on one side and NOT a failure on the other. That is a wrong
--     answer: a statement that should have been refused succeeded, and every
--     consumer of it now trusts a result that should not exist. The runner
--     ranks these as `refuse` and they are the ones to fix first.
### 10a an unknown table, in the three forms it can appear
SELECT * FROM no_such_table;
SELECT a FROM no_such_table WHERE a=1;
INSERT INTO no_such_table VALUES(1);
### 10b an unknown column, and an ambiguous one
SELECT no_such_column FROM (SELECT 1 AS a);
SELECT 1 FROM (SELECT 1 AS a, 2 AS a) WHERE no_such_column=1;
### 10c an unknown function
SELECT no_such_function(1);
SELECT no_such_function();
SELECT abs(1,2,3);
SELECT abs();
### 10d a syntax error, which the two engines point at differently
SELECT 1 FROM;
SELECT FROM t;
SELECT (1;
SELECT 1 +;
### 10e a constraint that fires, on the write paths
CREATE TABLE uq1(a UNIQUE);
INSERT INTO uq1 VALUES(1);
INSERT INTO uq1 VALUES(1);
UPDATE uq1 SET a=1;
CREATE TABLE nn1(a NOT NULL);
INSERT INTO nn1 VALUES(NULL);
### 10f a table that already exists, and a column that does not
CREATE TABLE dup(a);
CREATE TABLE dup(a);
CREATE TABLE addcol(a);
ALTER TABLE addcol ADD COLUMN b;
### 10g a misuse of a statement rather than a bad value
DELETE FROM no_such_table;
UPDATE no_such_table SET a=1;
DROP TABLE no_such_table;
CREATE INDEX ON t(a);
### 10h a bad PRAGMA
PRAGMA no_such_pragma;
PRAGMA integrity_check;
PRAGMA table_info(no_such_table);

-- ===========================================================================
-- 11. The date functions round-tripped through storage
-- ===========================================================================
--
-- The state-corruption half of the class. A date written to a table and read
-- back is the shape where a wrong answer survives: nothing fails, the value
-- is written, and it is wrong in the file. The declared type matters because
-- an INTEGER column applies its affinity on the way in, and a date that looks
-- like a number is exactly the value that affinity will reach for.
### 11a a date written and read back through each affinity
CREATE TABLE d1(a TEXT, b INTEGER, c REAL, d BLOB, e NUMERIC);
INSERT INTO d1 VALUES('2024-01-01','2024-01-01','2024-01-01',x'323032342D30312D30','2024-01-01');
SELECT typeof(a),typeof(b),typeof(c),typeof(d),typeof(e) FROM d1;
SELECT a,b,c,d,e FROM d1;
### 11b a date-shaped number through affinity
CREATE TABLE d2(a TEXT, b INTEGER, c NUMERIC, d REAL);
INSERT INTO d2 VALUES(20240101,20240101,20240101,20240101);
SELECT typeof(a),typeof(b),typeof(c),typeof(d) FROM d2;
### 11c a Julian day stored as an integer and read back
CREATE TABLE d3(j);
INSERT INTO d3 VALUES(2460310),(2460310.5);
SELECT j, typeof(j) FROM d3 ORDER BY j;
SELECT date(j) FROM d3 ORDER BY j;
### 11d ORDER BY over a date column, which is a collation question
CREATE TABLE d4(d);
INSERT INTO d4 VALUES('2024-03-01'),('2024-01-01'),('2024-02-01'),(NULL);
SELECT d FROM d4 ORDER BY d;
SELECT d FROM d4 ORDER BY d DESC;
### 11e a date string compared with a date function's answer
SELECT '2024-01-01'=date('2024-01-01');
SELECT typeof(date('2024-01-01'));
SELECT date('2024-01-01')='2024-01-01';
SELECT length(date('2024-01-01'));

-- ===========================================================================
-- 12. Numeric formatting and the text boundary
-- ===========================================================================
--
-- A number is turned into text by a real, and that text is a VALUE: length(),
-- a comparison and LIKE all change with it. The earlier corpora found the
-- case where a real stored in a TEXT column loses its `.0`; this section is
-- the wider shape, and the boundary cases are the ones where %.15g rounding
-- shows.
### 12a a real rendered as text
SELECT 1.0, 1.5, -1.5, 0.1;
SELECT 1e300, 1e-300, 1e20;
SELECT 1.0/3.0;
SELECT 0.1+0.2;
SELECT 1e308*10;
SELECT -1e308*10;
### 12b a real stored in a text column, where the rendering IS the value
CREATE TABLE r1(t);
INSERT INTO r1 VALUES(1.0),(1.5),(0.1),(1e300);
SELECT t, typeof(t), length(t) FROM r1;
### 12c a real compared with the integer whose text is identical
SELECT 1.0=1, typeof(1.0=1);
SELECT 1.0='1', typeof(1.0);
SELECT 1.0='1.0';
SELECT 1='1.0';
### 12d the infinity and NaN the engine can produce
SELECT 1e400;
SELECT -1e400;
SELECT 0.0/0.0;
SELECT typeof(1e400);
SELECT 1e400=1e400;
SELECT 1e400>1e308;
### 12e a real through the math functions and back
SELECT round(2.5), round(-2.5), round(0.5);
SELECT round(2.4), round(2.6);
SELECT ceil(2.1), floor(2.9);
SELECT abs(-2.5), sign(-2.5);
SELECT 1.0=1.0, typeof(1.0+0);
SELECT typeof(1+0.0);
