use std::marker::PhantomData;

use crate::error::BlockingError;



/// Trait for different spawn behaviors
pub trait SpawnBlocking: Send + Sync + 'static {
    async fn spawn_blocking<F, T>(f: F) -> Result<T, BlockingError>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static;
}

/// Trait for local spawn behaviors
pub trait SpawnBlockingLocal: Send + Sync + 'static {
    async fn spawn_blocking_local<F, T>(f: F) -> Result<T, BlockingError>
    where
        F: FnOnce() -> T + 'static,
        T: 'static;
}

/// Scope for scoped operations
pub struct Scope<'scope> {
    pub _lifetime: PhantomData<&'scope ()>,
}

impl<'scope> Scope<'scope> {
    pub fn spawn<F, T>(&self, f: F) -> Result<T, BlockingError>
    where
        F: FnOnce() -> T + Send + 'scope,
        T: Send + 'scope,
    {
        Ok(f())
    }
}

/// Scoped spawning (non-'static lifetimes)
pub trait SpawnBlockingScoped {
    fn scope<'scope, F, R>(f: F) -> R
    where
        F: FnOnce(&Scope<'scope>) -> R;
}

/// Immediate execution (no spawning)
pub trait ExecuteImmediate {
    fn execute<T>(f: impl FnOnce() -> T) -> T {
        f()
    }
}

/// Rayon-style parallel execution
#[cfg(feature = "parallel")]
pub trait SpawnParallel: Send + Sync + 'static {
    fn join<A, B, RA, RB>(oper_a: A, oper_b: B) -> (RA, RB)
    where
        A: FnOnce() -> RA + Send,
        B: FnOnce() -> RB + Send,
        RA: Send,
        RB: Send;
}
