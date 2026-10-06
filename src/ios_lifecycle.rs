//! Ownership-safe lifecycle for the default CLI backend. Existing streams are
//! reused; new foreground helpers belong to this MCP process, never detached.
use std::time::Duration;

use crate::process::{CommandRunner, ManagedProcess};

#[derive(Default)]
pub(crate) struct Lifecycle {
    owned: tokio::sync::Mutex<Option<(ManagedProcess, crate::ios::Backend)>>,
}

impl Lifecycle {
    pub async fn start(&self, runner: &dyn CommandRunner) -> anyhow::Result<String> {
        let mut owned = self.owned.lock().await;
        if let Some((process, backend)) = owned.as_mut() {
            if process.child.try_wait()?.is_none() {
                backend.status().await?;
                return Ok("MCP-owned serve-sim helper is running".to_owned());
            }
            owned.take();
        }
        let device = target(runner).await?;
        let (program, arguments) = crate::ios::command(vec!["--list".to_owned(), device.clone()])?;
        let output = runner.run(&program, &arguments).await?;
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
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                anyhow::ensure!(
                    process.child.try_wait()?.is_none(),
                    "serve-sim startup failed; install the pinned package and verify Xcode support"
                );
                if backend.status().await.is_ok() {
                    return Ok::<_, anyhow::Error>(());
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        })
        .await
        .map_err(|_| {
            anyhow::anyhow!("serve-sim readiness deadline exceeded; owned helper was stopped")
        })??;
        *owned = Some((process, backend));
        Ok(format!("MCP-owned serve-sim is ready at {helper}"))
    }
    pub async fn stop(&self) -> String {
        let mut owned = self.owned.lock().await;
        if let Some((mut process, _backend)) = owned.take() {
            process.stop().await;
            "Stopped MCP-owned serve-sim helper".to_owned()
        } else {
            "No MCP-owned helper to stop; external streams were not modified".to_owned()
        }
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
            owned: tokio::sync::Mutex::new(Some((
                process,
                crate::ios::Backend::for_helper("http://127.0.0.1:1/helper/test").unwrap(),
            ))),
        };
        assert!(lifecycle.stop().await.contains("Stopped MCP-owned"));
        // SAFETY: signal 0 checks existence of this test's own process only.
        assert_ne!(unsafe { libc::kill(pid, 0) }, 0);
    }
}
