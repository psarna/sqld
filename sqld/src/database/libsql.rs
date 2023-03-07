use std::path::Path;
use std::str::FromStr;
use std::time::{Duration, Instant};

use crossbeam::channel::RecvTimeoutError;
use rusqlite::OpenFlags;
use tokio::sync::oneshot;
use tracing::warn;

use crate::error::Error;
use crate::libsql::wal_hook::WalHook;
use crate::query::{Column, Params, Queries, Query, QueryResponse, QueryResult, ResultSet, Row};
use crate::query_analysis::{State, Statement};
use crate::Result;

use super::{Database, TXN_TIMEOUT_SECS};

/// Internal message used to communicate between the database thread and the `LibSqlDb` handle.
struct Message {
    queries: Queries,
    resp: oneshot::Sender<(Vec<QueryResult>, State)>,
}

#[derive(Clone)]
pub struct LibSqlDb {
    sender: crossbeam::channel::Sender<Message>,
}

fn execute_query(conn: &rusqlite::Connection, stmt: &Statement, params: Params) -> QueryResult {
    let mut rows = vec![];
    let mut prepared = conn.prepare(&stmt.stmt)?;
    let columns = prepared
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
        })
        .collect::<Vec<_>>();

    params
        .bind(&mut prepared)
        .map_err(Error::LibSqlInvalidQueryParams)?;

    let mut qresult = prepared.raw_query();
    while let Some(row) = qresult.next()? {
        let mut values = vec![];
        for (i, _) in columns.iter().enumerate() {
            values.push(row.get::<usize, rusqlite::types::Value>(i)?.into());
        }
        rows.push(Row { values });
    }

    // sqlite3_changes() is only modified for INSERT, UPDATE or DELETE; it is not reset for SELECT,
    // but we want to return 0 in that case.
    let affected_row_count = match stmt.is_iud {
        true => conn.changes(),
        false => 0,
    };

    Ok(QueryResponse::ResultSet(ResultSet {
        columns,
        rows,
        affected_row_count,
        include_column_defs: true,
    }))
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

fn handle_query(
    conn: &rusqlite::Connection,
    query: Query,
    state: &mut ConnectionState,
) -> QueryResult {
    let result = execute_query(conn, &query.stmt, query.params);

    // We drive the connection state on success. This is how we keep track of whether
    // a transaction timeouts
    if result.is_ok() {
        state.step(&query.stmt)
    }

    result
}

fn rollback(conn: &rusqlite::Connection) {
    conn.execute("rollback transaction;", ())
        .expect("failed to rollback");
}

macro_rules! ok_or_exit {
    ($e:expr) => {
        if let Err(_) = $e {
            return;
        }
    };
}

