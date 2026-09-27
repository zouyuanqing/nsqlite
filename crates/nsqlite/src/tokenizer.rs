//! The SQL tokenizer.
//!
//! Turns SQL text into a flat token stream. The tokenizer is deliberately
//! ignorant of grammar: where SQLite needs context to tell an identifier from a
//! keyword, or a string from a quoted name, the decision is left to the parser.
//! Two conventions carry that context across the boundary:
//!
//! * A double-quoted token comes back as [`Token::Identifier`], because SQL's
//!   quoted-names feature (and SQLite's misfeature of allowing `"t"` where a
//!   string is meant) means it is a name until the parser says otherwise.
//! * [`Keyword::as_ident`] turns a keyword back into a name when the parser
//!   finds one in name position.
//!
//! Everything here is allocation-light and total: arbitrary bytes either become
//! tokens or produce an [`Error`], and never a panic.

use crate::error::{Error, Result, ResultCode};

/// A half-open byte range in the source, tagged with where it started.
///
/// `line` and `col` are 1-based and point at `start`; `col` counts characters,
/// not bytes, so a multi-byte scalar before the token shows up as one column.
/// Comments do not move `pos` themselves, so a token's position is reported
/// exactly as it sits in the original text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Span {
    pub start: usize,
    pub end: usize,
    pub line: u32,
    pub col: u32,
}

/// An operator or separator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Punct {
    LParen,
    RParen,
    Comma,
    Semicolon,
    Dot,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    BitShiftLeft,
    BitShiftRight,
    BitAnd,
    BitOr,
    Concat,
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
    EqEq,
    Ne,
    NeBracket,
    Tilde,
    ArrowRight,
    ArrowRightRight,
}

/// A lexical token, with its position in the source.
#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    /// A bare or quoted name. Bare names are folded to lowercase; quoted names
    /// keep their case because SQLite treats them as case-insensitive at the
    /// b-tree level but preserves what was written.
    Identifier(String),
    /// A double-quoted name, which [`Token::Identifier`] is for every other
    /// quoting. The variant is the only record of the character that opened
    /// the name, and SQLite's `no such column: "x" - should this be a string
    /// literal in single-quotes?` needs it, so it is kept rather than folded
    /// into `Identifier` at scan time.
    DoubleQuotedIdentifier(String),
    Keyword(Keyword),
    /// A single-quoted text literal, with `''` already resolved to `'`.
    String(String),
    /// An `x'..'` blob literal.
    Blob(Vec<u8>),
    Integer(i64),
    Float(f64),
    /// `?`, `?NNN`, `?name`, `:name`, `@name`, or `$name`.
    Parameter {
        index: Option<usize>,
        name: Option<String>,
    },
    Punct(Punct),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Keyword {
    Abort,
    Action,
    Add,
    After,
    All,
    Alter,
    Always,
    Analyze,
    And,
    As,
    Asc,
    Attach,
    Autoincrement,
    Before,
    Begin,
    Between,
    By,
    Cascade,
    Case,
    Cast,
    Check,
    Collate,
    Column,
    Commit,
    Conflict,
    Constraint,
    Create,
    Cross,
    Current,
    CurrentDate,
    CurrentTime,
    CurrentTimestamp,
    Database,
    Default,
    Deferrable,
    Deferred,
    Delete,
    Desc,
    Detach,
    Distinct,
    Do,
    Drop,
    Each,
    Else,
    End,
    Escape,
    Except,
    Exclude,
    Exclusive,
    Exists,
    Explain,
    Fail,
    Filter,
    First,
    Following,
    For,
    Foreign,
    From,
    Full,
    Generated,
    Glob,
    Group,
    Groups,
    Having,
    If,
    Ignore,
    Immediate,
    In,
    Index,
    Indexed,
    Initially,
    Inner,
    Insert,
    Instead,
    Intersect,
    Into,
    Is,
    Isnull,
    Join,
    Key,
    Last,
    Left,
    Like,
    Limit,
    Match,
    Materialized,
    Natural,
    No,
    Not,
    Nothing,
    Notnull,
    Null,
    Nulls,
    Of,
    Offset,
    On,
    Or,
    Order,
    Others,
    Outer,
    Over,
    Partition,
    Plan,
    Pragma,
    Preceding,
    Primary,
    Query,
    Raise,
    Range,
    Recursive,
    References,
    Regexp,
    Reindex,
    Release,
    Rename,
    Replace,
    Restrict,
    Returning,
    Right,
    Rollback,
    Row,
    Rows,
    Savepoint,
    Select,
    Set,
    Table,
    Temp,
    Temporary,
    Then,
    Ties,
    To,
    Transaction,
    Trigger,
    Unbounded,
    Union,
    Unique,
    Update,
    Using,
    Vacuum,
    Values,
    View,
    Virtual,
    When,
    Where,
    Window,
    With,
    Within,
    Without,
}

