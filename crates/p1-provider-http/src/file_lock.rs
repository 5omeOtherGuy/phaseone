//! A cross-process advisory file lock taken WITHOUT blocking the runtime thread.
//!
//! Credential refresh is single-flight across processes and agents: the holder
//! keeps the lock across its refresh request. On a current-thread runtime a
//! blocking `File::lock` in a second agent would stop the only thread — the
//! holder's request could then never complete and release the lock, and neither
//! cancellation nor Ctrl-C could run. So waiting here is `try_lock` plus an async
//! pause: the thread stays free, and dropping the future abandons the wait.

use std::fs::{File, TryLockError};
use std::io;
use std::time::Duration;

const POLL: Duration = Duration::from_millis(50);

/// How long [`lock_exclusive`] waits for another holder before giving up. A
/// refresh is one short HTTPS request; a lock held this long is a stuck process.
pub const LOCK_PATIENCE: Duration = Duration::from_secs(120);

/// Take the exclusive advisory lock on `file`, waiting up to `patience` for another
/// holder without ever blocking the thread. The lock is released when the returned
/// handle drops. `ErrorKind::TimedOut` when `patience` runs out.
pub async fn lock_exclusive(file: File, patience: Duration) -> io::Result<File> {
    let give_up = tokio::time::Instant::now() + patience;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(TryLockError::Error(error)) => return Err(error),
            Err(TryLockError::WouldBlock) => {
                if tokio::time::Instant::now() >= give_up {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "the lock is held by another process",
                    ));
                }
                tokio::time::sleep(POLL).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;
    use std::task::{Context, Poll, Waker};

    fn open(path: &std::path::Path) -> File {
        File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)
            .unwrap()
    }

    #[tokio::test(start_paused = true)]
    async fn waiting_never_blocks_the_thread_and_ends_when_the_holder_lets_go() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.lock");
        let holder = lock_exclusive(open(&path), LOCK_PATIENCE).await.unwrap();

        let mut waiter = Box::pin(lock_exclusive(open(&path), LOCK_PATIENCE));
        // A blocking lock would hang right here, on the only thread.
        let mut context = Context::from_waker(Waker::noop());
        assert!(matches!(waiter.as_mut().poll(&mut context), Poll::Pending));

        drop(holder);
        assert!(waiter.await.is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn patience_runs_out_with_timed_out() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.lock");
        let _holder = lock_exclusive(open(&path), LOCK_PATIENCE).await.unwrap();

        let error = lock_exclusive(open(&path), Duration::from_secs(3))
            .await
            .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }

    #[tokio::test(start_paused = true)]
    async fn an_abandoned_wait_leaves_no_lock_behind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.lock");
        let holder = lock_exclusive(open(&path), LOCK_PATIENCE).await.unwrap();
        {
            let mut waiter = Box::pin(lock_exclusive(open(&path), LOCK_PATIENCE));
            let mut context = Context::from_waker(Waker::noop());
            assert!(matches!(waiter.as_mut().poll(&mut context), Poll::Pending));
        } // cancelled while waiting
        drop(holder);

        assert!(open(&path).try_lock().is_ok());
    }
}
