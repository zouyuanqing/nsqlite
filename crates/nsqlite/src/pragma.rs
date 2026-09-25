//! `PRAGMA`: the statement form, and the pragmas the test suite needs.
//!
//! A PRAGMA statement has three shapes, and the difference between them is not
//! cosmetic — the executor has to be able to tell a read from an assignment
//! before it can decide whether to produce rows or to change a setting:
//!
//! ```text
//! PRAGMA [schema.]name                  -- read
//! PRAGMA [schema.]name(arg)             -- read, with an argument
//! PRAGMA [schema.]name = value           -- assign
//! ```
//!
//! The parenthesised form and the `=` form are not variants of one another.
//! `PRAGMA table_info=t` and `PRAGMA table_info(t)` both name a table, because
//! SQLite resolves a read's argument either way, but `PRAGMA
//! full_column_names=on` has no parenthesised spelling at all.
//!
//! # What a pragma returns
//!
//! Three different answers, and they are easy to confuse:
//!
//! * A pragma that reports a value returns a one-column result whose column is
//!   named after the pragma. `PRAGMA page_size` answers a column called
//!   `page_size`.
//! * The introspection pragmas return their fixed column set **even when they
//!   return no rows**. `PRAGMA table_info(nosuchtable)` is an empty result with
//!   the six `table_info` columns. The column set is part of the answer, not a
//!   by-product of which rows happened to match.
//! * An assignment, and a pragma this engine does not implement, return **no
//!   result set at all** — not an empty result with no columns.
//!
//! That last one is easy to get backwards, and it is the single most important
//! thing in this file. An unknown pragma is a **silent no-op, not an error**:
//! sqlite3's own comment on it reads "IMP: R-43042-22504 No error messages are
//! generated if an unknown pragma is issued." The suite runs `PRAGMA
//! locking_mode`, `PRAGMA trusted_schema`, `PRAGMA temp_store` and a long tail
//! of settings this engine has no notion of, and every one of them has to run
//! without failing — a failure in a setup block abandons the whole block, and
//! every table it was going to create stays missing, so one missing PRAGMA
//! turns a hundred tests into "no such table".
//!
//! # Where the answers come from
//!
//! The introspection pragmas read the statement text the schema already stores,
//! not the parser's tree. That is not a shortcut, it is a requirement: the
//! engine's parser folds an unquoted identifier to lower case and discards a
//! `PRIMARY KEY` column list, so `CREATE TABLE t(A, B, PRIMARY KEY(A,B))` and
//! `CREATE TABLE t(a, b, primary key(a,b))` produce the same tree but have to
//! produce different `dflt_value` and `pk` answers. SQLite reports the default
//! `007` with its leading zero and the type `UNSIGNED BIG INT` in the case it
//! was written in, and the only place that spelling still exists is the schema
//! text.
//!
//! # Column naming
//!
//! `full_column_names` and `short_column_names` decide what a SELECT's result
//! columns are called. SQLite's rule, from `sqlite3GenerateColumnNames` in
//! `select.c`, is:
//!
//! * An explicit alias always wins.
//! * Otherwise, if the result is a direct reference to a table column, the name
//!   is the column name when `short_column_names` is on and `table.column` when
//!   it is off. `full_column_names` overrides the short spelling and is *not*
//!   cancelled by short being on — it is an OR, not a three-way choice.
//! * Otherwise the name is the expression's own source text, which is how
//!   `SELECT test1 . f1` keeps its spaces and `SELECT test1.f1` does not.
//!
//! A `*` follows the same intent through a different function, the expansion in
//! `expand_star`: it qualifies with the table name when `full_column_names` is
//! on *and* `short_column_names` is off. That last clause is the asymmetry
//! between the two spellings, and it is why the star is a separate question
//! rather than the same helper applied to `TABLE.COLUMN`.

use crate::error::{Error, Result, ResultCode};
use crate::tokenizer::{Keyword, Punct, Span, Token, Tokenizer};
use crate::value::Value;

// --- the statement --------------------------------------------------------

/// A parsed PRAGMA statement.
///
/// The three shapes are kept apart rather than collapsed into one "pragma with
/// an optional value", because the executor has to answer a read, apply a set,
/// and say nothing at all for an unimplemented pragma, and the source is the
/// only thing that tells the three apart.
#[derive(Debug, Clone, PartialEq)]
pub struct Pragma {
    /// The schema the pragma was qualified with, or `None` for the default
    /// `main`. An unknown one is `unknown database X`, which is the executor's
    /// error to raise because the executor owns the schema list.
    pub schema: Option<String>,
    /// The pragma's name, lowercased. SQLite's pragma table is sorted
    /// lexicographically and looked up exactly, so folding case here is what
    /// makes `PRAGMA PAGE_SIZE` and `PRAGMA page_size` one pragma.
    pub name: String,
    /// What the statement asks for.
    pub body: PragmaBody,
}

/// The three shapes a PRAGMA statement can take.
#[derive(Debug, Clone, PartialEq)]
pub enum PragmaBody {
    /// `PRAGMA name` — read the setting.
    Read,
    /// `PRAGMA name(arg)` — read, with an argument. The argument is a token
    /// that has not been interpreted: `PRAGMA cache_size(-2000)` is a read
    /// carrying `-2000`, not an assignment.
    ReadArg(String),
    /// `PRAGMA name = value` — assign. The value is likewise uninterpreted,
    /// because which pragmas take a boolean, an integer and a name is the
    /// executor's business, not the parser's.
    Set(String),
}

impl Pragma {
    /// The pragma's name, folded to lower case.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The argument, whether it arrived as `(arg)` or as `=arg`.
    pub fn argument(&self) -> Option<&str> {
        match &self.body {
            PragmaBody::ReadArg(a) | PragmaBody::Set(a) => Some(a),
            PragmaBody::Read => None,
        }
    }

    /// Whether this statement assigns rather than reads.
    pub fn is_set(&self) -> bool {
        matches!(self.body, PragmaBody::Set(_))
    }
}

/// A recursive-descent parser for the PRAGMA statement form.
///
/// SQLite parses PRAGMA over raw characters rather than over a token stream,
/// because it has to tell `incomplete input` (a name that runs to the end of
/// the statement) from `near "x": syntax error` (a name followed by something
/// unexpected). Both are reproducible from the token stream the rest of this
/// engine already has, and the tokens are what make the argument's own spelling
/// recoverable, which the `dflt_value` and `=` forms both need.
pub struct PragmaParser<'a> {
    tokens: Vec<(Token, Span)>,
    pos: usize,
    sql: &'a str,
}

