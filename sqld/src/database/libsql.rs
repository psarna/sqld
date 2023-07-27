use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam::channel::RecvTimeoutError;
use sqld_libsql_bindings::wal_hook::WalMethodsHook;
use tokio::sync::oneshot;
use tracing::warn;

use crate::auth::{Authenticated, Authorized};
use crate::error::Error;
use crate::libsql::wal_hook::WalHook;
use crate::query::Query;
use crate::query_analysis::{State, StmtKind};
use crate::query_result_builder::{QueryBuilderConfig, QueryResultBuilder};
use crate::stats::Stats;
use crate::Result;

use super::config::DatabaseConfigStore;
use super::factory::DbFactory;
use super::{
    Cond, Database, DescribeCol, DescribeParam, DescribeResponse, DescribeResult, Program, Step,
    TXN_TIMEOUT,
};

/// Internal message used to communicate between the database thread and the `LibSqlDb` handle.
type ExecCallback = Box<dyn FnOnce(Result<&mut Connection>) -> anyhow::Result<()> + Send + 'static>;

pub struct LibSqlDbFactory<W: WalHook + 'static> {
    db_path: PathBuf,
    hook: &'static WalMethodsHook<W>,
    ctx_builder: Box<dyn Fn() -> W::Context + Sync + Send + 'static>,
    stats: Stats,
    config_store: Arc<DatabaseConfigStore>,
    extensions: Vec<PathBuf>,
    max_response_size: u64,
    /// In wal mode, closing the last database takes time, and causes other databases creation to
    /// return sqlite busy. To mitigate that, we hold on to one connection
    _db: Option<LibSqlDb>,
}

impl<W: WalHook + 'static> LibSqlDbFactory<W>
where
    W: WalHook + 'static + Sync + Send,
    W::Context: Send + 'static,
{
    pub async fn new<F>(
        db_path: PathBuf,
        hook: &'static WalMethodsHook<W>,
        ctx_builder: F,
        stats: Stats,
        config_store: Arc<DatabaseConfigStore>,
        extensions: Vec<PathBuf>,
        max_response_size: u64,
    ) -> Result<Self>
    where
        F: Fn() -> W::Context + Sync + Send + 'static,
    {
        let mut this = Self {
            db_path,
            hook,
            ctx_builder: Box::new(ctx_builder),
            stats,
            config_store,
            extensions,
            max_response_size,
            _db: None,
        };

        let db = this.try_create_db().await?;
        this._db = Some(db);

        Ok(this)
    }

    /// Tries to create a database, retrying if the database is busy.
    async fn try_create_db(&self) -> Result<LibSqlDb> {
        // try 100 times to acquire initial db connection.
        let mut retries = 0;
        const BUSY: i32 = sqld_libsql_bindings::ffi::SQLITE_BUSY as std::ffi::c_int;
        loop {
            match self.create_database().await {
                Ok(conn) => return Ok(conn),
                Err(err @ Error::LibsqlError(BUSY)) => {
                    if retries < 100 {
                        tracing::warn!("Database file is busy, retrying...");
                        retries += 1;
                        tokio::time::sleep(Duration::from_millis(100)).await
                    } else {
                        Err(err)?;
                    }
                }
                Err(e) => Err(e)?,
            }
        }
    }

    async fn create_database(&self) -> Result<LibSqlDb> {
        LibSqlDb::new(
            self.db_path.clone(),
            self.extensions.clone(),
            self.hook,
            (self.ctx_builder)(),
            self.stats.clone(),
            self.config_store.clone(),
            QueryBuilderConfig {
                max_size: Some(self.max_response_size),
            },
        )
        .await
    }
}

#[async_trait::async_trait]
impl<W> DbFactory for LibSqlDbFactory<W>
where
    W: WalHook + 'static + Sync + Send,
    W::Context: Send + 'static,
{
    type Db = LibSqlDb;

    async fn create(&self) -> Result<Self::Db, Error> {
        self.create_database().await
    }
}

#[derive(Clone)]
pub struct LibSqlDb {
    sender: crossbeam::channel::Sender<ExecCallback>,
}

pub fn open_db<'a, W>(
    path: &Path,
    wal_methods: &'static WalMethodsHook<W>,
    hook_ctx: &'a mut W::Context,
    flags: Option<std::ffi::c_int>,
) -> Result<sqld_libsql_bindings::Connection<'a>, libsql::Error>
where
    W: WalHook,
{
    let flags = flags.unwrap_or(
        (sqld_libsql_bindings::ffi::SQLITE_OPEN_READWRITE
            | sqld_libsql_bindings::ffi::SQLITE_OPEN_CREATE
            | sqld_libsql_bindings::ffi::SQLITE_OPEN_URI
            | sqld_libsql_bindings::ffi::SQLITE_OPEN_NOMUTEX) as i32,
    );

    sqld_libsql_bindings::Connection::open(path, flags, wal_methods, hook_ctx)
        .map_err(|rc| libsql::Error::LibError(rc))
}

