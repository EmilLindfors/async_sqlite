use thiserror::Error;

#[derive(Error, Debug)]
pub enum DatabaseError {
    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("Pool error: {0}")]
    Pool(#[from] r2d2::Error),
    #[error("Blocking task error: {0}")]
    Blocking(#[from] BlockingError),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Migration error: {0}")]
    Migration(String),
}

#[derive(Error, Debug)]
pub enum BlockingError {
    #[error("Blocking task panicked")]
    TaskPanicked,
}