impl Keyword {
    /// Every keyword, in the order [`Keyword::from_name`] searches them.
    pub const ALL: &'static [Keyword] = &[
        Keyword::Abort,
        Keyword::Action,
        Keyword::Add,
        Keyword::After,
        Keyword::All,
        Keyword::Alter,
        Keyword::Always,
        Keyword::Analyze,
        Keyword::And,
        Keyword::As,
        Keyword::Asc,
        Keyword::Attach,
        Keyword::Autoincrement,
        Keyword::Before,
        Keyword::Begin,
        Keyword::Between,
        Keyword::By,
        Keyword::Cascade,
        Keyword::Case,
        Keyword::Cast,
        Keyword::Check,
        Keyword::Collate,
        Keyword::Column,
        Keyword::Commit,
        Keyword::Conflict,
        Keyword::Constraint,
        Keyword::Create,
        Keyword::Cross,
        Keyword::Current,
        Keyword::CurrentDate,
        Keyword::CurrentTime,
        Keyword::CurrentTimestamp,
        Keyword::Database,
        Keyword::Default,
        Keyword::Deferrable,
        Keyword::Deferred,
        Keyword::Delete,
        Keyword::Desc,
        Keyword::Detach,
        Keyword::Distinct,
        Keyword::Do,
        Keyword::Drop,
        Keyword::Each,
        Keyword::Else,
        Keyword::End,
        Keyword::Escape,
        Keyword::Except,
        Keyword::Exclude,
        Keyword::Exclusive,
        Keyword::Exists,
        Keyword::Explain,
        Keyword::Fail,
        Keyword::Filter,
        Keyword::First,
        Keyword::Following,
        Keyword::For,
        Keyword::Foreign,
        Keyword::From,
        Keyword::Full,
        Keyword::Generated,
        Keyword::Glob,
        Keyword::Group,
        Keyword::Groups,
        Keyword::Having,
        Keyword::If,
        Keyword::Ignore,
        Keyword::Immediate,
        Keyword::In,
        Keyword::Index,
        Keyword::Indexed,
        Keyword::Initially,
        Keyword::Inner,
        Keyword::Insert,
        Keyword::Instead,
        Keyword::Intersect,
        Keyword::Into,
        Keyword::Is,
        Keyword::Isnull,
        Keyword::Join,
        Keyword::Key,
        Keyword::Last,
        Keyword::Left,
        Keyword::Like,
        Keyword::Limit,
        Keyword::Match,
        Keyword::Materialized,
        Keyword::Natural,
        Keyword::No,
        Keyword::Not,
        Keyword::Nothing,
        Keyword::Notnull,
        Keyword::Null,
        Keyword::Nulls,
        Keyword::Of,
        Keyword::Offset,
        Keyword::On,
        Keyword::Or,
        Keyword::Order,
        Keyword::Others,
        Keyword::Outer,
        Keyword::Over,
        Keyword::Partition,
        Keyword::Plan,
        Keyword::Pragma,
        Keyword::Preceding,
        Keyword::Primary,
        Keyword::Query,
        Keyword::Raise,
        Keyword::Range,
        Keyword::Recursive,
        Keyword::References,
        Keyword::Regexp,
        Keyword::Reindex,
        Keyword::Release,
        Keyword::Rename,
        Keyword::Replace,
        Keyword::Restrict,
        Keyword::Returning,
        Keyword::Right,
        Keyword::Rollback,
        Keyword::Row,
        Keyword::Rows,
        Keyword::Savepoint,
        Keyword::Select,
        Keyword::Set,
        Keyword::Table,
        Keyword::Temp,
        Keyword::Temporary,
        Keyword::Then,
        Keyword::Ties,
        Keyword::To,
        Keyword::Transaction,
        Keyword::Trigger,
        Keyword::Unbounded,
        Keyword::Union,
        Keyword::Unique,
        Keyword::Update,
        Keyword::Using,
        Keyword::Vacuum,
        Keyword::Values,
        Keyword::View,
        Keyword::Virtual,
        Keyword::When,
        Keyword::Where,
        Keyword::Window,
        Keyword::With,
        Keyword::Within,
        Keyword::Without,
    ];

    /// Whether this keyword is the first token of a *name* in the grammar --
    /// the SELECT list, a WHERE, an ORDER BY term and the rest -- rather than
    /// the keyword that opens the clause itself.
    ///
    /// This is what tells `SELECT FROM t` (a syntax error, because `FROM` opens
    /// the FROM clause and there is nothing before it to select) from
    /// `SELECT from_col FROM t` (a name, and a very ordinary one). A name is
    /// read at the head of a clause before the expression grammar is
    /// consulted, so the two cannot be told apart further in -- by then the
    /// clause has already claimed its own keyword.
    pub fn heads_a_name(self) -> bool {
        !matches!(
            self,
            Keyword::Select
                | Keyword::From
                | Keyword::Where
                | Keyword::Order
                | Keyword::By
                | Keyword::Group
                | Keyword::Having
                | Keyword::Limit
                | Keyword::Offset
                | Keyword::As
                | Keyword::Asc
                | Keyword::Desc
        )
    }

    /// The keyword's canonical spelling, always lowercase.
    pub fn as_str(self) -> &'static str {
        match self {
            Keyword::Abort => "abort",
            Keyword::Action => "action",
            Keyword::Add => "add",
            Keyword::After => "after",
            Keyword::All => "all",
            Keyword::Alter => "alter",
            Keyword::Always => "always",
            Keyword::Analyze => "analyze",
            Keyword::And => "and",
            Keyword::As => "as",
            Keyword::Asc => "asc",
            Keyword::Attach => "attach",
            Keyword::Autoincrement => "autoincrement",
            Keyword::Before => "before",
            Keyword::Begin => "begin",
            Keyword::Between => "between",
            Keyword::By => "by",
            Keyword::Cascade => "cascade",
            Keyword::Case => "case",
            Keyword::Cast => "cast",
            Keyword::Check => "check",
            Keyword::Collate => "collate",
            Keyword::Column => "column",
            Keyword::Commit => "commit",
            Keyword::Conflict => "conflict",
            Keyword::Constraint => "constraint",
            Keyword::Create => "create",
            Keyword::Cross => "cross",
            Keyword::Current => "current",
            Keyword::CurrentDate => "current_date",
            Keyword::CurrentTime => "current_time",
            Keyword::CurrentTimestamp => "current_timestamp",
            Keyword::Database => "database",
            Keyword::Default => "default",
            Keyword::Deferrable => "deferrable",
            Keyword::Deferred => "deferred",
            Keyword::Delete => "delete",
            Keyword::Desc => "desc",
            Keyword::Detach => "detach",
            Keyword::Distinct => "distinct",
            Keyword::Do => "do",
            Keyword::Drop => "drop",
            Keyword::Each => "each",
            Keyword::Else => "else",
            Keyword::End => "end",
            Keyword::Escape => "escape",
            Keyword::Except => "except",
            Keyword::Exclude => "exclude",
            Keyword::Exclusive => "exclusive",
            Keyword::Exists => "exists",
            Keyword::Explain => "explain",
            Keyword::Fail => "fail",
            Keyword::Filter => "filter",
            Keyword::First => "first",
            Keyword::Following => "following",
            Keyword::For => "for",
            Keyword::Foreign => "foreign",
            Keyword::From => "from",
            Keyword::Full => "full",
            Keyword::Generated => "generated",
            Keyword::Glob => "glob",
            Keyword::Group => "group",
            Keyword::Groups => "groups",
            Keyword::Having => "having",
            Keyword::If => "if",
            Keyword::Ignore => "ignore",
            Keyword::Immediate => "immediate",
            Keyword::In => "in",
            Keyword::Index => "index",
            Keyword::Indexed => "indexed",
            Keyword::Initially => "initially",
            Keyword::Inner => "inner",
            Keyword::Insert => "insert",
            Keyword::Instead => "instead",
            Keyword::Intersect => "intersect",
            Keyword::Into => "into",
            Keyword::Is => "is",
            Keyword::Isnull => "isnull",
            Keyword::Join => "join",
            Keyword::Key => "key",
            Keyword::Last => "last",
            Keyword::Left => "left",
            Keyword::Like => "like",
            Keyword::Limit => "limit",
            Keyword::Match => "match",
            Keyword::Materialized => "materialized",
            Keyword::Natural => "natural",
            Keyword::No => "no",
            Keyword::Not => "not",
            Keyword::Nothing => "nothing",
            Keyword::Notnull => "notnull",
            Keyword::Null => "null",
            Keyword::Nulls => "nulls",
            Keyword::Of => "of",
            Keyword::Offset => "offset",
            Keyword::On => "on",
            Keyword::Or => "or",
            Keyword::Order => "order",
            Keyword::Others => "others",
            Keyword::Outer => "outer",
            Keyword::Over => "over",
            Keyword::Partition => "partition",
            Keyword::Plan => "plan",
            Keyword::Pragma => "pragma",
            Keyword::Preceding => "preceding",
            Keyword::Primary => "primary",
            Keyword::Query => "query",
            Keyword::Raise => "raise",
            Keyword::Range => "range",
            Keyword::Recursive => "recursive",
            Keyword::References => "references",
            Keyword::Regexp => "regexp",
            Keyword::Reindex => "reindex",
            Keyword::Release => "release",
            Keyword::Rename => "rename",
            Keyword::Replace => "replace",
            Keyword::Restrict => "restrict",
            Keyword::Returning => "returning",
            Keyword::Right => "right",
            Keyword::Rollback => "rollback",
            Keyword::Row => "row",
            Keyword::Rows => "rows",
            Keyword::Savepoint => "savepoint",
            Keyword::Select => "select",
            Keyword::Set => "set",
            Keyword::Table => "table",
            Keyword::Temp => "temp",
            Keyword::Temporary => "temporary",
            Keyword::Then => "then",
            Keyword::Ties => "ties",
            Keyword::To => "to",
            Keyword::Transaction => "transaction",
            Keyword::Trigger => "trigger",
            Keyword::Unbounded => "unbounded",
            Keyword::Union => "union",
            Keyword::Unique => "unique",
            Keyword::Update => "update",
            Keyword::Using => "using",
            Keyword::Vacuum => "vacuum",
            Keyword::Values => "values",
            Keyword::View => "view",
            Keyword::Virtual => "virtual",
            Keyword::When => "when",
            Keyword::Where => "where",
            Keyword::Window => "window",
            Keyword::With => "with",
            Keyword::Within => "within",
            Keyword::Without => "without",
        }
    }

    /// The keyword re-read as an identifier name.
    ///
    /// SQLite lets keywords name things when the grammar allows it, so the
    /// parser turns a keyword it did not expect into a name with this. The
    /// result borrows a static string; clone it when an owned `Token::Identifier`
    /// is what is wanted.
    pub fn as_ident(self) -> &'static str {
        self.as_str()
    }

    /// Resolves an identifier to a keyword, ignoring ASCII case.
    ///
    /// Matching is ASCII-only and allocation-free: the probe is compared
    /// against each entry's bytes with case folded, so `SeLeCt` and `select`
    /// both land on `Keyword::Select` while non-ASCII bytes, which SQLite
    /// never folds, are left alone.
    pub fn from_name(name: &str) -> Option<Keyword> {
        KEYWORDS
            .binary_search_by(|probe| cmp_folded(probe.0, name))
            .ok()
            .map(|i| KEYWORDS[i].1)
    }
}

/// The keyword table `Keyword::from_name` binary-searches, ordered by length
/// then bytes so the comparison is a total order.
static KEYWORDS: &[(&str, Keyword)] = &[
    ("as", Keyword::As),
    ("by", Keyword::By),
    ("do", Keyword::Do),
    ("if", Keyword::If),
    ("in", Keyword::In),
    ("is", Keyword::Is),
    ("no", Keyword::No),
    ("of", Keyword::Of),
    ("on", Keyword::On),
    ("or", Keyword::Or),
    ("to", Keyword::To),
    ("add", Keyword::Add),
    ("all", Keyword::All),
    ("and", Keyword::And),
    ("asc", Keyword::Asc),
    ("end", Keyword::End),
    ("for", Keyword::For),
    ("key", Keyword::Key),
    ("not", Keyword::Not),
    ("row", Keyword::Row),
    ("set", Keyword::Set),
    ("case", Keyword::Case),
    ("cast", Keyword::Cast),
    ("desc", Keyword::Desc),
    ("drop", Keyword::Drop),
    ("each", Keyword::Each),
    ("else", Keyword::Else),
    ("fail", Keyword::Fail),
    ("from", Keyword::From),
    ("full", Keyword::Full),
    ("glob", Keyword::Glob),
    ("into", Keyword::Into),
    ("join", Keyword::Join),
    ("last", Keyword::Last),
    ("left", Keyword::Left),
    ("like", Keyword::Like),
    ("null", Keyword::Null),
    ("over", Keyword::Over),
    ("plan", Keyword::Plan),
    ("rows", Keyword::Rows),
    ("temp", Keyword::Temp),
    ("then", Keyword::Then),
    ("ties", Keyword::Ties),
    ("view", Keyword::View),
    ("when", Keyword::When),
    ("with", Keyword::With),
    ("abort", Keyword::Abort),
    ("after", Keyword::After),
    ("alter", Keyword::Alter),
    ("begin", Keyword::Begin),
    ("check", Keyword::Check),
    ("cross", Keyword::Cross),
    ("first", Keyword::First),
    ("group", Keyword::Group),
    ("index", Keyword::Index),
    ("inner", Keyword::Inner),
    ("limit", Keyword::Limit),
    ("match", Keyword::Match),
    ("nulls", Keyword::Nulls),
    ("order", Keyword::Order),
    ("outer", Keyword::Outer),
    ("query", Keyword::Query),
    ("raise", Keyword::Raise),
    ("range", Keyword::Range),
    ("right", Keyword::Right),
    ("table", Keyword::Table),
    ("union", Keyword::Union),
    ("using", Keyword::Using),
    ("where", Keyword::Where),
    ("action", Keyword::Action),
    ("always", Keyword::Always),
    ("attach", Keyword::Attach),
    ("before", Keyword::Before),
    ("column", Keyword::Column),
    ("commit", Keyword::Commit),
    ("create", Keyword::Create),
    ("delete", Keyword::Delete),
    ("detach", Keyword::Detach),
    ("escape", Keyword::Escape),
    ("except", Keyword::Except),
    ("exists", Keyword::Exists),
    ("filter", Keyword::Filter),
    ("groups", Keyword::Groups),
    ("having", Keyword::Having),
    ("ignore", Keyword::Ignore),
    ("insert", Keyword::Insert),
    ("isnull", Keyword::Isnull),
    ("offset", Keyword::Offset),
    ("others", Keyword::Others),
    ("pragma", Keyword::Pragma),
    ("regexp", Keyword::Regexp),
    ("rename", Keyword::Rename),
    ("select", Keyword::Select),
    ("unique", Keyword::Unique),
    ("update", Keyword::Update),
    ("vacuum", Keyword::Vacuum),
    ("values", Keyword::Values),
    ("window", Keyword::Window),
    ("within", Keyword::Within),
    ("analyze", Keyword::Analyze),
    ("between", Keyword::Between),
    ("cascade", Keyword::Cascade),
    ("collate", Keyword::Collate),
    ("current", Keyword::Current),
    ("default", Keyword::Default),
    ("exclude", Keyword::Exclude),
    ("explain", Keyword::Explain),
    ("foreign", Keyword::Foreign),
    ("indexed", Keyword::Indexed),
    ("instead", Keyword::Instead),
    ("natural", Keyword::Natural),
    ("nothing", Keyword::Nothing),
    ("notnull", Keyword::Notnull),
    ("primary", Keyword::Primary),
    ("reindex", Keyword::Reindex),
    ("release", Keyword::Release),
    ("replace", Keyword::Replace),
    ("trigger", Keyword::Trigger),
    ("virtual", Keyword::Virtual),
    ("without", Keyword::Without),
    ("conflict", Keyword::Conflict),
    ("database", Keyword::Database),
    ("deferred", Keyword::Deferred),
    ("distinct", Keyword::Distinct),
    ("restrict", Keyword::Restrict),
    ("rollback", Keyword::Rollback),
    ("exclusive", Keyword::Exclusive),
    ("following", Keyword::Following),
    ("generated", Keyword::Generated),
    ("immediate", Keyword::Immediate),
    ("initially", Keyword::Initially),
    ("intersect", Keyword::Intersect),
    ("partition", Keyword::Partition),
    ("preceding", Keyword::Preceding),
    ("recursive", Keyword::Recursive),
    ("returning", Keyword::Returning),
    ("savepoint", Keyword::Savepoint),
    ("temporary", Keyword::Temporary),
    ("unbounded", Keyword::Unbounded),
    ("constraint", Keyword::Constraint),
    ("deferrable", Keyword::Deferrable),
    ("references", Keyword::References),
    ("transaction", Keyword::Transaction),
    ("current_date", Keyword::CurrentDate),
    ("current_time", Keyword::CurrentTime),
    ("materialized", Keyword::Materialized),
    ("autoincrement", Keyword::Autoincrement),
    ("current_timestamp", Keyword::CurrentTimestamp),
];

