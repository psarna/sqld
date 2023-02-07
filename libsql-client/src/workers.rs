use base64::Engine;
use worker::*;

use super::{parse_query_result, QueryResult, Statement};

/// Database connection. This is the main structure used to
/// communicate with the database.
#[derive(Clone, Debug)]
pub struct Connection {
    url: String,
    auth: String,
}

impl Connection {
    /// Establishes a database connection.
    ///
    /// # Arguments
    /// * `url` - URL of the database endpoint
    /// * `username` - database username
    /// * `pass` - user's password
    pub fn connect(
        url: impl Into<String>,
        username: impl Into<String>,
        pass: impl Into<String>,
    ) -> Self {
        let username = username.into();
        let pass = pass.into();
        let url = url.into();
        // Auto-update the URL to start with https:// if no protocol was specified
        let url = if !url.contains("://") {
            "https://".to_owned() + &url
        } else {
            url
        };
        Self {
            url,
            auth: format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD.encode(format!("{username}:{pass}"))
            ),
        }
    }

    /// Establishes a database connection from Cloudflare Workers context.
    /// Expects the context to contain the following secrets defined:
    /// * `LIBSQL_CLIENT_URL`
    /// * `LIBSQL_CLIENT_USER`
    /// * `LIBSQL_CLIENT_PASS`
    /// # Arguments
    /// * `ctx` - Cloudflare Workers route context
    pub fn connect_from_ctx<D>(ctx: &worker::RouteContext<D>) -> Result<Self> {
        Ok(Self::connect(
            ctx.secret("LIBSQL_CLIENT_URL")?.to_string(),
            ctx.secret("LIBSQL_CLIENT_USER")?.to_string(),
            ctx.secret("LIBSQL_CLIENT_PASS")?.to_string(),
        ))
    }

    /// Executes a single SQL statement
    ///
    /// # Arguments
    /// * `stmt` - the SQL statement
    ///
    /// # Examples
    ///
    /// ```
    /// # async fn f() {
    /// let db = libsql_client::Connection::connect("https://example.com", "admin", "s3cr3tp4ss");
    /// let result = db.execute("SELECT * FROM sqlite_master").await;
    /// let result_params = db
    ///     .execute(libsql_client::Statement::with_params(
    ///         "UPDATE t SET v = ? WHERE key = ?",
    ///         &[libsql_client::CellValue::Number(5), libsql_client::CellValue::Text("five".to_string())],
    ///     ))
    ///     .await;
    /// # }
    /// ```
    pub async fn execute(&self, stmt: impl Into<Statement>) -> Result<QueryResult> {
        let mut results = self.batch(std::iter::once(stmt)).await?;
        Ok(results.remove(0))
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
    pub async fn batch(
        &self,
        stmts: impl IntoIterator<Item = impl Into<Statement>>,
    ) -> Result<Vec<QueryResult>> {
        let mut headers = Headers::new();
        headers.append("Authorization", &self.auth).ok();
        // FIXME: serialize and deserialize with existing routines from sqld
        let mut body = "{\"statements\": [".to_string();
        let mut stmts_count = 0;
        for stmt in stmts {
            body += &format!("{},", stmt.into());
            stmts_count += 1;
        }
        if stmts_count > 0 {
            body.pop();
        }
        body += "]}";
        let request_init = RequestInit {
            body: Some(wasm_bindgen::JsValue::from_str(&body)),
            headers,
            cf: CfProperties::new(),
            method: Method::Post,
            redirect: RequestRedirect::Follow,
        };
        let req = Request::new_with_init(&self.url, &request_init)?;
        let response = Fetch::Request(req).send().await;
        let resp: String = response?.text().await?;
        let response_json: serde_json::Value = serde_json::from_str(&resp)?;
        match response_json {
            serde_json::Value::Array(results) => {
                if results.len() != stmts_count {
                    Err(worker::Error::from(format!(
                        "Response array did not contain expected {stmts_count} results"
                    )))
                } else {
                    let mut query_results: Vec<QueryResult> = Vec::with_capacity(stmts_count);
                    for (idx, result) in results.into_iter().enumerate() {
                        query_results.push(
                            parse_query_result(result, idx)
                                .map_err(|e| Error::from(format!("{e}")))?,
                        );
                    }

                    Ok(query_results)
                }
            }
            e => Err(worker::Error::from(format!(
                "Error: {} ({:?})",
                e, request_init.body
            ))),
        }
    }

    /// Executes an SQL transaction.
    /// Does not support nested transactions - do not use BEGIN or END
    /// inside a transaction.
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
    ///     .transaction(["CREATE TABLE t(id)", "INSERT INTO t VALUES (42)"])
    ///     .await;
    /// # }
    /// ```
    pub async fn transaction(
        &self,
        stmts: impl IntoIterator<Item = impl Into<Statement>>,
    ) -> Result<Vec<QueryResult>> {
        // TODO: Vec is not a good fit for popping the first element,
        // let's return a templated collection instead and let the user
        // decide where to store the result.
        let mut ret: Vec<QueryResult> = self
            .batch(
                std::iter::once(Statement::new("BEGIN"))
                    .chain(stmts.into_iter().map(|s| s.into()))
                    .chain(std::iter::once(Statement::new("END"))),
            )
            .await?
            .into_iter()
            .skip(1)
            .collect();
        ret.pop();
        Ok(ret)
    }
}
