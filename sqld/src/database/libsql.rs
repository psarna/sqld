use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{Duration, Instant};

use anyhow::Context;
use crossbeam::channel::RecvTimeoutError;
use rusqlite::{OpenFlags, StatementStatus};
use tokio::sync::oneshot;
use tracing::warn;

use crate::auth::{Authenticated, Authorized};
use crate::error::Error;
use crate::libsql::wal_hook::WalHook;
use crate::query::{Column, Query, QueryResponse, QueryResult, ResultSet, Row};
use crate::query_analysis::{State, Statement, StmtKind};
use crate::stats::Stats;
use crate::Result;

use super::{
    Cond, Database, DescribeCol, DescribeParam, DescribeResponse, DescribeResult, Program, Step,
    TXN_TIMEOUT_SECS,
};

/// Internal message used to communicate between the database thread and the `LibSqlDb` handle.
enum Message {
    Program {
        pgm: Program,
        resp: oneshot::Sender<(Vec<Option<QueryResult>>, State)>,
    },
    Describe {
        sql: String,
        resp: oneshot::Sender<DescribeResult>,
    },
}

#[derive(Clone)]
pub struct LibSqlDb {
    sender: crossbeam::channel::Sender<Message>,
}

struct ConnectionState {
    state: State,
    timeout_deadline: Option<Instant>,
}

impl ConnectionState {
    fn initial() -> Self {
        Self {
            state: State::Init,
            timeout_deadline: None,
        }
    }

    fn deadline(&self) -> Option<Instant> {
        self.timeout_deadline
    }

    fn reset(&mut self) {
        self.state.reset();
        self.timeout_deadline.take();
    }

    fn step(&mut self, stmt: &Statement) {
        let old_state = self.state;

        self.state.step(stmt.kind);

        match (old_state, self.state) {
            (State::Init, State::Txn) => {
                self.timeout_deadline
                    .replace(Instant::now() + Duration::from_secs(TXN_TIMEOUT_SECS));
            }
            (State::Txn, State::Init) => self.reset(),
            (_, State::Invalid) => panic!("invalid state"),
            _ => (),
        }
    }
}

macro_rules! ok_or_exit {
    ($e:expr) => {
        if let Err(_) = $e {
            return;
        }
    };
}

pub fn open_db(
    path: &Path,
    wal_hook: impl WalHook + Send + Clone + 'static,
    with_bottomless: bool,
) -> anyhow::Result<sqld_libsql_bindings::Connection> {
    let mut retries = 0;
    loop {
        let conn_result = crate::libsql::open_with_regular_wal(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_URI
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            wal_hook.clone(),
            with_bottomless,
        );

        match conn_result {
            Ok(conn) => return Ok(conn),
            Err(e) => {
                match e.downcast::<rusqlite::Error>() {
                    // > When the last connection to a particular database is closing, that
                    // > connection will acquire an exclusive lock for a short time while it cleans
                    // > up the WAL and shared-memory files. If a second database tries to open and
                    // > query the database while the first connection is still in the middle of its
                    // > cleanup process, the second connection might get an SQLITE_BUSY error.
                    //
                    // For this reason we may not be able to open the database right away, so we
                    // retry a couple of times before giving up.
                    Ok(rusqlite::Error::SqliteFailure(e, _))
                        if e.code == rusqlite::ffi::ErrorCode::DatabaseBusy && retries < 10 =>
                    {
                        std::thread::sleep(Duration::from_millis(10));
                        retries += 1;
                    }
                    Ok(e) => panic!("Unhandled error opening libsql: {e}"),
                    Err(e) => panic!("Unhandled error opening libsql: {e}"),
                }
            }
        }
    }
}

