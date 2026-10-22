//! Persistent XCTest controlled through the runner's app data container.
use std::sync::atomic::AtomicBool;

use super::*;
use crate::process::{CommandRunner, ProcessCommandRunner};

pub(super) struct XcodeSession {
    project: TempDir,
    udid: String,
    team_id: String,
    bundle_identifier: Option<String>,
    mailbox: String,
    process: Mutex<Option<ManagedProcess>>,
    operation: Mutex<()>,
    next_request_id: AtomicU64,
    failed: AtomicBool,
    simulator: bool,
}

struct InFlight<'a> {
    failed: &'a AtomicBool,
    completed: bool,
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        if !self.completed {
            self.failed.store(true, Ordering::SeqCst);
        }
    }
}

impl XcodeSession {
    pub(super) fn new(
        udid: &str,
        team_id: &str,
        bundle_identifier: Option<&str>,
    ) -> anyhow::Result<Self> {
        let project = write_runner_project(team_id)?;
        let mailbox = project
            .path()
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| anyhow::anyhow!("runner project directory has no valid file name"))?
            .to_owned();
        Ok(Self {
            project,
            udid: udid.to_owned(),
            team_id: team_id.to_owned(),
            bundle_identifier: bundle_identifier.map(str::to_owned),
            mailbox,
            process: Mutex::new(None),
            operation: Mutex::new(()),
            next_request_id: AtomicU64::new(1),
            failed: AtomicBool::new(false),
            simulator: false,
        })
    }

    fn runner_bundle_identifier(&self) -> String {
        format!(
            "org.devicesimulatormcp.tests.team{}.xctrunner",
            self.team_id.to_ascii_lowercase()
        )
    }

    pub(super) async fn start(&self) -> anyhow::Result<()> {
        let variables = format!(
            "<EnvironmentVariables><EnvironmentVariable key=\"DEVICE_RUNNER_MAILBOX\" value=\"{}\" isEnabled=\"YES\"/></EnvironmentVariables>",
            self.mailbox
        );
        let scheme = include_str!(
            "../../ios-runner/DeviceRunner.xcodeproj/xcshareddata/xcschemes/DeviceRunner.xcscheme"
        )
        .replace(
            "shouldUseLaunchSchemeArgsEnv=\"YES\"",
            "shouldUseLaunchSchemeArgsEnv=\"NO\"",
        )
        .replace("</TestAction>", &format!("{variables}</TestAction>"));
        tokio::fs::write(
            self.project
                .path()
                .join("DeviceRunner.xcodeproj/xcshareddata/xcschemes/DeviceRunner.xcscheme"),
            scheme,
        )
        .await?;
        let allow_provisioning_updates = if self.simulator {
            false
        } else {
            configure_physical_signing(self.project.path(), &self.team_id, &self.udid)?
        };
        let mut arguments = xcodebuild_arguments(
            self.project.path(),
            &self.project.path().join("derived"),
            &self.project.path().join("runner.xcresult"),
            &self.udid,
            &self.team_id,
            allow_provisioning_updates,
        );
        for argument in &mut arguments {
            if argument.starts_with("-only-testing:") {
                *argument =
                    "-only-testing:DeviceRunnerUITests/DeviceRunnerUITests/testFileCommandServer"
                        .to_owned();
            }
            if self.simulator && argument == &format!("platform=iOS,id={}", self.udid) {
                *argument = format!("platform=iOS Simulator,id={}", self.udid);
            }
            if self.simulator && argument == "CODE_SIGNING_ALLOWED=YES" {
                *argument = "CODE_SIGNING_ALLOWED=NO".to_owned();
            }
        }
        let log = File::create(self.project.path().join("xcodebuild.log"))?;
        let stderr = log.try_clone()?;
        *self.process.lock().await = Some(ManagedProcess::spawn_with_stdio(
            "xcodebuild",
            &arguments,
            Stdio::from(log),
            Stdio::from(stderr),
        )?);
        let deadline = Instant::now() + RUNNER_START_TIMEOUT;
        loop {
            let exited = {
                let mut process = self.process.lock().await;
                match process.as_mut() {
                    Some(process) => process.child.try_wait()?.is_some(),
                    None => true,
                }
            };
            if exited || Instant::now() >= deadline {
                self.stop_process().await;
                return Err(with_xcodebuild_diagnostics(
                    anyhow::anyhow!(
                        "Xcode file runner did not become ready; check signing, Developer Mode and device trust"
                    ),
                    &self.project.path().join("xcodebuild.log"),
                    &self.udid,
                    &self.team_id,
                ));
            }
            if let Ok(Some(ready)) = self.receive("ready.json").await
                && ready.get("mailbox").and_then(Value::as_str) == Some(self.mailbox.as_str())
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        self.request("health", Map::new()).await?;
        Ok(())
    }

    pub(super) async fn request(
        &self,
        command: &str,
        mut payload: Map<String, Value>,
    ) -> anyhow::Result<Value> {
        let _operation = self.operation.lock().await;
        anyhow::ensure!(
            !self.failed.load(Ordering::SeqCst),
            "Xcode file session has an uncertain previous command; stop/start it before further input"
        );
        let mut in_flight = InFlight {
            failed: &self.failed,
            completed: false,
        };
        let request_id = self
            .next_request_id
            .fetch_add(1, Ordering::Relaxed)
            .to_string();
        payload.insert("protocol_version".to_owned(), json!(1));
        payload.insert("request_id".to_owned(), json!(request_id));
        payload.insert("command".to_owned(), json!(command));
        if let Some(bundle_identifier) = &self.bundle_identifier {
            payload.insert(
                "initial_bundle_identifier".to_owned(),
                json!(bundle_identifier),
            );
        }
        let bytes = serde_json::to_vec(&payload)?;
        anyhow::ensure!(
            bytes.len() <= MAX_REQUEST_BYTES,
            "XCTest request exceeded the byte limit"
        );
        let result = timeout(Duration::from_secs(60), async {
            self.send(&bytes, &request_id).await?;
            loop {
                if let Some(response) = self.receive("response.json").await?
                    && response.get("request_id").and_then(Value::as_str)
                        == Some(request_id.as_str())
                {
                    return validate_response(response, &request_id);
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        })
        .await
        .map_err(|_| anyhow::anyhow!("Xcode file command timed out; do not retry input blindly"))?;
        in_flight.completed = result.is_ok();
        result
    }

    async fn simulator_mailbox(&self) -> anyhow::Result<PathBuf> {
        let output = ProcessCommandRunner
            .run(
                "xcrun",
                &[
                    "simctl".to_owned(),
                    "get_app_container".to_owned(),
                    self.udid.clone(),
                    self.runner_bundle_identifier(),
                    "data".to_owned(),
                ],
            )
            .await?;
        anyhow::ensure!(output.success, "Simulator runner container is unavailable");
        Ok(PathBuf::from(String::from_utf8(output.stdout)?.trim())
            .join("Documents")
            .join(&self.mailbox))
    }

    async fn send(&self, bytes: &[u8], request_id: &str) -> anyhow::Result<()> {
        // Commit only after the payload transfer completed. The runner leaves the
        // marker in place, so devicectl can finish applying its file metadata.
        self.upload("request.json", bytes).await?;
        let commit = serde_json::to_vec(&json!({"request_id":request_id}))?;
        self.upload("commit.json", &commit).await
    }

    async fn upload(&self, name: &str, bytes: &[u8]) -> anyhow::Result<()> {
        if self.simulator {
            let directory = self.simulator_mailbox().await?;
            let staged = directory.join(format!("{name}.upload"));
            tokio::fs::write(&staged, bytes).await?;
            tokio::fs::rename(staged, directory.join(name)).await?;
            return Ok(());
        }
        let source = self.project.path().join(name);
        tokio::fs::write(&source, bytes).await?;
        let arguments = copy_arguments(
            "to",
            &self.udid,
            &self.runner_bundle_identifier(),
            &source.display().to_string(),
            &format!("Documents/{}/{name}", self.mailbox),
        );
        let output = ProcessCommandRunner.run("xcrun", &arguments).await?;
        anyhow::ensure!(
            output.success,
            "could not submit Xcode file command; input may have been applied, do not retry blindly"
        );
        Ok(())
    }

    async fn receive(&self, name: &str) -> anyhow::Result<Option<Value>> {
        let path = if self.simulator {
            self.simulator_mailbox().await?.join(name)
        } else {
            let destination = self.project.path().join(format!("received-{name}"));
            // Never interpret a local copy left by an earlier transfer attempt.
            if destination.exists() {
                tokio::fs::remove_file(&destination).await?;
            }
            let arguments = copy_arguments(
                "from",
                &self.udid,
                &self.runner_bundle_identifier(),
                &format!("Documents/{}/{name}", self.mailbox),
                &destination.display().to_string(),
            );
            let output = ProcessCommandRunner.run("xcrun", &arguments).await?;
            if !output.success {
                return Ok(None);
            }
            destination
        };
        read_response(&path).await
    }

    pub(super) async fn stop(&self) {
        if !self.failed.load(Ordering::SeqCst) {
            let _ = timeout(Duration::from_secs(2), self.request("shutdown", Map::new())).await;
        }
        self.stop_process().await;
    }

    async fn stop_process(&self) {
        if let Some(mut process) = self.process.lock().await.take() {
            process.stop().await;
        }
    }
}

fn copy_arguments(
    direction: &str,
    udid: &str,
    bundle_identifier: &str,
    source: &str,
    destination: &str,
) -> Vec<String> {
    vec![
        "devicectl".to_owned(),
        "device".to_owned(),
        "copy".to_owned(),
        direction.to_owned(),
        "--device".to_owned(),
        udid.to_owned(),
        "--domain-type".to_owned(),
        "appDataContainer".to_owned(),
        "--domain-identifier".to_owned(),
        bundle_identifier.to_owned(),
        "--source".to_owned(),
        source.to_owned(),
        "--destination".to_owned(),
        destination.to_owned(),
    ]
}

async fn read_response(path: &Path) -> anyhow::Result<Option<Value>> {
    let file = match tokio::fs::File::open(path).await {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let mut bytes = Vec::new();
    file.take((MAX_RESPONSE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .await?;
    anyhow::ensure!(
        bytes.len() <= MAX_RESPONSE_BYTES,
        "XCTest response exceeded the byte limit"
    );
    Ok(Some(serde_json::from_slice(&bytes)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scopes_file_transfers_to_one_device_and_runner_container() {
        let arguments = copy_arguments(
            "to",
            "device-123",
            "test.runner.xctrunner",
            "/local/request.json",
            "Documents/session/request.json",
        );
        assert_eq!(&arguments[..4], ["devicectl", "device", "copy", "to"]);
        assert_eq!(arguments[5], "device-123");
        assert_eq!(arguments[9], "test.runner.xctrunner");
        assert_eq!(arguments[13], "Documents/session/request.json");
    }

    #[test]
    fn poisons_a_cancelled_or_uncertain_command_without_replaying_it() {
        let failed = AtomicBool::new(false);
        {
            let _in_flight = InFlight {
                failed: &failed,
                completed: false,
            };
        }
        assert!(failed.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn bounds_file_responses_and_rejects_malformed_json() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("response.json");
        assert!(read_response(&path).await.unwrap().is_none());
        std::fs::write(&path, b"invalid JSON").unwrap();
        assert!(read_response(&path).await.is_err());
        std::fs::write(&path, vec![b'x'; MAX_RESPONSE_BYTES + 1]).unwrap();
        assert!(
            read_response(&path)
                .await
                .unwrap_err()
                .to_string()
                .contains("byte limit")
        );
    }

    #[tokio::test]
    #[ignore = "requires an explicitly selected disposable iOS Simulator and Xcode"]
    async fn executes_real_xcode_commands_without_iproxy() {
        let udid = std::env::var("IOS_RUNNER_TEST_SIMULATOR_UDID").unwrap();
        let bundle_identifier = "org.devicesimulatormcp.runner.teamabcdefghij";
        let mut session = XcodeSession::new(&udid, "ABCDEFGHIJ", Some(bundle_identifier)).unwrap();
        session.simulator = true;
        session.start().await.unwrap();
        let mut launch = Map::new();
        launch.insert("bundle_identifier".to_owned(), json!(bundle_identifier));
        session.request("launch", launch).await.unwrap();
        let tree = session.request("accessibility", Map::new()).await.unwrap();
        let input = tree["elements"]
            .as_array()
            .unwrap()
            .iter()
            .find(|element| element["identifier"] == "runner-validation-input")
            .unwrap();
        let frame = &input["frame"];
        let application = &tree["application_frame"];
        let horizontal = (frame["x"].as_f64().unwrap() + frame["width"].as_f64().unwrap() / 2.0)
            / application["width"].as_f64().unwrap();
        let vertical = (frame["y"].as_f64().unwrap() + frame["height"].as_f64().unwrap() / 2.0)
            / application["height"].as_f64().unwrap();
        let mut tap = Map::new();
        tap.insert("x".to_owned(), json!(horizontal));
        tap.insert("y".to_owned(), json!(vertical));
        session.request("tap", tap).await.unwrap();
        let mut text = Map::new();
        text.insert("text".to_owned(), json!("Xcode validation"));
        session.request("type", text).await.unwrap();
        let tree = session.request("accessibility", Map::new()).await.unwrap();
        assert!(
            tree["elements"]
                .as_array()
                .unwrap()
                .iter()
                .any(|element| element["identifier"] == "runner-validation-input"
                    && element["value"] == "Xcode validation")
        );
        let replay = serde_json::to_vec(&json!({
            "protocol_version":1, "request_id":"1", "command":"type",
            "initial_bundle_identifier":bundle_identifier, "text":"must not be replayed"
        }))
        .unwrap();
        session.send(&replay, "1").await.unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
        let tree = session.request("accessibility", Map::new()).await.unwrap();
        assert!(tree["elements"].as_array().unwrap().iter().any(|element| {
            element["identifier"] == "runner-validation-input"
                && element["value"] == "Xcode validation"
        }));
        let mut swipe = Map::new();
        swipe.insert("start_x".to_owned(), json!(0.5));
        swipe.insert("start_y".to_owned(), json!(0.8));
        swipe.insert("end_x".to_owned(), json!(0.5));
        swipe.insert("end_y".to_owned(), json!(0.6));
        session.request("swipe", swipe).await.unwrap();
        session.stop().await;
        assert!(session.process.lock().await.is_none());
    }
}
