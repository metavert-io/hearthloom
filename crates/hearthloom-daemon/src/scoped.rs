//! Request-scoped reads across threads. Whose vault view a read gets,
//! and which life areas an area-limited key may see, live in the store's
//! THREAD-LOCAL read scope (`hearthloom_core::store::ReadScope`), entered
//! around every poll of a request by `auth_middleware`. Work a request
//! hands to another thread or task must carry it — or it reads with no
//! viewer, i.e. LOCKED (and without the area filter). In request paths
//! use these instead of `tokio::task::spawn_blocking` / `tokio::spawn`.
//! Background work a request merely kicks off (a consolidation cycle, a
//! sync) keeps the plain spawns: it must not inherit anyone's unlock.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use hearthloom_core::store::{capture_read_scope, ReadScope};

/// `tokio::task::spawn_blocking`, reading as the caller's request.
pub fn spawn_blocking<F, R>(f: F) -> tokio::task::JoinHandle<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    let scope = capture_read_scope();
    tokio::task::spawn_blocking(move || {
        let _scope = scope.enter();
        f()
    })
}

/// `tokio::spawn`, polling the task inside the caller's read scope.
pub fn spawn<F>(future: F) -> tokio::task::JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    tokio::spawn(Scoped::new(capture_read_scope(), future))
}

/// Runs a future inside a read scope, entered around every poll: whichever
/// worker resumes it, its store calls read as the scope says, and nothing
/// lingers on the thread between polls.
pub struct Scoped<F> {
    scope: ReadScope,
    inner: Pin<Box<F>>,
}

impl<F> Scoped<F> {
    pub fn new(scope: ReadScope, inner: F) -> Self {
        Scoped { scope, inner: Box::pin(inner) }
    }
}

impl<F: Future> Future for Scoped<F> {
    type Output = F::Output;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<F::Output> {
        let this = &mut *self;
        let _scope = this.scope.enter();
        this.inner.as_mut().poll(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hearthloom_core::store::{enter_viewer, Store};
    use std::sync::Arc;

    #[tokio::test]
    async fn test_spawned_work_reads_as_its_request() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let unlocked = {
            let _viewer = enter_viewer("session:owner");
            store.vault_unlock(15);
            let (s1, s2) = (store.clone(), store.clone());
            (
                spawn_blocking(move || s1.vault_status().0),
                spawn(async move { s2.vault_status().0 }),
            )
        };
        assert!(!unlocked.0.await.unwrap(), "spawn_blocking carries the viewer");
        assert!(!unlocked.1.await.unwrap(), "spawn carries the viewer");
        let s3 = store.clone();
        let plain = tokio::task::spawn_blocking(move || s3.vault_status().0);
        assert!(plain.await.unwrap(), "work with no viewer reads locked");
    }
}