impl<'a> PragmaParser<'a> {
    /// Starts a parser over a whole statement, `PRAGMA` keyword included.
    pub fn new(sql: &'a str) -> Result<PragmaParser<'a>> {
        Ok(PragmaParser {
            tokens: Tokenizer::tokenize_all(sql)?,
            pos: 0,
            sql,
        })
    }

    /// Parses the statement. The trailing semicolon is left, because consuming
    /// it is the statement loop's job and it is what separates two of them.
    pub fn parse(&mut self) -> Result<Pragma> {
        if !self.at_pragma() {
            return Err(self.syntax_error());
        }
        self.advance();
        let (schema, name) = self.pragma_name()?;
        let body = if self.at_punct(Punct::Eq) {
            self.advance();
            PragmaBody::Set(self.argument(false)?)
        } else if self.at_punct(Punct::LParen) {
            self.advance();
            // `PRAGMA table_info()` is a syntax error, not a pragma read with
            // an empty argument: sqlite3 answers `near ")": syntax error`.
            if self.at_punct(Punct::RParen) {
                return Err(self.syntax_error());
            }
            let arg = self.argument(true)?;
            // The closing paren is consumed here rather than inside
            // `argument`, so that the two functions agree about who owns the
            // cursor: `argument` stops ON the `)` and the caller takes it.
            if !self.eat_punct(Punct::RParen) {
                return Err(self.syntax_error());
            }
            PragmaBody::ReadArg(arg)
        } else {
            PragmaBody::Read
        };
        // A PRAGMA is a whole statement, so a leftover token is an error rather
        // than the start of the next one.
        if !matches!(self.peek(), None | Some(Token::Punct(Punct::Semicolon))) {
            return Err(self.syntax_error());
        }
        Ok(Pragma { schema, name, body })
    }

    /// Reads `[schema .] name`, keeping the schema's case and folding the
    /// pragma's own.
    fn pragma_name(&mut self) -> Result<(Option<String>, String)> {
        // A PRAGMA that stops at the end of the statement is incomplete:
        // sqlite3 says "incomplete input" for a bare `PRAGMA`, for `PRAGMA;`
        // and for `PRAGMA main.`. A name that is present but is punctuation is
        // a syntax error instead -- `PRAGMA =on` is `near "=": syntax error` --
        // because there is something there to complain about.
        let after = self.pos;
        let Some(first) = self.name_token_opt() else {
            return Err(self.missing_name_error(after));
        };
        if self.at_punct(Punct::Dot) {
            self.advance();
            let after = self.pos;
            let Some(second) = self.name_token_opt() else {
                return Err(self.missing_name_error(after));
            };
            // A third name is not a schema this grammar has.
            if self.at_punct(Punct::Dot) {
                return Err(self.syntax_error());
            }
            return Ok((Some(first), second.to_ascii_lowercase()));
        }
        Ok((None, first.to_ascii_lowercase()))
    }

    /// The error for a name that is not there at all, which is incomplete when
    /// the statement ended and a syntax error when something unusable is.
    ///
    /// `after` is the token index the name would have started at, because the
    /// failed read has already put the cursor back on it and this needs to
    /// report what FOLLOWS the PRAGMA rather than the PRAGMA itself.
    ///
    /// Only the end of input is incomplete. A semicolon is a token that is
    /// present, and sqlite3 names it: `PRAGMA;` is `near ";": syntax error`
    /// while `PRAGMA` alone is `incomplete input`. Verified on 3.53.4 with
    ///   printf 'PRAGMA'     | sqlite3   -> Parse error: incomplete input
    ///   printf 'PRAGMA;'    | sqlite3   -> Parse error: near ";": syntax error
    /// The same holds after a schema qualifier, where `PRAGMA main.;` names the
    /// semicolon while `PRAGMA main.` is incomplete.
    fn missing_name_error(&self, after: usize) -> Error {
        match self.tokens.get(after).map(|(t, _)| t) {
            None => self.incomplete(),
            _ => self.syntax_error_at(after),
        }
    }

    /// Reads one name-shaped token as it was written, or None if the cursor is
    /// not on one.
    ///
    /// A keyword is a name, because a pragma may be called `index` or
    /// `values`; only punctuation, a literal or the end of input is not.
    ///
    /// A single-quoted string is a name too, and this is the one shape that is
    /// easy to miss. SQLite parses a PRAGMA over raw characters, so any of the
    /// four quoting forms names a pragma:
    ///   PRAGMA "page_size"  -> 4096
    ///   PRAGMA [page_size]  -> 4096
    ///   PRAGMA 'page_size'  -> 4096
    /// A single quote is a *string literal* to the rest of this engine's
    /// tokenizer, so it arrives here as `Token::String` with the quotes already
    /// stripped and `''` resolved. `PRAGMA 'x'` is accepted silently (no error,
    /// no rows) and `PRAGMA 'page_size'` returns the real page_size row, so
    /// accepting only Identifier and Keyword would reject statements the real
    /// engine runs -- and a reject here is a syntax error that stops the whole
    /// setup block, which is the exact failure PRAGMA support exists to remove.
    ///
    /// The name is folded to lower case like any other, because the lookup is
    /// case-insensitive: `PRAGMA 'TABLE_INFO'(t)` answers table_info's rows, and
    /// so does `PRAGMA "TABLE_INFO"(t)`.
    fn name_token_opt(&mut self) -> Option<String> {
        match self.advance() {
            Some(Token::Identifier(n)) => Some(n),
            Some(Token::Keyword(k)) => Some(k.as_str().to_string()),
            Some(Token::String(s)) => Some(s),
            _ => {
                self.pos = self.pos.saturating_sub(1);
                None
            }
        }
    }

    /// Reads an argument and returns its source text.
    ///
    /// Both forms read the same thing, and the thing is **one token**. SQLite
    /// parses a PRAGMA over raw characters and takes a single name-shaped run
    /// for the argument, so `PRAGMA table_info(t)` is one name and everything
    /// else is a syntax error at the first thing that is not part of it. All of
    /// these were measured on 3.53.4:
    ///
    /// ```text
    /// PRAGMA x(a b)      -> near "b": syntax error
    /// PRAGMA x(a.b)      -> near ".": syntax error
    /// PRAGMA x(a-b)      -> near "-": syntax error
    /// PRAGMA x(a[0])     -> near "[0]": syntax error
    /// PRAGMA x(NULL)     -> near "NULL": syntax error
    /// PRAGMA x(a(b)c)    -> near "(": syntax error
    /// PRAGMA x((t))      -> near "(": syntax error
    /// PRAGMA x(1)        -> ok        PRAGMA x(-1)    -> ok
    /// PRAGMA x(1.5)      -> ok        PRAGMA x(0x10)  -> ok
    /// PRAGMA x('a b')    -> ok        PRAGMA x("a b") -> ok
    /// ```
    ///
    /// The two that decide the implementation are the sign and the paren. A sign
    /// belongs to the number it is written against -- `cache_size=-2000` is a
    /// `-` and then a literal, and refusing the `-` would reject a pragma the
    /// suite sets constantly -- but only when it is written against it, so
    /// `PRAGMA x(a-b)` still stops at the `-`. An opening paren is never part of
    /// an argument, so `PRAGMA table_info((t))` is `near "(": syntax error`
    /// rather than an argument of `(t)`. Tracking a paren depth here, which is
    /// what this function used to do, accepts the nested form the real engine
    /// rejects.
    ///
    /// `terminated` says whether a `)` is required to close the value. The
    /// `(arg)` form sets it true; the `=` form sets it false, because
    /// `PRAGMA table_info=t` is an assignment whose value is `t` and a closing
    /// paren of nothing at all is not part of it.
    ///
    /// The text is taken from the source rather than rebuilt from the tokens,
    /// because the value's own spelling is what the executor has to see: a
    /// string argument arrives as `'it''s'`, not as the unescaped `it's`, and
    /// `PRAGMA cache_size=-2000` has to arrive as `-2000`.
    fn argument(&mut self, terminated: bool) -> Result<String> {
        if self.tokens.get(self.pos).is_none() {
            // Nothing at all follows the `=` or the `(`.
            return Err(self.incomplete());
        }
        // The value is ONE token, so the argument ends at the gap before the
        // next one. `PRAGMA full_column_names=on extra` takes `on` and then
        // finds `extra` where the statement should have ended, which is
        // `near "extra": syntax error`; taking both as one value would silently
        // accept a statement SQLite rejects.
        //
        // A sign belongs to a NUMBER written against it and to nothing else.
        // `cache_size=-2000` is a `-` and then a literal, and refusing the `-`
        // would reject a pragma the suite sets constantly. But `PRAGMA x(-a)`
        // and `PRAGMA x(-on)` are both a syntax error at the `a`/`on`, and
        // `PRAGMA x(-)` is `near ")"`, so a sign with nothing numeric after it
        // is the error rather than a value. The test is on the token rather
        // than on the gap because `PRAGMA x(- 1)` and `PRAGMA user_version=- 5`
        // both take the `- 1` as -1.
        if matches!(
            self.tokens[self.pos].0,
            Token::Punct(Punct::Minus | Punct::Plus)
        ) {
            return match self.tokens.get(self.pos + 1).map(|(t, _)| t) {
                None => Err(self.incomplete()),
                Some(Token::Integer(_) | Token::Float(_)) => {
                    let start = self.tokens[self.pos].1.start;
                    let end = self.tokens[self.pos + 1].1.end;
                    self.pos += 2;
                    Ok(self.sql[start..end].trim().to_string())
                }
                Some(_) => Err(self.syntax_error_at(self.pos + 1)),
            };
        }
        if !is_a_pragma_value(&self.tokens[self.pos].0) {
            return Err(self.syntax_error_at(self.pos));
        }
        let text = self.sql[self.tokens[self.pos].1.start..self.tokens[self.pos].1.end]
            .trim()
            .to_string();
        self.pos += 1;
        if !terminated {
            return Ok(text);
        }
        // The `(arg)` form has to close its paren, and it has to be the very
        // next token: `PRAGMA table_info(t` and `PRAGMA table_info(` are both
        // "incomplete input", and `PRAGMA table_info(t x)` is a syntax error at
        // the `x`. Whitespace is not a token, so `PRAGMA table_info( t )` closes
        // normally -- the gap is already gone by the time the spans are read.
        match self.tokens.get(self.pos).map(|(t, _)| t) {
            None => Err(self.incomplete()),
            Some(Token::Punct(Punct::RParen)) => Ok(text),
            Some(_) => Err(self.syntax_error_at(self.pos)),
        }
    }

    fn at_pragma(&self) -> bool {
        matches!(self.peek(), Some(Token::Keyword(Keyword::Pragma)))
            || matches!(self.peek(), Some(Token::Identifier(n)) if n.eq_ignore_ascii_case("pragma"))
    }

    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos).map(|(t, _)| t)
    }

    fn advance(&mut self) -> Option<Token> {
        let t = self.tokens.get(self.pos).map(|(t, _)| t.clone());
        if t.is_some() {
            self.pos += 1;
        }
        t
    }

    fn at_punct(&self, p: Punct) -> bool {
        matches!(self.peek(), Some(Token::Punct(q)) if *q == p)
    }

    fn eat_punct(&mut self, p: Punct) -> bool {
        if self.at_punct(p) {
            self.advance();
            true
        } else {
            false
        }
    }

    /// SQLite's wording for a statement that stops early.
    fn incomplete(&self) -> Error {
        Error::new(ResultCode::Error, "incomplete input")
    }

    /// SQLite's wording for a token it cannot use here.
    fn syntax_error(&self) -> Error {
        self.syntax_error_at(self.pos)
    }

    /// SQLite's wording for the token at an index, which is what lets a scan
    /// that has already moved report the token that stopped it rather than the
    /// one the cursor has since reached.
    fn syntax_error_at(&self, at: usize) -> Error {
        let span = self.tokens.get(at).map(|(_, s)| *s).unwrap_or(Span {
            start: 0,
            end: 0,
            line: 1,
            col: 1,
        });
        let found = self.sql.get(span.start..span.end).unwrap_or("");
        Error::new(ResultCode::Error, format!("near \"{found}\": syntax error"))
    }
}

