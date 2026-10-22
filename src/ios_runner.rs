//! MCP-owned XCTest sessions for physical iOS devices.
pub(crate) mod prepare;
mod signing;
mod xcode;

use std::{
    collections::HashMap,
    fs::File,
    io::{Read, Seek, SeekFrom},
    net::{Ipv4Addr, SocketAddrV4, TcpListener},
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use anyhow::Context;
use serde_json::{Map, Value, json};
use tempfile::TempDir;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::Mutex,
    time::{Instant, timeout},
};

use crate::process::ManagedProcess;

const REMOTE_RUNNER_PORT: u16 = 39_173;
const MAX_REQUEST_BYTES: usize = 1_048_576;
const MAX_RESPONSE_BYTES: usize = 6_291_456;
const RUNNER_START_TIMEOUT: Duration = Duration::from_secs(240);
const RUNNER_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const RUNNER_POLL_INTERVAL: Duration = Duration::from_millis(250);
const MAX_XCODEBUILD_LOG_TAIL_BYTES: usize = 8192;
const RUNNER_BUNDLE_IDENTIFIER_TEMPLATE: &str = "org.devicesimulatormcp.runner.template";
const TEST_BUNDLE_IDENTIFIER_TEMPLATE: &str = "org.devicesimulatormcp.tests.template";
const TEAM_IDENTIFIER_TEMPLATE: &str = "TEAM_ID_HERE";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SigningMode {
    Automatic,
    Local,
}

