use super::{Meta, QueryResult, ResultSet, Row, Statement};
use async_trait::async_trait;
use std::collections::HashMap;

/// Database connection. This is the main structure used to
/// communicate with the database.
#[derive(Debug)]
pub struct Connection {
    inner: rusqlite::Connection,
}

impl Connection {
    /// Establishes a database connection.
    ///
    /// # Arguments
    /// * `path` - path of the local database
    pub fn connect(path: impl AsRef<std::path::Path>) -> anyhow::Result<Self> {
        Ok(Self {
            inner: rusqlite::Connection::open(path).map_err(|e| anyhow::anyhow!("{e}"))?,
        })
    }

    /// Executes a batch of SQL statements.
    /// Each statement is going to run in its own transaction,
    /// unless they're wrapped in BEGIN and END
    ///
    /// # Arguments
    /// * `stmts` - SQL statements
    ///
    /// # Examples
    ///
    /// ```
    /// # async fn f() {
    /// let db = libsql_client::Connection::connect("https://example.com", "admin", "s3cr3tp4ss");
    /// let result = db
    ///     .batch(["CREATE TABLE t(id)", "INSERT INTO t VALUES (42)"])
    ///     .await;
    /// # }
    /// ```
    async fn batch(
        &self,
        stmts: impl IntoIterator<Item = impl Into<Statement>>,
    ) -> anyhow::Result<Vec<QueryResult>> {
        let mut result = vec![];
        for stmt in stmts {
            let stmt = stmt.into();
            let sql_string = &stmt.q;
            let params = stmt.params;
            let mut stmt = self.inner.prepare(sql_string)?;
            let columns: Vec<String> = stmt
                .columns()
                .into_iter()
                .map(|c| c.name().to_string())
                .collect();
            // FIXME: params need to be passed as well
            let mut rows = Vec::new();
            let mut input_rows = stmt.query(())?;
            while let Some(row) = input_rows.next()? {
                println!("Row: {:?}", row);
                // FIXME: actually put stuff here
                rows.push(Row {
                    cells: HashMap::new(),
                })
            }
            let meta = Meta { duration: 0 };
            let result_set = ResultSet { columns, rows };
            result.push(QueryResult::Success((result_set, meta)))
        }
        Ok(result)
    }
}

#[async_trait(?Send)]
impl super::Connection for Connection {
    async fn batch(
        &self,
        stmts: impl IntoIterator<Item = impl Into<Statement>>,
    ) -> anyhow::Result<Vec<QueryResult>> {
        self.batch(stmts).await
    }
}