/// Whether a token can be a PRAGMA's name or its argument.
///
/// SQLite reads a PRAGMA over raw characters rather than over the token stream,
/// so the question is not really "is this a token" but "would this text be part
/// of a name". The accepted shapes are a bare name, any of the three quoting
/// forms, a numeric literal, and a single sign glued to one:
///
/// ```text
/// PRAGMA x(a)        -> ok      PRAGMA x(1)      -> ok
/// PRAGMA x('a b')    -> ok      PRAGMA x(0x10)  -> ok
/// PRAGMA x("a b")    -> ok      PRAGMA x(- 1)   -> ok
/// PRAGMA x(a b)      -> near "b"    PRAGMA x(a.b)   -> near "."
/// PRAGMA x(a-b)      -> near "-"    PRAGMA x((t))   -> near "("
/// PRAGMA x(NULL)     -> near "NULL" PRAGMA x(*)     -> near "*"
/// ```
///
/// The one deliberate laxity is keywords. A sweep of all 148 keywords on
/// 3.53.4 shows the name slot and the argument slot accept slightly different
/// sets -- 90 and 93 respectively, overlapping on 89 -- and reproducing two
/// hand-maintained lists buys nothing: the suite only ever passes table names,
/// index names and boolean or numeric values, every one of which this accepts.
/// A keyword that a pragma really is named after, such as `PRAGMA key` or
/// `PRAGMA blob`, is accepted here, which is the case that matters. The
/// rejections that a suite can observe -- `x(a b)`, `x(a.b)`, `x((t))` -- are
/// all punctuation, and all of those are refused.
fn is_a_pragma_value(tok: &Token) -> bool {
    match tok {
        Token::Identifier(_) | Token::Keyword(_) | Token::String(_) => true,
        Token::Integer(_) | Token::Float(_) => true,
        // A sign is a value only with a number after it, which the caller
        // checks; here it is a value so that `PRAGMA x=-1` gets that far.
        Token::Punct(Punct::Minus | Punct::Plus) => true,
        _ => false,
    }
}

/// Parses a PRAGMA statement, `PRAGMA` keyword included.
pub fn parse_pragma(sql: &str) -> Result<Pragma> {
    PragmaParser::new(sql)?.parse()
}

/// Parses a PRAGMA statement out of a script, starting at `from`.
///
/// The connection holds the whole statement text so the argument's own spelling
/// survives; this is the entry point for that.
pub fn parse_pragma_from(sql: &str, from: usize) -> Result<Pragma> {
    let head = sql
        .get(from..)
        .ok_or_else(|| Error::new(ResultCode::Error, "incomplete input"))?;
    parse_pragma(head)
}

// --- the result schemas --------------------------------------------------

/// The result column names of the introspection pragmas.
///
/// These are fixed even when the pragma returns no rows, which is what makes
/// `PRAGMA table_info(nosuchtable)` an empty *result* rather than nothing at
/// all.
pub mod columns {
    /// `PRAGMA table_info(t)`.
    pub const TABLE_INFO: &[&str] = &["cid", "name", "type", "notnull", "dflt_value", "pk"];
    /// `PRAGMA index_list(t)`.
    pub const INDEX_LIST: &[&str] = &["seq", "name", "unique", "origin", "partial"];
    /// `PRAGMA index_info(name)`.
    pub const INDEX_INFO: &[&str] = &["seqno", "cid", "name"];
    /// `PRAGMA foreign_key_list(t)`.
    pub const FOREIGN_KEY_LIST: &[&str] = &[
        "id",
        "seq",
        "table",
        "from",
        "to",
        "on_update",
        "on_delete",
        "match",
    ];
    /// `PRAGMA database_list`.
    pub const DATABASE_LIST: &[&str] = &["seq", "name", "file"];
}

// --- the settings ---------------------------------------------------------

/// The two settings that change what a result column is called.
///
/// These are the only pragmas here that a query can observe, and the two the
/// suite leans on hardest: `select1` reads result-column names out of
/// `execsql2` and compares them verbatim, so `f1`, `test1.f1` and
/// `test1 . f1` are three different tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColumnNameFlags {
    /// `PRAGMA full_column_names`, default off.
    pub full: bool,
    /// `PRAGMA short_column_names`, default on.
    pub short: bool,
}

impl Default for ColumnNameFlags {
    fn default() -> ColumnNameFlags {
        // SQLite's own default, from the comment above
        // `sqlite3GenerateColumnNames`: "short=ON, full=OFF".
        ColumnNameFlags {
            full: false,
            short: true,
        }
    }
}

impl ColumnNameFlags {
    /// Whether a result column that refers directly to a table column is named
    /// `table.column` rather than `column`.
    ///
    /// This is `full` alone: with `short` on and `full` off the name is the
    /// bare column, and turning `full` on makes it qualified even though
    /// `short` is still on.
    pub fn qualified_direct(&self) -> bool {
        self.full
    }

    /// Whether a direct reference is named from the schema at all.
    ///
    /// This is `srcName` in `sqlite3GenerateColumnNames`, which is
    /// `short || full` — the guard on the whole direct-reference branch. With
    /// SQLite's default of `short=on` it is almost always true, and the case
    /// that reaches the source text instead is a query that has turned BOTH
    /// settings off.
    pub fn names_direct(&self) -> bool {
        self.short || self.full
    }

    /// Whether a `*` expansion qualifies each column with its table.
    ///
    /// Here `short` does win: `longNames` in `select.c` is
    /// `FullColNames && !ShortColNames`, so an expansion stays bare while
    /// `short=ON` even with `full` set. The suite depends on the difference:
    /// `select1-6.9.13` runs a join with both settings on and expects `f1`
    /// twice from the bare names, while `select1-6.9.6` runs a star with
    /// `short=OFF, full=ON` and expects `a.f1` twice.
    pub fn qualified_star(&self) -> bool {
        self.full && !self.short
    }
}

/// The name a result column gets under the given settings.
///
/// The three cases, in SQLite's order of precedence: an explicit alias, which
/// always wins; a direct table-column reference, which gets `column` or
/// `table.column`; and otherwise the expression's source text, which is what
/// makes the three spellings of a join column all distinct.
///
/// `table` is the name of the table the column belongs to **as the schema
/// declares it**, not the alias the query used — SQLite qualifies with the
/// table, so `SELECT t.f1 FROM test1 t` under `full` answers `test1.f1`, not
/// `t.f1`. `column` is likewise the declared name, so a query that spells it in
/// lower case still gets back the case the schema wrote.
pub fn column_name(
    flags: ColumnNameFlags,
    alias: Option<&str>,
    table: Option<&str>,
    column: Option<&str>,
    source: &str,
) -> String {
    if let Some(a) = alias {
        return a.to_string();
    }
    // `srcName` in sqlite3GenerateColumnNames is `short || full`, and the
    // whole direct-reference branch is guarded by it. A direct reference
    // therefore names itself from the SCHEMA whenever either setting is on --
    // which, with SQLite's default of short=on, is almost always -- and only
    // falls through to the source text when both are off. The two questions
    // are asked of the flag struct rather than re-derived from `flags.short`
    // and `flags.full` here, so that the helpers which document the rule are
    // the ones the rule actually runs through.
    let (Some(t), Some(c)) = (table, column) else {
        return source.to_string();
    };
    if !flags.names_direct() {
        return source.to_string();
    }
    if flags.qualified_direct() {
        return format!("{t}.{c}");
    }
    c.to_string()
}

/// The result-column name a `*` expansion gives one column.
///
/// The expansion does not go through [`column_name`]: it is qualified by the
/// *source* the column was reached through — the alias where there is one —
/// and only when [`ColumnNameFlags::qualified_star`] says so. Otherwise it is
/// the bare column name.
pub fn star_column_name(flags: ColumnNameFlags, source: &str, column: &str) -> String {
    if flags.qualified_star() {
        format!("{source}.{column}")
    } else {
        column.to_string()
    }
}

/// The journal modes `PRAGMA journal_mode` can report.
///
/// SQLite has six; this engine's pager has a rollback journal, so `delete` and
/// `memory` are the whole set. `Memory` is here because an in-memory database
/// reports it, whatever was asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JournalMode {
    Delete,
    Memory,
}

impl JournalMode {
    /// The spelling SQLite reports, which is lower case.
    pub fn as_str(self) -> &'static str {
        match self {
            JournalMode::Delete => "delete",
            JournalMode::Memory => "memory",
        }
    }

    /// Reads a mode name the way `PRAGMA journal_mode=X` does, which is to say
    /// that an unrecognised name leaves the mode alone rather than failing.
    pub fn parse(name: &str) -> Option<JournalMode> {
        if name.eq_ignore_ascii_case("delete") {
            Some(JournalMode::Delete)
        } else if name.eq_ignore_ascii_case("memory") {
            Some(JournalMode::Memory)
        } else {
            None
        }
    }
}

/// What a connection holds for the pragmas that have state.
///
/// Only settings a query can observe are kept. A pragma outside this set is
/// accepted and discarded, which is what makes `PRAGMA trusted_schema=on` a
/// no-op rather than an error — and that is the behaviour which unblocks the
/// suite files that stop at their first PRAGMA.
#[derive(Debug, Clone, PartialEq)]
pub struct PragmaState {
    /// The result-column naming settings.
    pub column_names: ColumnNameFlags,
    /// `PRAGMA user_version`, stored in the database header.
    pub user_version: i64,
    /// `PRAGMA application_id`, stored in the database header.
    pub application_id: i64,
    /// `PRAGMA journal_mode`.
    pub journal_mode: JournalMode,
}

impl Default for PragmaState {
    fn default() -> PragmaState {
        PragmaState {
            column_names: ColumnNameFlags::default(),
            user_version: 0,
            application_id: 0,
            journal_mode: JournalMode::Delete,
        }
    }
}

/// Reads a boolean the way `sqlite3GetBoolean` does, which is what the flag
/// pragmas are assigned through.
///
/// Not "anything that is not `no` is yes": the yes-spellings are `on`, `yes`,
/// `true` and any non-zero number, the no-spellings are `off`, `no`, `false`
/// and zero, and **anything else is false**. `PRAGMA full_column_names=bogus`
/// turns the setting off rather than raising an error, which is what sqlite3
/// does and what the suite's `catch` expects.
pub fn parse_bool(value: &str) -> bool {
    let v = value.trim();
    if v.is_empty() {
        return false;
    }
    if let Ok(n) = v.parse::<i64>() {
        return n != 0;
    }
    if let Ok(f) = v.parse::<f64>() {
        return f != 0.0;
    }
    v.eq_ignore_ascii_case("yes") || v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("on")
}

/// Parses an integer setting, the way the numeric pragmas are assigned.
///
/// A value that is not an integer is not an error: SQLite ignores the
/// assignment and leaves the setting alone, which is what
/// `PRAGMA user_version=abc` does.
pub fn parse_int(value: &str) -> Option<i64> {
    value.trim().parse::<i64>().ok()
}