impl Punct {
    /// The operator's ASCII spelling, which is also how it is written in SQL.
    pub fn as_str(self) -> &'static str {
        match self {
            Punct::LParen => "(",
            Punct::RParen => ")",
            Punct::Comma => ",",
            Punct::Semicolon => ";",
            Punct::Dot => ".",
            Punct::Plus => "+",
            Punct::Minus => "-",
            Punct::Star => "*",
            Punct::Slash => "/",
            Punct::Percent => "%",
            Punct::BitShiftLeft => "<<",
            Punct::BitShiftRight => ">>",
            Punct::BitAnd => "&",
            Punct::BitOr => "|",
            Punct::Concat => "||",
            Punct::Lt => "<",
            Punct::Le => "<=",
            Punct::Gt => ">",
            Punct::Ge => ">=",
            Punct::Eq => "=",
            Punct::EqEq => "==",
            Punct::Ne => "!=",
            Punct::NeBracket => "<>",
            Punct::Tilde => "~",
            Punct::ArrowRight => "->",
            Punct::ArrowRightRight => "->>",
        }
    }
}

impl std::fmt::Display for Punct {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Compares a candidate name against an all-lowercase table entry with ASCII
/// case folded, giving the keyword table's `(length, bytes)` total order.
///
/// Length comes first because that is the order [`KEYWORDS`] is sorted in.
/// Folding is ASCII-only, matching SQLite: bytes at or above `0x80` are
/// compared as they are, so a non-ASCII name can never fold onto a keyword.
fn cmp_folded(name: &str, keyword: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match name.len().cmp(&keyword.len()) {
        Ordering::Equal => {}
        other => return other,
    }
    for (a, b) in name.bytes().zip(keyword.bytes()) {
        match a.to_ascii_lowercase().cmp(&b.to_ascii_lowercase()) {
            Ordering::Equal => {}
            other => return other,
        }
    }
    Ordering::Equal
}

/// True for a byte or scalar that may open a bare identifier.
///
/// `$` is deliberately excluded: SQLite reads a leading `$` as a bind
/// parameter, not as a name.
fn is_id_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_' || (c as u32) >= 0x80
}

/// True for a byte or scalar that may continue a bare identifier, including
/// digits and the `$` that SQLite allows mid-name.
fn is_id_continue(c: char) -> bool {
    is_id_start(c) || c.is_ascii_digit() || c == '$'
}

/// SQLite's `unrecognized token` error for a half-open slice of the source.
fn unrecognized(src: &str, start: usize, end: usize) -> Error {
    Error::new(
        ResultCode::Error,
        format!("unrecognized token: \"{}\"", &src[start..end]),
    )
}

/// The error for an integer that does not fit in an `i64`, spelled the way
/// SQLite spells the hex overflow so the two read alike.
fn integer_too_big(literal: &str) -> Error {
    Error::new(
        ResultCode::Error,
        format!("integer literal too big: {literal}"),
    )
}

/// Parses a numeric literal, ignoring `_` digit separators.
///
/// The separator-free path allocates nothing; the fallback only builds a
/// string when the literal actually contains a separator.
fn parse_int_literal(s: &str) -> Option<i64> {
    if let Ok(v) = s.parse::<i64>() {
        return Some(v);
    }
    let clean = s.replace('_', "");
    clean.parse::<i64>().ok()
}

/// Parses an unsigned decimal literal, ignoring `_` digit separators.
///
/// The separator-free path allocates nothing; the fallback only builds a
/// string when the literal actually contains a separator.
fn parse_u64_literal(s: &str) -> Option<u64> {
    if let Ok(v) = s.parse::<u64>() {
        return Some(v);
    }
    let clean = s.replace('_', "");
    clean.parse::<u64>().ok()
}

/// Parses a hex literal, ignoring `_` digit separators.
fn parse_hex_literal(s: &str) -> Option<u64> {
    if let Ok(v) = u64::from_str_radix(s, 16) {
        return Some(v);
    }
    let clean = s.replace('_', "");
    u64::from_str_radix(&clean, 16).ok()
}

fn parse_f64_literal(s: &str) -> Option<f64> {
    if let Ok(v) = s.parse::<f64>() {
        return Some(v);
    }
    let clean = s.replace('_', "");
    clean.parse::<f64>().ok()
}

/// A pull tokenizer over SQL text.
///
/// Created with [`Tokenizer::new`]; call [`Tokenizer::next_token`] until it
/// returns `None`, or [`Tokenizer::tokenize_all`] for the whole stream.
pub struct Tokenizer<'a> {
    src: &'a str,
    pos: usize,
    line: u32,
    col: u32,
}

impl<'a> Tokenizer<'a> {
    pub fn new(src: &'a str) -> Self {
        Tokenizer {
            src,
            pos: 0,
            line: 1,
            col: 1,
        }
    }

    /// The byte offset the next token will start at.
    pub fn position(&self) -> usize {
        self.pos
    }

    /// Tokenizes all of `src`, stopping at the first error.
    pub fn tokenize_all(src: &'a str) -> Result<Vec<(Token, Span)>> {
        let mut tokenizer = Tokenizer::new(src);
        let mut out = Vec::new();
        while let Some(token) = tokenizer.next_token()? {
            out.push(token);
        }
        Ok(out)
    }

    /// Reads the next token, or `None` at end of input.
    ///
    /// Whitespace and comments before the token are consumed but do not
    /// contribute to its [`Span`], whose `start` is the token's own first byte.
    pub fn next_token(&mut self) -> Result<Option<(Token, Span)>> {
        self.skip_trivia()?;
        let start = self.pos;
        let (line, col) = (self.line, self.col);
        let Some(ch) = self.peek() else {
            return Ok(None);
        };
        let token = self.scan_token(ch, start)?;
        let span = Span {
            start,
            end: self.pos,
            line,
            col,
        };
        Ok(Some((token, span)))
    }

    // ---- character-level helpers -----------------------------------------

    fn peek(&self) -> Option<char> {
        self.src[self.pos..].chars().next()
    }

    /// The character `n` positions past the cursor, without consuming it.
    fn peek_at(&self, n: usize) -> Option<char> {
        self.src[self.pos..].chars().nth(n)
    }

