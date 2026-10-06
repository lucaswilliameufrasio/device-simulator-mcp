//! Ownership-safe lifecycle for the default CLI backend. Existing streams are
//! reused; new foreground helpers belong to this MCP process, never detached.
use std::sync::{
    Arc, Weak,
    atomic::{AtomicU64, Ordering},
};
use std::time::Duration;

use crate::process::{CommandRunner, ManagedProcess};

const ENUMERATION_COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const OWNED_HELPER_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

pub(crate) struct Lifecycle {
    owned: Arc<tokio::sync::Mutex<Option<OwnedHelper>>>,
    next_id: AtomicU64,
    idle_timeout: Duration,
}

struct OwnedHelper {
    id: u64,
    process: ManagedProcess,
    backend: crate::ios::Backend,
    activity: tokio::sync::watch::Sender<()>,
}

impl Default for Lifecycle {
    fn default() -> Self {
        Self::with_idle_timeout(OWNED_HELPER_IDLE_TIMEOUT)
    }
}

impl Lifecycle {
    fn with_idle_timeout(idle_timeout: Duration) -> Self {
        Self {
            owned: Arc::new(tokio::sync::Mutex::new(None)),
            next_id: AtomicU64::new(0),
            idle_timeout,
        }
    }

    pub async fn start(&self, runner: &dyn CommandRunner) -> anyhow::Result<String> {
        let mut owned = self.owned.lock().await;
        if let Some(helper) = owned.as_mut() {
            let process = &mut helper.process;
            if process.child.try_wait()?.is_none() {
                helper.activity.send_replace(());
                helper.backend.status().await?;
                helper.activity.send_replace(());
                return Ok("MCP-owned serve-sim helper is running".to_owned());
            }
            owned.take();
        }
        let device = target(runner).await?;
        let (program, arguments) = crate::ios::command(vec!["--list".to_owned(), device.clone()])?;
        let output = runner
            .run_with_timeout(&program, &arguments, ENUMERATION_COMMAND_TIMEOUT)
            .await?;
        anyhow::ensure!(output.success, "could not enumerate serve-sim helpers");
        let state: serde_json::Value = serde_json::from_slice(&output.stdout)?;
        if state["running"] == true {
            let helper = state["streamUrl"]
                .as_str()
                .and_then(|url| url.strip_suffix("/stream.mjpeg"))
                .ok_or_else(|| anyhow::anyhow!("serve-sim stream has no helper endpoint"))?;
            let backend = crate::ios::Backend::for_helper(helper)?;
            backend.status().await?;
            return Ok("Reusing external serve-sim; device_stop will not stop it".to_owned());
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        drop(listener);
        let (program, arguments) = crate::ios::command(vec![
            "--no-preview".to_owned(),
            "--quiet".to_owned(),
            "--port".to_owned(),
            port.to_string(),
            device.clone(),
        ])?;
        let mut process = ManagedProcess::spawn(&program, &arguments)?;
        let helper = format!("http://127.0.0.1:{port}/helper/{device}");
        let backend = crate::ios::Backend::for_helper(&helper)?;
        let mut last_probe_error = None;
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                anyhow::ensure!(
                    process.child.try_wait()?.is_none(),
                    "serve-sim startup failed; install the pinned package and verify Xcode support"
                );
                match backend.status().await {
                    Ok(_) => return Ok::<_, anyhow::Error>(()),
                    Err(error) => last_probe_error = Some(error.to_string()),
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        })
        .await
        .map_err(|_| {
            let reason = last_probe_error
                .as_deref()
                .unwrap_or("no status probe completed");
            anyhow::anyhow!(
                "serve-sim readiness deadline exceeded; owned helper was stopped (last status probe: {reason})"
            )
        })??;
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (activity, activity_receiver) = tokio::sync::watch::channel(());
        *owned = Some(OwnedHelper {
            id,
            process,
            backend,
            activity,
        });
        Self::spawn_idle_shutdown(
            Arc::downgrade(&self.owned),
            id,
            activity_receiver,
            self.idle_timeout,
        );
        Ok(format!("MCP-owned serve-sim is ready at {helper}"))
    }

    pub async fn ensure_running(&self, runner: &dyn CommandRunner) -> anyhow::Result<String> {
        {
            let mut owned = self.owned.lock().await;
            if let Some(helper) = owned.as_mut() {
                if helper.process.child.try_wait()?.is_none() {
                    helper.activity.send_replace(());
                    return Ok("MCP-owned serve-sim helper is running".to_owned());
                }
                owned.take();
            }
        }
        self.start(runner).await
    }

    pub async fn stop(&self) -> String {
        let mut owned = self.owned.lock().await;
        if let Some(mut helper) = owned.take() {
            helper.process.stop().await;
            "Stopped MCP-owned serve-sim helper".to_owned()
        } else {
            "No MCP-owned helper to stop; external streams were not modified".to_owned()
        }
    }