fn open_db(
    path: impl AsRef<Path> + Send + 'static,
    wal_hook: impl WalHook + Send + Clone + 'static,
    with_bottomless: bool,
) -> anyhow::Result<rusqlite::Connection> {
    let mut retries = 0;
    loop {
        #[cfg(feature = "mwal_backend")]
        let conn_result = match crate::VWAL_METHODS.get().unwrap() {
            Some(ref vwal_methods) => crate::libsql::mwal::open_with_virtual_wal(
                &path,
                OpenFlags::SQLITE_OPEN_READ_WRITE
                    | OpenFlags::SQLITE_OPEN_CREATE
                    | OpenFlags::SQLITE_OPEN_URI
                    | OpenFlags::SQLITE_OPEN_NO_MUTEX,
                vwal_methods.clone(),
            ),
            None => crate::libsql::open_with_regular_wal(
                &path,
                OpenFlags::SQLITE_OPEN_READ_WRITE
                    | OpenFlags::SQLITE_OPEN_CREATE
                    | OpenFlags::SQLITE_OPEN_URI
                    | OpenFlags::SQLITE_OPEN_NO_MUTEX,
                wal_hook.clone(),
                with_bottomless,
            ),
        };

        #[cfg(not(feature = "mwal_backend"))]
        let conn_result = crate::libsql::open_with_regular_wal(
            &path,
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

fn init_postgres_tables(conn: &rusqlite::Connection) -> Result<()> {
    let payload = &[
        // FIXME: everything below should be virtual tables that return actual schema information
        "attach database 'file::memory:' AS pg_catalog",
        "create table if not exists pg_catalog.pg_type(oid, typname, typnamespace, typowner, typlen, typbyval, typtype, typcategory, typispreferred, typisdefined, typdelim, typrelid, typsubscript, typelem, typarray, typinput, typoutput, typreceive, typsend, typmodin, typmodout, typanalyze, typalign, typstorage, typnotnull, typbasetype, typtypmod, typndims, typcollation, typdefaultbin, typdefault, typacl);",
        "create table if not exists pg_catalog.pg_class(oid, relname, relnamespace, reltype, reloftype, relowner, relam, relfilenode, reltablespace, relpages, reltuples, relallvisible, reltoastrelid, relhasindex, relisshared, relpersistence, relkind, relnatts, relchecks, relhasrules, relhastriggers, relhassubclass, relrowsecurity, relforcerowsecurity, relispopulated, relreplident, relispartition, relrewrite, relfrozenxid, relminxid, relacl, reloptions, relpartbound)",
        "create table if not exists pg_catalog.pg_description(objoid, classoid, objsubid, description)",
        "create table if not exists pg_catalog.pg_namespace(oid, nspname, nspowner, nspacl)",
        "create table if not exists pg_catalog.pg_tablespace(oid, spcname, spcowner, spcacl, spcoptions)",

        "insert into pg_catalog.pg_namespace values (99, 'pg_toast', 10, null)",
        "insert into pg_catalog.pg_namespace values (11, 'pg_catalog', 10, null)",
        "insert into pg_catalog.pg_namespace values (2200, 'public', 10, null)",
        "insert into pg_catalog.pg_namespace values (13427, 'information_schema', 10, null)",

        "insert into pg_catalog.pg_description values (11, 2615, 0, 'system catalog schema')",
        "insert into pg_catalog.pg_description values (99, 2615, 0, 'reserved schema for TOAST tables')",
        "insert into pg_catalog.pg_description values (2200, 2615, 0, 'standard public schema')",

        // FIXME: this is just a small sample, this monstrosity needs to be loaded from the 'libsql_pg_type_payload.sql' file. Or maybe we just attach a preloaded database
        // with all the tables alrady in place, instead of loading the contents on boot.
        "INSERT INTO pg_catalog.pg_type  VALUES (16, 'bool', 11, 10, 1, true, 'b', 'B', true, true, ',', 0, '-', 0, 1000, 'boolin', 'boolout', 'boolrecv', 'boolsend', '-', '-', '-', 'c', 'p', false, 0, -1, 0, 0, NULL, NULL, NULL);",
        "INSERT INTO pg_catalog.pg_type  VALUES (17, 'bytea', 11, 10, -1, false, 'b', 'U', false, true, ',', 0, '-', 0, 1001, 'byteain', 'byteaout', 'bytearecv', 'byteasend', '-', '-', '-', 'i', 'x', false, 0, -1, 0, 0, NULL, NULL, NULL);",
        "INSERT INTO pg_catalog.pg_type  VALUES (18, 'char', 11, 10, 1, true, 'b', 'Z', false, true, ',', 0, '-', 0, 1002, 'charin', 'charout', 'charrecv', 'charsend', '-', '-', '-', 'c', 'p', false, 0, -1, 0, 0, NULL, NULL, NULL);",
        "INSERT INTO pg_catalog.pg_type  VALUES (19, 'name', 11, 10, 64, false, 'b', 'S', false, true, ',', 0, 'raw_array_subscript_handler', 18, 1003, 'namein', 'nameout', 'namerecv', 'namesend', '-', '-', '-', 'c', 'p', false, 0, -1, 0, 950, NULL, NULL, NULL);",
        "INSERT INTO pg_catalog.pg_type  VALUES (20, 'int8', 11, 10, 8, true, 'b', 'N', false, true, ',', 0, '-', 0, 1016, 'int8in', 'int8out', 'int8recv', 'int8send', '-', '-', '-', 'd', 'p', false, 0, -1, 0, 0, NULL, NULL, NULL);",
        "INSERT INTO pg_catalog.pg_type  VALUES (21, 'int2', 11, 10, 2, true, 'b', 'N', false, true, ',', 0, '-', 0, 1005, 'int2in', 'int2out', 'int2recv', 'int2send', '-', '-', '-', 's', 'p', false, 0, -1, 0, 0, NULL, NULL, NULL);",
        "INSERT INTO pg_catalog.pg_type  VALUES (22, 'int2vector', 11, 10, -1, false, 'b', 'A', false, true, ',', 0, 'array_subscript_handler', 21, 1006, 'int2vectorin', 'int2vectorout', 'int2vectorrecv', 'int2vectorsend', '-', '-', '-', 'i', 'p', false, 0, -1, 0, 0, NULL, NULL, NULL);",
        "INSERT INTO pg_catalog.pg_type  VALUES (23, 'int4', 11, 10, 4, true, 'b', 'N', false, true, ',', 0, '-', 0, 1007, 'int4in', 'int4out', 'int4recv', 'int4send', '-', '-', '-', 'i', 'p', false, 0, -1, 0, 0, NULL, NULL, NULL);",
        "INSERT INTO pg_catalog.pg_type  VALUES (24, 'regproc', 11, 10, 4, true, 'b', 'N', false, true, ',', 0, '-', 0, 1008, 'regprocin', 'regprocout', 'regprocrecv', 'regprocsend', '-', '-', '-', 'i', 'p', false, 0, -1, 0, 0, NULL, NULL, NULL);",
        "INSERT INTO pg_catalog.pg_type  VALUES (25, 'text', 11, 10, -1, false, 'b', 'S', true, true, ',', 0, '-', 0, 1009, 'textin', 'textout', 'textrecv', 'textsend', '-', '-', '-', 'i', 'x', false, 0, -1, 0, 100, NULL, NULL, NULL);",
        "INSERT INTO pg_catalog.pg_type  VALUES (26, 'oid', 11, 10, 4, true, 'b', 'N', true, true, ',', 0, '-', 0, 1028, 'oidin', 'oidout', 'oidrecv', 'oidsend', '-', '-', '-', 'i', 'p', false, 0, -1, 0, 0, NULL, NULL, NULL);",
        "INSERT INTO pg_catalog.pg_type  VALUES (27, 'tid', 11, 10, 6, false, 'b', 'U', false, true, ',', 0, '-', 0, 1010, 'tidin', 'tidout', 'tidrecv', 'tidsend', '-', '-', '-', 's', 'p', false, 0, -1, 0, 0, NULL, NULL, NULL);",
        "INSERT INTO pg_catalog.pg_type  VALUES (28, 'xid', 11, 10, 4, true, 'b', 'U', false, true, ',', 0, '-', 0, 1011, 'xidin', 'xidout', 'xidrecv', 'xidsend', '-', '-', '-', 'i', 'p', false, 0, -1, 0, 0, NULL, NULL, NULL);",
        "INSERT INTO pg_catalog.pg_type  VALUES (29, 'cid', 11, 10, 4, true, 'b', 'U', false, true, ',', 0, '-', 0, 1012, 'cidin', 'cidout', 'cidrecv', 'cidsend', '-', '-', '-', 'i', 'p', false, 0, -1, 0, 0, NULL, NULL, NULL);",
    ];
    for stmt in payload {
        conn.execute(stmt, ())?;
    }
    Ok(())
}

impl LibSqlDb {
    pub fn new(
        path: impl AsRef<Path> + Send + 'static,
        wal_hook: impl WalHook + Send + Clone + 'static,
        with_bottomless: bool,
    ) -> crate::Result<Self> {
        let (sender, receiver) = crossbeam::channel::unbounded::<Message>();

        tokio::task::spawn_blocking(move || {
            let conn = open_db(path, wal_hook, with_bottomless).unwrap();
            init_postgres_tables(&conn).unwrap();
            let mut state = ConnectionState::initial();
            let mut timedout = false;
            loop {
                let Message { queries, resp } = match state.deadline() {
                    Some(deadline) => match receiver.recv_deadline(deadline) {
                        Ok(msg) => msg,
                        Err(RecvTimeoutError::Timeout) => {
                            warn!("transaction timed out");
                            rollback(&conn);
                            timedout = true;
                            state.reset();
                            continue;
                        }
                        Err(RecvTimeoutError::Disconnected) => break,
                    },
                    None => match receiver.recv() {
                        Ok(msg) => msg,
                        Err(_) => break,
                    },
                };

                if !timedout {
                    let mut results = Vec::with_capacity(queries.len());
                    for query in queries {
                        let result = handle_query(&conn, query, &mut state);
                        results.push(result);
                    }
                    ok_or_exit!(resp.send((results, state.state)));
                } else {
                    // fail all the queries in the batch with timeout error
                    let errors = (0..queries.len())
                        .map(|idx| Err(Error::LibSqlTxTimeout(idx)))
                        .collect();
                    ok_or_exit!(resp.send((errors, state.state)));
                    timedout = false;
                }
            }
        });

        Ok(Self { sender })
    }
}

#[async_trait::async_trait]
impl Database for LibSqlDb {
    async fn execute_batch(&self, queries: Queries) -> Result<(Vec<QueryResult>, State)> {
        let (resp, receiver) = oneshot::channel();
        let msg = Message { queries, resp };
        let _ = self.sender.send(msg);

        Ok(receiver.await?)
    }
}
