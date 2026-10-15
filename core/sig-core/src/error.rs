/// Errors surfaced to Swift.
#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum CoreError {
    #[error("storage: {message}")]
    Storage { message: String },
    #[error("signal: {message}")]
    Signal { message: String },
    #[error("the core has stopped")]
    Closed,
}

impl From<sqlx::Error> for CoreError {
    fn from(e: sqlx::Error) -> Self {
        Self::Storage { message: e.to_string() }
    }
}