// --- the schema reader ----------------------------------------------------

/// The keywords that end a column's declared type.
///
/// A declared type is a run of words, and the only thing that stops the run is
/// a constraint or the end of the definition. This set is the column-level
/// half of that rule; `Keyword::starts_clause` is the statement-level half and
/// does not include these, because a `SELECT` never has to tell a type from a
/// constraint.
const ENDS_A_TYPE: &[Keyword] = &[
    Keyword::Primary,
    Keyword::Not,
    Keyword::Null,
    Keyword::Unique,
    Keyword::Check,
    Keyword::Default,
    Keyword::Collate,
    Keyword::References,
    Keyword::Constraint,
    Keyword::Autoincrement,
    Keyword::Asc,
    Keyword::Desc,
    Keyword::On,
    Keyword::Conflict,
    Keyword::Foreign,
    Keyword::Generated,
    Keyword::Always,
    Keyword::As,
    Keyword::If,
    Keyword::Deferrable,
    Keyword::Initially,
    Keyword::Match,
    Keyword::Action,
    Keyword::Before,
    Keyword::After,
];

/// The keywords that end a DEFAULT expression.
///
/// A default is an expression, so it ends where the next constraint begins
/// rather than where the next comma is: `b TEXT DEFAULT 'x' NOT NULL` has a
/// default of `'x'` and a NOT NULL after it.
const ENDS_A_DEFAULT: &[Keyword] = &[
    Keyword::Primary,
    Keyword::Not,
    Keyword::Null,
    Keyword::Unique,
    Keyword::Check,
    Keyword::Collate,
    Keyword::References,
    Keyword::Constraint,
    Keyword::Generated,
    Keyword::As,
];

/// Strips the quoting from a name the tokenizer handed through whole.
///
/// The tokenizer resolves the four quoting forms and keeps the name's case, so
/// by the time a name arrives here a quoted one still carries its delimiters.
/// SQLite reports a quoted declared type without them — `c "Weird Type"` has
/// the type `Weird Type` — which is the only place this is needed, since a
/// quoted COLUMN name is reported with whatever case it had and the quoting
/// does not change it.
fn unquote(n: &str) -> String {
    n.strip_prefix(['"', '`', '['])
        .and_then(|w| w.strip_suffix(['"', '`', ']']))
        .unwrap_or(n)
        .to_string()
}

/// One column of a table, as `PRAGMA table_info` sees it.
///
/// Every field is a reading of the schema's own text, because that is the only
/// place the original spelling survives. `pk` is the column's 1-based position
/// in the primary key, which is why a composite key answers 1 then 2 rather
/// than 1 then 1.
#[derive(Debug, Clone, PartialEq)]
pub struct ColumnInfo {
    pub name: String,
    /// The declared type, as written, minus the quoting. Empty when the column
    /// has no declared type.
    pub ty: String,
    pub not_null: bool,
    /// The DEFAULT expression as written, without the `DEFAULT` keyword.
    pub default: Option<String>,
    /// The column's 1-based position in the primary key, or 0 for a column
    /// that is not part of one.
    pub pk: u8,
}

/// A foreign key, as `PRAGMA foreign_key_list` sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct ForeignKeyInfo {
    /// Which constraint this is, counting from 0.
    pub id: i64,
    /// The referenced table, as written.
    pub table: String,
    /// The referencing columns and the referenced columns, paired. An empty
    /// `to` is a reference to the primary key with no column list written.
    pub columns: Vec<(String, String)>,
    pub on_update: String,
    pub on_delete: String,
}

/// One secondary index, as `PRAGMA index_list` sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct IndexInfo {
    pub name: String,
    /// Where the index came from: `c` for CREATE INDEX, `u` for a UNIQUE
    /// constraint, `pk` for a WITHOUT ROWID table's primary key.
    pub origin: &'static str,
    pub unique: bool,
    /// Whether the index was declared with a WHERE clause.
    pub partial: bool,
}

/// Reads a `CREATE TABLE` statement back out of the text the schema stores.
///
/// This is a small scanner over the statement, not a second SQL parser: it
/// knows the one statement it is given and nothing else, and it is only ever
/// pointed at text that already parsed once. What it is for is the spelling —
/// the declared type in the case it was written, the default with its leading
/// zero, the primary key's column list the engine's own parser discards.
pub struct SchemaText<'a> {
    sql: &'a str,
    tokens: Vec<(Token, Span)>,
    pos: usize,
}

impl<'a> SchemaText<'a> {
    /// Starts a reader over a `CREATE TABLE` statement.
    pub fn new(sql: &'a str) -> Result<SchemaText<'a>> {
        Ok(SchemaText {
            sql,
            tokens: Tokenizer::tokenize_all(sql)?,
            pos: 0,
        })
    }

    /// The columns of the table, in declaration order.
    pub fn columns(&self) -> Vec<ColumnInfo> {
        let mut p = self.clone_cursor();
        // CREATE [TEMP|TEMPORARY] TABLE [IF NOT EXISTS] name [( ... )]
        p.skip_until_paren();
        let mut out: Vec<ColumnInfo> = Vec::new();
        let mut constraints: Vec<(Vec<String>, bool)> = Vec::new();
        while let Some((tok, span)) = p.tokens.get(p.pos).cloned() {
            if matches!(tok, Token::Punct(Punct::RParen)) {
                break;
            }
            if matches!(tok, Token::Punct(Punct::Comma)) {
                p.pos += 1;
                continue;
            }
            // A table-level constraint is not a column. It is recognised by the
            // keyword that starts it, and it may be preceded by CONSTRAINT
            // and a name.
            if p.at_table_constraint() {
                if let Some(pk) = p.table_primary_key() {
                    constraints.push(pk);
                }
                p.skip_to_next_top_level_comma();
                continue;
            }
            if let Some(col) = p.column() {
                out.push(col);
            } else {
                p.pos += 1;
                let _ = span;
            }
        }
        // A table-level PRIMARY KEY(x, y) numbers its columns; without one, a
        // column-level PRIMARY KEY is position 1. A column that is not part of
        // any primary key keeps 0.
        for c in &mut out {
            for (cols, _) in &constraints {
                if let Some(i) = cols.iter().position(|n| n.eq_ignore_ascii_case(&c.name)) {
                    c.pk = (i + 1) as u8;
                }
            }
        }
        out
    }

    /// The foreign keys, in the order SQLite reports them.
    ///
    /// The order is the one that decides `select1`'s expectations and is not
    /// the order they were written: SQLite walks the column-level references
    /// **backwards**, so a table-level `FOREIGN KEY(c) REFERENCES p(y)` is
    /// reported before the column-level `b REFERENCES p(x)` that preceded it in
    /// the statement, while two table-level constraints keep their written
    /// order. A composite key takes one `id` across all of its columns, and
    /// each column pair is one row with `seq` counting within the constraint.
    pub fn foreign_keys(&self) -> Vec<ForeignKeyInfo> {
        let mut p = self.clone_cursor();
        p.skip_until_paren();
        let mut table_level: Vec<ForeignKeyInfo> = Vec::new();
        let mut column_level: Vec<(String, ForeignKeyInfo)> = Vec::new();
        while let Some((tok, _)) = p.tokens.get(p.pos).cloned() {
            if matches!(tok, Token::Punct(Punct::RParen)) {
                break;
            }
            if matches!(tok, Token::Punct(Punct::Comma)) {
                p.pos += 1;
                continue;
            }
            if p.at_table_constraint() {
                if let Some(fk) = p.table_foreign_key() {
                    table_level.push(fk);
                }
                p.skip_to_next_top_level_comma();
                continue;
            }
            // A column definition is consumed whole whether or not it carried a
            // REFERENCES, so the cursor is not rewound on a miss.
            if let Some((name, fk)) = p.column_with_foreign_key() {
                column_level.push((name, fk));
            } else {
                p.pos += 1;
            }
        }
        // SQLite reports a table-level FOREIGN KEY BEFORE the column-level
        // ones, and the column-level ones in reverse of the order they were
        // written. Checked on 3.53.4 with
        //   CREATE TABLE m2(a REFERENCES p, b REFERENCES p,
        //                     FOREIGN KEY (a,b) REFERENCES p(x,y));
        // which answers ids 0 (the table-level, two rows), 1 (column b) and
        // 2 (column a).
        let mut out: Vec<ForeignKeyInfo> = table_level;
        out.extend(column_level.into_iter().rev().map(|(_, fk)| fk));
        for (i, fk) in out.iter_mut().enumerate() {
            fk.id = i as i64;
        }
        out
    }

    /// The index the statement creates implicitly, if it creates one.
    ///
    /// `CREATE TABLE t(a UNIQUE)` creates `sqlite_autoindex_t_1`, whose
    /// `origin` is `u`. A `WITHOUT ROWID` table's primary key is `pk`. This
    /// engine does not build either, so the reader reports them and the
    /// executor decides.
    pub fn implicit_index(&self) -> Option<IndexInfo> {
        let mut p = self.clone_cursor();
        p.skip_until_paren();
        let name = p.table_name()?;
        let without_rowid = p.tokens.iter().any(|(_, s)| {
            p.sql
                .get(s.start..s.end)
                .is_some_and(|w| w.eq_ignore_ascii_case("rowid"))
        }) && p
            .tokens
            .iter()
            .any(|(_, s)| p.sql.get(s.start..s.end) == Some("WITHOUT"));
        while let Some((tok, _)) = p.tokens.get(p.pos).cloned() {
            if matches!(tok, Token::Punct(Punct::RParen)) {
                break;
            }
            if matches!(tok, Token::Punct(Punct::Comma)) {
                p.pos += 1;
                continue;
            }
            if p.at_table_constraint() {
                if p.table_primary_key().is_some() && without_rowid {
                    return Some(IndexInfo {
                        name: format!("sqlite_autoindex_{name}_1"),
                        origin: "pk",
                        unique: true,
                        partial: false,
                    });
                }
                p.skip_to_next_top_level_comma();
                continue;
            }
            let start = p.pos;
            let (_, uniq) = p.column_with_unique().unwrap_or_default();
            if uniq {
                return Some(IndexInfo {
                    name: format!("sqlite_autoindex_{name}_1"),
                    origin: "u",
                    unique: true,
                    partial: false,
                });
            }
            p.pos = start + 1;
        }
        None
    }

    fn clone_cursor(&self) -> SchemaText<'a> {
        SchemaText {
            sql: self.sql,
            tokens: self.tokens.clone(),
            pos: self.pos,
        }
    }