impl LibSqlDb {
    pub fn new(
        path: impl AsRef<Path> + Send + 'static,
        extensions: Vec<PathBuf>,
        wal_hook: impl WalHook + Send + Clone + 'static,
        stats: Stats,
        with_bottomless: bool,
    ) -> crate::Result<Self> {
        let (sender, receiver) = crossbeam::channel::unbounded::<Message>();

        tokio::task::spawn_blocking(move || {
            let mut connection =
                Connection::new(path.as_ref(), extensions, wal_hook, stats, with_bottomless)
                    .unwrap();
            loop {
                let message = match connection.state.deadline() {
                    Some(deadline) => match receiver.recv_deadline(deadline) {
                        Ok(msg) => msg,
                        Err(RecvTimeoutError::Timeout) => {
                            warn!("transaction timed out");
                            connection.rollback();
                            connection.timed_out = true;
                            connection.state.reset();
                            continue;
                        }
                        Err(RecvTimeoutError::Disconnected) => break,
                    },
                    None => match receiver.recv() {
                        Ok(msg) => msg,
                        Err(_) => break,
                    },
                };

                match message {
                    Message::Program { pgm, resp } => {
                        if !connection.timed_out {
                            let results = connection.run(pgm);
                            ok_or_exit!(resp.send((results, connection.state.state)));
                        } else {
                            // fail all the queries in the batch with timeout error
                            let errors = (0..pgm.steps.len())
                                .map(|idx| Some(Err(Error::LibSqlTxTimeout(idx))))
                                .collect();
                            ok_or_exit!(resp.send((errors, connection.state.state)));
                            connection.timed_out = false;
                        }
                    }
                    Message::Describe { sql, resp } => {
                        let result = connection.describe(&sql);
                        ok_or_exit!(resp.send(result));
                    }
                }
            }
        });

        Ok(Self { sender })
    }
}

struct Connection {
    state: ConnectionState,
    conn: sqld_libsql_bindings::Connection,
    timed_out: bool,
    stats: Stats,
}

impl Connection {
    fn new(
        path: &Path,
        extensions: Vec<PathBuf>,
        wal_hook: impl WalHook + Send + Clone + 'static,
        stats: Stats,
        with_bottomless: bool,
    ) -> anyhow::Result<Self> {
        let this = Self {
            conn: open_db(path, wal_hook, with_bottomless)?,
            state: ConnectionState::initial(),
            timed_out: false,
            stats,
        };

        for ext in extensions {
            unsafe {
                let _guard = rusqlite::LoadExtensionGuard::new(&this.conn).unwrap();
                this.conn
                    .load_extension(&ext, None)
                    .with_context(|| format!("Could not load extension: {}", &ext.display()))?;
                tracing::debug!("Loaded extension {}", ext.display());
            }
        }

        Ok(this)
    }

    fn run(&mut self, pgm: Program) -> Vec<Option<QueryResult>> {
        let mut results = Vec::with_capacity(pgm.steps.len());

        for step in pgm.steps() {
            let res = self.execute_step(step, &results);
            results.push(res);
        }

        results
    }

    fn execute_step(
        &mut self,
        step: &Step,
        results: &[Option<QueryResult>],
    ) -> Option<QueryResult> {
        let enabled = match step.cond.as_ref() {
            Some(cond) => match eval_cond(cond, results) {
                Ok(enabled) => enabled,
                Err(e) => return Some(Err(e)),
            },
            None => true,
        };

        enabled.then(|| self.execute_query(&step.query))
    }

    fn execute_query(&mut self, query: &Query) -> QueryResult {
        let result = self.execute_query_inner(query);

        // We drive the connection state on success. This is how we keep track of whether
        // a transaction timeouts
        if result.is_ok() {
            self.state.step(&query.stmt)
        }

        result
    }

    fn execute_query_inner(&self, query: &Query) -> QueryResult {
        tracing::trace!("executing query: {}", query.stmt.stmt);

        let mut rows = vec![];
        let mut stmt = self.conn.prepare(&query.stmt.stmt)?;
        let columns = stmt
            .columns()
            .iter()
            .map(|col| Column {
                name: col.name().into(),
                ty: col
                    .decl_type()
                    .map(FromStr::from_str)
                    .transpose()
                    .ok()
                    .flatten(),
                decltype: col.decl_type().map(|t| t.into()),
            })
            .collect::<Vec<_>>();

        query
            .params
            .bind(&mut stmt)
            .map_err(Error::LibSqlInvalidQueryParams)?;

        let mut qresult = stmt.raw_query();
        while let Some(row) = qresult.next()? {
            if !query.want_rows {
                // if the caller does not want rows, we keep `rows` empty, but we still iterate the
                // statement to completion to make sure that we don't miss any errors or side
                // effects
                continue;
            }

            let mut values = vec![];
            for (i, _) in columns.iter().enumerate() {
                values.push(row.get::<usize, rusqlite::types::Value>(i)?.into());
            }
            rows.push(Row { values });
        }

        // sqlite3_changes() is only modified for INSERT, UPDATE or DELETE; it is not reset for SELECT,
        // but we want to return 0 in that case.
        let affected_row_count = match query.stmt.is_iud {
            true => self.conn.changes(),
            false => 0,
        };

        // sqlite3_last_insert_rowid() only makes sense for INSERTs into a rowid table. we can't detect
        // a rowid table, but at least we can detect an INSERT
        let last_insert_rowid = match query.stmt.is_insert {
            true => Some(self.conn.last_insert_rowid()),
            false => None,
        };

        drop(qresult);

        self.update_stats(&stmt);

        Ok(QueryResponse::ResultSet(ResultSet {
            columns,
            rows,
            affected_row_count,
            last_insert_rowid,
            include_column_defs: true,
        }))
    }

