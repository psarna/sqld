use anyhow::Result;
use fallible_iterator::FallibleIterator;
use sqlite3_parser::{
    ast::{Cmd, Stmt},
    lexer::sql::{Parser, ParserError},
};

/// A group of statements to be executed together.
#[derive(Debug)]
pub struct Statement {
    pub stmt: String,
    pub kind: StmtKind,
    /// Is the statement an INSERT, UPDATE or DELETE?
    pub is_iud: bool,
}

impl Default for Statement {
    fn default() -> Self {
        Self::empty()
    }
}

/// Classify statement in categories of interest.
#[derive(Debug, PartialEq, Clone, Copy)]
pub enum StmtKind {
    /// The begining of a transaction
    TxnBegin,
    /// The end of a transaction
    TxnEnd,
    Read,
    Write,
    Other,
}

impl StmtKind {
    fn kind(cmd: &Cmd) -> Option<Self> {
        match cmd {
            Cmd::Explain(_) => Some(Self::Other),
            Cmd::ExplainQueryPlan(_) => Some(Self::Other),
            Cmd::Stmt(Stmt::Begin { .. }) => Some(Self::TxnBegin),
            Cmd::Stmt(Stmt::Commit { .. } | Stmt::Rollback { .. }) => Some(Self::TxnEnd),
            Cmd::Stmt(
                Stmt::Insert { .. }
                | Stmt::CreateTable { .. }
                | Stmt::Update { .. }
                | Stmt::Delete { .. }
                | Stmt::DropTable { .. }
                | Stmt::AlterTable { .. }
                | Stmt::CreateIndex { .. },
            ) => Some(Self::Write),
            Cmd::Stmt(Stmt::Select { .. }) => Some(Self::Read),
            _ => None,
        }
    }
}

/// The state of a transaction for a series of statement
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum State {
    /// The txn in an opened state
    Txn,
    /// The txn in a closed state
    Init,
    /// This is an invalid state for the state machine
    Invalid,
}

impl State {
    pub fn step(&mut self, kind: StmtKind) {
        *self = match (*self, kind) {
            (State::Txn, StmtKind::TxnBegin) | (State::Init, StmtKind::TxnEnd) => State::Invalid,
            (State::Txn, StmtKind::TxnEnd) => State::Init,
            (state, StmtKind::Other | StmtKind::Write | StmtKind::Read) => state,
            (State::Invalid, _) => State::Invalid,
            (State::Init, StmtKind::TxnBegin) => State::Txn,
        };
    }

    pub fn reset(&mut self) {
        *self = State::Init
    }
}

impl Statement {
    pub fn empty() -> Self {
        Self {
            stmt: String::new(),
            // empty statement is arbitrarely made of the read kind so it is not send to a writer
            kind: StmtKind::Read,
            is_iud: false,
        }
    }

    /// Returns a statement instance without performing any validation to the input
    /// It is always assumed that such a statement will be a write, and will always be handled by
    /// the primary
    pub fn new_unchecked(stmt: String) -> Self {
        Self {
            stmt,
            kind: StmtKind::Write,
            is_iud: false,
        }
    }

