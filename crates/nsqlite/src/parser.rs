//! A recursive-descent parser over the tokens the [`crate::tokenizer`] produces.
//!
//! The grammar is SQLite's, which differs from standard SQL in ways that matter
//! to a compatibility test suite:
//!
//! * A double-quoted string that does not name a column is a string literal.
//!   The parser cannot decide that, so it emits a `DoubleQuoted` node and name
//!   resolution settles it later.
//! * A bare identifier in an expression may be a column of the current table,
//!   and `expr AS alias` is legal wherever an expression is, which a strict
//!   grammar would reject.
//! * A keyword is a valid identifier when the schema says so, so every keyword
//!   is accepted where an identifier is expected.
//!
//! The parser is bounded: SQLite refuses expressions nested more than
//! `SQLITE_MAX_EXPR_DEPTH` deep, and this one refuses at the same depth rather
//! than overflowing the stack, because a recursive-descent parser at 1000
//! levels of nesting will exhaust a default-sized thread stack.

use crate::error::{Error, Result};
use crate::msg;
use crate::tokenizer::{Keyword, Punct, Span, Token, Tokenizer};
use crate::value::Value;

/// SQLite's own expression depth limit. Exceeding it is an error, not a crash.
pub const MAX_EXPR_DEPTH: usize = 100;

/// A binary operator, in the precedence order SQLite uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Or,
    And,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Is,
    IsNot,
    In,
    NotIn,
    Like,
    NotLike,
    Glob,
    NotGlob,
    Regexp,
    NotRegexp,
    BitwiseOr,
    BitwiseAnd,
    LeftShift,
    RightShift,
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Concat,
    JsonExtract,
}

/// The unary prefix operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    Negate,
    Plus,
    Not,
    BitwiseNot,
}

/// A literal value written in the statement.
#[derive(Debug, Clone, PartialEq)]
pub enum Literal {
    Null,
    Integer(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
    /// A bound parameter, by 1-based index.
    Parameter(usize),
}

/// An expression node.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Literal(Literal),
    /// A bare or quoted name, resolved to a column, a table, or a string
    /// depending on what the schema and the position allow.
    Column {
        table: Option<String>,
        name: String,
        span: Span,
    },
    /// A `?N` or named parameter that has not been assigned an index yet.
    NamedParameter(String, Span),
    Unary {
        op: UnaryOp,
        expr: Box<Expr>,
    },
    Binary {
        op: BinOp,
        left: Box<Expr>,
        right: Box<Expr>,
    },
    /// `x ISNULL`, `x NOTNULL`, and `x NOT NULL`.
    IsNull {
        expr: Box<Expr>,
        negated: bool,
    },
    Between {
        expr: Box<Expr>,
        low: Box<Expr>,
        high: Box<Expr>,
        negated: bool,
    },
    InList {
        expr: Box<Expr>,
        list: Vec<Expr>,
        negated: bool,
    },
    InSelect {
        expr: Box<Expr>,
        select: Box<Select>,
        negated: bool,
    },
    Like {
        expr: Box<Expr>,
        pattern: Box<Expr>,
        escape: Option<Box<Expr>>,
        negated: bool,
    },
    Function {
        name: String,
        args: Vec<Expr>,
        /// `COUNT(*)` and friends carry a star rather than an argument list.
        star: bool,
        distinct: bool,
    },
    Case {
        operand: Option<Box<Expr>>,
        whens: Vec<(Expr, Expr)>,
        otherwise: Option<Box<Expr>>,
    },
    Cast {
        expr: Box<Expr>,
        ty: String,
    },
    Exists {
        select: Box<Select>,
        negated: bool,
    },
    Subquery {
        select: Box<Select>,
    },
    Collate {
        expr: Box<Expr>,
        collation: String,
    },
}

/// One result column.
#[derive(Debug, Clone, PartialEq)]
pub struct ResultColumn {
    pub expr: Expr,
    /// The output name, which is the column name, or `expr` text, or the
    /// explicit alias.
    pub alias: Option<String>,
    pub span: Span,
    /// The expression's own source text, which is what SQLite uses to name a
    /// column that has no alias. It is the text as written, not a
    /// reconstruction: `1  +  2` is named `1  +  2` and `(1+2)*3` keeps its
    /// parentheses, and a reconstruction would print `1+2` and `((1+2)*3)`.
    pub source: String,
}

/// The join operator of a table reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinKind {
    Inner,
    Left,
    Right,
    Full,
    Cross,
}

/// A table in a FROM clause, with the constraint attached to it.
#[derive(Debug, Clone, PartialEq)]
pub struct TableRef {
    pub name: String,
    pub alias: Option<String>,
    /// How this table joins to everything to its left, or `None` when a comma
    /// introduced it and no operator was written. A comma is not a cross join on
    /// its own: a constraint may still follow, and a constraint makes the join an
    /// inner one whatever the operator said, so "no operator" has to stay
    /// distinguishable from `Cross`.
    pub join: Option<JoinKind>,
    /// The ON constraint, if the join had one. A USING clause is not an ON
    /// expression: it is a column list the resolver turns into equality tests,
    /// and it changes what `*` expands to.
    pub on: Option<Expr>,
    /// The columns a USING clause named, which must be equal across the join
    /// and appear once in the output of a star.
    pub using: Vec<String>,
    /// Whether the join was written NATURAL, in which case the columns that must
    /// be equal are the two tables' shared ones rather than a written list.
    /// The parser cannot work that out — it does not read the catalog — so the
    /// executor derives the list from the tables it resolves.
    pub natural: bool,
    pub indexed_by: Option<String>,
}

/// A compound select: `SELECT ... UNION SELECT ...`, and its INTERSECT and
/// EXCEPT siblings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompoundOp {
    Union,
    UnionAll,
    Intersect,
    Except,
}

/// A join order, or a parenthesised subquery standing in for one.
#[derive(Debug, Clone, PartialEq)]
pub enum FromItem {
    Table(TableRef),
    Subquery {
        select: Box<Select>,
        alias: Option<String>,
    },
}

/// A SELECT statement, possibly compound.
#[derive(Debug, Clone, PartialEq)]
pub struct Select {
    pub with: Vec<Cte>,
    pub body: SelectBody,
    pub order_by: Vec<(Expr, bool)>,
    pub limit: Option<Expr>,
    pub offset: Option<Expr>,
}

/// A common table expression from a WITH clause.
#[derive(Debug, Clone, PartialEq)]
pub struct Cte {
    pub name: String,
    pub columns: Vec<String>,
    pub select: Select,
}

/// The parts of a SELECT, before any compound operator is applied.
#[derive(Debug, Clone, PartialEq)]
pub enum SelectBody {
    Simple {
        distinct: bool,
        columns: Vec<ResultColumn>,
        from: Vec<FromItem>,
        /// An alias for the whole result set, which a subquery can be given.
        where_: Option<Expr>,
        group_by: Vec<Expr>,
        having: Option<Expr>,
        /// The bare column list of `VALUES`.
        values: Option<Vec<Vec<Expr>>>,
    },
    Compound {
        left: Box<SelectBody>,
        op: CompoundOp,
        right: Box<SelectBody>,
    },
    /// A parenthesised select, which may itself be compound. A subquery in a
    /// FROM clause is one of these.
    Nested(Box<Select>),
}

/// A column definition inside CREATE TABLE.
#[derive(Debug, Clone, PartialEq)]
pub struct ColumnDef {
    pub name: String,
    pub ty: String,
    pub constraints: Vec<Constraint>,
}

/// A table or column constraint.
#[derive(Debug, Clone, PartialEq)]
pub enum Constraint {
    PrimaryKey {
        ascending: bool,
        autoincrement: bool,
    },
    NotNull,
    Unique,
    Default(Expr),
    Check(Expr),
    ForeignKey {
        table: String,
        columns: Vec<String>,
    },
    Collate(String),
}

/// A statement the engine can execute.
#[derive(Debug, Clone, PartialEq)]
pub enum Stmt {
    Select(Select),
    /// A PRAGMA statement, parsed by the pragma module because its shape does
    /// not fit the expression grammar: a name, optionally qualified, then
    /// either nothing, a parenthesised argument, or an assignment.
    Pragma(crate::pragma::Pragma),
    /// A statement the parser recognises but cannot yet execute, kept so the
    /// caller can report progress rather than a syntax error.
    Unsupported(String),
    CreateTable {
        name: String,
        if_not_exists: bool,
        columns: Vec<ColumnDef>,
        constraints: Vec<Constraint>,
        without_rowid: bool,
        strict: bool,
        temp: bool,
        /// The statement's own source text, which is what sqlite_schema stores.
        /// A reopened connection reads the table's definition back from it, so
        /// the text has to be the original rather than a reconstruction.
        sql: String,
    },
    DropTable {
        name: String,
        if_exists: bool,
    },
    CreateIndex {
        name: Option<String>,
        table: String,
        columns: Vec<(String, bool)>,
        unique: bool,
        if_not_exists: bool,
        /// The statement's own text, which sqlite_schema stores. A reopened
        /// connection rebuilds the index from it, so it has to be what was
        /// written rather than a reconstruction.
        sql: String,
    },
    Insert {
        table: String,
        columns: Option<Vec<String>>,
        source: InsertSource,
    },
    Update {
        table: String,
        sets: Vec<(String, Expr)>,
        where_: Option<Expr>,
    },
    Delete {
        table: String,
        where_: Option<Expr>,
    },
    Begin,
    Commit,
    Rollback,
    /// `ANALYZE`, which collects planner statistics into `sqlite_stat1`.
    ///
    /// It is a statement of its own rather than a member of `Unsupported`
    /// because it has to *succeed*. The suite uses it to finish a setup block,
    /// so a syntax error there abandons the whole block and every table it was
    /// going to create stays missing -- one missing statement turns a hundred
    /// tests into "no such table". This engine has no statistics to collect
    /// and no query planner that consults them, so running it changes nothing
    /// that a query can observe.
    Analyze,
    /// `EXPLAIN <stmt>` and `EXPLAIN QUERY PLAN <stmt>`.
    ///
    /// The two are one keyword and two different statements: the first is the
    /// opcode listing of a virtual machine this engine does not have, the
    /// second is an access plan it does. The plan for what is wrapped is
    /// [`explain::Explain`], which holds the parsed inner statement rather
    /// than its text, so a syntax error inside it surfaces the way the bare
    /// statement's would.
    Explain(Box<crate::explain::Explain>),
}

/// Where an INSERT takes its rows from.
#[derive(Debug, Clone, PartialEq)]
pub enum InsertSource {
    Values(Vec<Vec<Expr>>),
    Select(Box<Select>),
}

/// Parses a whole script, requiring every statement to consume its terminator.
pub fn parse_script(sql: &str) -> Result<Vec<Stmt>> {
    let mut p = Parser::new(sql)?;
    let mut out = Vec::new();
    while p.peek_token()?.is_some() {
        let before = p.pos;
        out.push(p.statement()?);
        if p.pos == before {
            // A statement that consumed nothing would spin the loop for ever.
            return Err(msg::syntax_error(&p.text_at(p.span())));
        }
        // A stray semicolon between statements is fine; a missing one is not.
        while p.eat_punct(Punct::Semicolon)? {}
    }
    Ok(out)
}