    /// The table's own name, which a reader of the implicit index needs.
    fn table_name(&self) -> Option<String> {
        let mut p = self.clone_cursor();
        p.skip_until_paren();
        // The name is the token before the opening paren, which the table
        // qualifier (if any) sits in front of.
        let mut i = p.pos;
        while i > 0 {
            i -= 1;
            if let Some((Token::Identifier(n), _)) = p.tokens.get(i) {
                return Some(n.clone());
            }
        }
        None
    }

    fn skip_until_paren(&mut self) {
        while let Some((tok, _)) = self.tokens.get(self.pos) {
            if matches!(tok, Token::Punct(Punct::LParen)) {
                return;
            }
            self.pos += 1;
        }
    }

    fn skip_to_next_top_level_comma(&mut self) {
        let mut depth = 0i32;
        while let Some((tok, _)) = self.tokens.get(self.pos) {
            match tok {
                Token::Punct(Punct::LParen) => depth += 1,
                Token::Punct(Punct::RParen) if depth == 0 => return,
                Token::Punct(Punct::RParen) => depth -= 1,
                Token::Punct(Punct::Comma) if depth == 0 => {
                    self.pos += 1;
                    return;
                }
                _ => {}
            }
            self.pos += 1;
        }
    }

    /// Whether the cursor is on a table-level constraint rather than a column.
    fn at_table_constraint(&self) -> bool {
        let mut i = self.pos;
        // CONSTRAINT <name> precedes one.
        if let Some(Token::Keyword(Keyword::Constraint)) = self.tokens.get(i).map(|(t, _)| t) {
            i += 2;
        }
        matches!(
            self.tokens.get(i).map(|(t, _)| t),
            Some(Token::Keyword(
                Keyword::Primary | Keyword::Unique | Keyword::Check | Keyword::Foreign
            ))
        )
    }

    /// Reads a column definition.
    fn column(&mut self) -> Option<ColumnInfo> {
        let span = self.span();
        let name = match self.tokens.get(self.pos)? {
            (Token::Identifier(_), _) | (Token::Keyword(_), _) => self.text(span),
            _ => return None,
        };
        self.pos += 1;
        let ty = self.declared_type();
        let mut not_null = false;
        let mut default = None;
        let mut pk = false;
        loop {
            match self.tokens.get(self.pos).map(|(t, _)| t.clone()) {
                Some(Token::Keyword(Keyword::Not)) => {
                    self.pos += 1;
                    if matches!(
                        self.tokens.get(self.pos).map(|(t, _)| t),
                        Some(Token::Keyword(Keyword::Null))
                    ) {
                        self.pos += 1;
                        not_null = true;
                    }
                }
                Some(Token::Keyword(Keyword::Null)) => {
                    self.pos += 1;
                }
                Some(Token::Keyword(Keyword::Primary)) => {
                    self.pos += 1;
                    self.eat_word("key");
                    // PRIMARY KEY DESC makes a column an ordinary one, and
                    // INTEGER PRIMARY KEY DESC is not the rowid alias.
                    if self.eat_word("desc") {
                        pk = false;
                    } else {
                        self.eat_word("asc");
                        self.eat_word("autoincrement");
                        pk = true;
                    }
                }
                Some(Token::Keyword(Keyword::Default)) => {
                    self.pos += 1;
                    default = Some(self.default_text());
                }
                Some(Token::Keyword(Keyword::Unique)) => {
                    self.pos += 1;
                }
                Some(Token::Keyword(Keyword::Check)) => {
                    self.pos += 1;
                    self.skip_parens();
                }
                Some(Token::Keyword(Keyword::Collate)) => {
                    self.pos += 1;
                    self.pos += 1;
                }
                Some(Token::Keyword(Keyword::References)) => {
                    self.pos += 1;
                    // The reference is a constraint rather than part of the
                    // column, but it is still part of the DEFINITION, so the
                    // whole of it is consumed here. Leaving it would restart
                    // the scan inside it, and `p1` and `ON` would be read as
                    // the next column's name and type.
                    let _ = self.name();
                    let _ = self.name_list_if_parenthesised();
                    let _ = self.actions();
                }
                Some(Token::Keyword(Keyword::Constraint)) => {
                    self.pos += 2;
                }
                _ => break,
            }
        }
        Some(ColumnInfo {
            name,
            ty,
            not_null,
            default,
            pk: u8::from(pk),
        })
    }

    /// Reads a declared type, keeping the spelling it was written with.
    ///
    /// The type may be several words, and may carry a length in parentheses:
    /// `VARCHAR(20)`, `UNSIGNED BIG INT`, and `Weird Type` for a quoted one.
    fn declared_type(&mut self) -> String {
        // The words of the type, kept separate from their length so the two
        // join with a space and the length joins without one: SQLite reports
        // `UNSIGNED BIG INT` and `VARCHAR(20)`, and a source that wrote
        // `VARCHAR (20)` still reports without the space.
        let mut words: Vec<String> = Vec::new();
        let mut length: Option<String> = None;
        // A type written as a bare `(20)` is legal, so the words are optional.
        if matches!(
            self.tokens.get(self.pos).map(|(t, _)| t),
            Some(Token::Punct(Punct::LParen))
        ) {
            return self.type_length();
        }
        while let Some((tok, span)) = self.tokens.get(self.pos).cloned() {
            match &tok {
                // SQLite reserves no type name -- `TEXT` and `INTEGER` are
                // ordinary identifiers, and a type may be any words at all --
                // so the question is not "is this a type" but "does this end
                // the declaration". A column's type ends at a constraint
                // keyword, at the comma, or at the closing paren.
                Token::Keyword(k) if ENDS_A_TYPE.contains(k) => break,
                Token::Punct(Punct::Comma) | Token::Punct(Punct::RParen) => break,
                // A quoted name is a type, and SQLite reports it as the bare
                // words without the quotes. The tokenizer hands a quoted
                // identifier through as one `Identifier` whose text contains a
                // space, so the quotes are stripped rather than the token
                // refused: `c "Weird Type"` is a column c of that type.
                Token::Identifier(n) if n.contains(' ') => {
                    words.push(unquote(n));
                    self.pos += 1;
                }
                Token::Identifier(_) | Token::Keyword(_) => {
                    // The source text, not the token's text: a bare word is
                    // folded to lower case by the tokenizer, and SQLite reports
                    // the type in the case the schema wrote it.
                    words.push(self.text(span));
                    self.pos += 1;
                }
                // A single-quoted type name arrives as a string token, and
                // SQLite reports it as the words without the quotes.
                Token::String(s) => {
                    words.push(s.clone());
                    self.pos += 1;
                }
                // The length that may follow the words.
                Token::Punct(Punct::LParen) => {
                    length = Some(self.type_length());
                    break;
                }
                _ => break,
            }
        }
        let mut out = words.join(" ");
        if let Some(l) = length {
            out.push_str(&l);
        }
        out.trim().to_string()
    }

    /// Reads a `( ... )` type length, including the parentheses, and consumes
    /// it. A length SQLite cannot parse is still consumed, so a malformed one
    /// does not strand the scan.
    fn type_length(&mut self) -> String {
        self.pos += 1;
        let start_span = self.tokens.get(self.pos.saturating_sub(1)).map(|(_, s)| *s);
        let Some(open) = start_span else {
            return String::new();
        };
        let mut end = open.end;
        let mut depth = 1i32;
        while let Some((tok, span)) = self.tokens.get(self.pos).cloned() {
            match tok {
                Token::Punct(Punct::LParen) => depth += 1,
                Token::Punct(Punct::RParen) => {
                    depth -= 1;
                    self.pos += 1;
                    if depth == 0 {
                        end = span.end;
                        break;
                    }
                    continue;
                }
                _ => {}
            }
            end = span.end;
            self.pos += 1;
        }
        self.sql.get(open.start..end).unwrap_or("").to_string()
    }

    /// Reads a DEFAULT expression's source text, without the parentheses when
    /// the statement wrapped it — SQLite reports `1+2` for `DEFAULT (1+2)`.
    fn default_text(&mut self) -> String {
        let start = match self.tokens.get(self.pos) {
            Some((_, s)) => s.start,
            None => return String::new(),
        };
        let mut end = self.tokens[self.pos].1.end;
        let wrapped = matches!(
            self.tokens.get(self.pos).map(|(t, _)| t),
            Some(Token::Punct(Punct::LParen))
        );
        let mut i = self.pos;
        let mut depth = 0i32;
        while let Some((tok, span)) = self.tokens.get(i).cloned() {
            match tok {
                Token::Punct(Punct::RParen) if depth == 0 => break,
                Token::Punct(Punct::RParen) => depth -= 1,
                Token::Punct(Punct::LParen) => depth += 1,
                // A DEFAULT runs until the next constraint, not until the next
                // comma: `b TEXT DEFAULT 'x' NOT NULL` has the default `'x'`
                // and a NOT NULL after it, and reporting `'x' NOT NULL` as
                // the default would be wrong on both counts.
                Token::Keyword(k) if depth == 0 && ENDS_A_DEFAULT.contains(&k) => break,
                _ => {}
            }
            if depth == 0 && matches!(tok, Token::Punct(Punct::Comma)) {
                break;
            }
            end = span.end;
            i += 1;
        }
        self.pos = i;
        let text = self.sql.get(start..end).unwrap_or("").trim().to_string();
        if wrapped && text.starts_with('(') && text.ends_with(')') {
            text[1..text.len() - 1].trim().to_string()
        } else {
            text
        }
    }

