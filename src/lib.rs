pub mod error;
pub mod spawn;
pub mod rayon;

use std::path::PathBuf;
use error::{BlockingError, DatabaseError};
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use spawn::{ExecuteImmediate, Scope, SpawnBlocking, SpawnBlockingLocal, SpawnBlockingScoped, SpawnParallel};
use std::sync::Arc;
use std::marker::PhantomData;
use thiserror::Error;




/// Tokio executor implementation
#[cfg(feature = "tokio-runtime")]
#[derive(Debug, Clone, Copy)]
pub struct TokioExecutor;

#[cfg(feature = "tokio-runtime")]
impl SpawnBlocking for TokioExecutor {
    async fn spawn_blocking<F, T>(f: F) -> Result<T, BlockingError>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        tokio::task::spawn_blocking(f)
            .await
            .map_err(|_| BlockingError::TaskPanicked)
    }
}

/// Immediate executor (no spawning)
pub struct ImmediateExecutor;
impl ExecuteImmediate for ImmediateExecutor {}

impl SpawnBlocking for ImmediateExecutor {
    async fn spawn_blocking<F, T>(f: F) -> Result<T, BlockingError>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        Ok(f())
    }
}

#[derive(Debug, Clone, Copy)]
pub struct AsyncStdExecutor;


impl SpawnBlocking for AsyncStdExecutor {
    async fn spawn_blocking<F, T>(f: F) -> Result<T, BlockingError>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        Ok(async_std::task::spawn_blocking(f).await)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct SmolExecutor;


impl SpawnBlocking for SmolExecutor {
    async fn spawn_blocking<F, T>(f: F) -> Result<T, BlockingError>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        Ok(smol::unblock(f).await)
    }
}



/// Local executor implementation
#[derive(Debug, Clone, Copy)]
pub struct LocalExecutor;

impl SpawnBlockingLocal for LocalExecutor {
    async fn spawn_blocking_local<F, T>(f: F) -> Result<T, BlockingError>
    where
        F: FnOnce() -> T + 'static,
        T: 'static,
    {
        Ok(f())
    }
}

/// Main SQLite adapter that works with any async runtime
pub struct SQLiteAdapter<E> {
    pool: Arc<Pool<SqliteConnectionManager>>,
    _executor: PhantomData<E>,
}

impl<E> Clone for SQLiteAdapter<E> {
    fn clone(&self) -> Self {
        Self {
            pool: Arc::clone(&self.pool),
            _executor: PhantomData,
        }
    }
}

/// Crossbeam scoped executor
pub struct CrossbeamExecutor;


impl SpawnBlocking for CrossbeamExecutor {
    async fn spawn_blocking<F, T>(f: F) -> Result<T, BlockingError>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        Ok(crossbeam::scope(|_| f()).map_err(|_| BlockingError::TaskPanicked)?)
    }
}

impl SpawnBlockingScoped for CrossbeamExecutor {
    fn scope<'scope, F, R>(f: F) -> R
    where
        F: FnOnce(&Scope<'scope>) -> R,
    {
        crossbeam::scope(|s| {
            let scope = Scope { _lifetime: PhantomData };
            f(&scope)
        })
        .unwrap()
    }
}

// Implementation for scoped operations
impl<E: SpawnBlockingScoped> SQLiteAdapter<E> {
    /// Execute queries with references to stack data
    pub fn with_scope<'scope, F, R>(&self, f: F) -> R
    where
        F: FnOnce(&Scope<'scope>) -> R,
    {
        E::scope(f)
    }

    /// Execute a query that can reference stack data
    pub fn execute_scoped<'scope, F, T>(
        &self,
        scope: &Scope<'scope>,
        f: F,
    ) -> Result<T, DatabaseError>
    where
        F: FnOnce(&rusqlite::Connection) -> Result<T, DatabaseError> + Send + 'scope,
        T: Send + 'scope,
    {
        let pool = Arc::clone(&self.pool);
        scope.spawn(move || {
            let conn = pool.get()?;
            f(&conn)
        })?
    }
}

// Implementation for parallel operations
#[cfg(feature = "parallel")]
impl<E: SpawnParallel> SQLiteAdapter<E> {
    /// Execute two queries in parallel
    pub fn execute_parallel<F1, F2, T1, T2>(
        &self,
        f1: F1,
        f2: F2,
    ) -> Result<(T1, T2), DatabaseError>
    where
        F1: FnOnce(&rusqlite::Connection) -> Result<T1, DatabaseError> + Send,
        F2: FnOnce(&rusqlite::Connection) -> Result<T2, DatabaseError> + Send,
        T1: Send,
        T2: Send,
    {
        let pool1 = Arc::clone(&self.pool);
        let pool2 = Arc::clone(&self.pool);
        
        let (r1, r2) = E::join(
            move || {
                let conn = pool1.get()?;
                f1(&conn)
            },
            move || {
                let conn = pool2.get()?;
                f2(&conn)
            },
        );
        
        Ok((r1?, r2?))
    }
}

// Implementation for thread-safe executors
impl<E: SpawnBlocking> SQLiteAdapter<E> {
    pub async fn new_threaded(config: DatabaseConfig) -> Result<Self, DatabaseError> {
        let manager = SqliteConnectionManager::file(&config.path)
            .with_flags(rusqlite::OpenFlags::SQLITE_OPEN_CREATE | rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE);

        let pool = Pool::builder()
            .max_size(config.max_connections)
            .build(manager)?;

        let pool = Arc::new(pool);
        let pool_clone = Arc::clone(&pool);
        E::spawn_blocking(move || {
            let _conn = pool_clone.get()?;
            Ok::<_, DatabaseError>(())
        })
        .await??;

        Ok(Self {
            pool,
            _executor: PhantomData,
        })
    }