/// Parses a single statement, which may have a trailing semicolon.
pub fn parse_one(sql: &str) -> Result<Stmt> {
    let mut stmts = parse_script(sql)?;
    if stmts.len() == 1 {
        Ok(stmts.remove(0))
    } else if stmts.is_empty() {
        Err(Error::new(
            crate::error::ResultCode::Error,
            "no statement found",
        ))
    } else {
        Err(Error::new(
            crate::error::ResultCode::Error,
            "more than one statement in the input",
        ))
    }
}

thread_local! {
    static TRACE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static ADVANCE_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static TYPE_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static EXPR_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static PRIMARY_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Restores the parser's expression depth when it goes out of scope.
struct DepthGuard<'a> {
    depth: &'a mut usize,
}

impl Drop for DepthGuard<'_> {
    fn drop(&mut self) {
        *self.depth -= 1;
    }
}

/// A token with its position, plus the parser state that walks them.
struct Parser<'a> {
    tokens: Vec<(Token, Span)>,
    pos: usize,
    depth: usize,
    sql: &'a str,
    _marker: std::marker::PhantomData<&'a ()>,
}

impl<'a> Parser<'a> {
    fn new(sql: &'a str) -> Result<Parser<'a>> {
        let tokens = Tokenizer::tokenize_all(sql)?;
        Ok(Parser {
            tokens,
            pos: 0,
            depth: 0,
            sql,
            _marker: std::marker::PhantomData,
        })
    }

    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos).map(|(t, _)| t)
    }

    fn peek_token(&self) -> Result<Option<&Token>> {
        Ok(self.peek())
    }

    fn span(&self) -> Span {
        self.tokens
            .get(self.pos)
            .map(|(_, s)| *s)
            .or_else(|| self.tokens.last().map(|(_, s)| *s))
            .unwrap_or(Span {
                start: 0,
                end: 0,
                line: 1,
                col: 1,
            })
    }

    fn advance(&mut self) -> Option<Token> {
        let t = self.tokens.get(self.pos).map(|(t, _)| t.clone());
        if t.is_some() {
            self.pos += 1;
        }
        t
    }

    fn at_keyword(&self, kw: Keyword) -> bool {
        matches!(self.peek(), Some(Token::Keyword(k)) if *k == kw)
    }

    fn eat_keyword(&mut self, kw: Keyword) -> Result<bool> {
        if self.at_keyword(kw) {
            self.advance();
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn expect_keyword(&mut self, kw: Keyword, context: &str) -> Result<()> {
        if self.eat_keyword(kw)? {
            Ok(())
        } else {
            Err(self.unexpected(context))
        }
    }

    fn at_punct(&self, p: Punct) -> bool {
        matches!(self.peek(), Some(Token::Punct(q)) if *q == p)
    }

    fn eat_punct(&mut self, p: Punct) -> Result<bool> {
        if self.at_punct(p) {
            self.advance();
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn expect_punct(&mut self, p: Punct, context: &str) -> Result<()> {
        if self.eat_punct(p)? {
            Ok(())
        } else {
            Err(self.unexpected(context))
        }
    }

    /// SQLite's wording for a token it cannot use here.
    fn unexpected(&self, context: &str) -> Error {
        let found = self.describe_token_here();
        Error::new(
            crate::error::ResultCode::Error,
            format!("near \"{found}\": syntax error while {context}"),
        )
    }

    /// What sqlite3 would name as the token it choked on at the current
    /// position.
    ///
    /// This is the same shape [`describe_token`] gives for a token already in
    /// hand, and it exists so both sites take the spelling the *statement*
    /// wrote rather than the token's folded form. `describe_token` on its own
    /// cannot: it is given a `Token`, and by the time a name is a `Token` the
    /// tokenizer has already folded it -- so `SELECT FROM t` came out
    /// `near "from"` where sqlite3 says `near "FROM"`. `unexpected` is only
    /// this engine's own "while <clause>" wording rather than sqlite3's bare
    /// `near "X": syntax error`, so it stays inline; the catalogue gets the
    /// sites whose text does match.
    fn describe_token_here(&self) -> String {
        match self.peek() {
            Some(Token::Identifier(_)) | Some(Token::Keyword(_)) => {
                self.written_name(self.pos).unwrap_or_else(|| "end of input".to_string())
            }
            other => other.map_or_else(|| "end of input".to_string(), describe_token),
        }
    }

    /// Reads an identifier, accepting any keyword, which SQLite allows wherever
    /// the schema permits a name.
    /// Reads a name that may be written as a quoted string.
    ///
    /// `AS 'f1'` is how the suite spells an alias whose text is a keyword, and
    /// rejecting it as a string literal in that position would refuse a
    /// statement sqlite3 accepts.
    fn quoted_name(&mut self, context: &str) -> Result<String> {
        match self.peek() {
            Some(Token::String(s)) => {
                let s = s.clone();
                self.advance();
                Ok(s)
            }
            _ => self.name(context),
        }
    }

    /// The offset where the statement at the cursor ends.
    ///
    /// That is the next semicolon that is not inside parentheses, or the end of
    /// the input, since a script is tokenised up front and the pragma module
    /// needs a slice rather than a position.
    fn statement_end(&self) -> usize {
        let mut depth = 0i32;
        for (tok, span) in self.tokens.iter().skip(self.pos) {
            match tok {
                Token::Punct(Punct::LParen) => depth += 1,
                Token::Punct(Punct::RParen) => depth -= 1,
                Token::Punct(Punct::Semicolon) if depth <= 0 => return span.start,
                _ => {}
            }
        }
        self.sql.len()
    }

    fn identifier(&mut self, context: &str) -> Result<String> {
        match self.advance() {
            Some(Token::Identifier(n)) => Ok(n),
            Some(Token::Keyword(k)) => Ok(k.as_str().to_string()),
            _ => {
                self.pos = self.pos.saturating_sub(1);
                Err(self.unexpected(context))
            }
        }
    }

    /// Reads a name in a position where the grammar wants an identifier.
    ///
    /// Any keyword is accepted, because SQLite lets a column be called `key` or
    /// a table be called `values`; whether the name resolves is decided later,
    /// against the schema. Only a token that cannot be a name at all is an
    /// error here.
    fn name(&mut self, context: &str) -> Result<String> {
        let start = self.pos;
        match self.advance() {
            Some(Token::Identifier(n)) => Ok(self.written_name(start).unwrap_or(n)),
            Some(Token::Keyword(k)) => Ok(self
                .written_name(start)
                .unwrap_or_else(|| k.as_str().to_string())),
            _ => {
                self.pos = self.pos.saturating_sub(1);
                Err(self.unexpected(context))
            }
        }
    }

    /// The spelling of the token at `pos` as the statement wrote it, with any
    /// quoting removed, or `None` when the span is not usable text.
    ///
    /// A name is a *name* whichever case it was written in, so the token alone
    /// cannot say which spelling a message should echo: the tokenizer folds
    /// every unquoted identifier to lower case, and the schema keys off that
    /// fold, so a `Table` keyed by the spelling would go unfindable. The
    /// folding is what lookup wants and is the wrong answer for a message.
    ///
    /// sqlite3 resolves the two the same way this does -- by comparing the
    /// folded forms, and by echoing the source text when it has to name
    /// something. `SELECT * FROM Foo` is `no such table: Foo` even though the
    /// token the parser built is `foo`; `SELECT * FROM FOO` is
    /// `no such table: FOO`. A quoted name is already spelled as written, and
    /// its span covers the quotes, so the quotes are stripped here:
    /// `SELECT * FROM "Foo"` is also `no such table: Foo`.
    ///
    /// This is the same rule [`Parser::after_identifier`] already had to
    /// follow for a function call, lifted to every name the grammar reads.
    /// Getting the spelling at the point the name is read is what makes the
    /// message catalogue's case rule -- query-sourced names keep the case the
    /// statement wrote -- true of the whole engine rather than of one family.
    pub fn written_name(&self, pos: usize) -> Option<String> {
        let (_, span) = self.tokens.get(pos)?;
        self.sql.get(span.start..span.end).map(|w| {
            let w = w.trim();
            w.strip_prefix(['"', '`'])
                .and_then(|w| w.strip_suffix(['"', '`']))
                .or_else(|| w.strip_prefix('[').and_then(|w| w.strip_suffix(']')))
                .unwrap_or(w)
                .to_string()
        })
    }

    /// Consumes an identifier whose text is exactly `word`, case-insensitively.
    ///
    /// SQLite does not reserve every word its grammar uses, so options like
    /// STRICT and the second half of WITHOUT ROWID arrive as plain identifiers.
    fn eat_word(&mut self, word: &str) -> Result<bool> {
        let matches_word = match self.peek() {
            Some(Token::Identifier(n)) => n.eq_ignore_ascii_case(word),
            Some(Token::Keyword(k)) => k.as_str().eq_ignore_ascii_case(word),
            _ => false,
        };
        if matches_word {
            self.advance();
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Accounts for one more level of nesting, refusing past SQLite's limit.
    ///
    /// The caller pairs this with [`Parser::leave`] on every return path. A
    /// borrow-based guard would be tidier, but it cannot coexist with the
    /// further `&mut self` calls these functions make.
    fn enter(&mut self) -> Result<()> {
        self.depth += 1;
        if self.depth > MAX_EXPR_DEPTH {
            return Err(Error::new(
                crate::error::ResultCode::Error,
                format!("expression tree is too large (maximum depth {MAX_EXPR_DEPTH})"),
            ));
        }
        Ok(())
    }

    fn leave(&mut self) {
        self.depth -= 1;
    }

    /// Parses one statement.
    fn statement(&mut self) -> Result<Stmt> {
        match self.peek() {
            Some(Token::Keyword(Keyword::Explain)) => self.explain(),
            Some(Token::Keyword(Keyword::Select)) | Some(Token::Keyword(Keyword::With)) => {
                Ok(Stmt::Select(self.select()?))
            }
            Some(Token::Keyword(Keyword::Pragma)) => {
                // The pragma module owns this grammar, which does not fit the
                // expression one: a name, optionally schema-qualified, then
                // either nothing, a parenthesised argument, or an assignment.
                // It wants the statement text starting AT the keyword, not
                // after it, because it parses the whole shape itself.
                // match self.peek() did not advance, so self.pos is still
                // pointing AT the keyword. Its span is where the text starts.
                let from = self.tokens.get(self.pos).map(|(_, s)| s.start).unwrap_or(0);
                // The pragma module re-parses the text, so it has to be this
                // statement's text and not the whole script: a script is
                // tokenised up front, and handing over the rest of it would
                // make the pragma swallow the statements that follow.
                let to = self.statement_end();
                let text = self.sql.get(from..to).unwrap_or("").to_string();
                let stmt = crate::pragma::parse_pragma(&text)?;
                // The tokens for this statement still have to be consumed, or
                // the script loop would see the same PRAGMA for ever.
                while self.pos < self.tokens.len() && self.tokens[self.pos].1.start < to {
                    self.pos += 1;
                }
                return Ok(Stmt::Pragma(stmt));
            }
            Some(Token::Keyword(Keyword::Pragma)) => {
                // The pragma module owns this grammar, which does not fit the
                // expression one. It re-parses the text, so it gets this
                // statement's slice rather than the whole script: a script is
                // tokenised up front and handing over the rest would make the
                // pragma swallow the statements that follow.
                let from = self.tokens.get(self.pos).map(|(_, s)| s.start).unwrap_or(0);
                let to = self.statement_end();
                let text = self.sql.get(from..to).unwrap_or("").to_string();
                let stmt = crate::pragma::parse_pragma(&text)?;
                // The tokens still have to be consumed, or the script loop would
                // see the same PRAGMA for ever.
                while self.pos < self.tokens.len() && self.tokens[self.pos].1.start < to {
                    self.pos += 1;
                }
                return Ok(Stmt::Pragma(stmt));
            }
            Some(Token::Keyword(Keyword::Insert)) | Some(Token::Keyword(Keyword::Replace)) => {
                self.insert()
            }
            Some(Token::Keyword(Keyword::Update)) => self.update(),
            Some(Token::Keyword(Keyword::Delete)) => self.delete(),
            Some(Token::Keyword(Keyword::Create)) => self.create(),
            Some(Token::Keyword(Keyword::Drop)) => self.drop(),
            Some(Token::Keyword(Keyword::Begin)) => {
                self.advance();
                self.skip_to_semicolon()?;
                Ok(Stmt::Begin)
            }
            Some(Token::Keyword(Keyword::Commit)) => {
                self.advance();
                self.skip_to_semicolon()?;
                Ok(Stmt::Commit)
            }
            Some(Token::Keyword(Keyword::Rollback)) => {
                self.advance();
                self.skip_to_semicolon()?;
                Ok(Stmt::Rollback)
            }
            Some(Token::Keyword(Keyword::Analyze)) => {
                // ANALYZE optionally names a table or an index to look at, and
                // optionally an "index-list" argument after that. All of it is
                // consumed and discarded: this engine collects no statistics.
                self.advance();
                while self.peek_token()?.is_some() && !self.eat_punct(Punct::Semicolon)? {
                    self.advance();
                }
                Ok(Stmt::Analyze)
            }
            Some(Token::Keyword(k)) => {
                // The keyword is consumed here; leaving it in place would leave
                // the script loop looking at the same token for ever.
                Ok(Stmt::Unsupported(k.as_str().to_string()))
            }
            _ => {
                let span = self.span();
                self.advance();
                Err(msg::syntax_error(&self.text_at(span)))
            }
        }
    }

    /// `EXPLAIN <stmt>`, and the `EXPLAIN QUERY PLAN <stmt>` spelling.
    ///
    /// The explain module owns this grammar, for the same reason the pragma
    /// module owns its own: the two words are optional and in a fixed order,
    /// and the thing that follows is a whole statement rather than a piece of
    /// this expression grammar. It re-parses the text, so it gets this
    /// statement's slice and not the whole script, exactly as the pragma arm
    /// does.
    ///
    /// The slice runs from just past the keyword to the statement's own
    /// terminator, terminator included, and the module is written to take the
    /// text from *after* `EXPLAIN` -- that is what lets it look for a leading
    /// `QUERY PLAN`. Including the semicolon is not a detail: it is a token,
    /// and a lone `;` is `near ";": syntax error`, which is where the real
    /// engine stops on `EXPLAIN;`.
    fn explain(&mut self) -> Result<Stmt> {
        let from = self.span().end;
        self.advance();
        let to = self.statement_end();
        let text = self.sql.get(from..to).unwrap_or("").to_string();
        let stmt = crate::explain::parse(&text)?;
        // The tokens for this statement still have to be consumed, or the
        // script loop would see the same EXPLAIN for ever. `<=` and not `<`:
        // the slice ends *at* the semicolon, so the terminator is one of the
        // tokens that has to go.
        while self.pos < self.tokens.len() && self.tokens[self.pos].1.start <= to {
            self.pos += 1;
        }
        Ok(Stmt::Explain(Box::new(stmt)))
    }

    /// The source text a span covers, for error messages.
    fn text_at(&self, span: Span) -> String {
        self.sql.get(span.start..span.end).unwrap_or("").to_string()
    }

    /// Consumes tokens up to and including the next semicolon, for statements
    /// that are recognised but not yet implemented.
    fn skip_to_semicolon(&mut self) -> Result<()> {
        let mut depth = 0i32;
        loop {
            match self.advance() {
                None => return Ok(()),
                Some(Token::Punct(Punct::LParen)) => depth += 1,
                Some(Token::Punct(Punct::RParen)) => depth -= 1,
                Some(Token::Punct(Punct::Semicolon)) if depth <= 0 => return Ok(()),
                _ => {}
            }
        }
    }

    // --- SELECT ---------------------------------------------------------

    fn select(&mut self) -> Result<Select> {
        let with = self.with_clause()?;
        let mut body = self.select_body()?;
        // Compound operators bind left to right and are all at one level.
        loop {
            let op = if self.at_keyword(Keyword::Union) {
                self.advance();
                if self.eat_keyword(Keyword::All)? {
                    CompoundOp::UnionAll
                } else {
                    CompoundOp::Union
                }
            } else if self.at_keyword(Keyword::Intersect) {
                self.advance();
                CompoundOp::Intersect
            } else if self.at_keyword(Keyword::Except) {
                self.advance();
                CompoundOp::Except
            } else {
                break;
            };
            let right = self.select_body()?;
            body = SelectBody::Compound {
                left: Box::new(body),
                op,
                right: Box::new(right),
            };
        }
        let order_by = self.order_by_clause()?;
        let (limit, offset) = self.limit_clause()?;
        Ok(Select {
            with,
            body,
            order_by,
            limit,
            offset,
        })
    }

    fn with_clause(&mut self) -> Result<Vec<Cte>> {
        let mut out = Vec::new();
        if !self.eat_keyword(Keyword::With)? {
            return Ok(out);
        }
        // RECURSIVE and MATERIALIZED modifiers are accepted and ignored; they
        // change planning, not results.
        self.eat_keyword(Keyword::Recursive)?;
        loop {
            let name = self.name("parsing a CTE name")?;
            let mut columns = Vec::new();
            if self.eat_punct(Punct::LParen)? {
                loop {
                    columns.push(self.name("parsing a CTE column name")?);
                    if !self.eat_punct(Punct::Comma)? {
                        break;
                    }
                }
                self.expect_punct(Punct::RParen, "closing a CTE column list")?;
            }
            self.expect_keyword(Keyword::As, "after a CTE name")?;
            // MATERIALIZED / NOT MATERIALIZED.
            if self.eat_keyword(Keyword::Materialized)? {
                self.eat_keyword(Keyword::Not)?;
            }
            self.expect_punct(Punct::LParen, "opening a CTE body")?;
            let select = self.select()?;
            self.expect_punct(Punct::RParen, "closing a CTE body")?;
            out.push(Cte {
                name,
                columns,
                select,
            });
            if !self.eat_punct(Punct::Comma)? {
                break;
            }
        }
        Ok(out)
    }

    fn select_body(&mut self) -> Result<SelectBody> {
        if self.at_punct(Punct::LParen) {
            self.advance();
            let inner = self.select()?;
            self.expect_punct(Punct::RParen, "closing a parenthesised select")?;
            return Ok(SelectBody::Nested(Box::new(inner)));
        }
        self.expect_keyword(Keyword::Select, "starting a select")?;
        let distinct = if self.eat_keyword(Keyword::Distinct)? {
            true
        } else {
            self.eat_keyword(Keyword::All)?;
            false
        };
        let columns = self.result_columns()?;
        let from = self.from_clause()?;
        let where_ = if self.eat_keyword(Keyword::Where)? {
            Some(self.expr()?)
        } else {
            None
        };
        let group_by = self.group_by_clause()?;
        let having = if self.eat_keyword(Keyword::Having)? {
            Some(self.expr()?)
        } else {
            None
        };
        Ok(SelectBody::Simple {
            distinct,
            columns,
            from,
            where_,
            group_by,
            having,
            values: None,
        })
    }

    fn result_columns(&mut self) -> Result<Vec<ResultColumn>> {
        let mut out = Vec::new();
        // A bare `*` expands to every column, which resolution handles.
        if self.eat_punct(Punct::Star)? {
            out.push(ResultColumn {
                expr: Expr::Function {
                    name: "*".into(),
                    args: vec![],
                    star: true,
                    distinct: false,
                },
                alias: None,
                span: self.span(),
                source: "*".to_string(),
            });
            return Ok(out);
        }
        loop {
            let span = self.span();
            let expr_start = span.start;
            let expr = self.expr()?;
            // The text of the expression is what names the column when there is
            // no alias, and it ends where the expression did, before any alias.
            let expr_end = self
                .tokens
                .get(self.pos.saturating_sub(1))
                .map(|(_, s)| s.end)
                .unwrap_or(expr_start);
            let source = self
                .sql
                .get(expr_start..expr_end)
                .unwrap_or_default()
                .trim()
                .to_string();
            // An alias may be written with AS or bare, and a bare alias may be
            // quoted with any of the four forms: `AS 'f1'` is a name, not a
            // syntax error, and the suite uses it.
            let alias = if self.eat_keyword(Keyword::As)? {
                Some(self.quoted_name("after AS")?)
            } else if matches!(self.peek(), Some(Token::Identifier(_)))
                || matches!(self.peek(), Some(Token::Keyword(k)) if k.as_identable())
            {
                Some(self.quoted_name("after a result column")?)
            } else {
                None
            };
            out.push(ResultColumn {
                expr,
                alias,
                span,
                source,
            });
            if !self.eat_punct(Punct::Comma)? {
                break;
            }
            if self.at_keyword(Keyword::From) {
                break;
            }
        }
        Ok(out)
    }

    /// Parses a FROM clause into the list of table references it holds.
    ///
    /// A comma is the lowest-precedence join operator, so the clause is read as
    /// a flat list. Each item after the first carries the operator that attached
    /// it to what came before, which is what the executor walks: it takes the
    /// first item as the left side and folds the rest in from the left.
    fn from_clause(&mut self) -> Result<Vec<FromItem>> {
        let mut out = Vec::new();
        if !self.eat_keyword(Keyword::From)? {
            return Ok(out);
        }
        out.push(self.from_item()?);
        loop {
            // A comma and a join operator are the two ways another table can
            // follow. The operator comes first, then the table it attaches, then
            // the constraint: `a LEFT JOIN b ON ...`.
            //
            // A comma records no operator of its own, and a constraint may follow
            // it anyway: `a, b ON a.x=b.x` is legal, and SQLite executes it as an
            // inner join. So the operator is only recorded when one was written
            // and the executor decides the rest from whether a constraint is
            // present, which is what makes a bare `a JOIN b` a cross join and a
            // constrained `a CROSS JOIN b ON ...` an inner one. A missing operator
            // with a table still ahead means a comma was consumed and the next
            // table simply cross joins.
            let (kind, natural) = if self.eat_punct(Punct::Comma)? {
                (None, false)
            } else {
                match self.join_operator()? {
                    Some(op) => op,
                    // Neither a comma nor a join operator: the clause is over.
                    None => break,
                }
            };
            let mut next = self.from_item()?;
            // A NATURAL join's constraint is the columns the two tables share,
            // which only the catalog knows, so the parser records that the join
            // was NATURAL and the executor derives the list. An ON or a USING
            // alongside NATURAL is a mistake SQLite names outright, and it can
            // only be seen once the two words have both been read.
            if natural && self.at_join_constraint() {
                return Err(Error::new(
                    crate::error::ResultCode::Error,
                    "a NATURAL join may not have an ON or USING clause",
                ));
            }
            let (on, using) = self.join_constraint()?;
            if natural {
                set_natural(&mut next, kind)?;
            } else {
                set_join(&mut next, kind, on, using)?;
            }
            out.push(next);
        }
        Ok(out)
    }

    /// Whether an ON or a USING clause follows, for the NATURAL check.
    ///
    /// Only the two keywords matter, not the clause itself, so this does not
    /// consume anything.
    fn at_join_constraint(&self) -> bool {
        self.at_keyword(Keyword::On) || self.at_keyword(Keyword::Using)
    }

    /// The join operator at the cursor, or `None` if the FROM clause ends here.
    ///
    /// `INNER`, `LEFT` and `CROSS` are all written as an optional modifier in
    /// front of `JOIN`, and a bare `JOIN` is an inner join. `OUTER` may follow
    /// `LEFT` or `INNER` and changes nothing. `NATURAL` is a modifier in front of
    /// any of them, and the pair is returned so the caller knows to derive the
    /// constraint from the columns the two tables share.
    ///
    /// The second element says whether `NATURAL` was written, because it decides
    /// what a following ON or USING means: after `NATURAL` either is an error,
    /// and without it either is the join's constraint.
    fn join_operator(&mut self) -> Result<Option<(Option<JoinKind>, bool)>> {
        // A join operator is only one if a `JOIN` follows it. A bare `LEFT` with
        // nothing after it is a table named `left`, which SQLite allows, so
        // nothing is consumed until the `JOIN` is confirmed. The same holds for
        // `NATURAL`, which is also a usable table name.
        let save = self.pos;
        // `NATURAL` may stand alone in front of `JOIN` or in front of a type, so
        // it is eaten here and the type loop below runs either way. What decides
        // whether this is an operator at all is the `JOIN` that has to follow
        // both: `a natural` is a table called natural, not half a join.
        //
        // The type is spelled out as text rather than collapsed into a kind
        // first, because SQLite validates the combination of the two words and
        // names the invalid one: `INNER OUTER` is an error, `LEFT OUTER` is not.
        let mut spelled = String::new();
        let mut kind = JoinKind::Inner;
        // Whether the type written so far is one OUTER may follow. It only
        // matters once an OUTER is actually seen; a bare `INNER JOIN` is legal.
        let mut type_allows_outer = false;
        let mut have_type = false;
        // `NATURAL LEFT JOIN` and `LEFT NATURAL JOIN` are the same operator, so
        // it is read here and again after the type has been taken.
        let natural = self.eat_keyword(Keyword::Natural)?;
        for (kw, k, allows_outer) in [
            (Keyword::Left, JoinKind::Left, true),
            (Keyword::Right, JoinKind::Right, true),
            (Keyword::Full, JoinKind::Full, true),
            (Keyword::Inner, JoinKind::Inner, false),
            (Keyword::Cross, JoinKind::Cross, false),
        ] {
            if self.eat_keyword(kw)? {
                spelled.push_str(kw.as_str().to_ascii_uppercase().as_str());
                kind = k;
                type_allows_outer = allows_outer;
                have_type = true;
                break;
            }
        }
        let natural = natural || self.eat_keyword(Keyword::Natural)?;
        // `OUTER` is only valid after a type that is already an outer join. With
        // no type in front of it, or after a type that is not, the two words
        // together are not a join SQLite knows.
        let mut bad_outer = false;
        if self.at_keyword(Keyword::Outer) {
            if have_type {
                spelled.push(' ');
            }
            spelled.push_str("OUTER");
            self.advance();
            bad_outer = !(have_type && type_allows_outer);
        }
        if !self.at_keyword(Keyword::Join) {
            self.pos = save;
            return Ok(None);
        }
        self.advance();
        if bad_outer {
            return Err(Error::new(
                crate::error::ResultCode::Error,
                format!("unknown join type: {spelled}"),
            ));
        }
        Ok(Some((Some(kind), natural)))
    }

    /// The ON or USING constraint that closes a join, if it has one.
    ///
    /// A constraint is optional after a comma or a bare `JOIN`, and required
    /// after nothing else, so this reports its absence and lets the executor
    /// decide what an unconstrained join means.
    fn join_constraint(&mut self) -> Result<(Option<Expr>, Vec<String>)> {
        if self.eat_keyword(Keyword::On)? {
            return Ok((Some(self.expr()?), Vec::new()));
        }
        if self.eat_keyword(Keyword::Using)? {
            self.expect_punct(Punct::LParen, "after USING")?;
            let mut cols = Vec::new();
            loop {
                cols.push(self.name("in a USING clause")?);
                if !self.eat_punct(Punct::Comma)? {
                    break;
                }
            }
            self.expect_punct(Punct::RParen, "closing a USING clause")?;
            return Ok((None, cols));
        }
        Ok((None, Vec::new()))
    }

    /// Parses one table reference. The join operator and constraint belong to
    /// the item that follows them, so they are set by [`set_join`] afterwards.
    fn from_item(&mut self) -> Result<FromItem> {
        if self.at_punct(Punct::LParen) {
            self.advance();
            let select = self.select()?;
            self.expect_punct(Punct::RParen, "closing a subquery")?;
            let alias = self.optional_alias()?;
            return Ok(FromItem::Subquery {
                select: Box::new(select),
                alias,
            });
        }
        let name = self.name("after FROM")?;
        // A qualified name is written schema.table, which the engine treats as
        // the table part with the schema ignored for now.
        let mut full = name.clone();
        while self.eat_punct(Punct::Dot)? {
            full.push('.');
            full.push_str(&self.name("after a table qualifier")?);
        }
        let alias = self.optional_alias()?;
        let indexed_by = if self.eat_keyword(Keyword::Indexed)? {
            self.eat_keyword(Keyword::By)?;
            Some(self.name("after INDEXED BY")?)
        } else if self.eat_keyword(Keyword::Not)? {
            self.eat_keyword(Keyword::Indexed)?;
            self.eat_keyword(Keyword::By)?;
            None
        } else {
            None
        };
        Ok(FromItem::Table(TableRef {
            name: full,
            alias,
            join: None,
            on: None,
            using: Vec::new(),
            natural: false,
            indexed_by,
        }))
    }

    fn optional_alias(&mut self) -> Result<Option<String>> {
        if self.eat_keyword(Keyword::As)? {
            return Ok(Some(self.name("after AS")?));
        }
        // `OUTER` is a valid alias but also the second word of a join type, so
        // `a outer JOIN b` is a join and `a outer` is an alias. A `JOIN` after
        // the word is what tells them apart, so the alias is only taken when
        // none follows.
        if self.at_keyword(Keyword::Outer) && self.token_after_is(Keyword::Join) {
            return Ok(None);
        }
        match self.peek() {
            Some(Token::Identifier(_)) => Ok(Some(
                self.advance()
                    .and_then(|t| match t {
                        Token::Identifier(n) => Some(n),
                        _ => None,
                    })
                    .unwrap_or_default(),
            )),
            Some(Token::Keyword(k)) if k.as_identable() && !k.starts_clause() => {
                let n = k.as_str().to_string();
                self.advance();
                Ok(Some(n))
            }
            _ => Ok(None),
        }
    }

    /// Whether the token after the one at the cursor is `kw`.
    fn token_after_is(&self, kw: Keyword) -> bool {
        matches!(self.tokens.get(self.pos + 1), Some((Token::Keyword(k), _)) if *k == kw)
    }

    fn group_by_clause(&mut self) -> Result<Vec<Expr>> {
        let mut out = Vec::new();
        if !self.eat_keyword(Keyword::Group)? {
            return Ok(out);
        }
        self.eat_keyword(Keyword::By)?;
        loop {
            out.push(self.expr()?);
            if !self.eat_punct(Punct::Comma)? {
                break;
            }
        }
        Ok(out)
    }

    fn order_by_clause(&mut self) -> Result<Vec<(Expr, bool)>> {
        let mut out = Vec::new();
        if !self.eat_keyword(Keyword::Order)? {
            return Ok(out);
        }
        self.eat_keyword(Keyword::By)?;
        loop {
            let expr = self.expr()?;
            // Ascending is the default and is often left unstated.
            let ascending = if self.eat_keyword(Keyword::Desc)? {
                false
            } else {
                self.eat_keyword(Keyword::Asc)?;
                true
            };
            out.push((expr, ascending));
            if !self.eat_punct(Punct::Comma)? {
                break;
            }
        }
        Ok(out)
    }

    fn limit_clause(&mut self) -> Result<(Option<Expr>, Option<Expr>)> {
        if !self.eat_keyword(Keyword::Limit)? {
            return Ok((None, None));
        }
        let first = self.expr()?;
        // `LIMIT a, b` is `LIMIT b OFFSET a`; the forms are distinct in the
        // grammar but mean the same thing.
        if self.eat_punct(Punct::Comma)? {
            let second = self.expr()?;
            return Ok((Some(second), Some(first)));
        }
        let offset = if self.eat_keyword(Keyword::Offset)? {
            Some(self.expr()?)
        } else {
            None
        };
        Ok((Some(first), offset))
    }

    // --- Expressions ----------------------------------------------------

    /// Parses an expression at the lowest precedence, which is OR.
    ///
    /// Every precedence level calls [`Parser::enter`] before recursing, because
    /// a guard that only runs at the top fires long after the stack is gone:
    /// each level of this grammar is a handful of Rust frames, so a thousand
    /// levels of nesting is tens of thousands of frames deep.
    fn expr(&mut self) -> Result<Expr> {
        self.expr_or()
    }

    fn expr_or(&mut self) -> Result<Expr> {
        let mut left = self.expr_and()?;
        while self.eat_keyword(Keyword::Or)? {
            let right = self.expr_and()?;
            left = Expr::Binary {
                op: BinOp::Or,
                left: Box::new(left),
                right: Box::new(right),
            };
        }
        Ok(left)
    }

    fn expr_and(&mut self) -> Result<Expr> {
        let mut left = self.expr_not()?;
        while self.eat_keyword(Keyword::And)? {
            let right = self.expr_not()?;
            left = Expr::Binary {
                op: BinOp::And,
                left: Box::new(left),
                right: Box::new(right),
            };
        }
        Ok(left)
    }

    fn expr_not(&mut self) -> Result<Expr> {
        if self.eat_keyword(Keyword::Not)? {
            let inner = self.expr_not()?;
            return Ok(Expr::Unary {
                op: UnaryOp::Not,
                expr: Box::new(inner),
            });
        }
        self.expr_comparison()
    }

    fn expr_comparison(&mut self) -> Result<Expr> {
        let left = self.expr_bitwise()?;
        // SQLite does not chain comparisons: `a = b = c` parses as
        // `(a = b) = c`, so there is at most one comparison operator here.
        let op = if self.eat_punct(Punct::Eq)? || self.eat_punct(Punct::EqEq)? {
            BinOp::Eq
        } else if self.eat_punct(Punct::Ne)? || self.eat_punct(Punct::NeBracket)? {
            BinOp::Ne
        } else if self.eat_punct(Punct::Le)? {
            BinOp::Le
        } else if self.eat_punct(Punct::Lt)? {
            BinOp::Lt
        } else if self.eat_punct(Punct::Ge)? {
            BinOp::Ge
        } else if self.eat_punct(Punct::Gt)? {
            BinOp::Gt
        } else {
            return self.postfix_predicates(left);
        };
        let right = self.expr_bitwise()?;
        let combined = Expr::Binary {
            op,
            left: Box::new(left),
            right: Box::new(right),
        };
        // A predicate may follow the comparison, as in `a = 1 ISNULL`.
        self.postfix_predicates(combined)
    }

    /// The predicates that take a trailing clause: IS, IN, LIKE, BETWEEN, and
    /// their negations.
    fn postfix_predicates(&mut self, mut left: Expr) -> Result<Expr> {
        loop {
            // ISNULL and NOTNULL are the single-word spellings, which take no
            // operand on their right.
            if self.eat_keyword(Keyword::Isnull)? {
                left = Expr::IsNull {
                    expr: Box::new(left),
                    negated: false,
                };
                continue;
            }
            if self.eat_keyword(Keyword::Notnull)? {
                left = Expr::IsNull {
                    expr: Box::new(left),
                    negated: true,
                };
                continue;
            }
            if self.eat_keyword(Keyword::Is)? {
                let negated = self.eat_keyword(Keyword::Not)?;
                if self.eat_keyword(Keyword::Null)? {
                    left = Expr::IsNull {
                        expr: Box::new(left),
                        negated,
                    };
                    continue;
                }
                let right = self.expr_bitwise()?;
                left = Expr::Binary {
                    op: if negated { BinOp::IsNot } else { BinOp::Is },
                    left: Box::new(left),
                    right: Box::new(right),
                };
                continue;
            }
            let negated = if self.at_keyword(Keyword::Not)
                && matches!(
                    self.tokens.get(self.pos + 1).map(|(t, _)| t),
                    Some(Token::Keyword(Keyword::In))
                        | Some(Token::Keyword(Keyword::Like))
                        | Some(Token::Keyword(Keyword::Glob))
                        | Some(Token::Keyword(Keyword::Regexp))
                        | Some(Token::Keyword(Keyword::Between))
                ) {
                self.advance();
                true
            } else {
                false
            };
            if self.eat_keyword(Keyword::In)? {
                left = self.in_predicate(left, negated)?;
                continue;
            }
            if self.eat_keyword(Keyword::Like)? {
                let pattern = self.expr_bitwise()?;
                let escape = if self.eat_keyword(Keyword::Escape)? {
                    Some(Box::new(self.expr_bitwise()?))
                } else {
                    None
                };
                left = Expr::Like {
                    expr: Box::new(left),
                    pattern: Box::new(pattern),
                    escape,
                    negated,
                };
                continue;
            }
            if self.eat_keyword(Keyword::Glob)? {
                let pattern = self.expr_bitwise()?;
                let op = if negated { BinOp::NotGlob } else { BinOp::Glob };
                left = Expr::Binary {
                    op,
                    left: Box::new(left),
                    right: Box::new(pattern),
                };
                continue;
            }
            if self.eat_keyword(Keyword::Regexp)? {
                let pattern = self.expr_bitwise()?;
                let op = if negated {
                    BinOp::NotRegexp
                } else {
                    BinOp::Regexp
                };
                left = Expr::Binary {
                    op,
                    left: Box::new(left),
                    right: Box::new(pattern),
                };
                continue;
            }
            if self.eat_keyword(Keyword::Between)? {
                let low = self.expr_bitwise()?;
                self.expect_keyword(Keyword::And, "inside BETWEEN")?;
                let high = self.expr_bitwise()?;
                left = Expr::Between {
                    expr: Box::new(left),
                    low: Box::new(low),
                    high: Box::new(high),
                    negated,
                };
                continue;
            }
            break;
        }
        // COLLATE binds tighter than comparison and applies to the left.
        if self.eat_keyword(Keyword::Collate)? {
            let collation = self.name("after COLLATE")?;
            return Ok(Expr::Collate {
                expr: Box::new(left),
                collation,
            });
        }
        Ok(left)
    }

    fn in_predicate(&mut self, expr: Expr, negated: bool) -> Result<Expr> {
        if self.at_punct(Punct::LParen) {
            self.advance();
            if self.at_keyword(Keyword::Select) || self.at_keyword(Keyword::With) {
                let select = self.select()?;
                self.expect_punct(Punct::RParen, "closing an IN subquery")?;
                return Ok(Expr::InSelect {
                    expr: Box::new(expr),
                    select: Box::new(select),
                    negated,
                });
            }
            let mut list = Vec::new();
            if !self.at_punct(Punct::RParen) {
                loop {
                    list.push(self.expr()?);
                    if !self.eat_punct(Punct::Comma)? {
                        break;
                    }
                }
            }
            self.expect_punct(Punct::RParen, "closing an IN list")?;
            return Ok(Expr::InList {
                expr: Box::new(expr),
                list,
                negated,
            });
        }
        // `IN table` names a table rather than a list.
        let name = self.name("after IN")?;
        self.expect_keyword(Keyword::Select, "after an IN table name")?;
        Err(Error::new(
            crate::error::ResultCode::Error,
            format!("{name} is not a table"),
        ))
    }

    /// Bitwise operators sit below comparison and above additive, per SQLite's
    /// precedence table.
    fn expr_bitwise(&mut self) -> Result<Expr> {
        let mut left = self.expr_additive()?;
        loop {
            let op = if self.eat_punct(Punct::BitOr)? {
                BinOp::BitwiseOr
            } else if self.eat_punct(Punct::BitAnd)? {
                BinOp::BitwiseAnd
            } else if self.eat_punct(Punct::BitShiftLeft)? {
                BinOp::LeftShift
            } else if self.eat_punct(Punct::BitShiftRight)? {
                BinOp::RightShift
            } else {
                break;
            };
            let right = self.expr_additive()?;
            left = Expr::Binary {
                op,
                left: Box::new(left),
                right: Box::new(right),
            };
        }
        Ok(left)
    }

    fn expr_additive(&mut self) -> Result<Expr> {
        let mut left = self.expr_multiplicative()?;
        loop {
            let op = if self.eat_punct(Punct::Plus)? {
                BinOp::Add
            } else if self.eat_punct(Punct::Minus)? {
                BinOp::Sub
            } else {
                break;
            };
            let right = self.expr_multiplicative()?;
            left = Expr::Binary {
                op,
                left: Box::new(left),
                right: Box::new(right),
            };
        }
        // `||` concatenates, and binds tighter than arithmetic in SQLite.
        while self.eat_punct(Punct::Concat)? {
            let right = self.expr_multiplicative()?;
            left = Expr::Binary {
                op: BinOp::Concat,
                left: Box::new(left),
                right: Box::new(right),
            };
        }
        Ok(left)
    }

    fn expr_multiplicative(&mut self) -> Result<Expr> {
        let mut left = self.expr_unary()?;
        loop {
            let op = if self.eat_punct(Punct::Star)? {
                BinOp::Mul
            } else if self.eat_punct(Punct::Slash)? {
                BinOp::Div
            } else if self.eat_punct(Punct::Percent)? {
                BinOp::Mod
            } else {
                break;
            };
            let right = self.expr_unary()?;
            left = Expr::Binary {
                op,
                left: Box::new(left),
                right: Box::new(right),
            };
        }
        Ok(left)
    }

    fn expr_unary(&mut self) -> Result<Expr> {
        if self.eat_punct(Punct::Minus)? {
            let inner = self.expr_unary()?;
            // A negated literal folds immediately, which is what SQLite does and
            // what keeps -9223372036854775808 representable.
            if let Expr::Literal(Literal::Integer(i)) = inner {
                return Ok(Expr::Literal(Literal::Integer(i.wrapping_neg())));
            }
            if let Expr::Literal(Literal::Real(r)) = inner {
                return Ok(Expr::Literal(Literal::Real(-r)));
            }
            return Ok(Expr::Unary {
                op: UnaryOp::Negate,
                expr: Box::new(inner),
            });
        }
        if self.eat_punct(Punct::Plus)? {
            let inner = self.expr_unary()?;
            return Ok(Expr::Unary {
                op: UnaryOp::Plus,
                expr: Box::new(inner),
            });
        }
        if self.eat_punct(Punct::Tilde)? {
            let inner = self.expr_unary()?;
            return Ok(Expr::Unary {
                op: UnaryOp::BitwiseNot,
                expr: Box::new(inner),
            });
        }
        self.expr_primary()
    }

    fn expr_primary(&mut self) -> Result<Expr> {
        let span = self.span();
        match self.advance() {
            Some(Token::Integer(i)) => Ok(Expr::Literal(Literal::Integer(i))),
            Some(Token::Float(v)) => Ok(Expr::Literal(Literal::Real(v))),
            Some(Token::String(s)) => Ok(Expr::Literal(Literal::Text(s))),
            Some(Token::Blob(b)) => Ok(Expr::Literal(Literal::Blob(b))),
            // NULL is a keyword, not a literal token, and the CURRENT_* forms
            // are zero-argument functions that the evaluator supplies.
            Some(Token::Keyword(Keyword::Null)) => Ok(Expr::Literal(Literal::Null)),
            Some(Token::Keyword(Keyword::CurrentDate)) => Ok(Expr::Function {
                name: "current_date".into(),
                args: vec![],
                star: false,
                distinct: false,
            }),
            Some(Token::Keyword(Keyword::CurrentTime)) => Ok(Expr::Function {
                name: "current_time".into(),
                args: vec![],
                star: false,
                distinct: false,
            }),
            Some(Token::Keyword(Keyword::CurrentTimestamp)) => Ok(Expr::Function {
                name: "current_timestamp".into(),
                args: vec![],
                star: false,
                distinct: false,
            }),
            Some(Token::Parameter {
                index: Some(i),
                name: None,
            }) => Ok(Expr::Literal(Literal::Parameter(i))),
            Some(Token::Parameter {
                index: None,
                name: Some(n),
            }) => Ok(Expr::NamedParameter(n, span)),
            Some(Token::Parameter {
                index: None,
                name: None,
            }) => {
                // A bare `?` is assigned the next index at resolution time; the
                // caller rewrites it. Until then it carries index 0, which the
                // binder treats as "first unbound".
                Ok(Expr::NamedParameter("?".into(), span))
            }
            Some(Token::Punct(Punct::LParen)) => {
                // Either a parenthesised expression or a subquery. A parenthesis
                // is the only construct that recurses without consuming input,
                // so it is where the depth is counted: counting at every
                // precedence level instead would spend nine stack frames per
                // unit and so fire only after the stack was already gone.
                if self.at_keyword(Keyword::Select) || self.at_keyword(Keyword::With) {
                    let select = self.select()?;
                    self.expect_punct(Punct::RParen, "closing a subquery")?;
                    return Ok(Expr::Subquery {
                        select: Box::new(select),
                    });
                }
                self.enter()?;
                let inner = self.expr();
                self.leave();
                let inner = inner?;
                self.expect_punct(Punct::RParen, "closing a parenthesised expression")?;
                Ok(inner)
            }
            Some(Token::Identifier(name)) => self.after_identifier(name, span),
            Some(Token::Keyword(kw)) => self.after_keyword(kw, span),
            _ => {
                self.pos = self.pos.saturating_sub(1);
                Err(self.unexpected("parsing an expression"))
            }
        }
    }

    /// Handles a bare identifier: a column, a call, or the start of one of the
    /// keyword-led forms once the keyword is not one of them.
    fn after_identifier(&mut self, name: String, span: Span) -> Result<Expr> {
        // A call is an identifier immediately followed by an open paren.
        if self.at_punct(Punct::LParen) {
            // The tokenizer folds a bare identifier to lowercase, because SQL
            // names are case-insensitive, but sqlite3 reports an unknown
            // function the way it was spelled: `SELECT AbC(1)` answers
            // `no such function: AbC`. So the call takes the name back off the
            // source by span. A quoted name keeps its case already, and its span
            // covers the quotes, so the quotes are trimmed here -- sqlite3
            // answers `SELECT "AbC"(1)` with `no such function: AbC`, without
            // them.
            let name = self.written_name(self.pos - 1).unwrap_or(name);
            return self.function_call(name, span);
        }
        // A qualified column is `table.column` or `schema.table.column`.
        if self.at_punct(Punct::Dot) {
            // The identifier this function was handed has already been
            // consumed, so the qualifier is the *last* token read rather than
            // the one after it. `name` is the folded token spelling, which is
            // what a lookup wants and the wrong thing to echo.
            let mut qualifier = self.written_name(self.pos - 1).unwrap_or(name);
            self.advance();
            if self.eat_punct(Punct::Star)? {
                return Ok(Expr::Function {
                    name: format!("{qualifier}.*"),
                    args: vec![],
                    star: true,
                    distinct: false,
                });
            }
            let column = self.name("after a column qualifier")?;
            if self.at_punct(Punct::Dot) {
                // The name read so far is the *column*, not the table, so a
                // third part is the table and the part already in hand has to
                // move to the end. `SELECT main.Foo.x` names the column `x` of
                // table `Foo` in schema `main`, and this used to read it as
                // the table `main.Foo` and the column `x`.
                self.advance();
                let table = self.name("after a schema qualifier")?;
                // `schema.table.column`, and the order is schema, table,
                // column: `main.T.x` is the column `x` of table `T`. The
                // name read above as the "column" is the table, so the two
                // have to be put back the other way round.
                return Ok(Expr::Column {
                    table: Some(format!("{qualifier}.{column}")),
                    name: table,
                    span,
                });
            }
            return Ok(Expr::Column {
                table: Some(qualifier),
                name: column,
                span,
            });
        }
        Ok(Expr::Column {
            table: None,
            name,
            span,
        })
    }

    fn after_keyword(&mut self, kw: Keyword, span: Span) -> Result<Expr> {
        use Keyword::*;
        match kw {
            Cast => {
                self.expect_punct(Punct::LParen, "after CAST")?;
                let expr = self.expr()?;
                self.expect_keyword(As, "inside CAST")?;
                let mut ty = self.name("for a CAST type")?;
                // A type may be written with a length, as VARCHAR(10).
                if self.eat_punct(Punct::LParen)? {
                    while !self.eat_punct(Punct::RParen)? {
                        self.advance();
                    }
                }
                while self.eat_punct(Punct::Dot)? {
                    ty.push('.');
                    ty.push_str(&self.name("in a CAST type")?);
                }
                self.expect_punct(Punct::RParen, "closing CAST")?;
                Ok(Expr::Cast {
                    expr: Box::new(expr),
                    ty,
                })
            }
            Case => self.case_expr(),
            Exists => {
                self.expect_punct(Punct::LParen, "after EXISTS")?;
                let select = self.select()?;
                self.expect_punct(Punct::RParen, "closing EXISTS")?;
                Ok(Expr::Exists {
                    select: Box::new(select),
                    negated: false,
                })
            }
            Not if self.at_keyword(Exists) => {
                self.advance();
                self.expect_punct(Punct::LParen, "after NOT EXISTS")?;
                let select = self.select()?;
                self.expect_punct(Punct::RParen, "closing NOT EXISTS")?;
                Ok(Expr::Exists {
                    select: Box::new(select),
                    negated: true,
                })
            }
            _ if kw.as_identable() => {
                // A keyword standing where a column may, with the rest of the
                // expression grammar still to come.
                self.after_identifier(kw.as_str().to_string(), span)
            }
            _ => Err(msg::syntax_error(kw.as_str())),
        }
    }

    fn case_expr(&mut self) -> Result<Expr> {
        // The optional operand form is `CASE x WHEN a THEN b ...`.
        let operand = if self.at_keyword(Keyword::When) {
            None
        } else {
            Some(Box::new(self.expr()?))
        };
        let mut whens = Vec::new();
        while self.eat_keyword(Keyword::When)? {
            let cond = self.expr()?;
            self.expect_keyword(Keyword::Then, "in a CASE arm")?;
            let result = self.expr()?;
            whens.push((cond, result));
        }
        if whens.is_empty() {
            // sqlite3 stops at the token after the last WHEN, not at the word
            // `end`: `SELECT CASE END;` is `near ";": syntax error`. The
            // clause this used to raise is not a format string in the binary
            // at all, so there is no text to match it against.
            return Err(msg::syntax_error(&self.describe_token_here()));
        }
        let otherwise = if self.eat_keyword(Keyword::Else)? {
            Some(Box::new(self.expr()?))
        } else {
            None
        };
        self.expect_keyword(Keyword::End, "closing CASE")?;
        Ok(Expr::Case {
            operand,
            whens,
            otherwise,
        })
    }

    fn function_call(&mut self, name: String, span: Span) -> Result<Expr> {
        self.expect_punct(Punct::LParen, "opening an argument list")?;
        // COUNT(*) and the like.
        if self.eat_punct(Punct::Star)? {
            self.expect_punct(Punct::RParen, "closing an argument list")?;
            return Ok(Expr::Function {
                name,
                args: vec![],
                star: true,
                distinct: false,
            });
        }
        let distinct = self.eat_keyword(Keyword::Distinct)?;
        let mut args = Vec::new();
        if !self.at_punct(Punct::RParen) {
            loop {
                args.push(self.expr()?);
                if !self.eat_punct(Punct::Comma)? {
                    break;
                }
            }
        }
        // FILTER and OVER belong to aggregates and windows, which are not
        // executed yet; accepting and ignoring them keeps the parse total.
        if self.eat_keyword(Keyword::Filter)? {
            self.expect_punct(Punct::LParen, "after FILTER")?;
            self.expect_keyword(Keyword::Where, "inside FILTER")?;
            self.expr()?;
            self.expect_punct(Punct::RParen, "closing FILTER")?;
        }
        if self.eat_keyword(Keyword::Over)? {
            if self.eat_punct(Punct::LParen)? {
                let mut depth = 1;
                while depth > 0 {
                    match self.advance() {
                        Some(Token::Punct(Punct::LParen)) => depth += 1,
                        Some(Token::Punct(Punct::RParen)) => depth -= 1,
                        Some(_) => {}
                        None => break,
                    }
                }
            } else {
                self.name("after OVER")?;
            }
        }
        self.expect_punct(Punct::RParen, "closing an argument list")?;
        Ok(Expr::Function {
            name,
            args,
            star: false,
            distinct,
        })
    }

    // --- DML ------------------------------------------------------------

    fn insert(&mut self) -> Result<Stmt> {
        self.advance(); // INSERT or REPLACE
                        // OR IGNORE / OR REPLACE / OR ABORT / OR FAIL / OR ROLLBACK.
        if self.eat_keyword(Keyword::Or)? {
            self.advance();
        }
        // INTO is optional.
        self.eat_keyword(Keyword::Into)?;
        let table = self.name("after INTO")?;
        let mut full = table.clone();
        while self.eat_punct(Punct::Dot)? {
            full.push('.');
            full.push_str(&self.name("after a table qualifier")?);
        }
        let columns = if self.eat_punct(Punct::LParen)? {
            let mut names = Vec::new();
            loop {
                names.push(self.name("in a column list")?);
                if !self.eat_punct(Punct::Comma)? {
                    break;
                }
            }
            self.expect_punct(Punct::RParen, "closing a column list")?;
            Some(names)
        } else {
            None
        };
        let source = if self.at_keyword(Keyword::Values) || self.at_keyword(Keyword::Select) {
            self.insert_source()?
        } else if self.eat_keyword(Keyword::Default)? {
            self.expect_keyword(Keyword::Values, "after DEFAULT")?;
            InsertSource::Values(vec![vec![]])
        } else {
            return Err(self.unexpected("after a table name"));
        };
        // Upsert is accepted; the conflict target is recorded only as a marker.
        if self.eat_keyword(Keyword::On)? {
            self.expect_keyword(Keyword::Conflict, "after ON")?;
            self.skip_conflict_clause()?;
        }
        Ok(Stmt::Insert {
            table: full,
            columns,
            source,
        })
    }

    fn insert_source(&mut self) -> Result<InsertSource> {
        if self.eat_keyword(Keyword::Values)? {
            let mut rows = Vec::new();
            loop {
                self.expect_punct(Punct::LParen, "opening a VALUES row")?;
                let mut row = Vec::new();
                if !self.at_punct(Punct::RParen) {
                    loop {
                        row.push(self.expr()?);
                        if !self.eat_punct(Punct::Comma)? {
                            break;
                        }
                    }
                }
                self.expect_punct(Punct::RParen, "closing a VALUES row")?;
                rows.push(row);
                if !self.eat_punct(Punct::Comma)? {
                    break;
                }
            }
            return Ok(InsertSource::Values(rows));
        }
        Ok(InsertSource::Select(Box::new(self.select()?)))
    }

    /// Consumes an ON CONFLICT clause, which this parser does not yet act on.
    fn skip_conflict_clause(&mut self) -> Result<()> {
        if self.eat_punct(Punct::LParen)? {
            let mut depth = 1;
            while depth > 0 {
                match self.advance() {
                    Some(Token::Punct(Punct::LParen)) => depth += 1,
                    Some(Token::Punct(Punct::RParen)) => depth -= 1,
                    Some(_) => {}
                    None => break,
                }
            }
        }
        if self.eat_keyword(Keyword::Where)? {
            self.expr()?;
        }
        self.eat_keyword(Keyword::Do)?;
        self.eat_keyword(Keyword::Nothing)?;
        Ok(())
    }

    fn update(&mut self) -> Result<Stmt> {
        self.advance();
        if self.eat_keyword(Keyword::Or)? {
            self.advance();
        }
        let table = self.name("after UPDATE")?;
        let mut full = table;
        while self.eat_punct(Punct::Dot)? {
            full.push('.');
            full.push_str(&self.name("after a table qualifier")?);
        }
        self.expect_keyword(Keyword::Set, "after a table name")?;
        let mut sets = Vec::new();
        loop {
            let column = self.name("in a SET clause")?;
            // An optional table qualifier on the target column.
            if self.at_punct(Punct::Dot) {
                self.advance();
                self.name("after a column qualifier")?;
            }
            self.expect_punct(Punct::Eq, "in a SET clause")?;
            let value = self.expr()?;
            sets.push((column, value));
            if !self.eat_punct(Punct::Comma)? {
                break;
            }
        }
        let where_ = if self.eat_keyword(Keyword::Where)? {
            Some(self.expr()?)
        } else {
            None
        };
        Ok(Stmt::Update {
            table: full,
            sets,
            where_,
        })
    }

    fn delete(&mut self) -> Result<Stmt> {
        self.advance();
        self.expect_keyword(Keyword::From, "after DELETE")?;
        let table = self.name("after FROM")?;
        let mut full = table;
        while self.eat_punct(Punct::Dot)? {
            full.push('.');
            full.push_str(&self.name("after a table qualifier")?);
        }
        let where_ = if self.eat_keyword(Keyword::Where)? {
            Some(self.expr()?)
        } else {
            None
        };
        Ok(Stmt::Delete {
            table: full,
            where_,
        })
    }

    // --- DDL ------------------------------------------------------------

    fn create(&mut self) -> Result<Stmt> {
        self.advance();
        let temp = if self.eat_keyword(Keyword::Temp)? || self.eat_keyword(Keyword::Temporary)? {
            true
        } else {
            false
        };
        if self.eat_keyword(Keyword::Table)? {
            return self.create_table(temp);
        }
        if self.eat_keyword(Keyword::Unique)? || self.at_keyword(Keyword::Index) {
            return self.create_index();
        }
        if self.eat_keyword(Keyword::View)? {
            let name = self.name("after CREATE VIEW")?;
            if self.eat_punct(Punct::LParen)? {
                let mut depth = 1;
                while depth > 0 {
                    match self.advance() {
                        Some(Token::Punct(Punct::LParen)) => depth += 1,
                        Some(Token::Punct(Punct::RParen)) => depth -= 1,
                        Some(_) => {}
                        None => break,
                    }
                }
            }
            self.expect_keyword(Keyword::As, "after a view name")?;
            self.skip_to_semicolon()?;
            return Ok(Stmt::Unsupported(format!("view {name}")));
        }
        // CREATE TRIGGER, VIRTUAL TABLE, and the rest.
        let kind = self
            .advance()
            .map(|t| describe(&t))
            .unwrap_or_else(|| "object".to_string());
        self.skip_to_semicolon()?;
        Ok(Stmt::Unsupported(format!("{kind}")))
    }

    fn create_table(&mut self, temp: bool) -> Result<Stmt> {
        // The statement's text starts at CREATE, which is the token before the
        // one the caller consumed.
        let start = self
            .tokens
            .get(self.pos.saturating_sub(2))
            .map(|(t, s)| match t {
                Token::Keyword(Keyword::Create) => s.start,
                _ => s.start,
            });
        let if_not_exists = self.if_not_exists()?;
        let name = self.name("after CREATE TABLE")?;
        let mut full = name;
        while self.eat_punct(Punct::Dot)? {
            full.push('.');
            full.push_str(&self.name("after a table qualifier")?);
        }
        // A table may be given AS SELECT.
        if self.eat_keyword(Keyword::As)? {
            self.select()?;
            return Ok(Stmt::Unsupported(format!("table {full} as select")));
        }
        self.expect_punct(Punct::LParen, "after a table name")?;
        let mut columns = Vec::new();
        let mut constraints = Vec::new();
        loop {
            if self.table_constraint_start() {
                constraints.push(self.table_constraint()?);
            } else {
                columns.push(self.column_def()?);
            }
            if !self.eat_punct(Punct::Comma)? {
                break;
            }
        }
        self.expect_punct(Punct::RParen, "closing a table definition")?;
        let mut without_rowid = false;
        let mut strict = false;
        // Table options, in any order.
        loop {
            // The options are comma separated, and a trailing comma is allowed.
            if self.eat_punct(Punct::Comma)? {
                continue;
            }
            // WITHOUT ROWID and STRICT are not reserved words, so they arrive
            // as identifiers rather than keywords.
            if self.eat_word("without")? {
                self.eat_word("rowid")?;
                without_rowid = true;
                continue;
            }
            if self.eat_word("strict")? {
                strict = true;
                continue;
            }
            // Anything else that is a bare option is skipped.
            if matches!(self.peek(), Some(Token::Identifier(_))) {
                self.advance();
                continue;
            }
            break;
        }
        // The stored text is the original statement, which is what SQLite keeps
        // in sqlite_schema and what a reopened connection reads the table back
        // from. Reconstructing it from the parsed form would lose the original
        // spelling, which is what a schema dump is expected to show.
        let end = self
            .tokens
            .get(self.pos.saturating_sub(1))
            .map(|(_, s)| s.end)
            .unwrap_or(0);
        let sql = match start {
            Some(a) if a <= end && end <= self.sql.len() => self.sql[a..end].trim().to_string(),
            _ => String::new(),
        };
        Ok(Stmt::CreateTable {
            name: full,
            if_not_exists,
            columns,
            constraints,
            without_rowid,
            strict,
            temp,
            sql,
        })
    }

    fn if_not_exists(&mut self) -> Result<bool> {
        if self.eat_keyword(Keyword::If)? {
            self.expect_keyword(Keyword::Not, "after IF")?;
            self.expect_keyword(Keyword::Exists, "after IF NOT")?;
            return Ok(true);
        }
        Ok(false)
    }

    /// Whether the token at the cursor begins a table constraint rather than a
    /// column definition.
    fn table_constraint_start(&self) -> bool {
        matches!(
            self.peek(),
            Some(Token::Keyword(
                Keyword::Primary
                    | Keyword::Unique
                    | Keyword::Check
                    | Keyword::Foreign
                    | Keyword::Constraint
            ))
        )
    }

    fn column_def(&mut self) -> Result<ColumnDef> {
        let name = self.name("parsing a column name")?;
        // A declared type is a run of words, optionally followed by a
        // parenthesised length as in VARCHAR(255) or DECIMAL(10,5). The length
        // comes after the name, so the words are read first and a paren is only
        // a length when one follows them; reading the paren first would treat
        // `b CHAR(10)` as a column with no type at all.
        let mut ty = String::new();
        while let Some(word) = self.type_word() {
            if !ty.is_empty() {
                ty.push(' ');
            }
            ty.push_str(&word);
        }
        if self.at_punct(Punct::LParen) {
            self.advance();
            let mut inner = String::new();
            let mut first = true;
            loop {
                match self.advance() {
                    Some(Token::Punct(Punct::RParen)) | None => break,
                    Some(Token::Punct(Punct::Comma)) => {
                        inner.push(',');
                        first = true;
                    }
                    Some(Token::Integer(i)) => {
                        if !first {
                            inner.push(' ');
                        }
                        inner.push_str(&i.to_string());
                        first = false;
                    }
                    Some(Token::Identifier(n)) => {
                        if !first {
                            inner.push(' ');
                        }
                        inner.push_str(&n);
                        first = false;
                    }
                    Some(Token::Keyword(k)) => {
                        if !first {
                            inner.push(' ');
                        }
                        inner.push_str(k.as_str());
                        first = false;
                    }
                    Some(Token::Punct(p)) => inner.push_str(punct_text(p)),
                    Some(other) => {
                        return Err(Error::new(
                            crate::error::ResultCode::Error,
                            format!(
                                "near \"{}\": syntax error in a declared type",
                                describe_token(&other)
                            ),
                        ))
                    }
                }
            }
            ty = format!("{ty}({inner})");
        }
        let mut constraints = Vec::new();
        loop {
            if let Some(c) = self.column_constraint()? {
                constraints.push(c);
            } else {
                break;
            }
        }
        Ok(ColumnDef {
            name,
            ty,
            constraints,
        })
    }

    /// Reads one word of a column type, stopping at anything that starts a
    /// constraint or ends the definition.
    fn type_word(&mut self) -> Option<String> {
        match self.peek() {
            Some(Token::Identifier(n)) => {
                let n = n.clone();
                self.advance();
                Some(n)
            }
            Some(Token::Keyword(k)) if !k.as_identable() || k.starts_clause() => None,
            Some(Token::Keyword(k)) => {
                // A keyword that names a type, such as TEXT or BLOB, is part of
                // the type; one that starts a clause is not.
                if k.is_type_word() {
                    let n = k.as_str().to_string();
                    self.advance();
                    Some(n)
                } else {
                    None
                }
            }
            Some(Token::String(s)) => {
                let s = s.clone();
                self.advance();
                Some(format!("'{s}'"))
            }
            _ => None,
        }
    }

    fn column_constraint(&mut self) -> Result<Option<Constraint>> {
        if self.eat_keyword(Keyword::Primary)? {
            self.expect_keyword(Keyword::Key, "after PRIMARY")?;
            self.eat_keyword(Keyword::Asc)?;
            let mut ascending = true;
            let mut autoincrement = false;
            if self.eat_keyword(Keyword::Desc)? {
                ascending = false;
            }
            self.eat_keyword(Keyword::Asc)?;
            if self.eat_keyword(Keyword::Autoincrement)? {
                autoincrement = true;
            }
            // Conflict clauses follow and do not change the constraint.
            if self.eat_keyword(Keyword::On)? {
                self.eat_keyword(Keyword::Conflict)?;
                self.advance();
            }
            return Ok(Some(Constraint::PrimaryKey {
                ascending,
                autoincrement,
            }));
        }
        if self.eat_keyword(Keyword::Not)? {
            if self.eat_keyword(Keyword::Null)? {
                return Ok(Some(Constraint::NotNull));
            }
            // NOT NULL, NOT DEFAULT, and the rest are handled elsewhere.
            return Ok(None);
        }
        if self.eat_keyword(Keyword::Null)? {
            return Ok(Some(Constraint::NotNull));
        }
        if self.eat_keyword(Keyword::Unique)? {
            if self.eat_keyword(Keyword::On)? {
                self.eat_keyword(Keyword::Conflict)?;
                self.advance();
            }
            return Ok(Some(Constraint::Unique));
        }
        if self.eat_keyword(Keyword::Check)? {
            self.expect_punct(Punct::LParen, "after CHECK")?;
            let e = self.expr()?;
            self.expect_punct(Punct::RParen, "closing CHECK")?;
            return Ok(Some(Constraint::Check(e)));
        }
        if self.eat_keyword(Keyword::Default)? {
            if self.eat_punct(Punct::LParen)? {
                self.expr()?;
                self.expect_punct(Punct::RParen, "closing a DEFAULT expression")?;
                return Ok(Some(Constraint::Default(Expr::Literal(Literal::Null))));
            }
            let e = self.expr_unary()?;
            return Ok(Some(Constraint::Default(e)));
        }
        if self.eat_keyword(Keyword::Collate)? {
            let c = self.name("after COLLATE")?;
            return Ok(Some(Constraint::Collate(c)));
        }
        if self.eat_keyword(Keyword::References)? {
            let table = self.name("after REFERENCES")?;
            let mut columns = Vec::new();
            if self.eat_punct(Punct::LParen)? {
                loop {
                    columns.push(self.name("in a foreign key column list")?);
                    if !self.eat_punct(Punct::Comma)? {
                        break;
                    }
                }
                self.expect_punct(Punct::RParen, "closing a foreign key column list")?;
            }
            // The rest of a foreign key clause is accepted and not yet enforced.
            self.skip_foreign_key_tail()?;
            return Ok(Some(Constraint::ForeignKey { table, columns }));
        }
        // A bare constraint name introduced by CONSTRAINT applies to whatever
        // follows, which the caller already consumed.
        if self.eat_keyword(Keyword::Constraint)? {
            self.name("after CONSTRAINT")?;
            return self.column_constraint();
        }
        Ok(None)
    }

    fn skip_foreign_key_tail(&mut self) -> Result<()> {
        loop {
            if self.eat_keyword(Keyword::On)? {
                self.eat_keyword(Keyword::Delete)?;
                self.eat_keyword(Keyword::Set)?;
                self.advance();
                continue;
            }
            if self.eat_keyword(Keyword::On)? {
                self.eat_keyword(Keyword::Update)?;
                self.advance();
                continue;
            }
            if self.eat_keyword(Keyword::Match)? {
                self.name("after MATCH")?;
                continue;
            }
            if self.eat_keyword(Keyword::Deferrable)? {
                continue;
            }
            if self.eat_keyword(Keyword::Not)? {
                self.eat_keyword(Keyword::Deferrable)?;
                continue;
            }
            if self.eat_keyword(Keyword::Initially)? {
                self.advance();
                continue;
            }
            break;
        }
        Ok(())
    }

    fn table_constraint(&mut self) -> Result<Constraint> {
        if self.eat_keyword(Keyword::Constraint)? {
            self.name("after CONSTRAINT")?;
            return self.table_constraint();
        }
        if self.eat_keyword(Keyword::Primary)? {
            self.expect_keyword(Keyword::Key, "after PRIMARY")?;
            let mut ascending = true;
            if self.eat_keyword(Keyword::Desc)? {
                ascending = false;
            }
            self.eat_keyword(Keyword::Asc)?;
            let mut autoincrement = false;
            if self.eat_keyword(Keyword::Autoincrement)? {
                autoincrement = true;
            }
            if self.eat_keyword(Keyword::On)? {
                self.eat_keyword(Keyword::Conflict)?;
                self.advance();
            }
            self.expect_punct(Punct::LParen, "after PRIMARY KEY")?;
            // The column list is consumed; enforcement comes later.
            let mut depth = 1;
            while depth > 0 {
                match self.advance() {
                    Some(Token::Punct(Punct::LParen)) => depth += 1,
                    Some(Token::Punct(Punct::RParen)) => depth -= 1,
                    Some(_) => {}
                    None => break,
                }
            }
            return Ok(Constraint::PrimaryKey {
                ascending,
                autoincrement,
            });
        }
        if self.eat_keyword(Keyword::Unique)? {
            if self.eat_keyword(Keyword::On)? {
                self.eat_keyword(Keyword::Conflict)?;
                self.advance();
            }
            self.expect_punct(Punct::LParen, "after UNIQUE")?;
            let mut depth = 1;
            while depth > 0 {
                match self.advance() {
                    Some(Token::Punct(Punct::LParen)) => depth += 1,
                    Some(Token::Punct(Punct::RParen)) => depth -= 1,
                    Some(_) => {}
                    None => break,
                }
            }
            return Ok(Constraint::Unique);
        }
        if self.eat_keyword(Keyword::Check)? {
            self.expect_punct(Punct::LParen, "after CHECK")?;
            let e = self.expr()?;
            self.expect_punct(Punct::RParen, "closing CHECK")?;
            return Ok(Constraint::Check(e));
        }
        if self.eat_keyword(Keyword::Foreign)? {
            self.expect_keyword(Keyword::Key, "after FOREIGN")?;
            self.expect_punct(Punct::LParen, "after FOREIGN KEY")?;
            let mut depth = 1;
            while depth > 0 {
                match self.advance() {
                    Some(Token::Punct(Punct::LParen)) => depth += 1,
                    Some(Token::Punct(Punct::RParen)) => depth -= 1,
                    Some(_) => {}
                    None => break,
                }
            }
            let table = self.name("after REFERENCES")?;
            let mut columns = Vec::new();
            if self.eat_punct(Punct::LParen)? {
                loop {
                    columns.push(self.name("in a foreign key column list")?);
                    if !self.eat_punct(Punct::Comma)? {
                        break;
                    }
                }
                self.expect_punct(Punct::RParen, "closing a foreign key column list")?;
            }
            self.skip_foreign_key_tail()?;
            return Ok(Constraint::ForeignKey { table, columns });
        }
        Err(self.unexpected("parsing a table constraint"))
    }

    fn create_index(&mut self) -> Result<Stmt> {
        // The statement's own text starts at CREATE. By the time this runs the
        // cursor has moved past CREATE and possibly past UNIQUE and INDEX, so
        // the keyword is looked for backwards from where the cursor is.
        let start = self.tokens[..self.pos]
            .iter()
            .rposition(|(t, _)| matches!(t, Token::Keyword(Keyword::Create)))
            .map(|i| self.tokens[i].1.start);
        let unique = self.eat_keyword(Keyword::Unique)?;
        self.expect_keyword(Keyword::Index, "after CREATE")?;
        let if_not_exists = self.if_not_exists()?;
        let name = if self.at_keyword(Keyword::On) {
            None
        } else {
            Some(self.name("after CREATE INDEX")?)
        };
        self.expect_keyword(Keyword::On, "after an index name")?;
        let table = self.name("after ON")?;
        self.expect_punct(Punct::LParen, "opening an index column list")?;
        let mut columns = Vec::new();
        loop {
            // An indexed expression is parenthesised.
            let col = if self.at_punct(Punct::LParen) {
                self.advance();
                self.expr()?;
                self.expect_punct(Punct::RParen, "closing an index expression")?;
                String::new()
            } else {
                self.name("in an index column list")?
            };
            let ascending = if self.eat_keyword(Keyword::Desc)? {
                false
            } else {
                self.eat_keyword(Keyword::Asc)?;
                true
            };
            self.eat_keyword(Keyword::Collate)?;
            if matches!(self.peek(), Some(Token::Identifier(_))) {
                self.advance();
            }
            columns.push((col, ascending));
            if !self.eat_punct(Punct::Comma)? {
                break;
            }
        }
        self.expect_punct(Punct::RParen, "closing an index column list")?;
        if self.eat_keyword(Keyword::Where)? {
            self.expr()?;
        }
        // The text is what sqlite_schema stores and what a reopened connection
        // rebuilds the index from, so it is the original statement rather than
        // a reconstruction of the parsed form.
        let sql = match start {
            // Only this statement's text. The script is tokenised up front and
            // the cursor is already past the statement, so the end is where the
            // last token of the statement ended rather than where the next one
            // starts.
            Some(a) => {
                let to = self
                    .tokens
                    .get(self.pos.saturating_sub(1))
                    .map(|(_, s)| s.end)
                    .unwrap_or(a);
                let text = self.sql.get(a..to.max(a)).unwrap_or_default();
                crate::index_ddl::sql_for_statement(text).unwrap_or_default()
            }
            None => String::new(),
        };
        Ok(Stmt::CreateIndex {
            name,
            table,
            columns,
            unique,
            if_not_exists,
            sql,
        })
    }

    fn drop(&mut self) -> Result<Stmt> {
        self.advance();
        if self.eat_keyword(Keyword::Table)? {
            let if_exists = if self.eat_keyword(Keyword::If)? {
                self.expect_keyword(Keyword::Exists, "after IF")?;
                true
            } else {
                false
            };
            let name = self.name("after DROP TABLE")?;
            return Ok(Stmt::DropTable { name, if_exists });
        }
        let kind = self
            .advance()
            .map(|t| describe(&t))
            .unwrap_or_else(|| "object".to_string());
        self.skip_to_semicolon()?;
        Ok(Stmt::Unsupported(format!("drop {kind}")))
    }
}

/// Records the join operator and constraint on a freshly parsed FROM item.
///
/// The operator and the constraint are both written *after* the table they
/// belong to, so they are read once the table is known and written back here.
/// A subquery in FROM is not joinable yet, and the executor says so with a
/// better message than this layer can, so the caller's error is left alone.
fn set_join(
    item: &mut FromItem,
    kind: Option<JoinKind>,
    on: Option<Expr>,
    using: Vec<String>,
) -> Result<()> {
    match item {
        FromItem::Table(tref) => {
            tref.join = kind;
            tref.on = on;
            tref.using = using;
            Ok(())
        }
        FromItem::Subquery { .. } => Err(Error::new(
            crate::error::ResultCode::Error,
            "a subquery in FROM is not supported yet",
        )),
    }
}

/// Records a NATURAL join operator on a freshly parsed FROM item.
///
/// A NATURAL join has no written constraint: the columns that must be equal are
/// the two tables' shared ones, and only the executor knows what those are since
/// it is the one that reads the catalog. So the item records the operator and the
/// flag, and `sources_from` fills the column list in.
fn set_natural(item: &mut FromItem, kind: Option<JoinKind>) -> Result<()> {
    match item {
        FromItem::Table(tref) => {
            tref.join = kind;
            tref.natural = true;
            Ok(())
        }
        FromItem::Subquery { .. } => Err(Error::new(
            crate::error::ResultCode::Error,
            "a subquery in FROM is not supported yet",
        )),
    }
}

/// A token rendered for an error message.
fn describe_token(t: &Token) -> String {
    match t {
        Token::Identifier(n) => n.clone(),
        Token::Keyword(k) => k.as_str().to_string(),
        Token::String(s) => format!("'{s}'"),
        Token::Integer(i) => i.to_string(),
        Token::Float(v) => v.to_string(),
        Token::Punct(_) => "punctuation".to_string(),
        _ => "token".to_string(),
    }
}

/// A short description of a token, for the "unsupported" marker.
fn describe(t: &Token) -> String {
    match t {
        Token::Keyword(k) => k.as_str().to_string(),
        Token::Identifier(n) => n.clone(),
        _ => "object".to_string(),
    }
}

fn hex(b: &[u8]) -> String {
    const D: &[u8; 16] = b"0123456789ABCDEF";
    let mut s = String::with_capacity(b.len() * 2);
    for &byte in b {
        s.push(D[(byte >> 4) as usize] as char);
        s.push(D[(byte & 0xf) as usize] as char);
    }
    s
}

fn punct_text(p: Punct) -> &'static str {
    use Punct::*;
    match p {
        LParen => "(",
        RParen => ")",
        Comma => ",",
        Semicolon => ";",
        Dot => ".",
        Plus => "+",
        Minus => "-",
        Star => "*",
        Slash => "/",
        Percent => "%",
        Lt => "<",
        Le => "<=",
        Gt => ">",
        Ge => ">=",
        Eq => "=",
        EqEq => "==",
        Ne => "!=",
        NotEq => "<>",
        BitAnd => "&",
        BitOr => "|",
        BitwiseNot => "~",
        LeftShift => "<<",
        RightShift => ">>",
        Concat => "||",
        Arrow => "->",
        ArrowRight => "->>",
    }
}

#[cfg(test)]
#[path = "parser_tests.rs"]
mod parser_tests;