impl LibSqlDb {
    pub async fn new<W>(
        path: impl AsRef<Path> + Send + 'static,
        extensions: Vec<PathBuf>,
        wal_hook: &'static WalMethodsHook<W>,
        hook_ctx: W::Context,
        stats: Stats,
        config_store: Arc<DatabaseConfigStore>,
        builder_config: QueryBuilderConfig,
    ) -> crate::Result<Self>
    where
        W: WalHook,
        W::Context: Send,
    {
        let (sender, receiver) = crossbeam::channel::unbounded::<ExecCallback>();
        let (init_sender, init_receiver) = oneshot::channel();

        tokio::task::spawn_blocking(move || {
            let mut ctx = hook_ctx;
            let mut connection = match Connection::new(
                path.as_ref(),
                extensions,
                wal_hook,
                &mut ctx,
                stats,
                config_store,
                builder_config,
            ) {
                Ok(conn) => {
                    let Ok(_) = init_sender.send(Ok(())) else { return };
                    conn
                }
                Err(e) => {
                    let _ = init_sender.send(Err(e));
                    return;
                }
            };

            loop {
                let exec = match connection.timeout_deadline {
                    Some(deadline) => match receiver.recv_deadline(deadline) {
                        Ok(msg) => msg,
                        Err(RecvTimeoutError::Timeout) => {
                            warn!("transaction timed out");
                            connection.rollback();
                            connection.timed_out = true;
                            connection.timeout_deadline = None;
                            continue;
                        }
                        Err(RecvTimeoutError::Disconnected) => break,
                    },
                    None => match receiver.recv() {
                        Ok(msg) => msg,
                        Err(_) => break,
                    },
                };

                let maybe_conn = if !connection.timed_out {
                    Ok(&mut connection)
                } else {
                    Err(Error::LibSqlTxTimeout)
                };

                if exec(maybe_conn).is_err() {
                    tracing::warn!("Database connection closed unexpectedly");
                    return;
                };
            }
        });

        init_receiver.await??;

        Ok(Self { sender })
    }
}

struct Connection<'a> {
    timeout_deadline: Option<Instant>,
    conn: sqld_libsql_bindings::Connection<'a>,
    timed_out: bool,
    stats: Stats,
    config_store: Arc<DatabaseConfigStore>,
    builder_config: QueryBuilderConfig,
}

impl<'a> Connection<'a> {
    fn new<W: WalHook>(
        path: &Path,
        extensions: Vec<PathBuf>,
        wal_methods: &'static WalMethodsHook<W>,
        hook_ctx: &'a mut W::Context,
        stats: Stats,
        config_store: Arc<DatabaseConfigStore>,
        builder_config: QueryBuilderConfig,
    ) -> Result<Self> {
        let this = Self {
            conn: open_db(path, wal_methods, hook_ctx, None)?,
            timeout_deadline: None,
            timed_out: false,
            stats,
            config_store,
            builder_config,
        };

        /* FIXME: extensions via raw API
        for ext in extensions {
            unsafe {
                let _guard = xxxx::LoadExtensionGuard::new(&this.conn).unwrap();
                if let Err(e) = this.conn.load_extension(&ext, None) {
                    tracing::error!("failed to load extension: {}", ext.display());
                    Err(e)?;
                }
                tracing::debug!("Loaded extension {}", ext.display());
            }
        }
         */

        Ok(this)
    }

    fn run<B: QueryResultBuilder>(&mut self, pgm: Program, mut builder: B) -> Result<B> {
        let mut results = Vec::with_capacity(pgm.steps.len());

        builder.init(&self.builder_config)?;
        let is_autocommit_before = self.conn.is_autocommit();

        for step in pgm.steps() {
            let res = self.execute_step(step, &results, &mut builder)?;
            results.push(res);
        }

        // A transaction is still open, set up a timeout
        if is_autocommit_before && !self.conn.is_autocommit() {
            self.timeout_deadline = Some(Instant::now() + TXN_TIMEOUT)
        }

        builder.finish()?;

        Ok(builder)
    }

    fn execute_step(
        &mut self,
        step: &Step,
        results: &[bool],
        builder: &mut impl QueryResultBuilder,
    ) -> Result<bool> {
        builder.begin_step()?;
        let mut enabled = match step.cond.as_ref() {
            Some(cond) => match eval_cond(cond, results) {
                Ok(enabled) => enabled,
                Err(e) => {
                    builder.step_error(e).unwrap();
                    false
                }
            },
            None => true,
        };

        let (affected_row_count, last_insert_rowid) = if enabled {
            match self.execute_query(&step.query, builder) {
                // builder error interupt the execution of query. we should exit immediately.
                Err(e @ Error::BuilderError(_)) => return Err(e),
                Err(e) => {
                    builder.step_error(e)?;
                    enabled = false;
                    (0, None)
                }
                Ok(x) => x,
            }
        } else {
            (0, None)
        };

        builder.finish_step(affected_row_count, last_insert_rowid)?;

        Ok(enabled)
    }

