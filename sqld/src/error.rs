use crate::query_result_builder::QueryResultBuilderError;

#[allow(clippy::enum_variant_names)]
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("LibSQL failed to bind provided query parameters: `{0}`")]
    LibSqlInvalidQueryParams(anyhow::Error),
    #[error("Transaction timed-out")]
    LibSqlTxTimeout,
    #[error("Server can't handle additional transactions")]
    LibSqlTxBusy,
    #[error(transparent)]
    IOError(#[from] std::io::Error),
    #[error(transparent)]
    LibsqlError(#[from] std::ffi::c_int),
    #[error("Failed to execute query via RPC. Error code: {}, message: {}", .0.code, .0.message)]
    RpcQueryError(crate::rpc::proxy::rpc::Error),
    #[error("Failed to execute queries via RPC protocol: `{0}`")]
    RpcQueryExecutionError(tonic::Status),
    #[error("Database value error: `{0}`")]
    DbValueError(String),
    // Dedicated for most generic internal errors. Please use it sparingly.
    // Consider creating a dedicate enum value for your error.
    #[error("Internal Error: `{0}`")]
    Internal(String),
    #[error("Invalid batch step: {0}")]
    InvalidBatchStep(usize),
    #[error("Not authorized to execute query: {0}")]
    NotAuthorized(String),
    #[error("The replicator exited, instance cannot make any progress.")]
    ReplicatorExited,
    #[error("Timed out while openning database connection")]
    DbCreateTimeout,
    #[error(transparent)]
    BuilderError(#[from] QueryResultBuilderError),
    #[error("Operation was blocked{}", .0.as_ref().map(|msg| format!(": {}", msg)).unwrap_or_default())]
    Blocked(Option<String>),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

impl From<tokio::sync::oneshot::error::RecvError> for Error {
    fn from(inner: tokio::sync::oneshot::error::RecvError) -> Self {
        Self::Internal(format!(
            "Failed to receive response via oneshot channel: {inner}"
        ))
    }
}

impl From<bincode::Error> for Error {
    fn from(other: bincode::Error) -> Self {
        Self::Internal(other.to_string())
    }
}

impl From<libsql::Error> for Error {
    fn from(other: libsql::Error) -> Self {
        match other {
            libsql::Error::ConnectionFailed(msg) => Self::Internal(msg),
            libsql::Error::PrepareFailed(msg1, msg2) => {
                Self::Internal(format!("Prepare failed: {msg1} {msg2}"))
            }
            libsql::Error::FetchRowFailed(msg) => Self::Internal(msg),
            libsql::Error::UnknownColumnType(col, typ) => Self::Internal(format!(
                "Unknown value type for column `{}`: `{}`",
                col, typ
            )),
            libsql::Error::NullValue => Self::Internal("The value is NULL".to_string()),
            libsql::Error::Misuse(msg) => Self::Internal(msg),
            libsql::Error::InvalidColumnName(name) => {
                Self::Internal(format!("Invalid column name: {}", name))
            }
            libsql::Error::LibError(code) => Self::LibsqlError(code),
        }
    }
}