    fn rollback(&self) {
        self.conn
            .execute("ROLLBACK", ())
            .expect("failed to rollback");
    }

    fn update_stats(&self, stmt: &rusqlite::Statement) {
        self.stats
            .inc_rows_read(stmt.get_status(StatementStatus::RowsRead) as u64);
        self.stats
            .inc_rows_written(stmt.get_status(StatementStatus::RowsWritten) as u64);
    }

    fn describe(&self, sql: &str) -> DescribeResult {
        let stmt = self.conn.prepare(sql)?;

        let params = (1..=stmt.parameter_count())
            .map(|param_i| {
                let name = stmt.parameter_name(param_i).map(|n| n.into());
                DescribeParam { name }
            })
            .collect();

        let cols = stmt
            .columns()
            .into_iter()
            .map(|col| {
                let name = col.name().into();
                let decltype = col.decl_type().map(|t| t.into());
                DescribeCol { name, decltype }
            })
            .collect();

        let is_explain = stmt.is_explain() != 0;
        let is_readonly = stmt.readonly();
        Ok(DescribeResponse {
            params,
            cols,
            is_explain,
            is_readonly,
        })
    }
}

fn eval_cond(cond: &Cond, results: &[Option<QueryResult>]) -> Result<bool> {
    let get_step_res = |step: usize| -> Result<Option<&QueryResult>> {
        let res = results
            .get(step)
            .ok_or(Error::InvalidBatchStep(step))?
            .as_ref();

        Ok(res)
    };

    Ok(match cond {
        Cond::Ok { step } => get_step_res(*step)?.map(|r| r.is_ok()).unwrap_or(false),
        Cond::Err { step } => get_step_res(*step)?.map(|r| r.is_err()).unwrap_or(false),
        Cond::Not { cond } => !eval_cond(cond, results)?,
        Cond::And { conds } => conds
            .iter()
            .try_fold(true, |x, cond| eval_cond(cond, results).map(|y| x & y))?,
        Cond::Or { conds } => conds
            .iter()
            .try_fold(false, |x, cond| eval_cond(cond, results).map(|y| x | y))?,
    })
}

fn check_program_auth(auth: Authenticated, pgm: &Program) -> Result<()> {
    for step in pgm.steps() {
        let query = &step.query;
        match (query.stmt.kind, &auth) {
            (_, Authenticated::Anonymous) => {
                return Err(Error::NotAuthorized(
                    "anonymous access not allowed".to_string(),
                ));
            }
            (StmtKind::Read, Authenticated::Authorized(_)) => (),
            (StmtKind::TxnBegin, _) | (StmtKind::TxnEnd, _) => (),
            (_, Authenticated::Authorized(Authorized::FullAccess)) => (),
            _ => {
                return Err(Error::NotAuthorized(format!(
                    "Current session is not authorized to run: {}",
                    query.stmt.stmt
                )));
            }
        }
    }
    Ok(())
}

fn check_describe_auth(auth: Authenticated) -> Result<()> {
    match auth {
        Authenticated::Anonymous => {
            Err(Error::NotAuthorized("anonymous access not allowed".into()))
        }
        Authenticated::Authorized(_) => Ok(()),
    }
}

#[async_trait::async_trait]
impl Database for LibSqlDb {
    async fn execute_program(
        &self,
        pgm: Program,
        auth: Authenticated,
    ) -> Result<(Vec<Option<QueryResult>>, State)> {
        check_program_auth(auth, &pgm)?;
        let (resp, receiver) = oneshot::channel();
        let msg = Message::Program { pgm, resp };
        let _: Result<_, _> = self.sender.send(msg);

        Ok(receiver.await?)
    }

    async fn describe(&self, sql: String, auth: Authenticated) -> Result<DescribeResult> {
        check_describe_auth(auth)?;
        let (resp, receiver) = oneshot::channel();
        let msg = Message::Describe { sql, resp };
        let _: Result<_, _> = self.sender.send(msg);

        Ok(receiver.await?)
    }
}