#[derive(Clone, Default)]
pub(crate) struct Manager {
    sessions: Arc<Mutex<HashMap<String, Arc<RunningSession>>>>,
    xcode_sessions: Arc<Mutex<HashMap<String, Arc<xcode::XcodeSession>>>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Transport {
    Xcode,
    Iproxy,
}

pub(crate) fn configured_transport() -> anyhow::Result<Transport> {
    parse_transport(std::env::var("IOS_RUNNER_TRANSPORT").ok().as_deref())
}

fn parse_transport(value: Option<&str>) -> anyhow::Result<Transport> {
    match value {
        None | Some("xcode") => Ok(Transport::Xcode),
        Some("iproxy") => Ok(Transport::Iproxy),
        _ => anyhow::bail!("IOS_RUNNER_TRANSPORT must be xcode or iproxy"),
    }
}

struct RunningSession {
    stream: Mutex<TcpStream>,
    xcodebuild: Mutex<ManagedProcess>,
    iproxy: Mutex<ManagedProcess>,
    _project: TempDir,
    next_request_id: AtomicU64,
}

impl Manager {
    pub(crate) async fn start(
        &self,
        udid: &str,
        automation_udid: &str,
        app_bundle_identifier: Option<&str>,
    ) -> anyhow::Result<String> {
        validate_udid(udid)?;
        validate_udid(automation_udid)?;
        if let Some(bundle_identifier) = app_bundle_identifier {
            validate_bundle_identifier(bundle_identifier)?;
        }

        if configured_transport()? == Transport::Xcode {
            let team_id = configured_team_id()?;
            let previous = self.xcode_sessions.lock().await.remove(udid);
            if let Some(previous) = previous {
                previous.stop().await;
            }
            let session = Arc::new(xcode::XcodeSession::new(
                automation_udid,
                &team_id,
                app_bundle_identifier,
            )?);
            session.start().await?;
            if let Some(bundle_identifier) = app_bundle_identifier {
                let mut payload = Map::new();
                payload.insert("bundle_identifier".to_owned(), json!(bundle_identifier));
                session.request("launch", payload).await?;
            }
            self.xcode_sessions
                .lock()
                .await
                .insert(udid.to_owned(), session);
            return Ok(
                "Xcode-only iPhone control is ready; persistent XCTest uses app-container files without iproxy"
                    .to_owned(),
            );
        }

        let mut sessions = self.sessions.lock().await;
        if let Some(session) = sessions.get(udid) {
            if session.request("health", Map::new()).await.is_ok() {
                if let Some(bundle_identifier) = app_bundle_identifier {
                    session.launch(bundle_identifier.to_owned()).await?;
                }
                return Ok(format!(
                    "Physical iPhone XCTest runner is already running for {udid}"
                ));
            }
            let stale_session = sessions.remove(udid);
            if let Some(stale_session) = stale_session {
                stale_session.stop().await;
            }
        }

        let team_id = configured_team_id()?;
        let project = write_runner_project(&team_id)?;
        let allow_provisioning_updates =
            configure_physical_signing(project.path(), &team_id, automation_udid)?;
        let local_port = reserve_local_port()?;
        let iproxy_binary =
            std::env::var("IOS_IPROXY_BINARY").unwrap_or_else(|_| "iproxy".to_owned());
        let iproxy_arguments = iproxy_arguments(automation_udid, local_port);
        let mut iproxy = ManagedProcess::spawn(&iproxy_binary, &iproxy_arguments).map_err(|_| {
            anyhow::anyhow!(
                "iproxy transport requires iproxy; run brew install libusbmuxd, or select IOS_RUNNER_TRANSPORT=xcode"
            )
        })?;

        let result_bundle = project.path().join("runner.xcresult");
        let derived_data = derived_data_directory()?;
        let xcodebuild_arguments = xcodebuild_arguments(
            project.path(),
            &derived_data,
            &result_bundle,
            automation_udid,
            &team_id,
            allow_provisioning_updates,
        );
        let xcodebuild_log = project.path().join("xcodebuild.log");
        let xcodebuild_stdout = File::create(&xcodebuild_log)?;
        let xcodebuild_stderr = xcodebuild_stdout.try_clone()?;
        let xcodebuild = match ManagedProcess::spawn_with_stdio(
            "xcodebuild",
            &xcodebuild_arguments,
            Stdio::from(xcodebuild_stdout),
            Stdio::from(xcodebuild_stderr),
        ) {
            Ok(process) => process,
            Err(error) => {
                iproxy.stop().await;
                return Err(error).context("could not start the physical iPhone XCTest runner");
            }
        };
        let mut xcodebuild = xcodebuild;

        let stream = match wait_for_runner(local_port, &mut xcodebuild, &mut iproxy).await {
            Ok(stream) => stream,
            Err(error) => {
                xcodebuild.stop().await;
                let error =
                    with_xcodebuild_diagnostics(error, &xcodebuild_log, automation_udid, &team_id);
                iproxy.stop().await;
                return Err(error);
            }
        };

        let session = Arc::new(RunningSession {
            stream: Mutex::new(stream),
            xcodebuild: Mutex::new(xcodebuild),
            iproxy: Mutex::new(iproxy),
            _project: project,
            next_request_id: AtomicU64::new(1),
        });

        if let Err(error) = session.request("health", Map::new()).await {
            session.stop().await;
            return Err(error.context("XCTest runner did not pass its health check"));
        }
        if let Some(bundle_identifier) = app_bundle_identifier
            && let Err(error) = session.launch(bundle_identifier.to_owned()).await
        {
            session.stop().await;
            return Err(error);
        }

        sessions.insert(udid.to_owned(), session);
        Ok(format!(
            "Started the MCP-owned XCTest runner for physical iPhone {udid}"
        ))
    }

    pub(crate) async fn stop(&self, udid: &str) -> anyhow::Result<String> {
        let xcode_session = self.xcode_sessions.lock().await.remove(udid);
        if let Some(session) = xcode_session {
            session.stop().await;
            return Ok("Stopped the Xcode-only session; the iPhone was left running".to_owned());
        }
        let session = self.sessions.lock().await.remove(udid);
        let Some(session) = session else {
            return Ok("No MCP-owned XCTest session was running for this iPhone".to_owned());
        };
        session.stop().await;
        Ok(
            "Stopped the MCP-owned XCTest runner and USB forwarder; the iPhone was left running"
                .to_owned(),
        )
    }

    pub(crate) async fn is_running(&self, udid: &str) -> bool {
        self.sessions.lock().await.contains_key(udid)
            || self.xcode_sessions.lock().await.contains_key(udid)
    }

    pub(crate) async fn tap(&self, udid: &str, x: f64, y: f64) -> anyhow::Result<String> {
        let mut payload = Map::new();
        payload.insert("x".to_owned(), json!(x));
        payload.insert("y".to_owned(), json!(y));
        self.request(udid, "tap", payload).await?;
        Ok("Tap submitted to the MCP-owned XCTest runner".to_owned())
    }

