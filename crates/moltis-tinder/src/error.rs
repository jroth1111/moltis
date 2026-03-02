use thiserror::Error;

#[derive(Debug, Error)]
pub enum TinderError {
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
    #[error("invalid funnel transition")]
    InvalidTransition,
    #[error("session lock held")]
    LockHeld,
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("image error: {0}")]
    Image(String),
}