    pub async fn execute<F, T>(&self, f: F) -> Result<T, DatabaseError>
    where
        F: FnOnce(&rusqlite::Connection) -> Result<T, DatabaseError> + Send + 'static,
        T: Send + 'static,
    {
        let pool = Arc::clone(&self.pool);
        E::spawn_blocking(move || {
            let conn = pool.get()?;
            f(&conn)
        })
        .await?
    }

    pub async fn transaction<F, T>(&self, f: F) -> Result<T, DatabaseError>
    where
        F: FnOnce(&mut rusqlite::Transaction) -> Result<T, DatabaseError> + Send + 'static,
        T: Send + 'static,
    {
        let pool = Arc::clone(&self.pool);
        E::spawn_blocking(move || {
            let mut conn = pool.get()?;
            let mut tx = conn.transaction()?;
            match f(&mut tx) {
                Ok(result) => {
                    tx.commit()?;
                    Ok(result)
                }
                Err(e) => Err(e),
            }
        })
        .await?
    }
}

// Implementation for local executors
impl<E: SpawnBlockingLocal> SQLiteAdapter<E> {
    pub async fn new_local(config: DatabaseConfig) -> Result<Self, DatabaseError> {
        let manager = SqliteConnectionManager::file(&config.path)
            .with_flags(rusqlite::OpenFlags::SQLITE_OPEN_CREATE | rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE);

        let pool = Pool::builder()
            .max_size(config.max_connections)
            .build(manager)?;

        let pool = Arc::new(pool);
        let pool_clone = Arc::clone(&pool);
        E::spawn_blocking_local(move || {
            let _conn = pool_clone.get()?;
            Ok::<_, DatabaseError>(())
        })
        .await??;

        Ok(Self {
            pool,
            _executor: PhantomData,
        })
    }

    pub async fn execute_local<F, T>(&self, f: F) -> Result<T, DatabaseError>
    where
        F: FnOnce(&rusqlite::Connection) -> Result<T, DatabaseError> + 'static,
        T: 'static,
    {
        let pool = Arc::clone(&self.pool);
        E::spawn_blocking_local(move || {
            let conn = pool.get()?;
            f(&conn)
        })
        .await?
    }

    pub async fn transaction_local<F, T>(&self, f: F) -> Result<T, DatabaseError>
    where
        F: FnOnce(&mut rusqlite::Transaction) -> Result<T, DatabaseError> + 'static,
        T: 'static,
    {
        let pool = Arc::clone(&self.pool);
        E::spawn_blocking_local(move || {
            let mut conn = pool.get()?;
            let mut tx = conn.transaction()?;
            match f(&mut tx) {
                Ok(result) => {
                    tx.commit()?;
                    Ok(result)
                }
                Err(e) => Err(e),
            }
        })
        .await?
    }
}

#[derive(Clone, Debug)]
pub struct DatabaseConfig {
    pub path: PathBuf,
    pub max_connections: u32,
    pub enable_foreign_keys: bool,
    pub busy_timeout: std::time::Duration,
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            path: PathBuf::from(":memory:"),
            max_connections: 10,
            enable_foreign_keys: true,
            busy_timeout: std::time::Duration::from_secs(30),
        }
    }
}

// Example usage
#[cfg(test)]
mod tests {
    use super::*;

    //#[cfg(feature = "tokio-runtime")]
    #[tokio::test]
    async fn test_tokio_adapter() -> Result<(), DatabaseError> {
        let config = DatabaseConfig::default();
        let db = SQLiteAdapter::<TokioExecutor>::new_threaded(config).await?;
        
        // Must be Send + 'static
        db.execute(|conn| {
            conn.execute_batch("CREATE TABLE test (id INTEGER PRIMARY KEY)")?;
            Ok(())
        })
        .await?;

        Ok(())
    }

    #[test]
    fn test_local_adapter() {
        let res = async_std::task::block_on(async {
            let config = DatabaseConfig::default();
            let db = SQLiteAdapter::<LocalExecutor>::new_local(config).await?;
            
            // Can use !Send types
            let rc = std::rc::Rc::new(42);
            db.execute_local(move |conn| {
                let _rc = rc.clone();
                conn.execute_batch("CREATE TABLE test (id INTEGER PRIMARY KEY)")?;
                Ok(())
            })
            .await?;

            Ok::<_, DatabaseError>(())
        });

        assert!(res.is_ok());
    }
}