    fn execute_query(
        &self,
        query: &Query,
        builder: &mut impl QueryResultBuilder,
    ) -> Result<(u64, Option<i64>)> {
        tracing::trace!("executing query: {}", query.stmt.stmt);

        let config = self.config_store.get();
        let blocked = match query.stmt.kind {
            StmtKind::Read | StmtKind::TxnBegin | StmtKind::Other => config.block_reads,
            StmtKind::Write => config.block_reads || config.block_writes,
            StmtKind::TxnEnd => false,
        };
        if blocked {
            return Err(Error::Blocked(config.block_reason.clone()));
        }

        let mut stmt = self.conn.prepare(&query.stmt.stmt)?;

        let cols = stmt.columns();
        let cols_count = cols.len();
        builder.cols_description(cols.iter())?;
        drop(cols);

        query
            .params
            .bind(&mut stmt)
            .map_err(Error::LibSqlInvalidQueryParams)?;

        let mut qresult = stmt.raw_query();
        builder.begin_rows()?;
        while let Some(row) = qresult.next()? {
            builder.begin_row()?;
            for i in 0..cols_count {
                let val = row.get_ref(i)?;
                builder.add_row_value(val)?;
            }
            builder.finish_row()?;
        }

        builder.finish_rows()?;

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

        Ok((affected_row_count, last_insert_rowid))
    }

    fn rollback(&self) {
        let _ = self.conn.execute("ROLLBACK", ());
    }

    fn update_stats(&self, stmt: &libsql::Statement) {
        let rows_read = stmt.get_status(sqld_libsql_bindings::ffi::LIBSQL_STMTSTATUS_ROWS_READ);
        let rows_written =
            stmt.get_status(sqld_libsql_bindings::ffi::LIBSQL_STMTSTATUS_ROWS_WRITTEN);
        let rows_read = if rows_read == 0 && rows_written == 0 {
            1
        } else {
            rows_read
        };
        self.stats.inc_rows_read(rows_read as u64);
        self.stats.inc_rows_written(rows_written as u64);
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

fn eval_cond(cond: &Cond, results: &[bool]) -> Result<bool> {
    let get_step_res = |step: usize| -> Result<bool> {
        let res = results.get(step).ok_or(Error::InvalidBatchStep(step))?;

        Ok(*res)
    };

    Ok(match cond {
        Cond::Ok { step } => get_step_res(*step)?,
        Cond::Err { step } => !get_step_res(*step)?,
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
    async fn execute_program<B: QueryResultBuilder>(
        &self,
        pgm: Program,
        auth: Authenticated,
        builder: B,
    ) -> Result<(B, State)> {
        check_program_auth(auth, &pgm)?;
        let (resp, receiver) = oneshot::channel();
        let cb = Box::new(move |maybe_conn: Result<&mut Connection>| {
            let res = maybe_conn.and_then(|c| {
                let b = c.run(pgm, builder)?;
                let state = if c.conn.is_autocommit() {
                    State::Init
                } else {
                    State::Txn
                };

                Ok((b, state))
            });

            if resp.send(res).is_err() {
                anyhow::bail!("connection closed");
            }

            Ok(())
        });

        let _: Result<_, _> = self.sender.send(cb);

        Ok(receiver.await??)
    }

    async fn describe(&self, sql: String, auth: Authenticated) -> Result<DescribeResult> {
        check_describe_auth(auth)?;
        let (resp, receiver) = oneshot::channel();
        let cb = Box::new(move |maybe_conn: Result<&mut Connection>| {
            let res = maybe_conn.and_then(|c| c.describe(&sql));

            if resp.send(res).is_err() {
                anyhow::bail!("connection closed");
            }

            Ok(())
        });

        let _: Result<_, _> = self.sender.send(cb);

        Ok(receiver.await?)
    }
}

#[cfg(test)]
mod test {
    use itertools::Itertools;

    use crate::query_result_builder::{test::test_driver, IgnoreResult};

    use super::*;

    fn setup_test_conn(ctx: &mut ()) -> Connection {
        let mut conn = Connection {
            timeout_deadline: None,
            conn: sqld_libsql_bindings::Connection::test(ctx),
            timed_out: false,
            stats: Stats::default(),
            config_store: Arc::new(DatabaseConfigStore::new_test()),
            builder_config: QueryBuilderConfig::default(),
        };

        let stmts = std::iter::once("create table test (x)")
            .chain(std::iter::repeat("insert into test values ('hello world')").take(100))
            .collect_vec();
        conn.run(Program::seq(&stmts), IgnoreResult).unwrap();

        conn
    }

    #[test]
    fn test_libsql_conn_builder_driver() {
        test_driver(1000, |b| {
            let ctx = &mut ();
            let mut conn = setup_test_conn(ctx);
            conn.run(Program::seq(&["select * from test"]), b)
        })
    }
}