    /// Consumes one character, keeping `line` and `col` in step.
    ///
    /// `col` counts characters rather than bytes, so a multi-byte scalar
    /// advances it by one.
    fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.pos += c.len_utf8();
        if c == '\n' {
            self.line += 1;
            self.col = 1;
        } else {
            self.col += 1;
        }
        Some(c)
    }

    /// Consumes `n` characters if they are there, returning how many it got.
    fn eat(&mut self, mut n: usize) -> usize {
        let mut eaten = 0;
        while n > 0 {
            if self.bump().is_none() {
                break;
            }
            eaten += 1;
            n -= 1;
        }
        eaten
    }

    // ---- trivia -----------------------------------------------------------

    /// Consumes whitespace and comments. A block comment with no `*/` is an
    /// error rather than a silent run to end of input.
    fn skip_trivia(&mut self) -> Result<()> {
        loop {
            let Some(c) = self.peek() else { return Ok(()) };
            match c {
                ' ' | '\t' | '\n' | '\r' | '\u{b}' | '\u{c}' => {
                    self.bump();
                }
                '-' if self.peek_at(1) == Some('-') => {
                    // Runs to the end of the line, or to end of input.
                    while let Some(c) = self.peek() {
                        if c == '\n' {
                            break;
                        }
                        self.bump();
                    }
                }
                '/' if self.peek_at(1) == Some('*') => {
                    let start = self.pos;
                    self.eat(2);
                    loop {
                        match self.peek() {
                            None => return Err(unrecognized(self.src, start, self.pos)),
                            Some('*') if self.peek_at(1) == Some('/') => {
                                self.eat(2);
                                break;
                            }
                            Some(_) => {
                                self.bump();
                            }
                        }
                    }
                }
                _ => return Ok(()),
            }
        }
    }

    // ---- dispatch ---------------------------------------------------------

    fn scan_token(&mut self, ch: char, start: usize) -> Result<Token> {
        match ch {
            '\'' => Ok(Token::String(self.scan_quoted('\'', true, start)?)),
            // A double-quoted run is marked as one, because SQLite's own
            // message about it says the quotes are there. `SELECT "a+b"` is
            // `no such column: "a+b" - should this be a string literal in
            // single-quotes?`, while `SELECT [a+b]` and ``SELECT `a+b``` are
            // `no such column: a+b` -- the same unresolved name, and the only
            // difference between the two messages is the character that opened
            // it. Which is why the flag lives on the token rather than in the
            // resolver: by the time a name fails to resolve the quoting has
            // already been consumed.
            '"' => Ok(Token::DoubleQuotedIdentifier(
                self.scan_quoted('"', true, start)?,
            )),
            '`' => Ok(Token::Identifier(self.scan_quoted('`', true, start)?)),
            '[' => Ok(Token::Identifier(self.scan_bracket(start)?)),

            '0'..='9' => self.scan_number(start),

            '?' | ':' | '@' | '$' => self.scan_parameter(start),

            '(' => self.one(Punct::LParen),
            ')' => self.one(Punct::RParen),
            ',' => self.one(Punct::Comma),
            ';' => self.one(Punct::Semicolon),
            '.' => self.one(Punct::Dot),
            '+' => self.one(Punct::Plus),
            '*' => self.one(Punct::Star),
            '/' => self.one(Punct::Slash),
            '%' => self.one(Punct::Percent),
            '&' => self.one(Punct::BitAnd),
            '~' => self.one(Punct::Tilde),

            '-' => self.scan_minus(start),
            '<' => self.scan_lt(start),
            '>' => self.scan_gt(start),
            '=' => {
                self.bump();
                if self.peek() == Some('=') {
                    self.bump();
                    Ok(Token::Punct(Punct::EqEq))
                } else {
                    Ok(Token::Punct(Punct::Eq))
                }
            }
            '!' => {
                // `!` alone is not an operator; only `!=` is.
                self.bump();
                if self.peek() == Some('=') {
                    self.bump();
                    Ok(Token::Punct(Punct::Ne))
                } else {
                    Err(unrecognized(self.src, start, self.pos))
                }
            }
            '|' => {
                self.bump();
                if self.peek() == Some('|') {
                    self.bump();
                    Ok(Token::Punct(Punct::Concat))
                } else {
                    Ok(Token::Punct(Punct::BitOr))
                }
            }

            c if is_id_start(c) => self.scan_identifier(),

            other => Err(Error::new(
                ResultCode::Error,
                format!("unrecognized token: \"{other}\""),
            )),
        }
    }

    fn one(&mut self, p: Punct) -> Result<Token> {
        self.bump();
        Ok(Token::Punct(p))
    }

    /// `-`, `--` is already handled as trivia, so only `-` and the two arrows
    /// are left.
    fn scan_minus(&mut self, start: usize) -> Result<Token> {
        self.bump();
        if self.peek() != Some('>') {
            return Ok(Token::Punct(Punct::Minus));
        }
        self.bump();
        if self.peek() == Some('>') {
            self.bump();
            return Ok(Token::Punct(Punct::ArrowRightRight));
        }
        let _ = start;
        Ok(Token::Punct(Punct::ArrowRight))
    }

    fn scan_lt(&mut self, start: usize) -> Result<Token> {
        self.bump();
        let p = match self.peek() {
            Some('<') => {
                self.bump();
                Punct::BitShiftLeft
            }
            Some('=') => {
                self.bump();
                Punct::Le
            }
            Some('>') => {
                self.bump();
                Punct::NeBracket
            }
            _ => Punct::Lt,
        };
        let _ = start;
        Ok(Token::Punct(p))
    }

    fn scan_gt(&mut self, start: usize) -> Result<Token> {
        self.bump();
        let p = match self.peek() {
            Some('>') => {
                self.bump();
                Punct::BitShiftRight
            }
            Some('=') => {
                self.bump();
                Punct::Ge
            }
            _ => Punct::Gt,
        };
        let _ = start;
        Ok(Token::Punct(p))
    }

    // ---- names, strings, parameters ---------------------------------------

    /// A `'`, `"`, or `` ` `` run. A doubled quote is that quote; for `[..]`
    /// there is no escape at all, which is what keeps brackets from nesting.
    fn scan_quoted(&mut self, quote: char, doubled: bool, start: usize) -> Result<String> {
        self.bump();
        let mut out = String::new();
        loop {
            let Some(c) = self.bump() else {
                return Err(unrecognized(self.src, start, self.pos));
            };
            if c == quote {
                if doubled && self.peek() == Some(quote) {
                    self.bump();
                    out.push(quote);
                } else {
                    return Ok(out);
                }
            } else {
                out.push(c);
            }
        }
    }

    fn scan_bracket(&mut self, start: usize) -> Result<String> {
        self.bump();
        let mut out = String::new();
        loop {
            let Some(c) = self.bump() else {
                return Err(unrecognized(self.src, start, self.pos));
            };
            if c == ']' {
                return Ok(out);
            }
            out.push(c);
        }
    }

    fn scan_identifier(&mut self) -> Result<Token> {
        let start = self.pos;
        while self.peek().is_some_and(is_id_continue) {
            self.bump();
        }
        let raw = &self.src[start..self.pos];
        // A lone `x` immediately followed by a quote opens a blob literal, so
        // it has to be checked before the keyword/name decision. SQLite allows
        // no space between the two, and an odd digit count is an error.
        if (raw == "x" || raw == "X") && self.peek() == Some('\'') {
            return self.scan_blob(start);
        }
        // Keywords are matched on a lowercase view, but the token carries the
        // original bytes folded, so no extra allocation is needed.
        let mut folded = String::with_capacity(raw.len());
        folded.extend(raw.chars().map(|c| c.to_ascii_lowercase()));
        match Keyword::from_name(&folded) {
            Some(kw) => Ok(Token::Keyword(kw)),
            None => Ok(Token::Identifier(folded)),
        }
    }

    /// `x'ABCD'`: an even number of hex digits, which is what makes the byte
    /// count unambiguous. The `''` escape is *not* honoured here — a doubled
    /// quote is a hex digit, not a digit — so the run simply ends at the first
    /// quote.
    fn scan_blob(&mut self, start: usize) -> Result<Token> {
        self.bump(); // the opening quote
        let digits_start = self.pos;
        while self.peek().is_some_and(|c| c != '\'') {
            self.bump();
        }
        if self.peek().is_none() {
            return Err(unrecognized(self.src, start, self.pos));
        }
        let digits = &self.src[digits_start..self.pos];
        // An odd count cannot be split into whole bytes, which is exactly what
        // `& 1` tests. (Not `is_multiple_of`, which postdates the MSRV.)
        if digits.len() & 1 != 0 || !digits.chars().all(|c| c.is_ascii_hexdigit()) {
            // Close it only if it is there, so the message covers the run.
            self.bump();
            return Err(unrecognized(self.src, start, self.pos));
        }
        self.bump(); // the closing quote
        let mut out = Vec::with_capacity(digits.len() / 2);
        let bytes = digits.as_bytes();
        for pair in bytes.chunks_exact(2) {
            let hi = (pair[0] as char).to_digit(16).unwrap_or(0);
            let lo = (pair[1] as char).to_digit(16).unwrap_or(0);
            out.push(((hi << 4) | lo) as u8);
        }
        Ok(Token::Blob(out))
    }

    /// `?`, `?NNN`, `?name`, `:name`, `@name`, `$name`.
    ///
    /// A digit right after the sigil makes it an index, matching SQLite's
    /// `:1`; otherwise a name is read. A `?` with neither is an anonymous
    /// parameter. A sigil followed by nothing that can be part of a name is an
    /// error on all four, so a lone `$` is not mistaken for a parameter.
    ///
    /// The name may start with a digit or a `$`, because SQLite's test is
    /// `IdChar` (anything alphanumeric, `_`, `$` or non-ASCII) rather than the
    /// stricter "identifier start" test. That is what makes `SELECT :$`,
    /// `SELECT :0` and `SELECT $$` legal, with the whole run from the
    /// character after the sigil as the name.
    fn scan_parameter(&mut self, start: usize) -> Result<Token> {
        let sigil = self.bump().unwrap_or('?');
        if self.peek().is_some_and(|c| c.is_ascii_digit()) {
            let name_start = self.pos;
            while self.peek().is_some_and(|c| c.is_ascii_digit()) {
                self.bump();
            }
            let text = &self.src[name_start..self.pos];
            let index = parse_u64_literal(text)
                .and_then(|v| usize::try_from(v).ok())
                .ok_or_else(|| unrecognized(self.src, start, self.pos))?;
            return Ok(Token::Parameter {
                index: Some(index),
                name: None,
            });
        }
        if sigil == '?' && !self.peek().is_some_and(is_id_continue) {
            return Ok(Token::Parameter {
                index: None,
                name: None,
            });
        }
        if !self.peek().is_some_and(is_id_continue) {
            return Err(unrecognized(self.src, start, self.pos));
        }
        while self.peek().is_some_and(is_id_continue) {
            self.bump();
        }
        // The sigil is part of the name, because SQLite treats the three forms
        // as three different parameters: a statement binding `@x`, `:x` and
        // `$x` has three parameters, not one written three ways. Dropping the
        // sigil here would make them collide and bind one slot three times.
        let name = self.src[start..self.pos].to_string();
        Ok(Token::Parameter {
            index: None,
            name: Some(name),
        })
    }

    // ---- numbers ----------------------------------------------------------

    /// Consumes a digit run, allowing one `_` between two digits.
    ///
    /// Returns `false` when a separator is leading, trailing, or doubled. The
    /// run is consumed up to that point either way; the caller turns the
    /// result into an `unrecognized token` error, and the leftover is swept up
    /// by [`Self::reject_trailing_ident`] so the message covers the whole word.
    fn digits(&mut self, hex: bool) -> bool {
        let mut seen_digit = false;
        let mut after_sep = false;
        while let Some(c) = self.peek() {
            if c == '_' {
                if !seen_digit || after_sep {
                    return false;
                }
                after_sep = true;
                self.bump();
                continue;
            }
            let is_digit = if hex {
                c.is_ascii_hexdigit()
            } else {
                c.is_ascii_digit()
            };
            if !is_digit {
                break;
            }
            seen_digit = true;
            after_sep = false;
            self.bump();
        }
        seen_digit && !after_sep
    }

    /// Rejects a literal that runs straight into an identifier, as SQLite does
    /// for `1abc` and `1.2e5x`, sweeping the offending run into the message.
    fn reject_trailing_ident(&mut self, start: usize) -> Result<()> {
        if self.peek().is_some_and(is_id_continue) {
            while self.peek().is_some_and(is_id_continue) {
                self.bump();
            }
            return Err(unrecognized(self.src, start, self.pos));
        }
        Ok(())
    }

    /// Decimal, hex, and exponent literals. A number always starts with a
    /// digit here, so the integer part is never empty.
    fn scan_number(&mut self, start: usize) -> Result<Token> {
        if self.peek() == Some('0') && matches!(self.peek_at(1), Some('x' | 'X')) {
            return self.scan_hex(start);
        }
        if !self.digits(false) {
            // A separator that leads, trails, or doubles, as in `1_` or `1__0`.
            while self.peek().is_some_and(is_id_continue) {
                self.bump();
            }
            return Err(unrecognized(self.src, start, self.pos));
        }
        let mut is_float = false;
        if self.peek() == Some('.') {
            is_float = true;
            self.bump();
            // Digits after the point are optional, so a trailing `.` is fine.
            if !self.digits(false) {
                // A separator that trails, as in `1._5`.
                if self.peek() == Some('_') {
                    while self.peek().is_some_and(is_id_continue) {
                        self.bump();
                    }
                    return Err(unrecognized(self.src, start, self.pos));
                }
            }
        }
        if matches!(self.peek(), Some('e' | 'E')) {
            is_float = true;
            self.bump();
            // The sign belongs to the exponent only when digits follow it;
            // otherwise SQLite stops the token at the `e` and leaves the sign
            // for the next token, which is why `1e+` reports just `1e`.
            //
            // All three counters are saved and restored, not just `pos`:
            // `bump` advances `col` too, so rewinding `pos` alone would leave
            // the column permanently one ahead and every later token in the
            // stream would report a column that is too large.
            let before_sign = (self.pos, self.line, self.col);
            if matches!(self.peek(), Some('+' | '-')) {
                self.bump();
            }
            if !self.digits(false) {
                self.pos = before_sign.0;
                self.line = before_sign.1;
                self.col = before_sign.2;
                return Err(unrecognized(self.src, start, self.pos));
            }
        }
        self.reject_trailing_ident(start)?;
        let text = &self.src[start..self.pos];
        if is_float {
            return Ok(Token::Float(
                parse_f64_literal(text).ok_or_else(|| unrecognized(self.src, start, self.pos))?,
            ));
        }
        // A decimal too large for i64 widens to a real, the way SQLite does.
        match parse_int_literal(text) {
            Some(v) => Ok(Token::Integer(v)),
            None => match parse_f64_literal(text) {
                Some(v) => Ok(Token::Float(v)),
                // Unreachable for real digits, but a total function cannot
                // leave a value unrepresented.
                None => Err(integer_too_big(text)),
            },
        }
    }

    /// `0x` hex integers. They wrap rather than widen: `0xFFFFFFFFFFFFFFFF` is
    /// `-1`, and anything past 64 bits is rejected.
    fn scan_hex(&mut self, start: usize) -> Result<Token> {
        self.eat(2);
        if !self.digits(true) {
            // No usable digits, or a separator that leads, trails, or doubles.
            // Sweep the rest of the word so `0xg` and `0x_1` report in full.
            while self.peek().is_some_and(is_id_continue) {
                self.bump();
            }
            return Err(unrecognized(self.src, start, self.pos));
        }
        self.reject_trailing_ident(start)?;
        // The underscore is a separator, not a digit, so it is dropped before
        // the value is read: `0x1_0` is sixteen, not one-zero.
        let text = self.src[start + 2..self.pos].replace('_', "");
        let value = parse_hex_literal(&text).ok_or_else(|| {
            Error::new(
                ResultCode::Error,
                format!("hex literal too big: {}", &self.src[start..self.pos]),
            )
        })?;
        Ok(Token::Integer(value as i64))
    }
}