    pub(crate) async fn swipe(
        &self,
        udid: &str,
        (x1, y1, x2, y2): (f64, f64, f64, f64),
    ) -> anyhow::Result<String> {
        let mut payload = Map::new();
        payload.insert("start_x".to_owned(), json!(x1));
        payload.insert("start_y".to_owned(), json!(y1));
        payload.insert("end_x".to_owned(), json!(x2));
        payload.insert("end_y".to_owned(), json!(y2));
        self.request(udid, "swipe", payload).await?;
        Ok("Swipe submitted to the MCP-owned XCTest runner".to_owned())
    }

    pub(crate) async fn type_text(&self, udid: &str, text: &str) -> anyhow::Result<String> {
        let mut payload = Map::new();
        payload.insert("text".to_owned(), json!(text));
        self.request(udid, "type", payload).await?;
        Ok("Text submitted to the MCP-owned XCTest runner".to_owned())
    }

    pub(crate) async fn accessibility(
        &self,
        udid: &str,
        max_elements: usize,
    ) -> anyhow::Result<Value> {
        let mut payload = Map::new();
        payload.insert("max_elements".to_owned(), json!(max_elements));
        self.request(udid, "accessibility", payload).await
    }

    pub(crate) async fn shutdown(&self) {
        let sessions = std::mem::take(&mut *self.xcode_sessions.lock().await);
        for session in sessions.into_values() {
            session.stop().await;
        }
        let sessions = {
            let mut active_sessions = self.sessions.lock().await;
            std::mem::take(&mut *active_sessions)
                .into_values()
                .collect::<Vec<_>>()
        };
        for session in sessions {
            session.stop().await;
        }
    }

    async fn request(
        &self,
        udid: &str,
        command: &str,
        payload: Map<String, Value>,
    ) -> anyhow::Result<Value> {
        let xcode_session = self.xcode_sessions.lock().await.get(udid).cloned();
        if let Some(session) = xcode_session {
            return session.request(command, payload).await;
        }
        let session = self
            .sessions
            .lock()
            .await
            .get(udid)
            .cloned()
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "physical iPhone XCTest runner is not started; call device_start first"
                )
            })?;
        let result = session.request(command, payload).await;
        if result.is_err() {
            let mut sessions = self.sessions.lock().await;
            if sessions
                .get(udid)
                .is_some_and(|active_session| Arc::ptr_eq(active_session, &session))
            {
                sessions.remove(udid);
            }
            drop(sessions);
            session.stop().await;
        }
        result
    }
}

impl RunningSession {
    async fn launch(&self, bundle_identifier: String) -> anyhow::Result<()> {
        let mut payload = Map::new();
        payload.insert("bundle_identifier".to_owned(), json!(bundle_identifier));
        self.request("launch", payload).await?;
        Ok(())
    }

    async fn request(&self, command: &str, payload: Map<String, Value>) -> anyhow::Result<Value> {
        let request_id = self.next_request_id.fetch_add(1, Ordering::Relaxed);
        request_on_stream(&self.stream, request_id.to_string(), command, payload).await
    }

    async fn stop(&self) {
        let _ = timeout(Duration::from_secs(2), self.request("shutdown", Map::new())).await;
        self.xcodebuild.lock().await.stop().await;
        self.iproxy.lock().await.stop().await;
    }
}

async fn request_on_stream(
    stream: &Mutex<TcpStream>,
    request_id: String,
    command: &str,
    payload: Map<String, Value>,
) -> anyhow::Result<Value> {
    let mut request = payload;
    request.insert("protocol_version".to_owned(), json!(1));
    request.insert("request_id".to_owned(), json!(request_id));
    request.insert("command".to_owned(), json!(command));
    let mut request_bytes = serde_json::to_vec(&request)?;
    anyhow::ensure!(
        request_bytes.len() <= MAX_REQUEST_BYTES,
        "XCTest runner request exceeded the byte limit"
    );
    request_bytes.push(b'\n');

    let mut stream = stream.lock().await;
    timeout(RUNNER_REQUEST_TIMEOUT, async {
        stream.write_all(&request_bytes).await?;
        let response_bytes = read_line_bounded(&mut stream, MAX_RESPONSE_BYTES).await?;
        let response: Value = serde_json::from_slice(&response_bytes)?;
        validate_response(response, &request_id)
    })
    .await
    .map_err(|_| anyhow::anyhow!("XCTest runner request timed out; do not retry input blindly"))?
}

