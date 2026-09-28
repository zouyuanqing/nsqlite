# substr() on multi-byte text: the measured reference table

Every value below was measured against the real `sqlite3` 3.53.4 on 2026-09-28 by
running the expression through it and reading `hex()`. Nothing here is reasoned
about.

This is the last open finding from the fifth differential corpus: 14 wrong
answers, all of which turned out to be this one defect.

## The value under test

`S` is the three UTF-8 bytes `E6 97 A5 E6 9C AC E8 AA 9E`, which is `日本語`:

```sql
S := CAST(x'E697A5E69CACE8AA9E' AS TEXT)
```

`length(S)` is **3**.

## The table

| expression | reference |
| --- | --- |
| `substr(S,1)` | `E697A5E69CACE8AA9E` |
| `substr(S,2)` | `E69CACE8AA9E` |
| `substr(S,3)` | `E8AA9E` |
| `substr(S,4)` | *(empty)* |
| `substr(S,0)` | `E697A5E69CACE8AA9E` |
| `substr(S,-1)` | `E8AA9E` |
| `substr(S,1,1)` | `E697A5` |
| `substr(S,2,1)` | `E69CAC` |
| `substr(S,3,1)` | `E8AA9E` |
| `substr(S,1,2)` | `E697A5E69CAC` |
| `substr(S,2,2)` | `E69CACE8AA9E` |
| `substr(S,1,3)` | `E697A5E69CACE8AA9E` |
| `substr(S,0,1)` | *(empty)* |
| `substr(S,1,0)` | *(empty)* |
| `substr(S,-1,1)` | `E8AA9E` |
| `substr(S,-2,1)` | `E69CAC` |
| `substr(S,2,-1)` | `E697A5` |
| `substr(S,5,2)` | *(empty)* |

Two of these are worth reading twice because a wrong implementation can appear to
pass them: `substr(S,4)` and `substr(S,5,2)` are **empty** in the reference, and so
is the answer of an implementation that returns nothing at all. A fix has to be
counted against `substr(S,1,1)` = `E697A5` specifically.

Position 0 is the slot before the first character, not a synonym for 1, and the
length counts from that gap — so `substr(S,0,1)` is empty. A negative start counts
back from the end without being clamped; a negative length means "stop that many
before the end", so `substr(S,2,-1)` is the first character.

## What is NOT broken

Measured on the same three characters, by VALUE comparison (so immune to the
record-stream trap below). Every one returns 1 on **both** engines, so all four
are already correct and must not be "fixed":

```sql
SELECT length(S) = 3;                                          -- 1 / 1
SELECT instr(S, CAST(x'E69CAC' AS TEXT)) = 2;                  -- 1 / 1
SELECT replace(S, CAST(x'E69CAC' AS TEXT),'X')
     = CAST(x'E697A558E8AA9E' AS TEXT);                        -- 1 / 1
SELECT upper(S) = S;                                           -- 1 / 1
```

`instr` returning **1, 2, 3** for the three characters is the interesting one: a
byte-oriented implementation would answer 1, 4, 7. So `instr` is already
character-indexed, and `replace` and `upper` already preserve the bytes.

The defect is confined to `substr`. That is worth stating explicitly, because
"character-aware string handling" invites a rewrite of the whole module and four
of the five functions here are already right.

## Three rules that are not obvious

### 1. A character is never split, and an invalid byte is one whole character

```sh
sqlite3 -batch :memory: "SELECT hex(substr(CAST(x'41FF42' AS TEXT),2,1));"
# FF
sqlite3 -batch :memory: "SELECT length(CAST(x'41FF42' AS TEXT));"
# 3
```

The lone `FF` is not valid UTF-8. It counts as **one** character and it comes back
**whole**. This rules out a `String::from_utf8_lossy` implementation: lossy would
turn `FF` into `EF BF BD` and the answer would be `EFBFBD`.

