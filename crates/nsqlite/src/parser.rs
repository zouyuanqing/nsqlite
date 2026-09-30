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

use crate::error::{Error, Result, ResultCode};
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
    /// An integer literal too large for an i64 that may still be one once a
    /// sign is applied: 2^63. The sign is what settles it, and the value is
    /// negative, so the two spellings differ.
    Big(u64),
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
    /// `expr MATCH expr` and `expr NOT MATCH expr`.
    ///
    /// A separate variant rather than a `BinOp` because `MATCH` is not a
    /// comparison: it is a call the *planner* has to hand to a virtual table,
    /// and it is a syntax error to write it where no virtual table can take
    /// it. See the `eval` arm for the message that is measured against the
    /// reference.
    Match {
        expr: Box<Expr>,
        pattern: Box<Expr>,
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
        /// The columns a *table-level* `PRIMARY KEY(a,b)` named, in the order
        /// it wrote them. A column-level primary key leaves it empty, because
        /// the column carrying the constraint is the key and the catalog knows
        /// which one that is. It is the same rule the table's rowid alias uses,
        /// so one reading covers both.
        columns: Vec<String>,
    },
    NotNull,
    /// A uniqueness constraint: `UNIQUE` on a column, or a table-level
    /// `UNIQUE(a,b)`. `columns` is as it is for a table-level primary key.
    Unique { columns: Vec<String> },
    /// A `DEFAULT`, and whether it was written inside parentheses.
    ///
    /// The parentheses are not decoration: SQLite's grammar reads an
    /// unparenthesised `DEFAULT a` as a *string literal* and a parenthesised
    /// `DEFAULT (a)` as a *reference* to a column, and refuses the reference
    /// with `default value of column [c] is not constant` wherever it appears.
    /// The two spellings produce the same expression tree and differ only in
    /// this flag, so the flag is what has to survive the parse for that
    /// refusal to be reachable at all.
    Default {
        expr: Expr,
        parenthesized: bool,
    },
    Check {
        expr: Expr,
        /// The expression's source text, exactly as the statement wrote it.
        ///
        /// SQLite's refusal is `CHECK constraint failed: <that text>`, so the
        /// message is a quotation of the schema rather than a rendering of the
        /// tree. Re-printing the tree would spell it differently: `a > 0 AND a
        /// < 10` written with no spaces comes back with them, and a column
        /// quoted one way in the DDL would come back quoted another. The spans
        /// of the tokens the expression consumed are the only faithful source.
        text: String,
    },
    ForeignKey {
        table: String,
        columns: Vec<String>,
    },
    Collate(String),
}

/// What a write does when it hits a UNIQUE or PRIMARY KEY conflict: the
/// `OR IGNORE` / `OR REPLACE` / `OR ABORT` / `OR FAIL` / `OR ROLLBACK` clause,
/// and the absence of one.
///
/// This used to be parsed and thrown away in a single `advance()`, which left
/// no way to tell `INSERT OR IGNORE` from `INSERT` and made three different
/// behaviours indistinguishable. The default is [`ConflictAction::Abort`]
/// because ABORT is what a statement with no clause does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConflictAction {
    /// Skip the row that conflicts and carry on with the rest. Does not apply to
    /// a NOT NULL violation, which is not a conflict and still aborts.
    Ignore,
    /// Delete the conflicting row and insert the new one. This is a delete
    /// followed by an insert, so the surviving row has a *new* rowid.
    Replace,
    /// Undo the whole statement and report the error. The default.
    #[default]
    Abort,
    /// Stop the statement and report the error, keeping what earlier statements
    /// wrote.
    Fail,
    /// Undo the whole transaction. Like Abort for a statement outside one,
    /// which is the only kind this engine has.
    Rollback,
}