fn validate_response(response: Value, request_id: &str) -> anyhow::Result<Value> {
    anyhow::ensure!(
        response.get("protocol_version").and_then(Value::as_u64) == Some(1),
        "XCTest runner returned an unsupported protocol version"
    );
    anyhow::ensure!(
        response.get("request_id").and_then(Value::as_str) == Some(request_id),
        "XCTest runner response did not match the request"
    );
    anyhow::ensure!(
        response.get("ok").and_then(Value::as_bool) == Some(true),
        "XCTest runner rejected the request: {}",
        response
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("unknown runner error")
    );
    response
        .get("result")
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("XCTest runner response omitted its result"))
}

async fn read_line_bounded(
    stream: &mut TcpStream,
    maximum_bytes: usize,
) -> anyhow::Result<Vec<u8>> {
    let mut line = Vec::new();
    let mut buffer = [0; 4096];
    loop {
        let read_count = stream.read(&mut buffer).await?;
        anyhow::ensure!(
            read_count > 0,
            "XCTest runner closed the protocol connection"
        );
        let received = &buffer[..read_count];
        if let Some(newline) = received.iter().position(|byte| *byte == b'\n') {
            anyhow::ensure!(
                newline + 1 == read_count,
                "XCTest runner returned multiple protocol responses"
            );
            line.extend_from_slice(&received[..newline]);
            anyhow::ensure!(
                line.len() <= maximum_bytes,
                "XCTest runner response exceeded the byte limit"
            );
            return Ok(line);
        }
        anyhow::ensure!(
            line.len().saturating_add(read_count) <= maximum_bytes,
            "XCTest runner response exceeded the byte limit"
        );
        line.extend_from_slice(received);
    }
}

async fn wait_for_runner(
    local_port: u16,
    xcodebuild: &mut ManagedProcess,
    iproxy: &mut ManagedProcess,
) -> anyhow::Result<TcpStream> {
    let deadline = Instant::now() + RUNNER_START_TIMEOUT;
    loop {
        if xcodebuild.child.try_wait()?.is_some() {
            anyhow::bail!(
                "xcodebuild exited before the physical iPhone runner was ready; check Xcode signing, device trust, and Developer Mode"
            );
        }
        if iproxy.child.try_wait()?.is_some() {
            anyhow::bail!(
                "iproxy exited before the physical iPhone runner was ready; verify the USB connection and pairing"
            );
        }
        if let Ok(Ok(stream)) = timeout(
            Duration::from_millis(500),
            TcpStream::connect(SocketAddrV4::new(Ipv4Addr::LOCALHOST, local_port)),
        )
        .await
        {
            let connection = Mutex::new(stream);
            let readiness = timeout(
                Duration::from_secs(2),
                request_on_stream(&connection, "startup".to_owned(), "health", Map::new()),
            )
            .await;
            if let Ok(Ok(result)) = readiness
                && result.get("state").and_then(Value::as_str) == Some("ready")
            {
                return Ok(connection.into_inner());
            }
        }
        if Instant::now() >= deadline {
            anyhow::bail!(
                "physical iPhone XCTest runner startup timed out; unlock the iPhone, enable Developer Mode, trust the signing certificate, and verify provisioning"
            );
        }
        tokio::time::sleep(RUNNER_POLL_INTERVAL).await;
    }
}

fn write_runner_project(team_id: &str) -> anyhow::Result<TempDir> {
    let directory = tempfile::tempdir()?;
    write_runner_project_to(directory.path(), team_id)?;
    Ok(directory)
}