### 2. A truncated multi-byte sequence is one character and survives whole

```sh
sqlite3 -batch :memory: "SELECT hex(substr(CAST(x'E697' AS TEXT),1,1));"
# E697
sqlite3 -batch :memory: "SELECT length(CAST(x'E697' AS TEXT));"
# 1
```

`E6 97` is the first two bytes of a three-byte character. The reference does not
decode it, does not replace it, and does not count it as two characters: it counts
it as one and returns it unchanged.

Together, rules 1 and 2 mean the segmentation is **byte-level** — a byte in
`0x80..=0xBF` is a continuation, a leading byte starts a character whose length is
however many bytes are actually present — and the answer is always a byte range of
the input, never a re-encoding of it. A `Vec<char>` built with `b as char` cannot
express this, because the reference's answer is not a `String` round trip.

### 3. The character count depends on the database's declared encoding

```sh
sqlite3 file.db "PRAGMA encoding='UTF-8';    ... SELECT length(x), hex(substr(x,2,1)) FROM t;"
# 3|E69CAC
sqlite3 file.db "PRAGMA encoding='UTF-16le'; ... SELECT length(x), hex(substr(x,2,1)) FROM t;"
# 4|A5E6
```

The same bytes, a different answer, because the engine reads the text as UTF-16
there. This engine answers `UTF-8` for `PRAGMA encoding` and the file header
carries the encoding, so UTF-8 is the case that needs implementing — but the
encoding has to be an **explicit input** to the segmentation rather than an
assumption inside a helper, or the UTF-16 case becomes a rewrite instead of a
parameter.

Note the `PRAGMA encoding=...` form needs the value quoted. The unquoted
`encoding=UTF-16le` is a parse error (`near "-": syntax error`) and leaves the
database in its default encoding, which is a quiet way to measure the UTF-8 case
three times and think you have varied something.

## What this engine does, and the defect

```sh
printf "SELECT substr(CAST(x'E697A5' AS TEXT),1,1) IS CAST(x'E697A5' AS TEXT);\n" \
  | nsqlited --testsuite :memory:     # 0
printf "SELECT substr(CAST(x'E697A5' AS TEXT),1,1) IS CAST(x'E697A5' AS TEXT);\n" \
  | sqlite3 :memory:                  # 1
```

The value returned is `EF BF BD` — U+FFFD — where the reference returns
`E6 97 A5`. The character is **replaced**, not sliced.

`crates/nsqlite/src/func_string.rs:431`:

```rust
let chars: Vec<char> = match std::str::from_utf8(head) {
    Ok(s) => s.chars().collect(),
    Err(_) => head.iter().map(|&b| b as char).collect(),
};
```

The `Ok` arm is right. The `Err` arm is the defect: `b as char` maps a byte to
the code point of that number, and re-encoding that code point as UTF-8 produces
U+FFFD.

## How to check a fix — the two traps in this repo

**Do not judge a byte-level result by reading the record stream.** The CLI
hex-encodes a TEXT value's bytes, so a value whose *content* is the two
characters `E6` prints as `4536` — the bytes of those two ASCII characters,
which is correct. It does not mean the value contains `4536`. This was mistaken
for an engine bug during this work, and `hex()` was nearly reported as broken
when it was correct throughout:

```sh
printf "SELECT hex(x'E6')='E6';\n" | nsqlited --testsuite :memory:   # 1 — correct
```

**Do not measure a build that is older than the source.** If other work is
landing in the tree, `target/debug/nsqlited.exe` is stale and will confirm
defects that no longer exist:

```sh
ls -la --time-style=+%H:%M:%S target/debug/nsqlited.exe crates/nsqlite/src/func_string.rs
CARGO_TARGET_DIR=target-scratch cargo build -p nsqlited
```

Compare by **value** — with `=`, `IS`, `length()`, or `quote()` applied to an
expression. That is immune to both traps.