    /// parses a series of statements into unchecked statemments
    pub fn parse_unchecked(s: &[u8]) -> impl Iterator<Item = Result<Self>> + '_ {
        let mut parser = Box::new(Parser::new(s));
        std::iter::from_fn(move || match parser.next() {
            Ok(Some(cmd)) => Some(Ok(Self::new_unchecked(cmd.to_string()))),
            Ok(None) => None,
            Err(sqlite3_parser::lexer::sql::Error::ParserError(
                ParserError::SyntaxError {
                    token_type: _,
                    found: Some(found),
                },
                Some((line, col)),
            )) => Some(Err(anyhow::anyhow!(
                "syntax error around L{line}:{col}: `{found}`"
            ))),
            Err(e) => Some(Err(e.into())),
        })
    }

    pub fn rewrite_if_postgres(s: &str) -> String {
        if s.contains("SELECT c.oid,c.*,d.description,pg_catalog.pg_get_expr(c.relpartbound, c.oid) as partition_expr,  pg_catalog.pg_get_partkeydef(c.oid) as partition_key") {
            return "SELECT * FROM pg_catalog.pg_class WHERE relnamespace = ? AND relkind not in ('i','I','c')".to_string();
        }
        if s.contains(
            "SELECT c.contoencoding as encid,pg_catalog.pg_encoding_to_char(c.contoencoding)",
        ) {
            return "SELECT 6 as encid, 'UTF8' as encname".to_string();
        }
        /* postlite:
        func rewriteQuery(q string) string {
            // Ignore SET queries by rewriting them to empty resultsets.
            if strings.HasPrefix(q, "SET ") {
                return `SELECT 'SET'`
            }

            // Ignore this god forsaken query for pulling keywords.
            if strings.Contains(q, `select string_agg(word, ',') from pg_catalog.pg_get_keywords()`) {
                return `SELECT '' AS "string_agg" WHERE 1 = 2`
            }

            // Rewrite system information variables so they are functions so we can inject them.
            // https://www.postgresql.org/docs/9.1/functions-info.html
            q = systemFunctionRegex.ReplaceAllString(q, "$1()$2")

            // Rewrite double-colon casting by simply removing it.
            // https://www.postgresql.org/docs/7.3/sql-expressions.html#SQL-SYNTAX-TYPE-CASTS
            q = castRegex.ReplaceAllString(q, "")

            // Remove references to the pg_catalog.
            // q = pgCatalogRegex.ReplaceAllString(q, "")

            // Rewrite "SHOW" commands into function calls.
            q = showRegex.ReplaceAllString(q, "SELECT show('$1')")

            return q
        }
        */
        s.to_string()
            .replace("::regclass", "")
            .replace("$1", "?")
            .replace("$2", "?")
            .replace("$3", "?")
            .replace("$4", "?")
            .replace("$5", "?")
            .replace("$6", "?") // FIXME: regex instead of handcoded numbers
    }

    pub fn parse(s: &str) -> impl Iterator<Item = Result<Self>> + '_ {
        println!("Before: {s}");
        let s = Self::rewrite_if_postgres(s);
        println!("After: {s}");
        fn parse_inner(c: Cmd) -> Result<Statement> {
            let kind =
                StmtKind::kind(&c).ok_or_else(|| anyhow::anyhow!("unsupported statement"))?;
            let is_iud = matches!(
                c,
                Cmd::Stmt(Stmt::Insert { .. } | Stmt::Update { .. } | Stmt::Delete { .. })
            );

            Ok(Statement {
                stmt: c.to_string(),
                kind,
                is_iud,
            })
        }
        // The parser needs to be boxed because it's large, and you don't want it on the stack.
        // There's upstream work to make it smaller, but in the meantime the parser should remain
        // on the heap:
        // - https://github.com/gwenn/lemon-rs/issues/8
        // - https://github.com/gwenn/lemon-rs/pull/19
        let mut parser = Box::new(Parser::new(s.as_bytes().to_vec()));
        std::iter::from_fn(move || match parser.next() {
            Ok(Some(cmd)) => Some(parse_inner(cmd)),
            Ok(None) => None,
            Err(sqlite3_parser::lexer::sql::Error::ParserError(
                ParserError::SyntaxError {
                    token_type: _,
                    found: Some(found),
                },
                Some((line, col)),
            )) => Some(Err(anyhow::anyhow!(
                "syntax error around L{line}:{col}: `{found}`"
            ))),
            Err(e) => Some(Err(e.into())),
        })
    }

    pub fn is_read_only(&self) -> bool {
        matches!(
            self.kind,
            StmtKind::Read | StmtKind::TxnEnd | StmtKind::TxnBegin
        )
    }
}

/// Given a an initial state and an array of queries, return the final state obtained if all the
/// queries succeeded
pub fn final_state<'a>(mut state: State, stmts: impl Iterator<Item = &'a Statement>) -> State {
    for stmt in stmts {
        state.step(stmt.kind);
    }
    state
}