    fn spawn_idle_shutdown(
        owned: Weak<tokio::sync::Mutex<Option<OwnedHelper>>>,
        id: u64,
        mut activity: tokio::sync::watch::Receiver<()>,
        idle_timeout: Duration,
    ) {
        tokio::spawn(async move {
            loop {
                match tokio::time::timeout(idle_timeout, activity.changed()).await {
                    Ok(Ok(())) => {}
                    Ok(Err(_)) => return,
                    Err(_) => {
                        if activity.has_changed().unwrap_or(false) {
                            continue;
                        }
                        let Some(owned) = owned.upgrade() else {
                            return;
                        };
                        let mut owned = owned.lock().await;
                        if owned.as_ref().is_some_and(|helper| helper.id == id)
                            && let Some(mut helper) = owned.take()
                        {
                            helper.process.stop().await;
                        }
                        return;
                    }
                }
            }
        });
    }
}

async fn target(runner: &dyn CommandRunner) -> anyhow::Result<String> {
    if let Ok(device) = std::env::var("IOS_SIMULATOR_UDID") {
        crate::ios::validate_udid(&device)?;
        return Ok(device);
    }
    let output = runner
        .run(
            "xcrun",
            &[
                "simctl".to_owned(),
                "list".to_owned(),
                "devices".to_owned(),
                "booted".to_owned(),
                "--json".to_owned(),
            ],
        )
        .await?;
    anyhow::ensure!(output.success, "could not resolve booted iOS Simulator");
    let value: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    let devices = value["devices"]
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("invalid Simulator inventory"))?
        .values()
        .filter_map(|runtime| runtime.as_array())
        .flatten()
        .filter(|device| device["state"] == "Booted")
        .filter_map(|device| device["udid"].as_str())
        .collect::<Vec<_>>();
    anyhow::ensure!(
        devices.len() == 1,
        "start requires exactly one booted Simulator; set IOS_SIMULATOR_UDID otherwise"
    );
    crate::ios::validate_udid(devices[0])?;
    Ok(devices[0].to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stop_without_ownership_never_touches_external_streams() {
        let lifecycle = Lifecycle::default();
        assert!(
            lifecycle
                .stop()
                .await
                .contains("external streams were not modified")
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stops_only_the_helper_owned_by_this_session() {
        let process = ManagedProcess::spawn("sleep", &["30".to_owned()]).unwrap();
        let pid = process.child.id().unwrap() as i32;
        let lifecycle = Lifecycle {
            owned: Arc::new(tokio::sync::Mutex::new(Some(OwnedHelper {
                id: 0,
                process,
                backend: crate::ios::Backend::for_helper("http://127.0.0.1:1/helper/test").unwrap(),
                activity: tokio::sync::watch::channel(()).0,
            }))),
            next_id: AtomicU64::new(1),
            idle_timeout: OWNED_HELPER_IDLE_TIMEOUT,
        };
        assert!(lifecycle.stop().await.contains("Stopped MCP-owned"));
        // SAFETY: signal 0 checks existence of this test's own process only.
        assert_ne!(unsafe { libc::kill(pid, 0) }, 0);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn idle_timeout_stops_only_owned_helper() {
        let process = ManagedProcess::spawn("sleep", &["30".to_owned()]).unwrap();
        let pid = process.child.id().unwrap() as i32;
        let (activity, receiver) = tokio::sync::watch::channel(());
        let lifecycle = Lifecycle {
            owned: Arc::new(tokio::sync::Mutex::new(Some(OwnedHelper {
                id: 7,
                process,
                backend: crate::ios::Backend::for_helper("http://127.0.0.1:1/helper/test").unwrap(),
                activity,
            }))),
            next_id: AtomicU64::new(8),
            idle_timeout: Duration::from_millis(80),
        };
        Lifecycle::spawn_idle_shutdown(
            Arc::downgrade(&lifecycle.owned),
            7,
            receiver,
            lifecycle.idle_timeout,
        );

        tokio::time::sleep(Duration::from_millis(120)).await;
        assert!(lifecycle.owned.lock().await.is_none());
        // SAFETY: signal 0 checks existence of this test's own process only.
        assert_ne!(unsafe { libc::kill(pid, 0) }, 0);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn helper_activity_resets_idle_timeout() {
        let process = ManagedProcess::spawn("sleep", &["30".to_owned()]).unwrap();
        let pid = process.child.id().unwrap() as i32;
        let (activity, receiver) = tokio::sync::watch::channel(());
        let lifecycle = Lifecycle {
            owned: Arc::new(tokio::sync::Mutex::new(Some(OwnedHelper {
                id: 9,
                process,
                backend: crate::ios::Backend::for_helper("http://127.0.0.1:1/helper/test").unwrap(),
                activity,
            }))),
            next_id: AtomicU64::new(10),
            idle_timeout: Duration::from_millis(120),
        };
        Lifecycle::spawn_idle_shutdown(
            Arc::downgrade(&lifecycle.owned),
            9,
            receiver,
            lifecycle.idle_timeout,
        );

        tokio::time::sleep(Duration::from_millis(70)).await;
        lifecycle
            .owned
            .lock()
            .await
            .as_ref()
            .unwrap()
            .activity
            .send_replace(());
        tokio::time::sleep(Duration::from_millis(70)).await;
        assert!(lifecycle.owned.lock().await.is_some());
        tokio::time::sleep(Duration::from_millis(70)).await;
        assert!(lifecycle.owned.lock().await.is_none());
        // SAFETY: signal 0 checks existence of this test's own process only.
        assert_ne!(unsafe { libc::kill(pid, 0) }, 0);
    }
}
