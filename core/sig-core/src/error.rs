/// Errors surfaced to Swift.
#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum CoreError {
    #[error("storage: {message}")]
    Storage { message: String },
    /// Another Semaphore core already has this data directory open.
    #[error("the data is already open in another window or process")]
    AlreadyOpen,
    #[error("the core has stopped")]
    Closed,
}

impl From<sqlx::Error> for CoreError {
    fn from(e: sqlx::Error) -> Self {
        Self::Storage { message: e.to_string() }
    }
}