    /// Reads a table-level `PRIMARY KEY (...)` and returns its columns.
    fn table_primary_key(&mut self) -> Option<(Vec<String>, bool)> {
        if !matches!(
            self.tokens.get(self.pos).map(|(t, _)| t),
            Some(Token::Keyword(Keyword::Primary))
        ) {
            return None;
        }
        self.pos += 1;
        self.eat_word("key");
        // The column list is what `pk` is read from, so it is read rather than
        // skipped. This is the list the engine's own parser discards, which is
        // why `PRAGMA table_info` is answered from the schema text.
        let cols = self.name_list();
        // The conflict clause and the sort order after the list do not change
        // which columns are in the key.
        self.eat_word("on");
        self.eat_word("conflict");
        self.eat_word("asc");
        self.eat_word("desc");
        Some((cols, false))
    }

    /// Reads a table-level `FOREIGN KEY (...) REFERENCES t(...) ...`.
    fn table_foreign_key(&mut self) -> Option<ForeignKeyInfo> {
        if !matches!(
            self.tokens.get(self.pos).map(|(t, _)| t),
            Some(Token::Keyword(Keyword::Foreign))
        ) {
            return None;
        }
        self.pos += 1;
        self.eat_word("key");
        let from = self.name_list();
        if !self.eat_word("references") {
            return None;
        }
        let table = self.next_name();
        let to = self.name_list_if_parenthesised();
        let (on_update, on_delete) = self.actions();
        Some(ForeignKeyInfo {
            id: 0,
            table,
            // A target with no column list means its primary key, and SQLite
            // reports that as one row per referencing column whose `to` is
            // EMPTY. Pairing the two lists directly would drop the row instead.
            columns: match to.is_empty() {
                true => from.into_iter().map(|f| (f, String::new())).collect(),
                false => from.into_iter().zip(to).collect(),
            },
            on_update,
            on_delete,
        })
    }

    /// Reads a column definition, returning its name and any REFERENCES or
    /// UNIQUE constraint it carried.
    fn column_with_foreign_key(&mut self) -> Option<(String, ForeignKeyInfo)> {
        let start = self.pos;
        let name = self.name()?;
        self.declared_type();
        let mut fk: Option<ForeignKeyInfo> = None;
        let mut unique = false;
        loop {
            match self.tokens.get(self.pos).map(|(t, _)| t.clone()) {
                Some(Token::Keyword(Keyword::References)) => {
                    self.pos += 1;
                    let table = self.next_name();
                    let to = self.name_list_if_parenthesised();
                    let (on_update, on_delete) = self.actions();
                    fk = Some(ForeignKeyInfo {
                        id: 0,
                        table,
                        // A reference with no column list means the target's
                        // primary key, and SQLite reports that as one row whose
                        // `to` is EMPTY rather than as no row at all. So the
                        // pair always exists; only its second half is blank.
                        columns: if to.is_empty() {
                            vec![(name.clone(), String::new())]
                        } else {
                            to.into_iter().map(|t| (name.clone(), t)).collect()
                        },
                        on_update,
                        on_delete,
                    });
                }
                Some(Token::Keyword(Keyword::Unique)) => {
                    unique = true;
                    self.pos += 1;
                }
                Some(Token::Keyword(Keyword::Primary)) => {
                    self.pos += 1;
                    self.eat_word("key");
                }
                Some(Token::Keyword(Keyword::Not)) => {
                    self.pos += 1;
                }
                Some(Token::Keyword(Keyword::Null)) => {
                    self.pos += 1;
                }
                Some(Token::Keyword(Keyword::Default)) => {
                    self.pos += 1;
                    let _ = self.default_text();
                }
                Some(Token::Keyword(Keyword::Check)) => {
                    self.pos += 1;
                    self.skip_parens();
                }
                Some(Token::Keyword(Keyword::Collate)) => {
                    self.pos += 2;
                }
                Some(Token::Keyword(Keyword::Constraint)) => {
                    self.pos += 2;
                }
                _ => break,
            }
        }
        if let Some(f) = fk {
            return Some((name, f));
        }
        let _ = unique;
        let _ = start;
        // No REFERENCES, but the definition has still been consumed. The
        // cursor is left after the column either way, because a caller that
        // rewound on `None` would restart in the middle of this definition and
        // read the constraint keywords as the next column's name and type.
        None
    }

    /// Reads a column definition, returning whether it carried UNIQUE.
    fn column_with_unique(&mut self) -> Option<(String, bool)> {
        let name = self.name()?;
        self.declared_type();
        let mut unique = false;
        loop {
            match self.tokens.get(self.pos).map(|(t, _)| t.clone()) {
                Some(Token::Keyword(Keyword::Unique)) => {
                    unique = true;
                    self.pos += 1;
                }
                Some(Token::Keyword(Keyword::Primary)) => {
                    self.pos += 1;
                    self.eat_word("key");
                }
                Some(Token::Keyword(Keyword::Not)) | Some(Token::Keyword(Keyword::Null)) => {
                    self.pos += 1;
                }
                Some(Token::Keyword(Keyword::Default)) => {
                    self.pos += 1;
                    let _ = self.default_text();
                }
                Some(Token::Keyword(Keyword::Check)) => {
                    self.pos += 1;
                    self.skip_parens();
                }
                Some(Token::Keyword(Keyword::Collate)) => {
                    self.pos += 2;
                }
                Some(Token::Keyword(Keyword::References)) => {
                    self.pos += 1;
                    let _ = self.next_name();
                    let _ = self.name_list_if_parenthesised();
                    let _ = self.actions();
                }
                _ => break,
            }
        }
        Some((name, unique))
    }

    /// Reads the ON UPDATE and ON DELETE actions, defaulting to NO ACTION.
    fn actions(&mut self) -> (String, String) {
        let mut on_update = "NO ACTION".to_string();
        let mut on_delete = "NO ACTION".to_string();
        loop {
            if self.eat_word("on") {
                if self.eat_word("update") {
                    on_update = self.action_name();
                    continue;
                }
                if self.eat_word("delete") {
                    on_delete = self.action_name();
                    continue;
                }
                // `ON` followed by neither is not an action clause; rewind so
                // the rest of the scan sees it again.
                self.pos -= 1;
                break;
            }
            if self.eat_word("match") {
                let _ = self.name();
                continue;
            }
            if self.eat_word("deferrable") {
                continue;
            }
            if self.eat_word("not") {
                let _ = self.eat_word("deferrable");
                continue;
            }
            if self.eat_word("initially") {
                self.pos += 1;
                continue;
            }
            break;
        }
        (on_update, on_delete)
    }

    /// The action name after ON UPDATE or ON DELETE, defaulting to SET NULL
    /// when the statement wrote one that SQLite has no other name for.
    fn action_name(&mut self) -> String {
        for name in [
            "SET NULL",
            "SET DEFAULT",
            "CASCADE",
            "RESTRICT",
            "NO ACTION",
        ] {
            if self.eat_action(name) {
                return name.to_string();
            }
        }
        "NO ACTION".to_string()
    }

    /// Consumes an action spelled as its own words, e.g. `SET NULL`.
    fn eat_action(&mut self, action: &str) -> bool {
        let words: Vec<&str> = action.split(' ').collect();
        let start = self.pos;
        for (i, w) in words.iter().enumerate() {
            match self.tokens.get(self.pos) {
                Some((Token::Identifier(n), _)) if n.eq_ignore_ascii_case(w) => self.pos += 1,
                Some((Token::Keyword(k), _)) if k.as_str().eq_ignore_ascii_case(w) => self.pos += 1,
                _ => {
                    let _ = i;
                    self.pos = start;
                    return false;
                }
            }
        }
        true
    }

    /// Reads `( a, b )` if the cursor is on a parenthesis.
    fn name_list_if_parenthesised(&mut self) -> Vec<String> {
        if !matches!(
            self.tokens.get(self.pos).map(|(t, _)| t),
            Some(Token::Punct(Punct::LParen))
        ) {
            return Vec::new();
        }
        self.name_list()
    }

    /// Reads a parenthesised, comma-separated name list.
    fn name_list(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        if !matches!(
            self.tokens.get(self.pos).map(|(t, _)| t),
            Some(Token::Punct(Punct::LParen))
        ) {
            return out;
        }
        self.pos += 1;
        loop {
            match self.tokens.get(self.pos).map(|(t, _)| t.clone()) {
                Some(Token::Punct(Punct::RParen)) | None => {
                    self.pos += 1;
                    break;
                }
                Some(Token::Punct(Punct::Comma)) => self.pos += 1,
                _ => match self.name() {
                    Some(n) => out.push(n),
                    None => self.pos += 1,
                },
            }
        }
        out
    }

    /// Skips a balanced parenthesised group, including its opening paren.
    fn skip_parens(&mut self) {
        if !matches!(
            self.tokens.get(self.pos).map(|(t, _)| t),
            Some(Token::Punct(Punct::LParen))
        ) {
            return;
        }
        let mut depth = 0i32;
        while let Some((tok, _)) = self.tokens.get(self.pos) {
            match tok {
                Token::Punct(Punct::LParen) => depth += 1,
                Token::Punct(Punct::RParen) => {
                    depth -= 1;
                    self.pos += 1;
                    if depth == 0 {
                        return;
                    }
                    continue;
                }
                _ => {}
            }
            self.pos += 1;
        }
    }