pub(crate) fn write_runner_project_to(directory: &Path, team_id: &str) -> anyhow::Result<()> {
    validate_team_id(team_id)?;
    let team_suffix = team_id.to_ascii_lowercase();
    let runner_bundle_identifier = format!("org.devicesimulatormcp.runner.team{team_suffix}");
    let test_bundle_identifier = format!("org.devicesimulatormcp.tests.team{team_suffix}");
    let files = [
        (
            "DeviceRunner.xcodeproj/project.pbxproj",
            include_str!("../ios-runner/DeviceRunner.xcodeproj/project.pbxproj"),
        ),
        (
            "DeviceRunner.xcodeproj/xcshareddata/xcschemes/DeviceRunner.xcscheme",
            include_str!(
                "../ios-runner/DeviceRunner.xcodeproj/xcshareddata/xcschemes/DeviceRunner.xcscheme"
            ),
        ),
        (
            "DeviceRunner/DeviceRunnerApp.swift",
            include_str!("../ios-runner/DeviceRunner/DeviceRunnerApp.swift"),
        ),
        (
            "DeviceRunnerUITests/DeviceRunnerUITests.swift",
            include_str!("../ios-runner/DeviceRunnerUITests/DeviceRunnerUITests.swift"),
        ),
        (
            "DeviceRunnerUITests/DeviceRunnerCommandServer.swift",
            include_str!("../ios-runner/DeviceRunnerUITests/DeviceRunnerCommandServer.swift"),
        ),
    ];
    for (relative_path, contents) in files {
        let destination = directory.join(relative_path);
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let contents = if relative_path == "DeviceRunner.xcodeproj/project.pbxproj" {
            contents
                .replace(RUNNER_BUNDLE_IDENTIFIER_TEMPLATE, &runner_bundle_identifier)
                .replace(TEST_BUNDLE_IDENTIFIER_TEMPLATE, &test_bundle_identifier)
                .replace(TEAM_IDENTIFIER_TEMPLATE, team_id)
        } else {
            contents.to_owned()
        };
        std::fs::write(destination, contents)?;
    }
    Ok(())
}

fn runner_bundle_identifiers(team_id: &str) -> (String, String) {
    let team_suffix = team_id.to_ascii_lowercase();
    (
        format!("org.devicesimulatormcp.runner.team{team_suffix}"),
        format!("org.devicesimulatormcp.tests.team{team_suffix}.xctrunner"),
    )
}

fn configure_physical_signing(
    project_directory: &Path,
    team_id: &str,
    device_udid: &str,
) -> anyhow::Result<bool> {
    match configured_signing_mode()? {
        SigningMode::Automatic => configured_provisioning_updates(),
        SigningMode::Local => {
            let (runner_bundle_identifier, test_runner_bundle_identifier) =
                runner_bundle_identifiers(team_id);
            signing::validate_local_signing_profiles(
                team_id,
                device_udid,
                &runner_bundle_identifier,
                &test_runner_bundle_identifier,
            )?;
            let project_file = project_directory.join("DeviceRunner.xcodeproj/project.pbxproj");
            let project_contents = std::fs::read_to_string(&project_file)?;
            validate_local_automatic_signing_settings(&project_contents, team_id)?;
            Ok(false)
        }
    }
}

fn validate_local_automatic_signing_settings(
    project_file: &str,
    team_id: &str,
) -> anyhow::Result<()> {
    validate_team_id(team_id)?;
    anyhow::ensure!(
        project_file.matches("CODE_SIGN_STYLE = Automatic;").count() == 4,
        "Xcode runner project must use Xcode-managed automatic signing for both targets"
    );
    anyhow::ensure!(
        project_file
            .matches(&format!("DEVELOPMENT_TEAM = {team_id};"))
            .count()
            == 4,
        "Xcode runner project must select the configured team for both targets"
    );
    anyhow::ensure!(
        !project_file.contains("PROVISIONING_PROFILE_SPECIFIER =")
            && !project_file.contains("CODE_SIGN_IDENTITY ="),
        "Xcode-managed profiles cannot be assigned through manual signing settings"
    );
    Ok(())
}

fn derived_data_directory() -> anyhow::Result<PathBuf> {
    if let Some(path) = std::env::var_os("IOS_RUNNER_CACHE_DIR") {
        let path = PathBuf::from(path);
        std::fs::create_dir_all(&path)?;
        return Ok(path.join(env!("CARGO_PKG_VERSION")));
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("HOME is required to cache the XCTest runner"))?;
    let path = home
        .join("Library")
        .join("Caches")
        .join("device-simulator-mcp")
        .join("ios-runner")
        .join(env!("CARGO_PKG_VERSION"));
    std::fs::create_dir_all(&path)?;
    Ok(path)
}

fn configured_team_id() -> anyhow::Result<String> {
    let team_id = std::env::var("IOS_DEVICE_TEAM_ID")
        .map_err(|_| anyhow::anyhow!("physical iPhone runner requires IOS_DEVICE_TEAM_ID"))?;
    validate_team_id(&team_id)?;
    Ok(team_id)
}