impl Iterator for Tokenizer<'_> {
    type Item = Result<(Token, Span)>;

    fn next(&mut self) -> Option<Self::Item> {
        self.next_token().transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tokens of `src`, with spans dropped.
    fn toks(src: &str) -> Vec<Token> {
        Tokenizer::tokenize_all(src)
            .unwrap_or_else(|e| panic!("tokenizing {src:?} failed: {e}"))
            .into_iter()
            .map(|(t, _)| t)
            .collect()
    }

    /// The first token of `src`.
    fn one(src: &str) -> Token {
        let mut t = toks(src);
        assert_eq!(t.len(), 1, "expected one token from {src:?}, got {t:?}");
        t.pop().unwrap()
    }

    /// The error `src` produces, or a panic.
    fn err(src: &str) -> Error {
        match Tokenizer::tokenize_all(src) {
            Ok(ts) => panic!("expected {src:?} to fail, got {ts:?}"),
            Err(e) => e,
        }
    }

    fn ident(s: &str) -> Token {
        Token::Identifier(s.to_string())
    }

    /// A name written in double quotes, which is its own token so the quoting
    /// survives to the `no such column` message that is about it.
    fn dquoted(s: &str) -> Token {
        Token::DoubleQuotedIdentifier(s.to_string())
    }

    fn kw(s: &str) -> Token {
        Token::Keyword(Keyword::from_name(s).unwrap())
    }

    fn punct(p: Punct) -> Token {
        Token::Punct(p)
    }

    // ---- names and keywords ------------------------------------------------

    #[test]
    fn bare_identifiers_fold_to_lowercase() {
        assert_eq!(one("Foo"), ident("foo"));
        assert_eq!(one("FOO"), ident("foo"));
        assert_eq!(one("fOo"), ident("foo"));
    }

    #[test]
    fn identifiers_take_digits_underscores_and_dollars() {
        assert_eq!(one("a1"), ident("a1"));
        assert_eq!(one("_x"), ident("_x"));
        assert_eq!(one("__"), ident("__"));
        assert_eq!(one("a_b_1"), ident("a_b_1"));
        // SQLite allows `$` inside a name, just not at the front.
        assert_eq!(one("a$b$c"), ident("a$b$c"));
        assert_eq!(one("x9_$"), ident("x9_$"));
    }

    #[test]
    fn a_leading_dollar_is_a_parameter_not_a_name() {
        assert_eq!(
            one("$x"),
            Token::Parameter {
                index: None,
                name: Some("$x".into())
            }
        );
    }

    #[test]
    fn non_ascii_identifiers_survive() {
        assert_eq!(one("café"), ident("café"));
        assert_eq!(one("中文"), ident("中文"));
    }

    #[test]
    fn keywords_fold_regardless_of_case() {
        assert_eq!(one("SeLeCt"), kw("select"));
        assert_eq!(one("SELECT"), kw("select"));
        assert_eq!(kw("select"), Token::Keyword(Keyword::Select));
        assert_eq!(one("wItHoUt"), kw("without"));
        assert_eq!(one("CuRrEnT_tImEsTaMp"), kw("current_timestamp"));
    }

    #[test]
    fn the_keyword_table_matches_sqlites() {
        // `WITHIN` is in SQLite's `aKeywordTable` (TK_WITHIN, ORDERSET), so it
        // is a keyword here too; it is the one word a first pass leaves out.
        assert_eq!(one("WiThIn"), Token::Keyword(Keyword::Within));
        assert_eq!(Keyword::from_name("within"), Some(Keyword::Within));
        assert_eq!(Keyword::Within.as_ident(), "within");
    }

    #[test]
    fn as_ident_hands_back_the_lowercase_name() {
        assert_eq!(Keyword::Select.as_ident(), "select");
        assert_eq!(Keyword::CurrentTimestamp.as_ident(), "current_timestamp");
        // Round trip: every keyword resolves back to its own spelling.
        for &k in Keyword::ALL {
            assert_eq!(Keyword::from_name(k.as_ident()), Some(k));
        }
    }

    #[test]
    fn non_keywords_stay_identifiers() {
        for name in [
            "selecta", "sel", "keys", "values2", "tabled", "iffy", "orders", "x",
        ] {
            assert_eq!(one(name), ident(name), "{name} should be a name");
        }
    }

    #[test]
    fn keyword_lookup_is_case_insensitive_both_ways() {
        assert_eq!(Keyword::from_name("SELECT"), Some(Keyword::Select));
        assert_eq!(Keyword::from_name("sElEcT"), Some(Keyword::Select));
        assert_eq!(Keyword::from_name("nonesuch"), None);
        // A prefix of a keyword must not match it.
        assert_eq!(Keyword::from_name("sel"), None);
    }

    #[test]
    fn the_keyword_table_is_sorted_for_binary_search() {
        // The table is searched by `(length, bytes)`; this catches a row that
        // was inserted out of order, which would silently break lookups.
        for pair in KEYWORDS.windows(2) {
            let (a, b) = (pair[0].0, pair[1].0);
            assert!(
                (a.len(), a) < (b.len(), b),
                "keyword table out of order: {a:?} then {b:?}"
            );
        }
    }

    // ---- quoting -----------------------------------------------------------

    #[test]
    fn all_four_quoting_forms_are_accepted() {
        assert_eq!(one("\"a b\""), dquoted("a b"));
        assert_eq!(one("[a b]"), ident("a b"));
        assert_eq!(one("`a b`"), ident("a b"));
        assert_eq!(one("'a b'"), Token::String("a b".into()));
    }

    #[test]
    fn a_doubled_quote_is_that_quote() {
        assert_eq!(one("'a''b'"), Token::String("a'b".into()));
        assert_eq!(one("\"a\"\"b\""), dquoted("a\"b"));
        assert_eq!(one("`a``b`"), ident("a`b"));
    }

    #[test]
    fn single_quotes_do_not_escape_inside_double_or_bracket_quotes() {
        assert_eq!(one("\"a'b\""), dquoted("a'b"));
        assert_eq!(one("[a'b]"), ident("a'b"));
        assert_eq!(one("'a\"b'"), Token::String("a\"b".into()));
    }

    #[test]
    fn brackets_do_not_nest() {
        // The first `]` closes the name, so an inner `[` is ordinary content
        // and the leftover `]` is what fails, not the name itself.
        assert_eq!(one("[a[b]"), ident("a[b"));
        assert_eq!(err("[a]b]").message, "unrecognized token: \"]\"");
    }

    #[test]
    fn quoted_names_keep_their_case() {
        assert_eq!(one("\"MixedCase\""), dquoted("MixedCase"));
        assert_eq!(one("[MixedCase]"), ident("MixedCase"));
        assert_eq!(one("`MixedCase`"), ident("MixedCase"));
    }

    // ---- strings and blobs -------------------------------------------------

    #[test]
    fn strings_keep_their_contents_verbatim() {
        assert_eq!(one("''"), Token::String(String::new()));
        assert_eq!(one("'hello world'"), Token::String("hello world".into()));
        assert_eq!(
            one("'tab\there\nnewline'"),
            Token::String("tab\there\nnewline".into())
        );
    }

    #[test]
    fn backslash_is_not_an_escape_in_a_string() {
        // SQLite has no backslash escapes by default, so both survive: the
        // backslash is content and the quote still closes the literal.
        assert_eq!(one("'a\\b'"), Token::String("a\\b".into()));
        assert_eq!(one("'a\\'"), Token::String("a\\".into()));
    }

    #[test]
    fn blob_literals_decode_hex_pairs() {
        assert_eq!(one("x'ABCD'"), Token::Blob(vec![0xab, 0xcd]));
        assert_eq!(one("X'abcd'"), Token::Blob(vec![0xab, 0xcd]));
        assert_eq!(one("x''"), Token::Blob(vec![]));
        assert_eq!(one("x'00ff7F'"), Token::Blob(vec![0x00, 0xff, 0x7f]));
    }

    #[test]
    fn an_odd_blob_digit_count_is_an_error() {
        assert_eq!(err("x'ABC'").message, "unrecognized token: \"x'ABC'\"");
        assert_eq!(err("x'ABCDE'").message, "unrecognized token: \"x'ABCDE'\"");
    }

    #[test]
    fn a_non_hex_blob_digit_is_an_error() {
        assert_eq!(err("x'ABCG'").message, "unrecognized token: \"x'ABCG'\"");
        assert_eq!(err("x'zz'").message, "unrecognized token: \"x'zz'\"");
    }

    #[test]
    fn a_doubled_quote_in_a_blob_is_not_an_escape() {
        // `''` is a hex digit, not a digit, so `x'AB''CD'` is the single byte
        // `AB` and then whatever follows the first closing quote.
        let t = toks("x'AB''CD'");
        assert_eq!(t[0], Token::Blob(vec![0xab]));
        assert_eq!(t[1], Token::String("CD".into()));
    }

    #[test]
    fn x_followed_by_a_space_is_just_the_name_x() {
        assert_eq!(toks("x 'ab'")[0], ident("x"));
    }

    #[test]
    fn a_quote_after_a_longer_name_is_not_a_blob() {
        assert_eq!(toks("xy 'ab'")[0], ident("xy"));
    }

    // ---- numbers -----------------------------------------------------------

    #[test]
    fn decimal_integers_parse() {
        assert_eq!(one("0"), Token::Integer(0));
        assert_eq!(one("42"), Token::Integer(42));
        assert_eq!(one("00"), Token::Integer(0));
        assert_eq!(one("9223372036854775807"), Token::Integer(i64::MAX));
    }

    #[test]
    fn a_minus_is_its_own_operator() {
        // SQLite folds a minus into a following literal, but that is the
        // parser's job: lexically they are two tokens.
        assert_eq!(
            toks("-9223372036854775808"),
            vec![
                punct(Punct::Minus),
                Token::Float(9_223_372_036_854_775_808.0)
            ]
        );
        assert_eq!(toks("-1"), vec![punct(Punct::Minus), Token::Integer(1)]);
        assert_eq!(
            toks("5 -2"),
            vec![Token::Integer(5), punct(Punct::Minus), Token::Integer(2)]
        );
    }

    #[test]
    fn an_integer_past_i64_widens_to_a_real() {
        assert_eq!(
            one("9223372036854775808"),
            Token::Float(9_223_372_036_854_775_808.0)
        );
        assert_eq!(
            one("12345678901234567890"),
            Token::Float(1.2345678901234567e19)
        );
    }

    #[test]
    fn floats_parse_in_every_dialect() {
        assert_eq!(one("0.5"), Token::Float(0.5));
        assert_eq!(one("1."), Token::Float(1.0));
        assert_eq!(one("1e10"), Token::Float(1e10));
        assert_eq!(one("1E-5"), Token::Float(1e-5));
        assert_eq!(one("1.5e+3"), Token::Float(1500.0));
        assert_eq!(one("1.e5"), Token::Float(1e5));
    }

    #[test]
    fn a_leading_dot_is_not_a_number() {
        // `.` is always the member operator, so `.5` is a dot followed by the
        // number five rather than one number that begins with a dot.
        assert_eq!(toks(".5"), vec![punct(Punct::Dot), Token::Integer(5)]);
        assert_eq!(one("."), punct(Punct::Dot));
        assert_eq!(one("0.5"), Token::Float(0.5));
    }

    #[test]
    fn hex_integers_parse_and_wrap() {
        assert_eq!(one("0xff"), Token::Integer(255));
        assert_eq!(one("0XFF"), Token::Integer(255));
        assert_eq!(one("0x0"), Token::Integer(0));
        assert_eq!(one("0x7fffffffffffffff"), Token::Integer(i64::MAX));
        // 64 bits wrap rather than widen, matching SQLite.
        assert_eq!(one("0xFFFFFFFFFFFFFFFF"), Token::Integer(-1));
    }

    #[test]
    fn a_hex_literal_past_64_bits_is_an_error() {
        assert_eq!(
            err("0x10000000000000000").message,
            "hex literal too big: 0x10000000000000000"
        );
    }

    #[test]
    fn a_hex_literal_with_no_digits_is_an_error() {
        assert_eq!(err("0x").message, "unrecognized token: \"0x\"");
        assert_eq!(err("0xg").message, "unrecognized token: \"0xg\"");
        assert_eq!(err("0X").message, "unrecognized token: \"0X\"");
        assert_eq!(err("0x_1").message, "unrecognized token: \"0x_1\"");
    }

    #[test]
    fn underscores_separate_digits_in_every_dialect() {
        assert_eq!(one("1_000"), Token::Integer(1000));
        assert_eq!(one("0x1_0"), Token::Integer(16));
        assert_eq!(one("1_0.5_5e1_0"), Token::Float(1.055e11));
    }

    #[test]
    fn a_misplaced_underscore_is_an_error() {
        assert_eq!(err("1_abc").message, "unrecognized token: \"1_abc\"");
        assert_eq!(err("1_").message, "unrecognized token: \"1_\"");
        assert_eq!(err("1__0").message, "unrecognized token: \"1__0\"");
        assert_eq!(err("0x1__0").message, "unrecognized token: \"0x1__0\"");
    }

    #[test]
    fn a_number_may_not_run_into_a_name() {
        assert_eq!(err("1abc").message, "unrecognized token: \"1abc\"");
        assert_eq!(err("12a34").message, "unrecognized token: \"12a34\"");
        assert_eq!(err("1.a").message, "unrecognized token: \"1.a\"");
        assert_eq!(err("1.2e5x").message, "unrecognized token: \"1.2e5x\"");
    }

    #[test]
    fn a_dangling_exponent_is_an_error() {
        assert_eq!(err("1e").message, "unrecognized token: \"1e\"");
        // The sign is left out of the message, as SQLite leaves it out of the
        // token; it would be reported on its own.
        assert_eq!(err("1e+").message, "unrecognized token: \"1e\"");
        assert_eq!(err("1.5e+").message, "unrecognized token: \"1.5e\"");
    }

    #[test]
    fn a_dangling_exponent_leaves_the_column_counter_in_step() {
        // The rewind on the exponent error path must restore the column along
        // with the position, or every later token reports a column that is too
        // large. The column is 1-based, so it is the count of characters before
        // the token's first byte.
        for src in ["1e+ 2", "1e- 2", "1e+1_ 2", "1e+ ;", "1e+1_ 2 3"] {
            let mut t = Tokenizer::new(src);
            let mut seen_error = false;
            loop {
                match t.next_token() {
                    Ok(Some((token, span))) => {
                        let col = src[..span.start].chars().count() as u32 + 1;
                        assert_eq!(span.col, col, "wrong col for {token:?} in {src:?}");
                    }
                    Ok(None) => break,
                    Err(_) => {
                        seen_error = true;
                        break;
                    }
                }
            }
            assert!(seen_error, "{src:?} was expected to fail somewhere");
        }
    }

    // ---- parameters --------------------------------------------------------

    #[test]
    fn positional_parameters_carry_an_index() {
        assert_eq!(
            one("?"),
            Token::Parameter {
                index: None,
                name: None
            }
        );
        assert_eq!(
            one("?1"),
            Token::Parameter {
                index: Some(1),
                name: None
            }
        );
        assert_eq!(
            one("?12345"),
            Token::Parameter {
                index: Some(12345),
                name: None
            }
        );
    }

    #[test]
    fn named_parameters_carry_a_name() {
        for sigil in [':', '@', '$'] {
            let src = format!("{sigil}name");
            // The sigil is kept, so the three spellings are three parameters
            // rather than one name written three ways.
            assert_eq!(
                one(&src),
                Token::Parameter {
                    index: None,
                    name: Some(src.clone())
                },
                "{src} should keep its sigil"
            );
        }
        assert_eq!(
            one("?name"),
            Token::Parameter {
                index: None,
                // The sigil stays in the name here too, so ?name is a name and
                // not the anonymous ? that takes the next free index.
                name: Some("?name".into())
            }
        );
    }

    #[test]
    fn a_sigil_may_follow_a_digit_for_an_index() {
        assert_eq!(
            one(":1"),
            Token::Parameter {
                index: Some(1),
                name: None
            }
        );
    }

    #[test]
    fn a_bare_sigil_is_an_error() {
        assert_eq!(err("$").message, "unrecognized token: \"$\"");
        assert_eq!(err(":").message, "unrecognized token: \":\"");
        assert_eq!(err("@").message, "unrecognized token: \"@\"");
        // A sigil followed by something that cannot be in a name is the same
        // error, still naming only the sigil.
        assert_eq!(err("$(").message, "unrecognized token: \"$\"");
        assert_eq!(err(": ").message, "unrecognized token: \":\"");
        assert_eq!(err("@;").message, "unrecognized token: \"@\"");
    }

    #[test]
    fn a_sigil_name_may_start_with_a_digit_or_dollar() {
        // SQLite tests the first name character with `IdChar`, which admits
        // digits and `$`, not just alphabetic ones. Checked against
        // `sqlite3 3.53.4`, which accepts every one of these.
        for src in [":$", "@$", "$$", ":0", "@0", "$0", ":$x", "$$abc", ":_a"] {
            assert!(
                Tokenizer::tokenize_all(src).is_ok(),
                "{src:?} should tokenize"
            );
        }
        assert_eq!(
            one(":$"),
            Token::Parameter {
                index: None,
                name: Some(":$".into())
            }
        );
        assert_eq!(
            one("$$abc"),
            Token::Parameter {
                index: None,
                name: Some("$$abc".into())
            }
        );
        // A digit run right after the sigil is still an index, not a name.
        assert_eq!(
            one(":0"),
            Token::Parameter {
                index: Some(0),
                name: None
            }
        );
    }

    #[test]
    fn an_anonymous_question_mark_needs_no_name() {
        // `?` alone is fine, but a `?` followed by a `$` is not: SQLite reads
        // a name after `?` only when an `IdChar` comes next, and `$` is one,
        // so the name is `$` here and the run simply ends.
        assert_eq!(
            one("?"),
            Token::Parameter {
                index: None,
                name: None
            }
        );
        assert!(Tokenizer::tokenize_all("?x").is_ok());
        assert!(Tokenizer::tokenize_all("?1").is_ok());
    }

    // ---- punctuation -------------------------------------------------------

    #[test]
    fn punctuation_splits_into_the_right_variants() {
        assert_eq!(
            toks("( ) , ; ."),
            vec![
                punct(Punct::LParen),
                punct(Punct::RParen),
                punct(Punct::Comma),
                punct(Punct::Semicolon),
                punct(Punct::Dot),
            ]
        );
    }

    #[test]
    fn multi_character_operators_split_the_sqlite_way() {
        assert_eq!(
            toks("|| << >> & | + - * / % ~"),
            vec![
                punct(Punct::Concat),
                punct(Punct::BitShiftLeft),
                punct(Punct::BitShiftRight),
                punct(Punct::BitAnd),
                punct(Punct::BitOr),
                punct(Punct::Plus),
                punct(Punct::Minus),
                punct(Punct::Star),
                punct(Punct::Slash),
                punct(Punct::Percent),
                punct(Punct::Tilde),
            ]
        );
    }

    #[test]
    fn comparison_operators_split_the_sqlite_way() {
        assert_eq!(
            toks("< <= > >= = == != <>"),
            vec![
                punct(Punct::Lt),
                punct(Punct::Le),
                punct(Punct::Gt),
                punct(Punct::Ge),
                punct(Punct::Eq),
                punct(Punct::EqEq),
                punct(Punct::Ne),
                punct(Punct::NeBracket),
            ]
        );
    }

    #[test]
    fn the_json_arrow_operators_split_the_sqlite_way() {
        assert_eq!(
            toks("-> ->>"),
            vec![punct(Punct::ArrowRight), punct(Punct::ArrowRightRight)]
        );
    }

    #[test]
    fn punct_spells_itself() {
        assert_eq!(Punct::ArrowRightRight.as_str(), "->>");
        assert_eq!(Punct::NeBracket.to_string(), "<>");
        assert_eq!(Punct::LParen.as_str(), "(");
    }

    // ---- comments and whitespace ------------------------------------------

    #[test]
    fn line_comments_run_to_the_end_of_the_line() {
        assert_eq!(
            toks("1 -- trailing\n2"),
            vec![Token::Integer(1), Token::Integer(2)]
        );
        assert_eq!(
            toks("1 -- trailing\r\n2"),
            vec![Token::Integer(1), Token::Integer(2)]
        );
        // A comment that runs to end of input is fine.
        assert_eq!(toks("1 -- trailing"), vec![Token::Integer(1)]);
    }

    #[test]
    fn block_comments_do_not_nest() {
        assert_eq!(
            toks("1 /* a /* b */ 2"),
            vec![Token::Integer(1), Token::Integer(2)]
        );
        assert_eq!(toks("/* a */ /* b */ 1"), vec![Token::Integer(1)]);
    }

    #[test]
    fn all_whitespace_is_skipped() {
        assert_eq!(toks(" \t\r\n\u{b}\u{c} 1 "), vec![Token::Integer(1)]);
        assert_eq!(toks("   "), Vec::<Token>::new());
    }

    // ---- errors ------------------------------------------------------------

    #[test]
    fn an_unterminated_string_is_an_error_not_a_panic() {
        assert_eq!(err("'abc").message, "unrecognized token: \"'abc\"");
        assert_eq!(err("'").message, "unrecognized token: \"'\"");
    }

    #[test]
    fn an_unterminated_quoted_name_is_an_error() {
        assert_eq!(err(r#""abc"#).message, r#"unrecognized token: ""abc""#);
        assert_eq!(err("`abc").message, "unrecognized token: \"`abc\"");
        assert_eq!(err("[abc").message, "unrecognized token: \"[abc\"");
    }

    #[test]
    fn an_unterminated_block_comment_is_an_error() {
        assert!(Tokenizer::tokenize_all("/* never closed").is_err());
        assert!(Tokenizer::tokenize_all("1 /* x").is_err());
    }

    #[test]
    fn an_unterminated_blob_is_an_error() {
        assert!(Tokenizer::tokenize_all("x'AB").is_err());
        assert!(Tokenizer::tokenize_all("x'").is_err());
    }

    #[test]
    fn an_unknown_character_reports_itself() {
        assert_eq!(err("!").message, "unrecognized token: \"!\"");
        assert_eq!(err("#").message, "unrecognized token: \"#\"");
        assert_eq!(err("^").message, "unrecognized token: \"^\"");
        assert_eq!(err("]").message, "unrecognized token: \"]\"");
        assert_eq!(err("\u{1}").message, "unrecognized token: \"\u{1}\"");
    }

    #[test]
    fn errors_carry_the_error_result_code() {
        assert_eq!(err("!").code, ResultCode::Error);
        assert_eq!(err("'abc").code, ResultCode::Error);
    }

    // ---- spans and positions ----------------------------------------------

    #[test]
    fn spans_cover_the_token_text() {
        let got = Tokenizer::tokenize_all("SELECT a FROM t").unwrap();
        let spans: Vec<_> = got.iter().map(|(_, s)| (s.start, s.end)).collect();
        assert_eq!(spans, vec![(0, 6), (7, 8), (9, 13), (14, 15)]);
    }

    #[test]
    fn spans_report_line_and_column() {
        let got = Tokenizer::tokenize_all("SELECT\n  a\nFROM t").unwrap();
        assert_eq!((got[0].1.line, got[0].1.col), (1, 1));
        assert_eq!((got[1].1.line, got[1].1.col), (2, 3));
        assert_eq!((got[2].1.line, got[2].1.col), (3, 1));
    }

    #[test]
    fn a_comment_does_not_disturb_the_next_token_position() {
        let got = Tokenizer::tokenize_all("-- lead\n  x").unwrap();
        assert_eq!((got[0].1.line, got[0].1.col), (2, 3));
    }

    #[test]
    fn column_counts_characters_not_bytes() {
        // `é` is two bytes, so the token after it starts one column later.
        let got = Tokenizer::tokenize_all("é x").unwrap();
        assert_eq!(got[1].1.col, 3);
    }

    #[test]
    fn the_iterator_and_tokenize_all_agree() {
        let src = "SELECT a, b FROM t WHERE a = 1";
        let via_iter: Vec<Token> = Tokenizer::new(src).map(|r| r.unwrap().0).collect();
        assert_eq!(via_iter, toks(src));
    }

    #[test]
    fn empty_input_yields_no_tokens() {
        assert_eq!(Tokenizer::tokenize_all("").unwrap(), Vec::new());
        assert_eq!(Tokenizer::new("").next_token().unwrap(), None);
    }

    #[test]
    fn a_semicolon_and_trailing_space_end_the_stream() {
        assert_eq!(
            toks("1;  "),
            vec![Token::Integer(1), punct(Punct::Semicolon)]
        );
    }

    #[test]
    fn tokenizing_leaves_the_cursor_where_the_error_was() {
        let mut tokenizer = Tokenizer::new("a b ^ c");
        assert_eq!(tokenizer.next_token().unwrap().unwrap().0, ident("a"));
        assert_eq!(tokenizer.next_token().unwrap().unwrap().0, ident("b"));
        assert!(tokenizer.next_token().is_err());
        // The position is still reported so a caller can point at the failure.
        assert!(tokenizer.position() > 0);
    }

    // ---- round trips -------------------------------------------------------

    #[test]
    fn a_create_table_round_trips_to_the_expected_sequence() {
        let sql = "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT NOT NULL)";
        // `integer` and `text` are type *names*, not keywords: SQLite does not
        // list them, and a column may well be called one of them.
        assert_eq!(
            toks(sql),
            vec![
                kw("create"),
                kw("table"),
                ident("t"),
                punct(Punct::LParen),
                ident("id"),
                ident("integer"),
                kw("primary"),
                kw("key"),
                punct(Punct::Comma),
                ident("name"),
                ident("text"),
                kw("not"),
                kw("null"),
                punct(Punct::RParen),
            ]
        );
    }

    #[test]
    fn a_select_with_quotes_comments_and_parameters_round_trips() {
        // Written on one line per line, so the `--` comment ends at the
        // newline and `FROM` is a token rather than part of the comment.
        let sql = concat!(
            "SELECT [a], \"b\", `c` ",
            "-- pick a column\n",
            "FROM t WHERE x = ?1 AND y LIKE '%z%';"
        );
        let got = toks(sql);
        assert_eq!(
            got,
            vec![
                kw("select"),
                ident("a"),
                punct(Punct::Comma),
                dquoted("b"),
                punct(Punct::Comma),
                ident("c"),
                kw("from"),
                ident("t"),
                kw("where"),
                ident("x"),
                punct(Punct::Eq),
                Token::Parameter {
                    index: Some(1),
                    name: None
                },
                kw("and"),
                ident("y"),
                kw("like"),
                Token::String("%z%".into()),
                punct(Punct::Semicolon),
            ]
        );
    }

    // ---- robustness --------------------------------------------------------

    /// Inputs chosen to hit the awkward corners: open quotes, lone operators,
    /// digit runs, and bytes no rule covers.
    fn adversarial() -> Vec<String> {
        let mut v: Vec<String> = [
            "",
            " ",
            "'",
            "\"",
            "`",
            "[",
            "]",
            "x'",
            "x'0",
            "0x",
            "0x'",
            "1e",
            "1e+",
            ".",
            "..",
            "...",
            "$",
            ":",
            "@",
            "?",
            "??",
            "?0",
            "?99999999999999999999",
            "^",
            "!",
            "#",
            "\\",
            "/*",
            "*/",
            "--",
            "---",
            "->",
            "->>",
            "<<",
            ">>",
            "==",
            "<>",
            "1.",
            ".1",
            "0.0.0",
            "1__2",
            "0x_",
            "0x0x0",
            "1e1e1",
            "'\\",
            "''''",
            "\"\"\"",
            "[[[[",
            "]]]]",
            "```",
            "x''",
            "x'zz'",
            "1_",
            "_1",
            "$$$",
            "@@@",
            ":::",
            "e",
            "E",
            "0X",
            "0b1",
            "1a",
            "a1",
            "--\n",
            "/*/",
            "/* /* */",
            "\u{0}",
            "\u{1}",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        // And a few composites that mix the rules against each other.
        for a in ["'", "\"", "`", "[", "x'", "/*", "0x", "1e", "."] {
            for b in ["'", "0", "a", "]", " ", "\\", "-", "_"] {
                v.push(format!("{a}{b}"));
                v.push(format!("1{a}{b}2"));
            }
        }
        v
    }

    #[test]
    fn tokenizing_arbitrary_garbage_never_panics() {
        for src in adversarial() {
            // Either outcome is fine; not panicking is the whole assertion.
            let _ = Tokenizer::tokenize_all(&src);
        }
    }

    #[test]
    fn there_are_enough_adversarial_inputs_to_be_meaningful() {
        assert!(adversarial().len() >= 200);
    }

    #[test]
    fn every_prefix_of_a_nasty_string_never_panics() {
        let src = "SELECT x'ab' /* c -- d 'e' \"f\" `g` [h] 0x1f 1.5e-3 ?1 $n ^";
        for cut in 0..=src.len() {
            if src.is_char_boundary(cut) {
                let _ = Tokenizer::tokenize_all(&src[..cut]);
            }
        }
    }

    #[test]
    fn random_bytes_never_panic() {
        // A small xorshift keeps the test self-contained and deterministic.
        let mut state: u64 = 0x2545_f491_4f6c_dd1d;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..500 {
            let len = (next() % 24) as usize;
            let bytes: Vec<u8> = (0..len).map(|_| (next() % 128) as u8).collect();
            // Only well-formed UTF-8 reaches the tokenizer, since it takes a
            // `&str`; anything else cannot be constructed as input at all.
            if let Ok(src) = std::str::from_utf8(&bytes) {
                let _ = Tokenizer::tokenize_all(src);
            }
        }
    }

    #[test]
    fn an_oversized_parameter_index_is_an_error_rather_than_a_wrap() {
        // Too many digits for any index, so it cannot be represented.
        let src = format!("?{}", "9".repeat(30));
        assert!(Tokenizer::tokenize_all(&src).is_err());
    }
}

impl Keyword {
    /// Whether the keyword may stand in for an identifier.
    ///
    /// SQLite's parser accepts a keyword as a name wherever a name is
    /// grammatically required and defers the decision to name resolution: if
    /// the schema has a column by that name, it is a column, and otherwise the
    /// statement is an error. Rejecting keywords here would break every table
    /// with a column called `key` or one named `values`.
    pub fn as_identable(self) -> bool {
        use Keyword::*;
        !matches!(
            self,
            Select
                | From
                | Where
                | Group
                | Having
                | Order
                | Limit
                | Offset
                | Join
                | Inner
                | Left
                | Right
                | Full
                | Cross
                | Natural
                | On
                | Using
                | Union
                | Intersect
                | Except
                | Insert
                | Update
                | Delete
                | Create
                | Drop
                | Alter
                | Set
                | Values
                | Into
                | And
                | Or
                | Not
                | Is
                | In
                | Like
                | Glob
                | Match
                | Regexp
                | Between
                | Case
                | When
                | Then
                | Else
                | End
                | Distinct
                | All
                | As
                | With
                | Recursive
                | Primary
                | Key
                | Unique
                | Check
                | Default
                | References
                | Foreign
                | Constraint
                | Collate
                | Escape
                | Exists
                | Cast
                | Begin
                | Commit
                | Rollback
                | Savepoint
                | Release
                | To
                | Transaction
                | Explain
                | Pragma
                | Vacuum
                | Attach
                | Detach
                | Indexed
                | By
                | Temp
                | Temporary
                | If
        )
    }

    /// Whether the keyword begins a clause, so it cannot be a bare alias.
    pub fn starts_clause(self) -> bool {
        use Keyword::*;
        matches!(
            self,
            From | Where
                | Group
                | Having
                | Order
                | Limit
                | Offset
                | Join
                | Inner
                | Left
                | Right
                | Full
                | Cross
                | Natural
                | On
                | Using
                | Union
                | Intersect
                | Except
                | When
                | Then
                | Else
                | End
                | Set
                | Values
                | Into
                | And
                | Or
                | Not
                | Is
                | In
                | Like
                | Glob
                | Match
                | Regexp
                | Between
                | Collate
                | Window
        )
    }

    /// Whether the keyword names a type, so it belongs to a column definition
    /// rather than starting a constraint.
    ///
    /// This is always false, and deliberately so: SQLite does not reserve any
    /// type name. `TEXT`, `BLOB` and `INTEGER` are ordinary identifiers, and
    /// SQLite accepts any word as a declared type, so a column definition reads
    /// its type as identifiers rather than keywords. The method exists so the
    /// parser can ask the question without assuming a fixed set of type names.
    pub fn is_type_word(self) -> bool {
        false
    }
}