    /// Consumes the next token if it is the given word, in either case and
    /// whether it arrives as a keyword or a bare identifier.
    fn eat_word(&mut self, word: &str) -> bool {
        let matches_word = match self.tokens.get(self.pos).map(|(t, _)| t) {
            Some(Token::Identifier(n)) => n.eq_ignore_ascii_case(word),
            Some(Token::Keyword(k)) => k.as_str().eq_ignore_ascii_case(word),
            _ => false,
        };
        if matches_word {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    /// Reads the next name, consuming it.
    fn name(&mut self) -> Option<String> {
        let t = self.tokens.get(self.pos).map(|(t, _)| t.clone())?;
        let out = match &t {
            Token::Identifier(n) => n.clone(),
            Token::Keyword(k) => k.as_str().to_string(),
            _ => return None,
        };
        self.pos += 1;
        Some(out)
    }

    /// The next name, or an error if there is not one. Used where the
    /// statement is known to spell one.
    fn next_name(&mut self) -> String {
        self.name().unwrap_or_default()
    }

    /// The source text a token was written as.
    fn text(&self, span: Span) -> String {
        self.sql.get(span.start..span.end).unwrap_or("").to_string()
    }

    /// The span of the token the cursor is on.
    fn span(&self) -> Span {
        self.tokens.get(self.pos).map(|(_, s)| *s).unwrap_or(Span {
            start: 0,
            end: 0,
            line: 1,
            col: 1,
        })
    }
}

// --- the row builders -----------------------------------------------------

/// The rows of `PRAGMA table_info(t)`, in SQLite's column order.
///
/// `dflt_value` is NULL for a column with no default and the text of the
/// default otherwise; `notnull` and `pk` are 0/1 integers.
pub fn table_info_rows(columns: &[ColumnInfo]) -> Vec<Vec<Value>> {
    columns
        .iter()
        .enumerate()
        .map(|(i, c)| {
            vec![
                Value::Integer(i as i64),
                Value::Text(c.name.clone()),
                Value::Text(c.ty.clone()),
                Value::Integer(i64::from(c.not_null)),
                match &c.default {
                    Some(d) => Value::Text(d.clone()),
                    None => Value::Null,
                },
                Value::Integer(i64::from(c.pk)),
            ]
        })
        .collect()
}

/// The rows of `PRAGMA foreign_key_list(t)`, in SQLite's column order.
pub fn foreign_key_list_rows(keys: &[ForeignKeyInfo]) -> Vec<Vec<Value>> {
    let mut out = Vec::new();
    for fk in keys {
        for (seq, (from, to)) in fk.columns.iter().enumerate() {
            out.push(vec![
                Value::Integer(fk.id),
                Value::Integer(seq as i64),
                Value::Text(fk.table.clone()),
                Value::Text(from.clone()),
                Value::Text(to.clone()),
                Value::Text(fk.on_update.clone()),
                Value::Text(fk.on_delete.clone()),
                // SQLite's `match` is always NONE: it does not implement MATCH
                // for foreign keys, and reports the column rather than raising.
                Value::Text("NONE".to_string()),
            ]);
        }
    }
    out
}

/// The rows of `PRAGMA index_list(t)`, in SQLite's column order.
///
/// `seq` counts down from the newest index, because SQLite lists an index list
/// most-recently-created first.
pub fn index_list_rows(indexes: &[IndexInfo]) -> Vec<Vec<Value>> {
    indexes
        .iter()
        .rev()
        .enumerate()
        .map(|(i, ix)| {
            vec![
                Value::Integer(i as i64),
                Value::Text(ix.name.clone()),
                Value::Integer(i64::from(ix.unique)),
                Value::Text(ix.origin.to_string()),
                Value::Integer(i64::from(ix.partial)),
            ]
        })
        .collect()
}

/// The rows of `PRAGMA index_info(name)`, in SQLite's column order.
///
/// `cid` is the column's index in the *table*, and `seqno` its position in the
/// index, so an index over the table's columns in a different order answers
/// with the two out of step.
pub fn index_info_rows(key_columns: &[(usize, String)]) -> Vec<Vec<Value>> {
    key_columns
        .iter()
        .enumerate()
        .map(|(seqno, (cid, name))| {
            vec![
                Value::Integer(seqno as i64),
                Value::Integer(*cid as i64),
                Value::Text(name.clone()),
            ]
        })
        .collect()
}

/// The rows of `PRAGMA database_list`, in SQLite's column order.
///
/// A database this connection has not opened is not listed: SQLite skips a
/// schema with no b-tree rather than answering it with an empty file.
pub fn database_list_rows(databases: &[(i64, String, String)]) -> Vec<Vec<Value>> {
    databases
        .iter()
        .map(|(seq, name, file)| {
            vec![
                Value::Integer(*seq),
                Value::Text(name.clone()),
                Value::Text(file.clone()),
            ]
        })
        .collect()
}

/// One row a PRAGMA produced, with the columns it produced it under.
pub type PragmaResult = (Vec<String>, Vec<Vec<Value>>);

/// The pragmas this engine answers a read for, and the columns each answers
/// under.
///
/// The column set is the whole answer for the introspection pragmas: an
/// unknown table is an empty *result* carrying the six `table_info` columns, not
/// no result at all, because `sqlite3_column_count` is 6 either way and a
/// harness reading the column names sees six.
pub fn scalar_columns(name: &str) -> Option<&'static [&'static str]> {
    let cols: &'static [&'static str] = match name {
        "table_info" => columns::TABLE_INFO,
        "index_list" => columns::INDEX_LIST,
        "index_info" => columns::INDEX_INFO,
        "foreign_key_list" => columns::FOREIGN_KEY_LIST,
        "database_list" => columns::DATABASE_LIST,
        "full_column_names" | "short_column_names" | "page_count" | "page_size" | "encoding"
        | "schema_version" | "user_version" | "application_id" | "journal_mode" | "cache_size" => {
            // A pragma that reports one value names its column after itself:
            // `PRAGMA page_size` answers a column called `page_size`.
            return None;
        }
        _ => return None,
    };
    Some(cols)
}

/// The one-column result of a pragma that reports a single value, with the
/// column named after the pragma itself.
///
/// This is the shape `PRAGMA page_size`, `PRAGMA encoding` and
/// `PRAGMA journal_mode` all answer, and it is the shape the module was missing:
/// without it, four of the eight scalar pragmas have nowhere to put their value
/// even though the settings that hold them exist.
pub fn scalar_row(name: &str, value: Value) -> PragmaResult {
    (vec![name.to_string()], vec![vec![value]])
}

/// The scalar pragmas' default values, as sqlite3 reports them on a fresh file.
///
/// Measured on 3.53.4 with `printf 'PRAGMA x' | sqlite3 fresh.db`:
///
/// ```text
/// page_size 4096   page_count 0    encoding UTF-8   schema_version 0
/// user_version 0   application_id 0   journal_mode delete   cache_size -2000
/// ```
///
/// `cache_size` is -2000 rather than 0 because that is the default SQLite
/// reports in kibibytes, and `page_count` is 0 on a file with nothing in it
/// because the header's page-count field has not been written yet.
pub fn default_scalar(name: &str) -> Option<Value> {
    let v = match name {
        "page_size" => Value::Integer(4096),
        "page_count" => Value::Integer(0),
        "encoding" => Value::Text("UTF-8".to_string()),
        "schema_version" => Value::Integer(0),
        "user_version" => Value::Integer(0),
        "application_id" => Value::Integer(0),
        "journal_mode" => Value::Text("delete".to_string()),
        "cache_size" => Value::Integer(-2000),
        _ => return None,
    };
    Some(v)
}

/// The database header values a scalar pragma reads, gathered so the executor
/// can answer them without borrowing the pager.
///
/// `PRAGMA encoding`, `PRAGMA schema_version`, `PRAGMA user_version` and
/// `PRAGMA application_id` are all header fields, and `PRAGMA page_size` and
/// `PRAGMA page_count` are two more, so one read of the header answers six of
/// the eight. The values here are the ones [`HeaderScalars::from_header`] takes
/// rather than re-deriving, which keeps the byte offsets in one place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeaderScalars {
    pub page_size: i64,
    pub page_count: i64,
    pub encoding: String,
    pub schema_version: i64,
    pub user_version: i64,
    pub application_id: i64,
}

impl Default for HeaderScalars {
    /// A brand new file's header, which is what a connection with nothing in
    /// its pager holds.
    ///
    /// This is [`crate::page::DbHeader::default`] read through
    /// [`HeaderScalars::from_header`], and it is spelled out rather than
    /// derived so that the values are the ones the binary prints: a derived
    /// `Default` would answer `page_size 0` and `encoding ""`, neither of which
    /// is a thing sqlite3 can report. `page_count` really is 0 on a fresh
    /// file, because the header's page-count field is not written until a
    /// change fills it in.
    fn default() -> HeaderScalars {
        HeaderScalars::from_header(&crate::page::DbHeader::default())
    }
}

impl HeaderScalars {
    /// Reads the six header-backed values off a decoded database header.
    pub fn from_header(h: &crate::page::DbHeader) -> HeaderScalars {
        HeaderScalars {
            page_size: h.page_size as i64,
            // The header's page count is zero until a write fills it in, which
            // is why a fresh file answers 0 and not 1.
            page_count: h.db_size_pages as i64,
            encoding: h.text_encoding.name().to_string(),
            schema_version: h.schema_cookie as i64,
            user_version: h.user_version as i64,
            application_id: h.application_id as i64,
        }
    }

    /// The one-column result for whichever of the six this is.
    pub fn value_for(&self, name: &str) -> Option<Value> {
        let v = match name {
            "page_size" => Value::Integer(self.page_size),
            "page_count" => Value::Integer(self.page_count),
            "encoding" => Value::Text(self.encoding.clone()),
            "schema_version" => Value::Integer(self.schema_version),
            "user_version" => Value::Integer(self.user_version),
            "application_id" => Value::Integer(self.application_id),
            _ => return None,
        };
        Some(v)
    }
}