pub(crate) fn validate_team_id(team_id: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        team_id.len() == 10 && team_id.bytes().all(|byte| byte.is_ascii_alphanumeric()),
        "Apple Developer Team ID must contain exactly 10 letters or digits"
    );
    Ok(())
}

fn validate_udid(udid: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        udid.len() <= 128
            && !udid.is_empty()
            && udid
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-'),
        "physical iPhone UDID contains unsupported characters"
    );
    Ok(())
}

pub(crate) fn validate_bundle_identifier(value: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        value.len() <= 255
            && value.contains('.')
            && value.split('.').all(|part| {
                !part.is_empty()
                    && part
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            }),
        "app_bundle_id must be a valid reverse-DNS bundle identifier"
    );
    Ok(())
}

fn reserve_local_port() -> anyhow::Result<u16> {
    let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))?;
    Ok(listener.local_addr()?.port())
}

fn iproxy_arguments(udid: &str, local_port: u16) -> Vec<String> {
    vec![
        "-u".to_owned(),
        udid.to_owned(),
        "-s".to_owned(),
        "127.0.0.1".to_owned(),
        format!("{local_port}:{REMOTE_RUNNER_PORT}"),
    ]
}

fn xcodebuild_arguments(
    project_directory: &Path,
    derived_data: &Path,
    result_bundle: &Path,
    udid: &str,
    team_id: &str,
    allow_provisioning_updates: bool,
) -> Vec<String> {
    let mut arguments = vec![
        "test".to_owned(),
        "-quiet".to_owned(),
        "-project".to_owned(),
        project_directory
            .join("DeviceRunner.xcodeproj")
            .display()
            .to_string(),
        "-scheme".to_owned(),
        "DeviceRunner".to_owned(),
        "-destination".to_owned(),
        format!("platform=iOS,id={udid}"),
        "-destination-timeout".to_owned(),
        "30".to_owned(),
        "-derivedDataPath".to_owned(),
        derived_data.display().to_string(),
        "-resultBundlePath".to_owned(),
        result_bundle.display().to_string(),
        "-parallel-testing-enabled".to_owned(),
        "NO".to_owned(),
        "-test-timeouts-enabled".to_owned(),
        "NO".to_owned(),
        "-only-testing:DeviceRunnerUITests/DeviceRunnerUITests/testPersistentCommandServer"
            .to_owned(),
        format!("DEVELOPMENT_TEAM={team_id}"),
        "CODE_SIGN_STYLE=Automatic".to_owned(),
        "CODE_SIGNING_ALLOWED=YES".to_owned(),
    ];
    if allow_provisioning_updates {
        arguments.push("-allowProvisioningUpdates".to_owned());
        arguments.push("-allowProvisioningDeviceRegistration".to_owned());
    }
    arguments
}

fn configured_provisioning_updates() -> anyhow::Result<bool> {
    parse_provisioning_updates(
        std::env::var("IOS_RUNNER_ALLOW_PROVISIONING_UPDATES")
            .ok()
            .as_deref(),
    )
}

fn parse_provisioning_updates(value: Option<&str>) -> anyhow::Result<bool> {
    match value {
        None | Some("false") => Ok(false),
        Some("true") => Ok(true),
        _ => anyhow::bail!("IOS_RUNNER_ALLOW_PROVISIONING_UPDATES must be true or false"),
    }
}

pub(crate) fn configured_signing_mode() -> anyhow::Result<SigningMode> {
    parse_signing_mode(std::env::var("IOS_RUNNER_SIGNING_MODE").ok().as_deref())
}

fn parse_signing_mode(value: Option<&str>) -> anyhow::Result<SigningMode> {
    match value {
        None | Some("automatic") => Ok(SigningMode::Automatic),
        Some("local") => Ok(SigningMode::Local),
        _ => anyhow::bail!("IOS_RUNNER_SIGNING_MODE must be automatic or local"),
    }
}

