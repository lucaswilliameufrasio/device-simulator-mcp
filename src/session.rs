use std::{
    future::Future,
    sync::Arc,
    time::{Duration, Instant},
};

use rmcp::model::{CallToolResult, ContentBlock};
use tokio::sync::{Mutex, Semaphore};

/// One configured target per MCP process; clones share ordering and limits.
pub(crate) struct Session {
    queue: Semaphore,
    operation: Mutex<()>,
}

impl Default for Session {
    fn default() -> Self {
        Self {
            queue: Semaphore::new(8),
            operation: Mutex::new(()),
        }
    }
}

impl Session {
    pub async fn execute(
        self: &Arc<Self>,
        name: &'static str,
        deadline: Duration,
        future: impl Future<Output = CallToolResult>,
    ) -> CallToolResult {
        let Ok(_slot) = self.queue.try_acquire() else {
            return error("device queue is full; no action was submitted");
        };
        let started = Instant::now();
        let mut executing = false;
        let result = tokio::time::timeout(deadline, async {
            let _guard = self.operation.lock().await;
            executing = true;
            future.await
        })
        .await;
        tracing::debug!(
            operation = name,
            elapsed_ms = started.elapsed().as_millis(),
            timed_out = result.is_err(),
            "device operation completed"
        );
        match result {
            Ok(result) => result,
            Err(_) if executing => error(
                "operation deadline exceeded; an in-flight input may have been applied, do not retry blindly",
            ),
            Err(_) => error("queue wait deadline exceeded; no action was submitted"),
        }
    }
}

fn error(message: &str) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(message)])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn serializes_operations_across_clones() {
        let session = Arc::new(Session::default());
        let active = AtomicUsize::new(0);
        let operation = || async {
            assert_eq!(active.fetch_add(1, Ordering::SeqCst), 0);
            tokio::time::sleep(Duration::from_millis(10)).await;
            active.fetch_sub(1, Ordering::SeqCst);
            CallToolResult::success(vec![])
        };
        tokio::join!(
            session.execute("test", Duration::from_secs(1), operation()),
            session.execute("test", Duration::from_secs(1), operation()),
        );
    }

    #[tokio::test]
    async fn rejects_excess_pending_operations() {
        let session = Arc::new(Session::default());
        let _slots = session.queue.acquire_many(8).await.unwrap();
        let result = session
            .execute("test", Duration::from_secs(1), async {
                panic!("rejected operation must not run")
            })
            .await;
        assert_eq!(result.is_error, Some(true));
    }

    #[tokio::test]
    async fn releases_the_session_after_a_deadline() {
        let session = Arc::new(Session::default());
        let result = session
            .execute("test", Duration::from_millis(1), std::future::pending())
            .await;
        assert_eq!(result.is_error, Some(true));
        let result = session
            .execute("next", Duration::from_secs(1), async {
                CallToolResult::success(vec![])
            })
            .await;
        assert_eq!(result.is_error, Some(false));
    }
}