/// Whether a pragma name is one this engine reads a value for.
///
/// The list is deliberately short and the omission deliberate: a name outside it
/// is a **silent no-op**, not an error, which is sqlite3's own rule
/// ("IMP: R-43042-22504 No error messages are generated if an unknown pragma is
/// issued"). The suite's setup blocks run `PRAGMA locking_mode`,
/// `PRAGMA trusted_schema`, `PRAGMA temp_store` and a long tail of settings this
/// engine has no notion of, and each one has to run without failing, because a
/// failure there abandons the whole block and every table it was going to create
/// stays missing. One missing PRAGMA turns a hundred tests into "no such table".
pub fn is_known(name: &str) -> bool {
    matches!(
        name,
        "table_info"
            | "index_list"
            | "index_info"
            | "foreign_key_list"
            | "database_list"
            | "full_column_names"
            | "short_column_names"
            | "page_count"
            | "page_size"
            | "encoding"
            | "schema_version"
            | "user_version"
            | "application_id"
            | "journal_mode"
            | "cache_size"
    )
}

/// The `PRAGMA table_info(t)` / `foreign_key_list(t)` rows for a table.
///
/// The schema text is the only place the original spelling survives -- the
/// engine's own parser folds an unquoted identifier to lower case and discards a
/// `PRIMARY KEY` column list -- so this is a reading of the CREATE TABLE the
/// catalog was built from and not of the catalog.
///
/// A table this connection does not know produces the pragma's columns and no
/// rows, which is the whole point of the fixed column set.
pub fn table_rows(name: &str, _arg: &str, create_sql: Option<&str>) -> PragmaResult {
    match name {
        "table_info" => {
            let columns = create_sql
                .and_then(|sql| SchemaText::new(sql).ok())
                .map(|s| s.columns())
                .unwrap_or_default();
            (
                columns::TABLE_INFO.iter().map(|c| c.to_string()).collect(),
                table_info_rows(&columns),
            )
        }
        "foreign_key_list" => {
            let keys = create_sql
                .and_then(|sql| SchemaText::new(sql).ok())
                .map(|s| s.foreign_keys())
                .unwrap_or_default();
            (
                columns::FOREIGN_KEY_LIST
                    .iter()
                    .map(|c| c.to_string())
                    .collect(),
                foreign_key_list_rows(&keys),
            )
        }
        "index_list" => {
            // The engine does not build secondary indexes yet, so a table
            // contributes at most the implicit index a UNIQUE or a WITHOUT
            // ROWID primary key creates. sqlite3 lists those too, which is why
            // `PRAGMA index_list` on `CREATE TABLE u(x UNIQUE)` is not empty
            // even with no CREATE INDEX anywhere.
            let indexes = create_sql
                .and_then(|sql| SchemaText::new(sql).ok())
                .and_then(|s| s.implicit_index())
                .into_iter()
                .collect::<Vec<_>>();
            (
                columns::INDEX_LIST.iter().map(|c| c.to_string()).collect(),
                index_list_rows(&indexes),
            )
        }
        _ => (Vec::new(), Vec::new()),
    }
}

/// Strips the quoting from an argument, so `PRAGMA table_info("t")` and
/// `PRAGMA table_info('t')` find the same table as the bare spelling.
///
/// SQLite accepts all four quoting forms here for the same reason it accepts
/// them for the pragma's own name, and a quoted argument is a name the same as
/// an unquoted one. The case is left alone: SQLite matches a table name
/// case-insensitively for ASCII, which the catalog's own lookup does.
pub fn unquote_arg(arg: &str) -> String {
    let a = arg.trim();
    if a.len() >= 2 {
        if a.starts_with('\'') && a.ends_with('\'') {
            return a[1..a.len() - 1].replace("''", "'");
        }
        if a.starts_with('"') && a.ends_with('"') {
            return a[1..a.len() - 1].replace("\"\"", "\"");
        }
        if a.starts_with('`') && a.ends_with('`') {
            return a[1..a.len() - 1].replace("``", "`");
        }
        if a.starts_with('[') && a.ends_with(']') {
            return a[1..a.len() - 1].to_string();
        }
    }
    a.to_string()
}

/// What a connection should do with a parsed PRAGMA.
///
/// The point of this type is that the two halves of the contract cannot be
/// confused. A read produces a result set, an assignment produces none, and a
/// pragma this engine does not implement produces none **and is not an error**.
/// A connection that only had a `Result<PragmaResult>` would have to invent an
/// error for the unknown case, and inventing one is exactly the mistake the
/// module's own comment warns against.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// A pragma that reports rows, with the columns it reports them under.
    Result(PragmaResult),
    /// An assignment, or a pragma this engine does not implement: no result set
    /// at all, and no error.
    Nothing,
}

/// Everything a connection has to supply to answer a PRAGMA, and everything the
/// pragma may change.
///
/// This is a struct rather than a set of arguments so the executor is a single
/// function the connection calls, and so the settings a PRAGMA can change are
/// visible in one list rather than spread across the connection's fields.
pub struct Exec<'a> {
    /// The pragmas with state: the result-column naming flags, the two header
    /// integers and the journal mode.
    pub state: &'a mut PragmaState,
    /// The six header-backed scalars, read once per statement.
    pub header: HeaderScalars,
    /// The CREATE TABLE text of a table the argument names, for the
    /// introspection pragmas.
    pub create_sql: Option<&'a str>,
    /// The `(seq, name, file)` rows for `PRAGMA database_list`.
    pub databases: &'a [(i64, String, String)],
    /// The `PRAGMA cache_size` value.
    pub cache_size: i64,
}

impl Exec<'_> {
    /// Answers a parsed PRAGMA.
    ///
    /// The order of the arms is the order of sqlite3's own pragma table, and
    /// the last arm is the one that matters most: a name this engine does not
    /// know is a **silent no-op**, not an error.
    pub fn run(&mut self, p: &Pragma) -> Result<Outcome> {
        if !is_known(&p.name) {
            // sqlite3: "IMP: R-43042-22504 No error messages are generated if an
            // unknown pragma is issued." The suite depends on this: a setup
            // block that sets a pragma this engine has no notion of must still
            // run to completion, or every table it created stays missing.
            return Ok(Outcome::Nothing);
        }
        match &p.body {
            PragmaBody::Set(arg) => {
                self.set(&p.name, arg);
                // `PRAGMA journal_mode=X` answers the mode it settled on, which
                // is the one pragma whose assignment returns a result set.
                if p.name == "journal_mode" {
                    return Ok(Outcome::Result(scalar_row(
                        "journal_mode",
                        Value::Text(self.state.journal_mode.as_str().to_string()),
                    )));
                }
                Ok(Outcome::Nothing)
            }
            PragmaBody::Read => self.read(&p.name, None),
            PragmaBody::ReadArg(arg) => self.read(&p.name, Some(arg)),
        }
    }

    /// Applies an assignment, ignoring a value the pragma cannot use.
    ///
    /// Ignoring rather than raising is sqlite3's behaviour: `PRAGMA
    /// user_version=abc` leaves the setting alone and is not an error, and
    /// `PRAGMA full_column_names=bogus` turns the flag OFF rather than failing,
    /// because `sqlite3GetBoolean` treats anything it does not recognise as
    /// false.
    fn set(&mut self, name: &str, arg: &str) {
        let arg = unquote_arg(arg);
        match name {
            "full_column_names" => self.state.column_names.full = parse_bool(&arg),
            "short_column_names" => self.state.column_names.short = parse_bool(&arg),
            "user_version" => {
                if let Some(v) = parse_int(&arg) {
                    self.state.user_version = v;
                }
            }
            "application_id" => {
                if let Some(v) = parse_int(&arg) {
                    self.state.application_id = v;
                }
            }
            "journal_mode" => {
                // An unrecognised name leaves the mode alone, which is what
                // `PRAGMA journal_mode=bogus` does -- and it still answers the
                // mode it kept.
                if let Some(m) = JournalMode::parse(&arg) {
                    self.state.journal_mode = m;
                }
            }
            // page_size, encoding, cache_size and the rest are either not
            // changeable after the file exists or have no effect a query can
            // observe. They are accepted and discarded, never refused.
            _ => {}
        }
    }

    /// Answers a read, with the fixed column set where the pragma has one.
    fn read(&mut self, name: &str, arg: Option<&str>) -> Result<Outcome> {
        // The introspection pragmas answer their columns even when the argument
        // names nothing, because the column set is part of the answer.
        if let Some(cols) = scalar_columns(name) {
            let (columns, rows) = match name {
                "table_info" | "foreign_key_list" | "index_list" => {
                    table_rows(name, arg.unwrap_or(""), self.create_sql)
                }
                "index_info" => (
                    columns::INDEX_INFO.iter().map(|c| c.to_string()).collect(),
                    Vec::new(),
                ),
                "database_list" => (
                    columns::DATABASE_LIST
                        .iter()
                        .map(|c| c.to_string())
                        .collect(),
                    database_list_rows(self.databases),
                ),
                _ => (Vec::new(), Vec::new()),
            };
            debug_assert!(columns.len() == cols.len());
            return Ok(Outcome::Result((columns, rows)));
        }
        // Everything else is a single value in a column named after the pragma.
        let value = match name {
            "full_column_names" => Value::Integer(i64::from(self.state.column_names.full)),
            "short_column_names" => Value::Integer(i64::from(self.state.column_names.short)),
            "journal_mode" => Value::Text(self.state.journal_mode.as_str().to_string()),
            "cache_size" => Value::Integer(self.cache_size),
            other => match self.header.value_for(other) {
                Some(v) => v,
                None => return Ok(Outcome::Nothing),
            },
        };
        Ok(Outcome::Result(scalar_row(name, value)))
    }
}
