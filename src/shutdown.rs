//! Shared desktop lifetime accounting; Iced and core facts use one barrier.
pub use gitterm_core::shutdown::*;
use std::sync::OnceLock;

/// Tokio's default Runtime destructor waits forever for blocking jobs. Iced
/// must not enter that destructor on a timed-out quit. This runtime lives to
/// process exit; main's barrier supplies its bounded write/join boundary.
pub struct Executor(&'static tokio::runtime::Runtime);
impl Executor {
    pub fn runtime() -> Result<&'static tokio::runtime::Runtime, std::io::Error> {
        static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
        if let Some(runtime) = RUNTIME.get() {
            return Ok(runtime);
        }
        let runtime = tokio::runtime::Runtime::new()?;
        // Only the desktop main thread initializes this runtime.
        let _ = RUNTIME.set(runtime);
        Ok(RUNTIME.get().unwrap())
    }
}
impl iced::Executor for Executor {
    fn new() -> Result<Self, std::io::Error> {
        Self::runtime().map(Self)
    }
    fn spawn(&self, future: impl std::future::Future<Output = ()> + Send + 'static) {
        drop(self.0.spawn(future));
    }
    fn block_on<T>(&self, future: impl std::future::Future<Output = T>) -> T {
        self.0.block_on(future)
    }
    fn enter<R>(&self, f: impl FnOnce() -> R) -> R {
        let _entered = self.0.enter();
        f()
    }
}
