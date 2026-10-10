use std::{process::Stdio, time::Instant};

use async_trait::async_trait;
use tokio::{io::AsyncReadExt, process::Command, time::timeout};

pub(crate) const MAX_OUTPUT_BYTES: usize = 24 * 1024 * 1024;
pub(crate) const COMMAND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

#[derive(Debug)]
pub(crate) struct CommandOutput {
    pub success: bool,
    pub status: String,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

#[async_trait]
pub(crate) trait CommandRunner: Send + Sync {
    async fn run_with_timeout(
        &self,
        command: &str,
        arguments: &[String],
        command_timeout: std::time::Duration,
    ) -> anyhow::Result<CommandOutput>;

    async fn run(&self, command: &str, arguments: &[String]) -> anyhow::Result<CommandOutput> {
        self.run_with_timeout(command, arguments, COMMAND_TIMEOUT)
            .await
    }
}

pub(crate) struct ProcessCommandRunner;

// Every command owns its process group on Unix. Dropping the future (MCP
// cancellation included) kills descendants as well as the immediate child.
struct ProcessGroup(Option<u32>);

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        let Some(pid) = self.0.take() else {
            return;
        };
        #[cfg(unix)]
        if let Ok(pid) = i32::try_from(pid) {
            // SAFETY: this is the new process group created for our own child.
            unsafe { libc::kill(-pid, libc::SIGKILL) };
        }
        #[cfg(not(unix))]
        let _ = pid;
    }
}

async fn read_bounded(reader: impl tokio::io::AsyncRead + Unpin) -> anyhow::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader
        .take((MAX_OUTPUT_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .await?;
    anyhow::ensure!(
        bytes.len() <= MAX_OUTPUT_BYTES,
        "command output exceeded the byte limit"
    );
    Ok(bytes)
}

/// Long-lived helper owned by this server. Terminated on startup cancellation,
/// explicit stop and server shutdown; never detached from the owning process.
pub(crate) struct ManagedProcess {
    pub child: tokio::process::Child,
    group: Option<u32>,
}

impl ManagedProcess {
    pub fn spawn(command: &str, arguments: &[String]) -> anyhow::Result<Self> {
        let mut builder = Command::new(command);
        builder
            .args(arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        #[cfg(unix)]
        builder.process_group(0);
        let child = builder.spawn()?;
        let group = child.id();
        Ok(Self { child, group })
    }
    pub async fn stop(&mut self) {
        self.kill_group();
        let _ = self.child.kill().await;
        let _ = self.child.wait().await;
    }
    fn kill_group(&mut self) {
        #[cfg(unix)]
        if let Some(pid) = self.group.take().and_then(|pid| i32::try_from(pid).ok()) {
            // SAFETY: this process group was created exclusively for our child.
            unsafe { libc::kill(-pid, libc::SIGKILL) };
        }
        #[cfg(not(unix))]
        self.group.take();
    }
}
impl Drop for ManagedProcess {
    fn drop(&mut self) {
        self.kill_group();
        let _ = self.child.start_kill();
    }
}

#[async_trait]
impl CommandRunner for ProcessCommandRunner {
    async fn run_with_timeout(
        &self,
        command: &str,
        arguments: &[String],
        command_timeout: std::time::Duration,
    ) -> anyhow::Result<CommandOutput> {
        let started = Instant::now();
        let mut builder = Command::new(command);
        builder
            .args(arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        builder.process_group(0);
        let mut child = builder.spawn()?;
        let group = ProcessGroup(child.id());
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let result = timeout(command_timeout, async {
            tokio::try_join!(
                async { Ok::<_, anyhow::Error>(child.wait().await?) },
                read_bounded(stdout),
                read_bounded(stderr),
            )
        })
        .await;
        let result = match result {
            Ok(result) => result,
            Err(_) => Err(anyhow::anyhow!(
                "command deadline exceeded; an input may have been applied, do not retry blindly"
            )),
        };
        if result.is_err() {
            drop(group);
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
        let (status, stdout, stderr) = result?;
        // Never log arguments, stdout/stderr, typed text or screen contents.
        tracing::debug!(
            program = command,
            elapsed_ms = started.elapsed().as_millis(),
            stdout_bytes = stdout.len(),
            stderr_bytes = stderr.len(),
            success = status.success(),
            "device command completed"
        );
        Ok(CommandOutput {
            success: status.success(),
            status: status.to_string(),
            stdout,
            stderr,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn captures_output_and_exit_status() {
        let output = ProcessCommandRunner
            .run("rustc", &["--version".to_owned()])
            .await
            .unwrap();
        assert!(output.success);
        assert!(output.stdout.starts_with(b"rustc"));
    }

    #[tokio::test]
    async fn bounds_output_before_unbounded_allocation() {
        let bytes = vec![0; MAX_OUTPUT_BYTES + 1];
        assert!(read_bounded(bytes.as_slice()).await.is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancellation_terminates_the_process_group() {
        let result = timeout(
            std::time::Duration::from_millis(100),
            ProcessCommandRunner.run("sleep", &["30".to_owned()]),
        )
        .await;
        assert!(result.is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn honors_a_command_specific_timeout() {
        let result = ProcessCommandRunner
            .run_with_timeout(
                "sleep",
                &["30".to_owned()],
                std::time::Duration::from_millis(50),
            )
            .await;
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("command deadline exceeded")
        );
    }
}