fn with_xcodebuild_diagnostics(
    error: anyhow::Error,
    log_path: &Path,
    udid: &str,
    team_id: &str,
) -> anyhow::Error {
    let Ok(mut log_file) = File::open(log_path) else {
        return error;
    };
    let Ok(log_size) = log_file.metadata().map(|metadata| metadata.len()) else {
        return error;
    };
    let start = log_size.saturating_sub(MAX_XCODEBUILD_LOG_TAIL_BYTES as u64);
    if log_file.seek(SeekFrom::Start(start)).is_err() {
        return error;
    }
    let mut log_tail = Vec::new();
    if log_file
        .take(MAX_XCODEBUILD_LOG_TAIL_BYTES as u64)
        .read_to_end(&mut log_tail)
        .is_err()
    {
        return error;
    }
    let mut diagnostics = String::from_utf8_lossy(&log_tail).into_owned();
    for identifier in [udid, team_id] {
        diagnostics = diagnostics.replace(identifier, "[redacted]");
    }
    let diagnostics = diagnostics.trim();
    if diagnostics.is_empty() {
        return error;
    }
    anyhow::anyhow!("{error}; xcodebuild log tail:\n{diagnostics}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_reverse_dns_bundle_identifiers() {
        assert!(validate_bundle_identifier("com.example.app").is_ok());
        assert!(validate_bundle_identifier("com.apple.mobilesafari").is_ok());
        assert!(validate_bundle_identifier("com..app").is_err());
        assert!(validate_bundle_identifier("../app").is_err());
        assert!(validate_bundle_identifier("single").is_err());
    }

    #[test]
    fn defaults_to_xcode_and_requires_explicit_iproxy_selection() {
        assert_eq!(parse_transport(None).unwrap(), Transport::Xcode);
        assert_eq!(parse_transport(Some("xcode")).unwrap(), Transport::Xcode);
        assert_eq!(parse_transport(Some("iproxy")).unwrap(), Transport::Iproxy);
        assert!(parse_transport(Some("auto")).is_err());
        assert!(parse_transport(Some("")).is_err());
    }

    #[test]
    fn builds_iproxy_arguments_with_explicit_udid_and_ports() {
        assert_eq!(
            iproxy_arguments("device-123", 42_317),
            ["-u", "device-123", "-s", "127.0.0.1", "42317:39173"]
        );
    }

    #[test]
    fn builds_xcodebuild_arguments_for_one_physical_udid() {
        let arguments = xcodebuild_arguments(
            Path::new("/runner"),
            Path::new("/derived"),
            Path::new("/results.xcresult"),
            "device-123",
            "ABCDEFGHIJ",
            false,
        );
        assert!(arguments.contains(&"platform=iOS,id=device-123".to_owned()));
        assert!(arguments.contains(&"DEVELOPMENT_TEAM=ABCDEFGHIJ".to_owned()));
        assert!(arguments.contains(&"CODE_SIGN_STYLE=Automatic".to_owned()));
        assert!(arguments.contains(&"-parallel-testing-enabled".to_owned()));
        assert!(arguments.contains(&"NO".to_owned()));
        assert!(
            !arguments
                .iter()
                .any(|argument| argument.starts_with("-allowProvisioning"))
        );
    }

    #[test]
    fn provisioning_and_device_registration_require_explicit_opt_in() {
        assert!(!parse_provisioning_updates(None).unwrap());
        assert!(!parse_provisioning_updates(Some("false")).unwrap());
        assert!(parse_provisioning_updates(Some("true")).unwrap());
        assert!(parse_provisioning_updates(Some("yes")).is_err());
        let arguments = xcodebuild_arguments(
            Path::new("/runner"),
            Path::new("/derived"),
            Path::new("/result.xcresult"),
            "device-123",
            "ABCDEFGHIJ",
            true,
        );
        assert!(arguments.contains(&"-allowProvisioningUpdates".to_owned()));
        assert!(arguments.contains(&"-allowProvisioningDeviceRegistration".to_owned()));
    }

    #[test]
    fn writes_team_scoped_bundle_identifiers_into_the_temporary_runner_project() {
        let project = write_runner_project("ABCDEFGHIJ").unwrap();
        let project_file = std::fs::read_to_string(
            project
                .path()
                .join("DeviceRunner.xcodeproj/project.pbxproj"),
        )
        .unwrap();

        assert!(project_file.contains("org.devicesimulatormcp.runner.teamabcdefghij"));
        assert!(project_file.contains("org.devicesimulatormcp.tests.teamabcdefghij"));
        assert!(project_file.contains("DEVELOPMENT_TEAM = ABCDEFGHIJ;"));
        assert!(!project_file.contains(RUNNER_BUNDLE_IDENTIFIER_TEMPLATE));
        assert!(!project_file.contains(TEST_BUNDLE_IDENTIFIER_TEMPLATE));
        assert!(!project_file.contains(TEAM_IDENTIFIER_TEMPLATE));
    }

    #[test]
    fn validates_xcode_managed_profiles_use_automatic_signing_without_overrides() {
        let project = write_runner_project("ABCDEFGHIJ").unwrap();
        let project_file = std::fs::read_to_string(
            project
                .path()
                .join("DeviceRunner.xcodeproj/project.pbxproj"),
        )
        .unwrap();

        validate_local_automatic_signing_settings(&project_file, "ABCDEFGHIJ").unwrap();

        assert_eq!(
            project_file.matches("CODE_SIGN_STYLE = Automatic;").count(),
            4
        );
        assert_eq!(
            project_file
                .matches("DEVELOPMENT_TEAM = ABCDEFGHIJ;")
                .count(),
            4
        );
        assert!(!project_file.contains("PROVISIONING_PROFILE_SPECIFIER ="));
        assert!(!project_file.contains("CODE_SIGN_IDENTITY ="));

        let manually_signed_project =
            project_file.replace("CODE_SIGN_STYLE = Automatic;", "CODE_SIGN_STYLE = Manual;");
        assert!(
            validate_local_automatic_signing_settings(&manually_signed_project, "ABCDEFGHIJ")
                .is_err()
        );
    }

    #[test]
    fn parses_local_signing_mode_without_changing_automatic_defaults() {
        assert_eq!(parse_signing_mode(None).unwrap(), SigningMode::Automatic);
        assert_eq!(
            parse_signing_mode(Some("automatic")).unwrap(),
            SigningMode::Automatic
        );
        assert_eq!(
            parse_signing_mode(Some("local")).unwrap(),
            SigningMode::Local
        );
        assert!(parse_signing_mode(Some("manual")).is_err());
    }

    #[test]
    fn bounds_and_redacts_xcodebuild_diagnostics() {
        let directory = tempfile::tempdir().unwrap();
        let log_path = directory.path().join("xcodebuild.log");
        let mut log = vec![b'x'; MAX_XCODEBUILD_LOG_TAIL_BYTES + 32];
        log.extend_from_slice(b"failed for device-123 with team ABCDEFGHIJ");
        std::fs::write(&log_path, log).unwrap();

        let error = with_xcodebuild_diagnostics(
            anyhow::anyhow!("runner startup failed"),
            &log_path,
            "device-123",
            "ABCDEFGHIJ",
        );
        let message = error.to_string();

        assert!(message.contains("xcodebuild log tail"));
        assert!(message.contains("[redacted]"));
        assert!(!message.contains("device-123"));
        assert!(!message.contains("ABCDEFGHIJ"));
        assert!(message.len() <= MAX_XCODEBUILD_LOG_TAIL_BYTES + 128);
    }

    #[tokio::test]
    async fn exchanges_one_bounded_request_and_response_with_the_runner() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            loop {
                let mut byte = [0; 1];
                stream.read_exact(&mut byte).await.unwrap();
                if byte[0] == b'\n' {
                    break;
                }
                request.push(byte[0]);
            }
            let request: Value = serde_json::from_slice(&request).unwrap();
            assert_eq!(request["protocol_version"], 1);
            assert_eq!(request["request_id"], "73");
            assert_eq!(request["command"], "health");
            stream
                .write_all(
                    br#"{"protocol_version":1,"request_id":"73","ok":true,"result":{"state":"ready"}}
"#,
                )
                .await
                .unwrap();
        });
        let stream = TcpStream::connect(address).await.unwrap();
        let result = request_on_stream(&Mutex::new(stream), "73".to_owned(), "health", Map::new())
            .await
            .unwrap();
        assert_eq!(result["state"], "ready");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn rejects_runner_response_with_a_different_request_id() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            loop {
                let mut byte = [0; 1];
                stream.read_exact(&mut byte).await.unwrap();
                if byte[0] == b'\n' {
                    break;
                }
                request.push(byte[0]);
            }
            stream
                .write_all(
                    br#"{"protocol_version":1,"request_id":"wrong","ok":true,"result":{}}
"#,
                )
                .await
                .unwrap();
        });
        let stream = TcpStream::connect(address).await.unwrap();
        let result =
            request_on_stream(&Mutex::new(stream), "73".to_owned(), "health", Map::new()).await;
        assert!(result.is_err());
        server.await.unwrap();
    }
}