impl ConflictAction {
    /// Whether a conflict leaves this statement's earlier rows behind, which is
    /// what tells FAIL and ROLLBACK apart from ABORT when there is no
    /// transaction to roll back.
    pub fn leaves_prior_rows(self) -> bool {
        matches!(self, ConflictAction::Fail | ConflictAction::Rollback)
    }
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
    /// `ALTER TABLE <name> ADD COLUMN <column>`, the one ALTER shape this
    /// engine runs.
    ///
    /// The other two -- `RENAME TO` and `DROP COLUMN` -- are separate and
    /// larger, and stay in [`Stmt::Unsupported`] rather than being refused as
    /// syntax errors.
    AlterTableAddColumn {
        name: String,
        /// The added column's `DEFAULT (name)`, when it had one: a reference
        /// rather than a constant, which SQLite refuses -- but only after the
        /// table has been resolved, since a missing table is the louder
        /// answer. See [`Parser::default_name_reference`].
        default_name_reference: Option<String>,
        /// The new column, read by the same `column_def` a `CREATE TABLE`
        /// reads, so the four name quotings and the constraint list are the
        /// ones the table grammar already accepts.
        column: ColumnDef,
        /// The added column's own source text, from the first token of its
        /// name through the end of its last constraint.
        ///
        /// This is what gets spliced into the stored `CREATE TABLE` text, and
        /// it is sliced out of the source rather than rebuilt from
        /// [`Self::AlterTableAddColumn::column`]: SQLite keeps the spelling
        /// the statement used, so `DEFAULT x'00FF'` and `DEFAULT (1+2)` both
        /// have to survive verbatim.
        column_sql: String,
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
    /// `CREATE VIRTUAL TABLE name USING module(args)`.
    ///
    /// The four fields are exactly what `vec0_bridge::CreateVirtualTable`
    /// asks the parser for. `args` is the parenthesised list **verbatim**,
    /// parentheses included, because the module owns that grammar and a
    /// re-serialised list would lose the original spelling -- which is the
    /// same reason `CreateIndex` carries `sql`. Measured: the reference
    /// stores `sql = <the whole CREATE VIRTUAL TABLE text>`, and a reopened
    /// connection reads the definition back out of that column, so a
    /// reconstruction is not equivalent to the original.
    CreateVirtualTable {
        name: String,
        /// The module name -- `vec0`.
        module: String,
        /// The module's argument list, verbatim, parentheses included.
        args: String,
        /// The whole statement's text, which is what `sqlite_schema` stores.
        sql: String,
    },
    Insert {
        table: String,
        columns: Option<Vec<String>>,
        source: InsertSource,
        /// What to do about a UNIQUE conflict. `Abort` when the statement said
        /// nothing, which is what the reference does.
        conflict: ConflictAction,
    },
    Update {
        table: String,
        sets: Vec<(String, Expr)>,
        where_: Option<Expr>,
        /// As on INSERT: the default is `Abort`.
        conflict: ConflictAction,
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

impl Stmt {
    /// The double-quoted names this statement wrote, with the quotes removed.
    ///
    /// `text` is the text the statement was parsed from, which is what the
    /// span a statement records points into. The two shapes that keep their own
    /// copy of it answer from that instead, so a caller that has the text and
    /// a caller that does not both get the right answer.
    ///
    /// A statement with neither -- a hand-built [`Stmt`], or one of the shapes
    /// that does not record a span -- reports an empty list, which is the safe
    /// answer: the plain `no such column: x` is right whenever no name was
    /// written in double quotes.
    pub fn double_quoted_names(&self, text: &str) -> Vec<String> {
        crate::parser::double_quoted_names(&self.statement_text(text))
    }

    /// The statement's own text, out of `text`.
    ///
    /// `text` is what the statement was parsed from. Only a statement that has
    /// been through the grammar can answer, because only the grammar knows
    /// where a statement began and ended.
    ///
    /// A statement that cannot answer produces an empty string rather than a
    /// panic, because the answer is only ever a refinement of a message the
    /// caller is about to raise anyway, and a hand-built [`Stmt`] is the one
    /// case where a range would be a guess. The two DDL shapes answer from the
    /// text they carry, so they are right whether or not `text` is.
    pub fn statement_text(&self, text: &str) -> String {
        let sql = match self {
            Stmt::CreateTable { sql, .. }
            | Stmt::CreateIndex { sql, .. }
            | Stmt::CreateVirtualTable { sql, .. } => sql.as_str(),
            Stmt::Select(s) => {
                let span = select_span(s);
                text.get(span.start..span.end).unwrap_or("")
            }
            Stmt::Insert { source, .. } => match source {
                InsertSource::Select(inner) => {
                    let span = select_span(inner);
                    text.get(span.start..span.end).unwrap_or("")
                }
                // A VALUES list has no names of its own; its expressions do, and
                // they are read off the same tokens either way.
                InsertSource::Values(_) => "",
            },
            Stmt::Update { where_, .. } | Stmt::Delete { where_, .. } => match where_ {
                Some(w) => {
                    let mut span: Option<Span> = None;
                    expr_spans(w, &mut |s| {
                        span = Some(match span {
                            Some(cur) => Span {
                                start: cur.start.min(s.start),
                                end: cur.end.max(s.end),
                                line: cur.line,
                                col: cur.col,
                            },
                            None => s,
                        });
                    });
                    match span {
                        Some(s) => text.get(s.start..s.end).unwrap_or(""),
                        None => "",
                    }
                }
                None => "",
            },
            Stmt::DropTable { name, .. } => {
                // The name is a token's text, and the quoting it was written
                // with is gone by the time the statement exists.
                let _ = name;
                ""
            }
            // The added column keeps the statement's own spelling, which is the
            // text a `"..."` name has to be read back out of: this variant is
            // the one whose names are in its own copy rather than the
            // caller's, exactly as the two DDL shapes above are.
            Stmt::AlterTableAddColumn { column_sql, .. } => column_sql.as_str(),
            Stmt::Unsupported(_)
            | Stmt::Pragma(_)
            | Stmt::Begin
            | Stmt::Commit
            | Stmt::Rollback
            | Stmt::Analyze
            | Stmt::Explain(_) => "",
        };
        sql.to_string()
    }
}

/// The range a SELECT occupies in the text it was parsed from.
///
/// The smallest range that holds every expression in the statement, which is
/// the first token of its first arm through the last token of its last. A name
/// is inside the range exactly when it was written inside the statement, which
/// is the only question the caller asks of it, and a range wider than the
/// statement could only pick up a name from a *different* statement.
fn select_span(sel: &Select) -> Span {
    let mut span = None::<Span>;
    let mut widen = |s: Span| {
        span = Some(match span {
            Some(cur) => Span {
                start: cur.start.min(s.start),
                end: cur.end.max(s.end),
                line: cur.line,
                col: cur.col,
            },
            None => s,
        });
    };
    select_body_spans(&sel.body, &mut widen);
    for (e, _) in &sel.order_by {
        expr_spans(e, &mut widen);
    }
    if let Some(l) = &sel.limit {
        expr_spans(l, &mut widen);
    }
    if let Some(o) = &sel.offset {
        expr_spans(o, &mut widen);
    }
    span.unwrap_or(Span {
        start: 0,
        end: 0,
        line: 1,
        col: 1,
    })
}

/// Every expression span a SELECT body holds.
fn select_body_spans(body: &SelectBody, visit: &mut impl FnMut(Span)) {
    match body {
        SelectBody::Simple {
            columns,
            from,
            where_,
            group_by,
            having,
            values,
            ..
        } => {
            for c in columns {
                expr_spans(&c.expr, visit);
            }
            for f in from {
                from_item_spans(f, visit);
            }
            if let Some(w) = where_ {
                expr_spans(w, visit);
            }
            for g in group_by {
                expr_spans(g, visit);
            }
            if let Some(h) = having {
                expr_spans(h, visit);
            }
            if let Some(rows) = values {
                for row in rows {
                    for v in row {
                        expr_spans(v, visit);
                    }
                }
            }
        }
        SelectBody::Compound { left, right, .. } => {
            select_body_spans(left, visit);
            select_body_spans(right, visit);
        }
        SelectBody::Nested(inner) => select_body_spans(&inner.body, visit),
    }
}

/// Every expression span a FROM item holds.
///
/// A named table has none of its own: its name is a string, not a reference
/// with a span, and by the time a name is resolved the quoting it was written
/// with is gone.
fn from_item_spans(item: &FromItem, visit: &mut impl FnMut(Span)) {
    match item {
        FromItem::Table(t) => {
            if let Some(on) = &t.on {
                expr_spans(on, visit);
            }
        }
        FromItem::Subquery { select, .. } => {
            let span = select_span(select);
            visit(span);
        }
    }
}

/// Every expression span an expression holds, deepest child first.
///
/// A `Column` is the only node that records its own span -- every other node's
/// span is a range between its operands' -- so the walk stops wherever it finds
/// one. A node with no operands at all contributes nothing, which is why a
/// `CASE` and a literal are not walked: neither can hold a name.
fn expr_spans(e: &Expr, visit: &mut impl FnMut(Span)) {
    match e {
        Expr::Column { span, .. } | Expr::NamedParameter(_, span) => visit(*span),
        Expr::Literal(_) => {}
        Expr::Unary { expr, .. } | Expr::IsNull { expr, .. } | Expr::Cast { expr, .. } => {
            expr_spans(expr, visit);
        }
        Expr::Collate { expr, .. } => expr_spans(expr, visit),
        Expr::Binary { left, right, .. } => {
            expr_spans(left, visit);
            expr_spans(right, visit);
        }
        Expr::Like {
            expr,
            pattern,
            escape,
            ..
        } => {
            expr_spans(expr, visit);
            expr_spans(pattern, visit);
            if let Some(x) = escape {
                expr_spans(x, visit);
            }
        }
        Expr::Match {
            expr, pattern, ..
        } => {
            expr_spans(expr, visit);
            expr_spans(pattern, visit);
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            expr_spans(expr, visit);
            expr_spans(low, visit);
            expr_spans(high, visit);
        }
        Expr::InList { expr, list, .. } => {
            expr_spans(expr, visit);
            for i in list {
                expr_spans(i, visit);
            }
        }
        Expr::InSelect { expr, select, .. } => {
            expr_spans(expr, visit);
            select_body_spans(&select.body, visit);
        }
        Expr::Exists { select, .. } | Expr::Subquery { select } => {
            select_body_spans(&select.body, visit);
        }
        Expr::Function { args, .. } => {
            for a in args {
                expr_spans(a, visit);
            }
        }
        Expr::Case {
            operand,
            whens,
            otherwise,
        } => {
            if let Some(o) = operand {
                expr_spans(o, visit);
            }
            for (w, t) in whens {
                expr_spans(w, visit);
                expr_spans(t, visit);
            }
            if let Some(o) = otherwise {
                expr_spans(o, visit);
            }
        }
    }
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
        let before = p.index;
        out.push(p.statement()?);
        if p.index == before {
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

/// The double-quoted names in `sql`, with the quotes already removed.
///
/// A `"..."` that resolves to nothing is a mistake about quoting, not about
/// the name, and SQLite says so: `no such column: "a+b" - should this be a
/// string literal in single-quotes?`. The name is resolved long after the
/// quoting is gone, so the names are collected here, where the tokens are
/// still what the statement wrote, and the result is a plain [`String`]s
/// vector a caller can match a failed name against without a borrow of a
/// [`Parser`].
///
/// A name written twice is collected twice, and a name inside a string
/// literal is not in the list at all: only the tokenizer's
/// [`Token::DoubleQuotedIdentifier`] counts, which is the same decision the
/// token itself made.
///
/// The tokens are not scanned beyond the last statement's terminator, so
/// text that is not a statement -- a comment tail, or a second script pasted
/// after the one being run -- is not searched.
pub fn double_quoted_names(sql: &str) -> Vec<String> {
    let Ok(mut p) = Parser::new(sql) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    while let Ok(Some(token)) = p.peek_token() {
        if let Token::DoubleQuotedIdentifier(name) = token {
            out.push(name.clone());
        }
        if p.advance().is_none() {
            break;
        }
    }
    out
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
    /// The cursor: the index of the token `peek` would hand back.
    ///
    /// Every place that needs a token by position goes through it -- `peek`, the
    /// `describe` helpers, `lookup_name` -- rather than reading the field
    /// directly, so that a site cannot be right about the token at hand and
    /// wrong about the cursor that names it.
    index: usize,
    depth: usize,
    sql: &'a str,
    /// The column whose `DEFAULT (name)` is a reference, and so is not a
    /// constant, set by the column grammar and cleared by whoever consumes it.
    ///
    /// It is held here rather than raised on the spot because the order of two
    /// refusals is measured and the two belong to different layers. SQLite
    /// refuses it in its *grammar*, so a `CREATE TABLE t(a,b DEFAULT (a))` is
    /// a parse error and a parse error is not overridable. The same grammar
    /// rule applies to an `ALTER TABLE`, but SQLite resolves the table first
    /// there, so `ALTER TABLE nosuch ADD COLUMN c DEFAULT (a)` says `no such
    /// table: nosuch` instead (both measured on 3.53.4). One flag, consulted
    /// where the order is known, is what lets the two agree.
    default_name_reference: Option<String>,
    _marker: std::marker::PhantomData<&'a ()>,
}

impl<'a> Parser<'a> {
    fn new(sql: &'a str) -> Result<Parser<'a>> {
        let tokens = Tokenizer::tokenize_all(sql)?;
        Ok(Parser {
            tokens,
            index: 0,
            depth: 0,
            sql,
            default_name_reference: None,
            _marker: std::marker::PhantomData,
        })
    }

    /// The token under the cursor, without moving it.
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.index).map(|(t, _)| t)
    }

    /// The token `ahead` places past the cursor, without moving it.
    fn peek_at(&self, ahead: usize) -> Option<&Token> {
        self.tokens.get(self.index + ahead).map(|(t, _)| t)
    }

    /// The source text the tokens in `start..end` came from.
    ///
    /// Empty when the range is empty or its spans do not fall inside the
    /// statement. It never guesses, because the result is quoted verbatim in an
    /// error message and a wrong quotation is worse than none.
    fn text_between(&self, start: usize, end: usize) -> String {
        let Some((_, first)) = self.tokens.get(start) else {
            return String::new();
        };
        let Some((_, last)) = self.tokens.get(end.saturating_sub(1)) else {
            return String::new();
        };
        self.sql
            .get(first.start..last.end)
            .unwrap_or_default()
            .to_string()
    }

    fn peek_token(&self) -> Result<Option<&Token>> {
        Ok(self.peek())
    }

    fn span(&self) -> Span {
        self.tokens
            .get(self.index)
            .map(|(_, s)| *s)
            .or_else(|| self.tokens.last().map(|(_, s)| *s))
            .unwrap_or(Span {
                start: 0,
                end: 0,
                line: 1,
                col: 1,
            })
    }

    /// The offset a statement's own text starts at.
    ///
    /// A statement's text -- the one `CREATE TABLE` stores in `sqlite_schema` --
    /// begins at its first token, and by the time the grammar has read a
    /// `CREATE TEMP TABLE` the cursor is two tokens past it, so the span of the
    /// token at the cursor is not where the text starts. A caller that wants
    /// the text asks here.
    ///
    /// The search is for the `;` that ended the statement before this one,
    /// because the script loop consumes a terminator itself and hands the
    /// parser the token after it: the statement's first token is the one whose
    /// span begins where that `;` ends. A script that opens on whitespace or a
    /// comment has its first token at or after offset 0, and a script with no
    /// terminator at all before this statement -- which `parse_script` will not
    /// produce, because it requires one -- falls back to the cursor's own
    /// token.
    fn statement_start_offset(&self) -> usize {
        let Some((_, span)) = self.tokens.get(self.index) else {
            return 0;
        };
        if let Some(prev) = self
            .tokens
            .iter()
            .rposition(|(_, s)| s.end > 0 && s.end <= span.start)
        {
            let after = self.tokens[prev].1.end;
            if matches!(self.tokens[prev].0, Token::Punct(Punct::Semicolon)) {
                // The token after the `;` starts the statement. It is the one
                // in hand, unless the cursor has already moved past it, in
                // which case the next one after it does.
                let first = self
                    .tokens
                    .iter()
                    .position(|(_, s)| s.start >= after)
                    .unwrap_or(self.tokens.len());
                if first <= self.index {
                    return self.tokens[first].1.start;
                }
            }
        }
        // A statement that opens the text has no `;` before it, and its own
        // first token is the only one that can begin it. A statement that is
        // neither is not one `parse_script` produces, and falls back to the
        // cursor's token.
        match self.tokens.first() {
            Some((_, s)) if s.start == 0 => 0,
            _ => span.start,
        }
    }

    fn advance(&mut self) -> Option<Token> {
        let t = self.tokens.get(self.index).map(|(t, _)| t.clone());
        if t.is_some() {
            self.index += 1;
        }
        t
    }

    fn at_keyword(&self, kw: Keyword) -> bool {
        matches!(self.peek(), Some(Token::Keyword(k)) if *k == kw)
    }

    /// Whether the cursor is on the *word* `kw`, spelled that way and not
    /// quoted.
    ///
    /// A quoted name that happens to spell a keyword is a name and not the
    /// keyword: `SELECT CASE "WHEN";` is `near ";": syntax error` because
    /// sqlite3 read `"WHEN"` as the CASE operand and then wanted a `WHEN` arm
    /// it did not find, while an unquoted `WHEN` would have ended the operand.
    /// The two spellings therefore have to be told apart, and a quoted
    /// identifier is an [`Token::Identifier`] even when the tokenizer could
    /// have read it as the keyword.
    fn spells_keyword_ahead(&self, kw: Keyword) -> bool {
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
    ///
    /// `context` is this engine's own note about which clause wanted the token
    /// and is deliberately not part of the message. sqlite3 has no
    /// `while <clause>` wording at all -- the format string it owns is
    /// `near "%T": syntax error` and nothing longer -- so the clause used to be
    /// appended here, which made every one of these a message the catalogue did
    /// not have. The clause names are still worth having at the call sites,
    /// where they say what the parser was doing, so the argument stays and is
    /// simply not read here.
    ///
    /// The `;` is the one exception the general rule makes, and it is made here
    /// rather than in [`syntax_error_here`] because the two spellings differ:
    /// SQLite's own comment on `near "%T": syntax error` says a stopped
    /// statement is `incomplete input` whatever the clause, so
    /// `SELECT * FROM` is `incomplete input` and not the `near "FROM": syntax
    /// error` the cursor alone would give. `SELECT * FROM;` is the other way
    /// round -- `near ";": syntax error` -- because the terminator is a token
    /// the parser could not use rather than an absence.
    fn unexpected(&self, _context: &str) -> Error {
        match self.peek() {
            None => msg::incomplete_input(),
            _ => self.syntax_error_here(),
        }
    }

    /// `near "TOKEN": syntax error` for whatever the cursor is on, and
    /// `incomplete input` for a statement that ran out.
    ///
    /// The two are one decision, and it is SQLite's rather than this engine's.
    /// A statement with nothing left is `incomplete input`; a statement with a
    /// *further* token in it is `near "that token": syntax error`. Which is
    /// which is not a question about the clause, and measuring it says the
    /// clause is not consulted at all:
    ///
    /// ```text
    /// SELECT 1 *          ->  incomplete input
    /// SELECT 1 .          ->  near ".": syntax error
    /// SELECT * FROM t t2 t3;
    ///                     ->  near "t3": syntax error
    /// SELECT * FROM;      ->  near ";": syntax error
    /// SELECT * FROM       ->  incomplete input
    /// SELECT a FROM       ->  incomplete input
    /// ```
    ///
    /// A trailing `*` and a trailing `,` are both `incomplete input` while a
    /// trailing `.` is a named syntax error, so the answer is not "the token is
    /// punctuation" either. What separates them is where the *cursor* sits when
    /// the parser gives up: a `*` or a `,` has already been consumed as part of
    /// the grammar and its operand is what is missing, so the cursor is at the
    /// end of the input, while a `.` is a token the parser cannot start an
    /// operand with and names outright. The `;` is the one token that is
    /// genuinely there and genuinely unusable, and it is the one that decided
    /// `SELECT * FROM;` is `near ";"` where the same words without it are
    /// `incomplete input`.
    fn syntax_error_here(&self) -> Error {
        match self.peek() {
            // Past the end, or stopped on the terminator. Both are a statement
            // that ran out, and `SELECT * FROM;` is the one case where the
            // terminator itself is named: see `describe_token_here` on why a
            // `;` is the exception the general rule makes.
            None => msg::incomplete_input(),
            Some(Token::Punct(Punct::Semicolon)) => msg::syntax_error(&self.describe_token_here()),
            _ => msg::syntax_error(&self.describe_token_here()),
        }
    }

    /// What sqlite3 would name as the token it choked on at the current
    /// position.
    ///
    /// This is the same shape [`describe_token`] gives for a token already in
    /// hand, and it exists so both sites take the spelling the *statement*
    /// wrote rather than the token's folded form. `describe_token` on its own
    /// cannot: it is given a `Token`, and by the time a name is a `Token` the
    /// tokenizer has already folded it -- so `SELECT FROM t` came out
    /// `near "from"` where sqlite3 says `near "FROM"`.
    ///
    /// Punctuation is the one kind the token cannot answer for either, because
    /// the spelling is the punctuation: sqlite3 names `near ";"`, not
    /// `near "punctuation"`, and the span is where the character was written.
    fn describe_token_here(&self) -> String {
        match self.peek() {
            Some(Token::Identifier(_))
            | Some(Token::DoubleQuotedIdentifier(_))
            | Some(Token::Keyword(_)) => self
                .written_name(self.index)
                .unwrap_or_else(|| "end of input".to_string()),
            Some(Token::Punct(_)) => self
                .tokens
                .get(self.index)
                .map(|(_, span)| self.text_at(*span))
                .unwrap_or_default(),
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
    /// The index of the first token in a run of them, which for a statement
    /// that begins `CREATE` is the `CREATE` itself.
    ///
    /// The grammar reaches `CREATE TEMP TABLE` with the cursor two tokens past
    /// its `CREATE`, so a fixed step back is right for one spelling and wrong
    /// for the other; asking for the run's own start is right for both.
    fn create_keyword_index(&self) -> usize {
        self.index.saturating_sub(2)
    }

    fn quoted_name(&mut self, context: &str) -> Result<String> {
        let start = self.index;
        match self.peek() {
            Some(Token::String(s)) => {
                let s = s.clone();
                self.advance();
                Ok(s)
            }
            // The spelling the statement wrote, with the quotes stripped, which
            // is the same rule `name` follows: a `"..."` alias is spelled as it
            // was written. It matters for the result column a statement reports:
            // `SELECT 1 AS "Xy Zz"` is named `Xy Zz`, not `xy zz`.
            Some(Token::DoubleQuotedIdentifier(_)) => {
                let n = self.token_name_at(start);
                self.advance();
                Ok(n)
            }
            _ => self.name(context),
        }
    }

    /// The offset where the statement at the cursor ends.
    ///
    /// That is the end of the next semicolon that is not inside parentheses, or
    /// the end of the input, since a script is tokenised up front and the
    /// pragma and explain modules need a slice rather than a position.
    ///
    /// The semicolon is *included*, and that is the whole point of returning
    /// its end rather than its start. Both modules that re-parse this slice
    /// decide between two different error messages on the strength of it: a
    /// token the parser dislikes is `near "<token>": syntax error`, while an
    /// input that simply ran out is `incomplete input`. A `;` that was
    /// actually written is a token, so it is named, and only a slice that
    /// stopped because the input did is the other one:
    ///
    /// ```text
    /// PRAGMA;                near ";": syntax error      (input ran out after)
    /// PRAGMA page_size;      -- nothing to name
    /// ```
    ///
    /// Dropping the semicolon collapsed those two into one, and the collapse
    /// was silent: both arms reported `incomplete input` for input SQLite
    /// words differently. Measured on 3.53.4, `EXPLAIN;`, `EXPLAIN QUERY;`,
    /// `EXPLAIN QUERY PLAN;` and `PRAGMA;` are all `near ";": syntax error`
    /// while the same words with the semicolon left off are `incomplete input`.
    fn statement_end(&self) -> usize {
        let mut depth = 0i32;
        for (tok, span) in self.tokens.iter().skip(self.index) {
            match tok {
                Token::Punct(Punct::LParen) => depth += 1,
                Token::Punct(Punct::RParen) => depth -= 1,
                Token::Punct(Punct::Semicolon) if depth <= 0 => return span.end,
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
                self.index = self.index.saturating_sub(1);
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
        let start = self.index;
        match self.advance() {
            Some(Token::Identifier(n)) => Ok(self.written_name(start).unwrap_or(n)),
            Some(Token::Keyword(k)) => Ok(k.as_str().to_string()),
            _ => {
                self.index = self.index.saturating_sub(1);
                Err(self.unexpected(context))
            }
        }
    }

    /// A name read where a query may spell it any way it likes, so the message
    /// that has to echo it gets the spelling the statement used.
    ///
    /// Every read goes through [`Parser::name`], which folds a bare identifier
    /// because the catalog is keyed on the fold. This is the other half: the
    /// places whose name reaches a *message* want the fold undone. The two are
    /// not in conflict -- `SELECT * FROM Foo` finds no table (it is folded for
    /// the lookup and un-folded for the text) and says `no such table: Foo`.
    ///
    /// A *keyword* is never routed through here, and that is deliberate: a
    /// keyword is written one way, so its token spelling is the whole of it.
    /// `SELECT CAST(1 AS Integer)` reports its result column as
    /// `CAST(1 AS Integer)`, which is the source text and not a name, and the
    /// declared type is compared folded, so there is nothing to un-fold.
    fn query_name(&mut self, context: &str) -> Result<String> {
        let start = self.index;
        match self.advance() {
            Some(Token::Identifier(_)) | Some(Token::DoubleQuotedIdentifier(_)) => Ok(self
                .written_name(start)
                .unwrap_or_else(|| self.token_name_at(start))),
            Some(Token::Keyword(k)) => Ok(k.as_str().to_string()),
            _ => {
                self.index = self.index.saturating_sub(1);
                Err(self.unexpected(context))
            }
        }
    }

    /// The token's own spelling, for a name whose source text is unusable.
    fn token_name_at(&self, pos: usize) -> String {
        match self.tokens.get(pos).map(|(t, _)| t) {
            Some(Token::Identifier(n)) | Some(Token::DoubleQuotedIdentifier(n)) => n.clone(),
            Some(Token::Keyword(k)) => k.as_str().to_string(),
            _ => String::new(),
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

    /// Whether the token at the cursor is a double-quoted name.
    ///
    /// A `"..."` is an identifier everywhere a name is read, exactly like the
    /// bracketed and back-quoted forms, so the grammar asks about it in the
    /// same places -- but not with a `Token::Identifier` pattern, because that
    /// would hide the quoting the `no such column` message is about.
    fn at_double_quoted(&self) -> bool {
        self.at_double_quoted_at(self.index)
    }

    /// Whether the token at `pos` is a double-quoted name.
    pub fn at_double_quoted_at(&self, pos: usize) -> bool {
        matches!(
            self.tokens.get(pos).map(|(t, _)| t),
            Some(Token::DoubleQuotedIdentifier(_))
        )
    }

    /// Whether the token at `pos` was a double-quoted name.
    ///
    /// SQLite treats a `"..."` that resolves to nothing as a mistake about
    /// quoting rather than about the name, and says so:
    /// `no such column: "a+b" - should this be a string literal in
    /// single-quotes?`. The same name in brackets or backticks is a plain
    /// `no such column: a+b`, so the quoting -- not the name -- is what the
    /// message turns on, and the resolver that raises the message has to be
    /// able to ask.
    pub fn is_double_quoted(&self, pos: usize) -> bool {
        matches!(
            self.tokens.get(pos).map(|(t, _)| t),
            Some(Token::DoubleQuotedIdentifier(_))
        )
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
                // match self.peek() did not advance, so self.index is still
                // pointing AT the keyword. Its span is where the text starts.
                let from = self
                    .tokens
                    .get(self.index)
                    .map(|(_, s)| s.start)
                    .unwrap_or(0);
                // The pragma module re-parses the text, so it has to be this
                // statement's text and not the whole script: a script is
                // tokenised up front, and handing over the rest of it would
                // make the pragma swallow the statements that follow.
                let to = self.statement_end();
                let text = self.sql.get(from..to).unwrap_or("").to_string();
                let stmt = crate::pragma::parse_pragma(&text)?;
                // The tokens for this statement still have to be consumed, or
                // the script loop would see the same PRAGMA for ever.
                while self.index < self.tokens.len() && self.tokens[self.index].1.start < to {
                    self.index += 1;
                }
                return Ok(Stmt::Pragma(stmt));
            }
            Some(Token::Keyword(Keyword::Pragma)) => {
                // The pragma module owns this grammar, which does not fit the
                // expression one. It re-parses the text, so it gets this
                // statement's slice rather than the whole script: a script is
                // tokenised up front and handing over the rest would make the
                // pragma swallow the statements that follow.
                let from = self
                    .tokens
                    .get(self.index)
                    .map(|(_, s)| s.start)
                    .unwrap_or(0);
                let to = self.statement_end();
                let text = self.sql.get(from..to).unwrap_or("").to_string();
                let stmt = crate::pragma::parse_pragma(&text)?;
                // The tokens still have to be consumed, or the script loop would
                // see the same PRAGMA for ever.
                while self.index < self.tokens.len() && self.tokens[self.index].1.start < to {
                    self.index += 1;
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
            Some(Token::Keyword(Keyword::Alter)) => self.alter(),
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
        // script loop would see the same EXPLAIN for ever. `<` and not `<=`:
        // the slice ends *after* the semicolon, so every token in it starts
        // strictly before that offset. Comparing with `<=` would swallow the
        // token after the terminator as well, which for `EXPLAIN SELECT 1;
        // SELECT 2` is the `SELECT` that opens the next statement.
        while self.index < self.tokens.len() && self.tokens[self.index].1.start < to {
            self.index += 1;
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
            let name = self.quoted_name("parsing a CTE name")?;
            let mut columns = Vec::new();
            if self.eat_punct(Punct::LParen)? {
                loop {
                    columns.push(self.quoted_name("parsing a CTE column name")?);
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
            Some(self.resolve_expr()?)
        } else {
            None
        };
        let group_by = self.group_by_clause()?;
        let having = if self.eat_keyword(Keyword::Having)? {
            Some(self.resolve_expr()?)
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
                .get(self.index.saturating_sub(1))
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
                || self.at_double_quoted()
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
        let save = self.index;
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
            self.index = save;
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
            return Ok((Some(self.resolve_expr()?), Vec::new()));
        }
        if self.eat_keyword(Keyword::Using)? {
            self.expect_punct(Punct::LParen, "after USING")?;
            let mut cols = Vec::new();
            loop {
                cols.push(self.quoted_name("in a USING clause")?);
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
        // The name is not optional, but an absent one is not a `near` message
        // either: sqlite3 stops at the clause keyword and says the input ran
        // out, so `SELECT * FROM` and `SELECT * FROM;` differ only in the
        // terminator while both blame the input rather than a token. The check
        // has to be the *keyword* and not the token, because `SELECT * FROM
        // where;` is `near "where": syntax error` -- the word is not a name
        // there either, but a token of it is a real one the parser can go on
        // to use.
        if self.at_keyword(Keyword::From) || self.at_keyword(Keyword::Where) {
            return Err(msg::incomplete_input());
        }
        let name = self.query_name("after FROM")?;
        // A qualified name is written schema.table, which the engine treats as
        // the table part with the schema ignored for now.
        let mut full = name.clone();
        while self.eat_punct(Punct::Dot)? {
            full.push('.');
            full.push_str(&self.query_name("after a table qualifier")?);
        }
        let alias = self.optional_alias()?;
        let indexed_by = if self.eat_keyword(Keyword::Indexed)? {
            self.eat_keyword(Keyword::By)?;
            Some(self.quoted_name("after INDEXED BY")?)
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
        // A result alias is the one name a statement may spell in any case and
        // still mean the same column: `SELECT b AS bb ... ORDER BY BB` reads
        // the alias, and both spellings have to reach the match for it to be
        // found. An alias is matched, never echoed, so it keeps the fold like
        // every other name in the schema -- which is the opposite of
        // `query_name` and the reason the two are not the same function.
        if self.eat_keyword(Keyword::As)? {
            return Ok(Some(self.quoted_name("after AS")?));
        }
        // `OUTER` is a valid alias but also the second word of a join type, so
        // `a outer JOIN b` is a join and `a outer` is an alias. A `JOIN` after
        // the word is what tells them apart, so the alias is only taken when
        // none follows.
        if self.at_keyword(Keyword::Outer) && self.token_after_is(Keyword::Join) {
            return Ok(None);
        }
        match self.peek() {
            // A `"..."` is an alias like any other, and is spelled as it was
            // written: `SELECT 1 AS a FROM t AS "Xy"` names the table `Xy`.
            Some(Token::Identifier(_)) | Some(Token::DoubleQuotedIdentifier(_)) => Ok(Some(
                self.advance()
                    .and_then(|t| match t {
                        Token::Identifier(n) | Token::DoubleQuotedIdentifier(n) => Some(n),
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
        matches!(self.tokens.get(self.index + 1), Some((Token::Keyword(k), _)) if *k == kw)
    }

    fn group_by_clause(&mut self) -> Result<Vec<Expr>> {
        let mut out = Vec::new();
        if !self.eat_keyword(Keyword::Group)? {
            return Ok(out);
        }
        self.eat_keyword(Keyword::By)?;
        loop {
            out.push(self.resolve_expr()?);
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
            let expr = self.resolve_expr()?;
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

    /// The whole of a resolution context: the qualified name or function call
    /// SQLite looks up before the expression grammar is consulted, or `None` to
    /// say the expression grammar is the only way in.
    ///
    /// The seven callers are the seven places SQLite resolves a name before it
    /// parses an expression -- the SELECT list, WHERE, GROUP BY, ORDER BY,
    /// HAVING, a join's ON, and the second WHERE -- and in all of them a name is
    /// the thing the statement wrote, matched against a list rather than
    /// resolved by the expression grammar. The distinction matters because the
    /// two routes spell a name differently: `expr_primary` gives a bare column
    /// reference the statement's spelling, because a `no such column` message
    /// echoes it, and it gives a *function call* that spelling too
    /// (`no such function: XYZZY`). But the qualified reference `expr_primary`
    /// builds is a different shape: its qualifier is read by `self::name`, which
    /// folds, and a message for one is the folded form -- which is what
    /// `SELECT 1 FROM t ORDER BY t.NOSUCHCOL` says. So the head of a name in a
    /// resolution context is read here, where the fold is right, and everything
    /// else is left to the expression grammar.
    fn lookup_name(&mut self) -> Result<Option<Expr>> {
        let span = self.span();
        let Some(first) = self.peek_token()? else {
            return Ok(None);
        };
        let named = matches!(
            first,
            Token::Identifier(_) | Token::DoubleQuotedIdentifier(_) | Token::Keyword(_)
        );
        // The token *after* the head decides which of the two shapes this is.
        let after = self.peek_at(self.index + 1);
        if !named {
            return Ok(None);
        }
        match after {
            Some(Token::Punct(Punct::Dot)) => {
                // `t.x`: the qualifier, the dot, then the column -- the same
                // three reads `after_identifier` makes, and for the same reason.
                // The qualifier is matched on its fold while the column keeps the
                // spelling the statement wrote, so `ORDER BY t.NOSUCHCOL` says
                // what `SELECT t.NOSUCHCOL` says.
                //
                // Nothing has been consumed yet, so the head is the token *at*
                // the cursor and reading it leaves the cursor on the dot. The
                // dot is then consumed in its own right. Advancing past the head
                // first instead would leave the cursor sitting on the dot, and
                // the read after it would try to make a name of the dot and say
                // `near ".": syntax error`.
                let table = self.quoted_name("at the head of a qualified name")?;
                self.advance();
                if self.eat_punct(Punct::Star)? {
                    return Ok(Some(Expr::Function {
                        name: format!("{table}.*"),
                        args: vec![],
                        star: true,
                        distinct: false,
                    }));
                }
                let column = self.query_name("after a column qualifier")?;
                // A third part makes it `schema.table.column`: the two read so
                // far are the schema and the table, and the column comes last.
                if self.at_punct(Punct::Dot) {
                    self.advance();
                    let second = self.query_name("after a schema qualifier")?;
                    return Ok(Some(Expr::Column {
                        table: Some(format!("{table}.{column}")),
                        name: second,
                        span,
                    }));
                }
                Ok(Some(Expr::Column {
                    table: Some(table),
                    name: column,
                    span,
                }))
            }
            Some(Token::Punct(Punct::LParen)) => {
                // `f(`, and the call's name is echoed by `no such function` and
                // by `wrong number of arguments to function f()`, so it is read
                // the way `after_identifier` reads a call's name -- by the span
                // the head was written with, which is the token at the cursor
                // because nothing has been consumed yet. Reading the *next*
                // token instead would take the open paren's span and name the
                // call `(`, which is not a name the statement wrote.
                let start = self.index;
                let name = self
                    .written_name(start)
                    .unwrap_or_else(|| self.token_name_at(start));
                self.advance();
                Ok(Some(self.function_call(name, span)?))
            }
            _ => Ok(None),
        }
    }

    /// An expression in one of the resolution contexts, parsing the whole of it.
    ///
    /// [`Parser::lookup_name`] reads a leading qualified name or function call
    /// so that the name it echoes is the statement's spelling, but a name in one
    /// of these positions is only the *first operand*: `ON a.x = b.x` and `WHERE
    /// t.x IS NULL` are both expressions, and handing back the bare name would
    /// leave the operator and everything after it unread, so the statement would
    /// fail on the operator with `near "=": syntax error`.
    ///
    /// So the name seeds the expression and the grammar carries on from it. The
    /// seed goes in at the additive level rather than at the top, because that
    /// is the innermost level that still lets `=`, `<`, `IS NULL` and the
    /// bitwise operators bind it -- the same shape `expr_primary` has when it
    /// is reached the ordinary way.
    fn resolve_expr(&mut self) -> Result<Expr> {
        let Some(seed) = self.lookup_name()? else {
            return self.expr();
        };
        let seed = self.expr_additive_tail(seed)?;
        let seed = self.expr_bitwise_tail(seed)?;
        self.expr_comparison_tail(seed)
    }

    /// The rest of [`Parser::expr_additive`], for an operand already in hand.
    fn expr_additive_tail(&mut self, mut left: Expr) -> Result<Expr> {
        loop {
            let op = if self.eat_punct(Punct::Plus)? {
                BinOp::Add
            } else if self.eat_punct(Punct::Minus)? {
                BinOp::Sub
            } else if self.eat_punct(Punct::Concat)? {
                BinOp::Concat
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
        Ok(left)
    }

    /// The rest of [`Parser::expr_bitwise`], for an operand already in hand.
    fn expr_bitwise_tail(&mut self, mut left: Expr) -> Result<Expr> {
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

    /// The rest of [`Parser::expr_comparison`], for an operand already in hand.
    fn expr_comparison_tail(&mut self, left: Expr) -> Result<Expr> {
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
        self.postfix_predicates(combined)
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
                    self.tokens.get(self.index + 1).map(|(t, _)| t),
                    Some(Token::Keyword(Keyword::In))
                        | Some(Token::Keyword(Keyword::Like))
                        | Some(Token::Keyword(Keyword::Glob))
                        | Some(Token::Keyword(Keyword::Regexp))
                        | Some(Token::Keyword(Keyword::Match))
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
            if self.eat_keyword(Keyword::Match)? {
                // MEASURED on 3.53.4: the right operand of MATCH is a full
                // `expr`, not a bitwise one, and it is greedy -- `'a' MATCH 'b'
                // 'c'` is accepted and is still the one MATCH, so the trailing
                // 'c' is not a syntax error. `expr_bitwise` stops before AND
                // and OR, which is the same right operand LIKE gets.
                let pattern = self.expr_bitwise()?;
                left = Expr::Match {
                    expr: Box::new(left),
                    pattern: Box::new(pattern),
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
            // 2^63 is the one integer that only fits once it is negative, so
            // the sign decides. sqlite3: typeof(-9223372036854775808) is
            // integer and typeof(9223372036854775808) is real.
            if let Expr::Literal(Literal::Big(v)) = inner {
                let as_i64 = (v as i128) as i64;
                return Ok(Expr::Literal(Literal::Integer(as_i64)));
            }
            // A negated literal folds immediately, which is what SQLite does --
            // but the most negative integer has no positive counterpart, so
            // folding it wraps back to itself. sqlite3 answers
            // -(-9223372036854775808) with a real, and the fold has to agree.
            if let Expr::Literal(Literal::Integer(i)) = inner {
                return Ok(match i.checked_neg() {
                    Some(v) => Expr::Literal(Literal::Integer(v)),
                    None => Expr::Literal(Literal::Real(-(i as f64))),
                });
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
        let start = self.index;
        match self.advance() {
            Some(Token::Integer(i)) => Ok(Expr::Literal(Literal::Integer(i))),
            // 2^63 reaches here unnegated too, and is a real in that position.
            Some(Token::BigInt(v)) => Ok(Expr::Literal(Literal::Big(v))),
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
            Some(Token::Identifier(name)) | Some(Token::DoubleQuotedIdentifier(name)) => {
                // A bare column reference carries the spelling the statement
                // wrote, because a `no such column` message echoes the name that
                // did not resolve rather than the token: sqlite3 answers
                // `SELECT BadCol` with `no such column: BadCol` and
                // `SELECT 1 FROM t ORDER BY NOSUCHCOL` with
                // `no such column: NOSUCHCOL`. A function call is already read
                // this way just below, for the same reason and the same
                // sentence, so the two are the same rule. The fold is still
                // what every lookup matches on, so nothing downstream is
                // affected.
                let name = self.written_name(start).unwrap_or(name);
                self.after_identifier(name, span)
            }
            Some(Token::Keyword(kw)) => self.after_keyword(kw, span),
            _ => {
                self.index = self.index.saturating_sub(1);
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
            let name = self.written_name(self.index - 1).unwrap_or(name);
            return self.function_call(name, span);
        }
        // A qualified column is `table.column` or `schema.table.column`.
        if self.at_punct(Punct::Dot) {
            // The identifier this function was handed has already been
            // consumed, so the qualifier is the *last* token read rather than
            // the one after it. `name` is the folded token spelling, which is
            // what a lookup wants and the wrong thing to echo.
            let mut qualifier = self.written_name(self.index - 1).unwrap_or(name);
            self.advance();
            if self.eat_punct(Punct::Star)? {
                return Ok(Expr::Function {
                    name: format!("{qualifier}.*"),
                    args: vec![],
                    star: true,
                    distinct: false,
                });
            }
            let column = self.query_name("after a column qualifier")?;
            if self.at_punct(Punct::Dot) {
                // The name read so far is the *column*, not the table, so a
                // third part is the table and the part already in hand has to
                // move to the end. `SELECT main.Foo.x` names the column `x` of
                // table `Foo` in schema `main`, and this used to read it as
                // the table `main.Foo` and the column `x`.
                self.advance();
                let table = self.query_name("after a schema qualifier")?;
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
        //
        // Whether the operand is there is decided by *looking ahead* for the
        // `WHEN`, not by asking whether the next token can begin an expression.
        // The two disagree exactly where it matters: `end` is a keyword SQLite
        // allows as a column name, so `SELECT CASE END;` parses `END` as the
        // operand and the failure comes one token later, at the `;` -- which is
        // the message sqlite3 gives. Deciding it the other way round is what
        // made this one `near "end"` where the oracle says `near ";"`. Only
        // `WHEN` itself ends the operand, and the test is spelled `WHEN` because
        // sqlite3 takes the word, not the token: `SELECT CASE "WHEN";` is
        // `no such column: WHEN` and names the quoted name, not the keyword.
        let operand = if self.spells_keyword_ahead(Keyword::When) {
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
            // sqlite3 stops at the token *after* the last WHEN, not at the word
            // `end`: `SELECT CASE END;` is `near ";": syntax error`, where the
            // `end` is still perfectly good -- it is only useless without a WHEN
            // arm, and the parser has to have consumed one before it can reach
            // the END. So the message names whatever the cursor is on, which is
            // the semicolon there and the word that was meant to be a WHEN here
            // (`SELECT CASE WHEN THEN 1 END;` is `near "THEN": syntax error`).
            // The clause this used to raise is not a format string in the
            // binary at all, so there is no text to match it against.
            return Err(self.syntax_error_here());
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
        // OR IGNORE / OR REPLACE / OR ABORT / OR FAIL / OR ROLLBACK. The
        // leading `REPLACE` is consumed by the `advance()` above and cannot be
        // told from `INSERT` here, so `REPLACE INTO t VALUES(1)` is
        // indistinguishable from `INSERT INTO t VALUES(1)` -- which is how the
        // grammar has always read it. `OR REPLACE` does reach this arm and is
        // not lost.
        let mut conflict = ConflictAction::Abort;
        if self.eat_keyword(Keyword::Or)? {
            conflict = self.conflict_action();
        }
        // INTO is optional.
        self.eat_keyword(Keyword::Into)?;
        let table = self.query_name("after INTO")?;
        let mut full = table.clone();
        while self.eat_punct(Punct::Dot)? {
            full.push('.');
            full.push_str(&self.query_name("after a table qualifier")?);
        }
        let columns = if self.eat_punct(Punct::LParen)? {
            let mut names = Vec::new();
            loop {
                names.push(self.quoted_name("in a column list")?);
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
            conflict,
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
        let mut conflict = ConflictAction::Abort;
        if self.eat_keyword(Keyword::Or)? {
            conflict = self.conflict_action();
        }
        let table = self.query_name("after UPDATE")?;
        let mut full = table;
        while self.eat_punct(Punct::Dot)? {
            full.push('.');
            full.push_str(&self.query_name("after a table qualifier")?);
        }
        self.expect_keyword(Keyword::Set, "after a table name")?;
        let mut sets = Vec::new();
        loop {
            let column = self.quoted_name("in a SET clause")?;
            // An optional table qualifier on the target column.
            if self.at_punct(Punct::Dot) {
                self.advance();
                self.quoted_name("after a column qualifier")?;
            }
            self.expect_punct(Punct::Eq, "in a SET clause")?;
            let value = self.expr()?;
            sets.push((column, value));
            if !self.eat_punct(Punct::Comma)? {
                break;
            }
        }
        let where_ = if self.eat_keyword(Keyword::Where)? {
            Some(self.resolve_expr()?)
        } else {
            None
        };
        Ok(Stmt::Update {
            table: full,
            sets,
            where_,
            conflict,
        })
    }

    fn delete(&mut self) -> Result<Stmt> {
        self.advance();
        self.expect_keyword(Keyword::From, "after DELETE")?;
        let table = self.query_name("after FROM")?;
        let mut full = table;
        while self.eat_punct(Punct::Dot)? {
            full.push('.');
            full.push_str(&self.query_name("after a table qualifier")?);
        }
        let where_ = if self.eat_keyword(Keyword::Where)? {
            Some(self.resolve_expr()?)
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
        // `CREATE UNIQUE INDEX` must NOT eat the UNIQUE here. It is
        // `create_index` that reads it, so that the flag it records is the one
        // the statement actually carried: consuming it twice left every UNIQUE
        // index recorded as non-unique, and nothing then enforced it.
        if self.at_keyword(Keyword::Unique) || self.at_keyword(Keyword::Index) {
            return self.create_index();
        }
        if self.eat_keyword(Keyword::View)? {
            let name = self.quoted_name("after CREATE VIEW")?;
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
        if self.at_keyword(Keyword::Virtual) {
            return self.create_virtual_table();
        }
        // CREATE TRIGGER, and the rest.
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
            .get(self.create_keyword_index())
            .map(|(t, s)| match t {
                // `CREATE TABLE`: the caller consumed `TABLE` and this is
                // still on `CREATE`.
                Token::Keyword(Keyword::Create) => s.start,
                // `CREATE TEMP TABLE`: two tokens were consumed, so counting
                // back two lands on `TEMP` and the text would start there.
                // The statement's own first token is the `CREATE`.
                _ => self.statement_start_offset(),
            });
        let if_not_exists = self.if_not_exists()?;
        let name = self.query_name("after CREATE TABLE")?;
        let mut full = name;
        while self.eat_punct(Punct::Dot)? {
            full.push('.');
            full.push_str(&self.query_name("after a table qualifier")?);
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
        // A `DEFAULT (name)` is refused here rather than in the column
        // grammar, and the difference is only in where it can be seen. On the
        // CREATE path the grammar *is* the whole answer -- a parse error
        // cannot be overridden -- so this is raised before the statement is
        // handed on. Measured on 3.53.4: `CREATE TABLE t(a,b DEFAULT (a))` is
        // `default value of column [b] is not constant`, and so is
        // `DEFAULT (+a)` and `DEFAULT (a COLLATE nocase)`.
        if let Some(column) = self.default_name_reference.take() {
            return Err(msg::cannot_add::default_not_constant(&column));
        }
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
            .get(self.index.saturating_sub(1))
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

    /// `ALTER TABLE`, of which this engine runs `ADD COLUMN`.
    ///
    /// `ADD` and `ADD COLUMN` are one clause: measured against sqlite3
    /// 3.53.4, `ALTER TABLE t ADD c` stores `CREATE TABLE t(a,b, c)` exactly
    /// as `ALTER TABLE t ADD COLUMN c` does, so the `COLUMN` word is optional
    /// and is not part of the text that gets spliced either way.
    ///
    /// `RENAME TO` and `DROP COLUMN` are not this, and neither is a bare
    /// `ALTER TABLE t`. All three are refused as [`Stmt::Unsupported`] with the
    /// tokens consumed up to the semicolon, so they keep answering `rename is
    /// not supported yet` rather than regressing into the `near "ALTER":
    /// syntax error` the statement dispatcher used to hand back for the whole of
    /// `ALTER`.
    ///
    /// Only the two ALTER shapes that name an unimplemented *feature* take that
    /// channel. A statement that is merely malformed is a syntax error, and
    /// keeping the two apart is the point: measured on 3.53.4,
    /// `ALTER TABLE t RENAME TO z2` is a real statement this engine does not
    /// run, while `ALTER TABLE t` with no clause and `ALTER TABLE t XYZZY` are
    /// not statements at all and are `near ";": syntax error` and
    /// `near "XYZZY": syntax error`.
    fn alter(&mut self) -> Result<Stmt> {
        self.expect_keyword(Keyword::Alter, "after a statement keyword")?;
        self.expect_keyword(Keyword::Table, "after ALTER")?;
        let mut name = self.query_name("after ALTER TABLE")?;
        while self.eat_punct(Punct::Dot)? {
            name.push('.');
            name.push_str(&self.query_name("after a table qualifier")?);
        }
        if !self.at_keyword(Keyword::Add) {
            // RENAME and DROP COLUMN land here. The keyword goes into the
            // message, so `ALTER TABLE t RENAME TO z2` says `rename is not
            // supported yet` -- the same refusal a `DROP INDEX` gets, rather
            // than a syntax error about a statement this engine never
            // implemented.
            let what = match self.peek() {
                // These two *are* ALTER clauses SQLite defines, and they are the
                // ones this engine has not written.
                Some(Token::Keyword(Keyword::Rename)) | Some(Token::Keyword(Keyword::Drop)) => {
                    let k = self.peek().and_then(|t| match t {
                        Token::Keyword(k) => Some(k.as_str().to_string()),
                        _ => None,
                    });
                    k.unwrap_or_else(|| "alter".to_string())
                }
                // Anything else is a token where an ALTER clause belongs, which
                // is a syntax error rather than a missing feature -- including
                // the end of the statement, which is `near ";": syntax error`
                // and not an `incomplete input`, measured.
                _ => {
                    let span = self.span();
                    self.skip_to_semicolon()?;
                    return Err(msg::syntax_error(&self.text_at(span)));
                }
            };
            self.skip_to_semicolon()?;
            return Ok(Stmt::Unsupported(what));
        }
        self.advance();
        // COLUMN is the optional word, not a second thing to require.
        self.eat_keyword(Keyword::Column)?;

        // The column's own text runs from the first token of its name to the
        // end of its last constraint, and it is recorded by span before the
        // parse so the text is the statement's spelling and not a
        // reconstruction. `column_def` stops at the token it cannot use, which
        // is the `;` here and is a comma in a script that has one.
        let start = self
            .tokens
            .get(self.index)
            .map(|(_, s)| s.start)
            .unwrap_or(0);
        let column = self.column_def()?;
        let end = self
            .tokens
            .get(self.index.saturating_sub(1))
            .map(|(_, s)| s.end)
            .unwrap_or(start);
        let column_sql = self.sql.get(start..end).unwrap_or("").trim().to_string();
        // `column_def` stops at a token it cannot use, and a token that is not
        // the end of the statement means the statement was not this one:
        // `DEFAULT 1+2` is `near "+": syntax error`, not an ADD COLUMN whose
        // default is `1`. Consuming the tail here would swallow that.
        if self.peek().is_some() && !self.eat_punct(Punct::Semicolon)? {
            return Err(self.syntax_error_here());
        }
        Ok(Stmt::AlterTableAddColumn {
            name,
            default_name_reference: self.default_name_reference.take(),
            column,
            column_sql,
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
        let name = self.quoted_name("parsing a column name")?;
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
                    Some(Token::Identifier(n)) | Some(Token::DoubleQuotedIdentifier(n)) => {
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
            if let Some(c) = self.column_constraint(&name)? {
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
            // A declared type may be written in any of the four quotings, and a
            // `"..."` one is read here rather than refused: `c "Weird Type"`
            // is a column c whose type is `Weird Type`.
            Some(Token::Identifier(n)) | Some(Token::DoubleQuotedIdentifier(n)) => {
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

    /// Reads the `COLLATE name` that may sit on a `DEFAULT`, returning the
    /// expression with it attached.
    ///
    /// The `COLLATE` is the last thing an unparenthesised `DEFAULT` can be
    /// followed by, so this runs once and takes whatever it finds; a second
    /// `COLLATE` is left for the caller to refuse as the syntax error it is.
    fn eat_default_collation(&mut self, e: Expr) -> Result<Expr> {
        if !self.eat_keyword(Keyword::Collate)? {
            return Ok(e);
        }
        let collation = self.name("after COLLATE")?;
        Ok(Expr::Collate {
            expr: Box::new(e),
            collation,
        })
    }

    /// Reads one constraint on a column, or `None` when the next token does
    /// not start one.
    ///
    /// `column` is the name the column was declared under, which is the name
    /// the one refusal here has to report. See the `DEFAULT` arm.
    fn column_constraint(&mut self, column: &str) -> Result<Option<Constraint>> {
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
                columns: Vec::new(),
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
            return Ok(Some(Constraint::Unique { columns: Vec::new() }));
        }
        if self.eat_keyword(Keyword::Check)? {
            self.expect_punct(Punct::LParen, "after CHECK")?;
            let start = self.index;
            let e = self.expr()?;
            let text = self.text_between(start, self.index);
            self.expect_punct(Punct::RParen, "closing CHECK")?;
            let checked = Constraint::Check { expr: e, text };
            return Ok(Some(checked));
        }
        if self.eat_keyword(Keyword::Default)? {
            // A parenthesised default is SQLite's expression form, and it is
            // kept as that rather than reduced to a NULL the way the
            // parenthesised case used to be: a `DEFAULT (a)` has to be told
            // apart from a `DEFAULT a`, and the two are not the same value.
            // See `is_builtin_constant_default` for what SQLite accepts and
            // `is_a_name_reference` for the spelling it refuses.
            if self.eat_punct(Punct::LParen)? {
                let e = self.expr()?;
                self.expect_punct(Punct::RParen, "closing a DEFAULT expression")?;
                // Recorded rather than raised: see
                // [`Parser::default_name_reference`] for why the two callers
                // that reach here answer differently.
                if is_a_name_reference(&e) {
                    self.default_name_reference = Some(column.to_string());
                }
                return Ok(Some(Constraint::Default {
                    expr: e,
                    parenthesized: true,
                }));
            }
            // Without the parens SQLite's grammar does not read a default as an
            // expression at all: it reads a literal, a signed literal, or a
            // collation applied to either. The measured width of that is
            // `literal +/- COLLATE name`, and anything past it is a syntax
            // error at the token that ends it -- `DEFAULT 1+2` is `near "+"`,
            // `DEFAULT abs(1)` is `near "("` -- so the expression stops here
            // rather than taking a run of operators into the next column's
            // definition.
            let e = self.expr_unary()?;
            let e = self.eat_default_collation(e)?;
            return Ok(Some(Constraint::Default {
                expr: e,
                parenthesized: false,
            }));
        }
        if self.eat_keyword(Keyword::Collate)? {
            let c = self.name("after COLLATE")?;
            return Ok(Some(Constraint::Collate(c)));
        }
        if self.eat_keyword(Keyword::References)? {
            let table = self.quoted_name("after REFERENCES")?;
            let mut columns = Vec::new();
            if self.eat_punct(Punct::LParen)? {
                loop {
                    columns.push(self.quoted_name("in a foreign key column list")?);
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
            self.quoted_name("after CONSTRAINT")?;
            return self.column_constraint(column);
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
                self.quoted_name("after MATCH")?;
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
            self.quoted_name("after CONSTRAINT")?;
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
            let columns = self.indexed_column_list()?;
            return Ok(Constraint::PrimaryKey {
                ascending,
                autoincrement,
                columns,
            });
        }
        if self.eat_keyword(Keyword::Unique)? {
            if self.eat_keyword(Keyword::On)? {
                self.eat_keyword(Keyword::Conflict)?;
                self.advance();
            }
            self.expect_punct(Punct::LParen, "after UNIQUE")?;
            let columns = self.indexed_column_list()?;
            return Ok(Constraint::Unique { columns });
        }
        if self.eat_keyword(Keyword::Check)? {
            self.expect_punct(Punct::LParen, "after CHECK")?;
            let start = self.index;
            let e = self.expr()?;
            let text = self.text_between(start, self.index);
            self.expect_punct(Punct::RParen, "closing CHECK")?;
            let checked = Constraint::Check { expr: e, text };
            return Ok(checked);
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
            let table = self.quoted_name("after REFERENCES")?;
            let mut columns = Vec::new();
            if self.eat_punct(Punct::LParen)? {
                loop {
                    columns.push(self.quoted_name("in a foreign key column list")?);
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

    /// The word naming a conflict action, with the cursor left on it.
    ///
    /// A word that is not one of the five is a syntax error in the reference, but
    /// this parser's job here is only to tell the actions apart, and ABORT is
    /// what an absent clause means anyway -- so an unrecognised one advances and
    /// reads as ABORT rather than refusing a statement the old parser accepted.
    fn conflict_action(&mut self) -> ConflictAction {
        let action = match self.peek() {
            Some(Token::Keyword(Keyword::Ignore)) => ConflictAction::Ignore,
            Some(Token::Keyword(Keyword::Replace)) => ConflictAction::Replace,
            Some(Token::Keyword(Keyword::Fail)) => ConflictAction::Fail,
            Some(Token::Keyword(Keyword::Rollback)) => ConflictAction::Rollback,            // ABORT is a keyword, and the default arm covers it and anything
            // else. Falling through to ABORT rather than erroring keeps every
            // statement the old parser took still parsing.
            _ => ConflictAction::Abort,
        };
        self.advance();
        action
    }

    /// The comma-separated column names inside a table constraint's parentheses,
    /// as in `UNIQUE(a,b)` or `PRIMARY KEY(a,b)`.
    ///
    /// This list used to be swallowed token by token, which made a multi-column
    /// constraint unenforceable: the catalog is told a key exists but not which
    /// columns it is over. Only the names are taken. A per-column `COLLATE` or
    /// `ASC`/`DESC` is left where it is, which is not a column name and not a
    /// comma, so it ends the list and a following `)` is still in the right
    /// place. A collation on a unique key is a real thing sqlite3 honours and
    /// this does not, so `UNIQUE(a COLLATE NOCASE)` lands here as just `a` --
    /// a key over the right column, compared the way this engine compares
    /// every key. That is the same case the write path already has for
    /// `CREATE UNIQUE INDEX ... COLLATE`, and it is narrower than getting the
    /// common case wrong.
    fn indexed_column_list(&mut self) -> Result<Vec<String>> {
        let mut columns = Vec::new();
        loop {
            columns.push(self.quoted_name("in a constraint column list")?);
            if !self.eat_punct(Punct::Comma)? {
                break;
            }
        }
        self.expect_punct(Punct::RParen, "closing a constraint column list")?;
        Ok(columns)
    }

    /// `CREATE VIRTUAL TABLE name USING module(args)`.
    ///
    /// The statement's own text starts at CREATE, exactly as `create_index`
    /// finds it, and for the same reason: by the time this runs the cursor is
    /// already past CREATE, VIRTUAL and TABLE, so the keyword is looked for
    /// backwards rather than remembered.
    fn create_virtual_table(&mut self) -> Result<Stmt> {
        let start = self.tokens[..self.index]
            .iter()
            .rposition(|(t, _)| matches!(t, Token::Keyword(Keyword::Create)))
            .map(|i| self.tokens[i].1.start);
        self.expect_keyword(Keyword::Virtual, "after CREATE")?;
        self.expect_keyword(Keyword::Table, "after CREATE VIRTUAL")?;
        let name = self.quoted_name("after CREATE VIRTUAL TABLE")?;
        self.expect_keyword(Keyword::Using, "after a virtual table name")?;
        let module = self.quoted_name("after USING")?;
        // The argument list is taken VERBATIM, parentheses and all, by
        // recording where it starts and where the matching close parenthesis
        // ends. The module owns that grammar -- `Schema::parse` strips the
        // parentheses itself -- so re-serialising a token list here would lose
        // both the original spacing and any quoting a module needs. The depth
        // counter is what makes a nested list or a function call inside the
        // arguments not end the capture early.
        let args = match self.peek() {
            Some(Token::Punct(Punct::LParen)) => {
                let open = self.tokens[self.index].1.start;
                let mut depth = 0i32;
                loop {
                    match self.advance() {
                        Some(Token::Punct(Punct::LParen)) => depth += 1,
                        Some(Token::Punct(Punct::RParen)) => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        Some(_) => {}
                        None => {
                            return Err(Error::new(
                                ResultCode::Error,
                                "unterminated argument list after USING",
                            ))
                        }
                    }
                }
                // The close parenthesis's own end, taken from the token the
                // cursor stopped on -- the same span `create_index` uses to
                // find where its statement ends.
                let to = self
                    .tokens
                    .get(self.index.saturating_sub(1))
                    .map(|(_, s)| s.end)
                    .unwrap_or(open);
                Some(self.sql.get(open..to.max(open)).unwrap_or_default().to_string())
            }
            _ => None,
        };
        // Anything after the list -- the options some modules take -- belongs
        // to the module too, so it is swept up rather than dropped.
        self.skip_to_semicolon()?;
        // The statement's own text, from CREATE to the last token of it. The
        // script is tokenised up front, so the end is where the previous token
        // ended rather than where the next one starts -- `create_index` reads
        // it the same way, and `sqlite_schema` stores the result verbatim.
        let sql = match start {
            Some(a) => {
                let to = self
                    .tokens
                    .get(self.index.saturating_sub(1))
                    .map(|(_, s)| s.end)
                    .unwrap_or(a);
                self.sql.get(a..to.max(a)).unwrap_or_default().to_string()
            }
            None => String::new(),
        };
        Ok(Stmt::CreateVirtualTable {
            name,
            module,
            // A module with no argument list gets an empty pair of parentheses
            // rather than nothing, so `Schema::parse` reports its own
            // "expected a parenthesised list" message instead of this layer
            // inventing one.
            args: args.unwrap_or_else(|| "()".to_string()),
            sql,
        })
    }

    fn create_index(&mut self) -> Result<Stmt> {
        // The statement's own text starts at CREATE. By the time this runs the
        // cursor has moved past CREATE and possibly past UNIQUE and INDEX, so
        // the keyword is looked for backwards from where the cursor is.
        let start = self.tokens[..self.index]
            .iter()
            .rposition(|(t, _)| matches!(t, Token::Keyword(Keyword::Create)))
            .map(|i| self.tokens[i].1.start);
        let unique = self.eat_keyword(Keyword::Unique)?;
        self.expect_keyword(Keyword::Index, "after CREATE")?;
        let if_not_exists = self.if_not_exists()?;
        let name = if self.at_keyword(Keyword::On) {
            None
        } else {
            Some(self.quoted_name("after CREATE INDEX")?)
        };
        self.expect_keyword(Keyword::On, "after an index name")?;
        let table = self.quoted_name("after ON")?;
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
                self.quoted_name("in an index column list")?
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
                    .get(self.index.saturating_sub(1))
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
            let name = self.quoted_name("after DROP TABLE")?;
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

/// Whether an expression is a *reference* to a name rather than a value.
///
/// This is the one shape SQLite's `DEFAULT` grammar refuses wherever it
/// appears, and it is decided on the spelling rather than on the name: a bare
/// `a` is a string literal and a parenthesised `(a)` is a reference, so the
/// same word is a constant in one and is not in the other. Measured on
/// 3.53.4, `CREATE TABLE t(a,b DEFAULT (a))` is `default value of column [b]
/// is not constant` while `DEFAULT a` is accepted, and a `COLLATE` on either
/// side does not change the answer -- `DEFAULT (a COLLATE nocase)` is still
/// refused and `DEFAULT a COLLATE nocase` is still fine.
///
/// Every other expression is a value as far as this check is concerned,
/// including `(1+2)`: whether *that* one is constant is a different question,
/// asked of a table's contents and answered in the executor.
pub fn is_a_name_reference(e: &Expr) -> bool {
    match e {
        Expr::Column { .. } => true,
        Expr::Collate { expr, .. } => is_a_name_reference(expr),
        // A sign does not make a name into a literal: `(+a)` is a reference
        // just as `(a)` is, and is refused the same way. Measured on 3.53.4,
        // `(+a)`, `(-a)` and `(+ nosuchcol)` are all `default value of column
        // [c] is not constant`, and the parentheses are not what decides it --
        // `+a` without them is the string-literal form and is accepted.
        Expr::Unary { expr, .. } => is_a_name_reference(expr),
        _ => false,
    }
}

/// Whether a `DEFAULT` is one of the spellings SQLite will store as a value it
/// can supply again later.
///
/// An `ALTER TABLE ... ADD COLUMN` does not rewrite the rows already on disk,
/// so a default on the new column is applied when one of those rows is *read*.
/// That is only possible if the default is a value the executor can produce
/// from the stored text without the table, and the shapes below are the ones
/// that qualify. They are not a reading of "constant" in general: `1+2` is
/// constant in every ordinary sense and is *not* on this list, which is why the
/// check is a list rather than an evaluation.
///
/// `parenthesized` is the `Constraint::Default` flag of the same name, and it
/// is not a detail: SQLite's grammar reads a bare default as a *literal* and a
/// parenthesised one as an *expression*, and the two lists below are the two
/// grammars.
///
/// Measured on 3.53.4, all of these are accepted against a table that already
/// has a row:
///
/// ```text
/// 7   -7   +7   - 3   x'00FF'   'str'   NULL   0x10   TRUE   a
/// anything COLLATE name
/// (7)  (-7)  ((7))  (- (- (-7)))  (NULL)  ('str')  (x'00')  (0x10)  (+(-(+2)))
/// ```
///
/// and these are not, each with `Cannot add a column with non-constant
/// default`: `(1+2)`, `(1+2)` and every other parenthesised operator,
/// `CURRENT_TIMESTAMP`, `CURRENT_DATE`, `CURRENT_TIME`, every function call,
/// and `(x COLLATE name)` for any literal `x` -- the parentheses are what make
/// a collation fatal, so `1 COLLATE nocase` is fine and
/// `(1 COLLATE nocase)` is not.
///
/// A bare name is on the first list for a reason of its own: it is a string
/// literal by that grammar. This engine does not have the bare-name functions
/// SQLite has, so a default that is *only* a name is accepted here and left to
/// fail where it is used. That is the status quo for `DEFAULT
/// CURRENT_TIMESTAMP` at `CREATE TABLE` time, which this engine has always
/// accepted and never evaluated, and it is not made worse by saying so.
pub fn is_builtin_constant_default(e: &Expr, parenthesized: bool) -> bool {
    match e {
        Expr::Literal(_) => true,
        // A bare name is a string literal only when the parentheses are
        // absent, which is what the flag records. Measured on 3.53.4,
        // `DEFAULT a` and `DEFAULT nosuchcol` are accepted against a table
        // that already has a row, and `DEFAULT (a)` is refused -- by the
        // parser, before the executor is reached, which is why the
        // parenthesised form never actually gets this far.
        Expr::Column { .. } => !parenthesized,
        // The parentheses are what make a COLLATE fatal, and the flag is
        // exactly that difference. Measured on 3.53.4 against a table that
        // already holds a row: `DEFAULT 1 COLLATE nocase` and
        // `DEFAULT 'a' COLLATE nocase` are both accepted, while
        // `DEFAULT (1 COLLATE nocase)` and `DEFAULT ('a' COLLATE nocase)` are
        // both `Cannot add a column with non-constant default`. The bare form
        // is read as a literal with a collation attached, which is one of the
        // values listed above; the parenthesised one is read as an expression,
        // and an expression is never one of them.
        Expr::Collate { .. } => !parenthesized,
        // A sign is part of the literal, not an operator applied to one, so
        // `-7` and `(+(-(+2)))` are both values however deeply the signs nest.
        // Measured on 3.53.4, all of `(+3)`, `(- 3)`, `(-(-(-7)))`,
        // `(+(-(+2)))`, `(+x'00FF')`, `(-'a')` and `(+NULL)` are accepted
        // against a table that already has a row.
        //
        // `~` is not a sign and is not on the list: `(~1)` is refused, as are
        // `NOT` and anything else, because those are operators and this is a
        // list of literals.
        Expr::Unary { op, expr } => {
            matches!(op, UnaryOp::Negate | UnaryOp::Plus) && is_builtin_constant_default(expr, false)
        }
        _ => false,
    }
}

/// A token rendered for an error message.
fn describe_token(t: &Token) -> String {
    match t {
        Token::Identifier(n) => n.clone(),
        Token::DoubleQuotedIdentifier(n) => n.clone(),
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
