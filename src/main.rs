use std::{path::PathBuf, sync::Arc, time::Duration};

use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::STANDARD as BASE64_STANDARD};
use rmcp::{
    ServiceExt,
    handler::server::wrapper::Parameters,
    model::{CallToolResult, ContentBlock},
    schemars, tool, tool_router,
    transport::stdio,
};
use tokio::{process::Command, time::timeout};

const DEFAULT_PREVIEW_URL: &str = "http://127.0.0.1:3100";
const PREVIEW_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct CaptureParameters {
    #[schemars(description = "Screenshot name without the .png extension")]
    name: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct TapParameters {
    #[schemars(description = "Normalized horizontal coordinate from 0 to 1")]
    x: f64,
    #[schemars(description = "Normalized vertical coordinate from 0 to 1")]
    y: f64,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct SwipeParameters {
    #[schemars(description = "Normalized starting horizontal coordinate")]
    x1: f64,
    #[schemars(description = "Normalized starting vertical coordinate")]
    y1: f64,
    #[schemars(description = "Normalized ending horizontal coordinate")]
    x2: f64,
    #[schemars(description = "Normalized ending vertical coordinate")]
    y2: f64,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct TypeParameters {
    #[schemars(description = "Text to type into the focused control")]
    text: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Platform {
    Ios,
    Android,
}

#[derive(Debug)]
struct CommandOutput {
    success: bool,
    status: String,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

#[async_trait]
trait CommandRunner: Send + Sync {
    async fn run(&self, command: &str, arguments: &[String]) -> anyhow::Result<CommandOutput>;
}

struct ProcessCommandRunner;

#[async_trait]
impl CommandRunner for ProcessCommandRunner {
    async fn run(&self, command: &str, arguments: &[String]) -> anyhow::Result<CommandOutput> {
        let output = Command::new(command).args(arguments).output().await?;
        Ok(CommandOutput {
            success: output.status.success(),
            status: output.status.to_string(),
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }
}

#[derive(Clone)]
struct DeviceSimulatorMcp {
    runner: Arc<dyn CommandRunner>,
}

impl Default for DeviceSimulatorMcp {
    fn default() -> Self {
        Self {
            runner: Arc::new(ProcessCommandRunner),
        }
    }
}

#[tool_router(server_handler)]
impl DeviceSimulatorMcp {
    #[tool(description = "Start inspection for the configured device")]
    async fn device_start(&self) -> CallToolResult {
        let platform = match configured_platform() {
            Ok(platform) => platform,
            Err(error) => return failure(error),
        };
        match start_device(platform, self.runner.as_ref()).await {
            Ok(message) => success(message),
            Err(error) => failure(error),
        }
    }

    #[tool(description = "Stop the active iOS inspection stream")]
    async fn device_stop(&self) -> CallToolResult {
        let platform = match configured_platform() {
            Ok(platform) => platform,
            Err(error) => return failure(error),
        };
        match platform {
            Platform::Ios => match run_serve_sim(self.runner.as_ref(), &["--kill"]).await {
                Ok(message) => success(message),
                Err(error) => failure(error),
            },
            Platform::Android => success(
                "Android emulator lifecycle is managed externally; no emulator was stopped."
                    .to_owned(),
            ),
        }
    }

    #[tool(description = "Show the configured device status")]
    async fn device_status(&self) -> CallToolResult {
        let platform = match configured_platform() {
            Ok(platform) => platform,
            Err(error) => return failure(error),
        };
        match status_device(platform, self.runner.as_ref()).await {
            Ok(message) => success(message),
            Err(error) => failure(error),
        }
    }

    #[tool(description = "Capture the current device display as a PNG image")]
    async fn device_capture(
        &self,
        Parameters(parameters): Parameters<CaptureParameters>,
    ) -> CallToolResult {
        let screenshot_name = parameters.name.as_deref().unwrap_or("device");
        if let Err(error) = validate_screenshot_name(screenshot_name) {
            return failure(error);
        }

        let platform = match configured_platform() {
            Ok(platform) => platform,
            Err(error) => return failure(error),
        };
        match capture_device(platform, screenshot_name, self.runner.as_ref()).await {
            Ok((image_bytes, description)) => CallToolResult::success(vec![
                ContentBlock::text(description),
                ContentBlock::image(BASE64_STANDARD.encode(image_bytes), "image/png"),
            ]),
            Err(error) => failure(error),
        }
    }

    #[tool(description = "Tap a normalized coordinate on the configured device")]
    async fn device_tap(
        &self,
        Parameters(parameters): Parameters<TapParameters>,
    ) -> CallToolResult {
        if let Err(error) = validate_coordinates(&[parameters.x, parameters.y]) {
            return failure(error);
        }

        let platform = match configured_platform() {
            Ok(platform) => platform,
            Err(error) => return failure(error),
        };
        match tap_device(platform, parameters.x, parameters.y, self.runner.as_ref()).await {
            Ok(message) => success(message),
            Err(error) => failure(error),
        }
    }

    #[tool(description = "Swipe between normalized coordinates on the device")]
    async fn device_swipe(
        &self,
        Parameters(parameters): Parameters<SwipeParameters>,
    ) -> CallToolResult {
        if let Err(error) =
            validate_coordinates(&[parameters.x1, parameters.y1, parameters.x2, parameters.y2])
        {
            return failure(error);
        }

        let platform = match configured_platform() {
            Ok(platform) => platform,
            Err(error) => return failure(error),
        };
        match swipe_device(platform, parameters, self.runner.as_ref()).await {
            Ok(message) => success(message),
            Err(error) => failure(error),
        }
    }

    #[tool(description = "Type text into the currently focused device control")]
    async fn device_type(
        &self,
        Parameters(parameters): Parameters<TypeParameters>,
    ) -> CallToolResult {
        if parameters.text.is_empty() {
            return failure(anyhow::anyhow!("text must not be empty"));
        }

        let platform = match configured_platform() {
            Ok(platform) => platform,
            Err(error) => return failure(error),
        };
        match type_on_device(platform, &parameters.text, self.runner.as_ref()).await {
            Ok(message) => success(message),
            Err(error) => failure(error),
        }
    }
}

fn configured_platform() -> anyhow::Result<Platform> {
    let platform = std::env::var("DEVICE_PLATFORM").unwrap_or_else(|_| "ios".to_owned());
    parse_platform(&platform)
}

fn parse_platform(platform: &str) -> anyhow::Result<Platform> {
    match platform.to_ascii_lowercase().as_str() {
        "ios" => Ok(Platform::Ios),
        "android" => Ok(Platform::Android),
        platform => Err(anyhow::anyhow!(
            "unsupported DEVICE_PLATFORM '{platform}'; use ios or android"
        )),
    }
}

async fn start_device(platform: Platform, runner: &dyn CommandRunner) -> anyhow::Result<String> {
    match platform {
        Platform::Ios => {
            run_serve_sim(runner, &["--detach", "--quiet", "--fit"]).await?;
            let preview_url =
                std::env::var("SERVE_SIM_URL").unwrap_or_else(|_| DEFAULT_PREVIEW_URL.to_owned());
            wait_for_preview(&preview_url).await?;
            Ok(preview_url)
        }
        Platform::Android => {
            run_adb_command(runner, &["start-server"]).await?;
            timeout(PREVIEW_TIMEOUT, run_adb(runner, &["wait-for-device"]))
                .await
                .map_err(|_| anyhow::anyhow!("Android Emulator did not become ready"))??;
            Ok("Android Emulator is ready".to_owned())
        }
    }
}

async fn status_device(platform: Platform, runner: &dyn CommandRunner) -> anyhow::Result<String> {
    match platform {
        Platform::Ios => run_serve_sim(runner, &["--list"]).await,
        Platform::Android => run_adb(runner, &["devices"]).await,
    }
}

async fn capture_device(
    platform: Platform,
    name: &str,
    runner: &dyn CommandRunner,
) -> anyhow::Result<(Vec<u8>, String)> {
    match platform {
        Platform::Ios => {
            let path = temporary_screenshot_path(name);
            let output = run_command(
                runner,
                "xcrun",
                &[
                    "simctl".to_owned(),
                    "io".to_owned(),
                    ios_device_target(),
                    "screenshot".to_owned(),
                    path.display().to_string(),
                ],
            )
            .await?;
            let image_bytes = if output.stdout.is_empty() {
                tokio::fs::read(&path).await?
            } else {
                output.stdout
            };
            let _ = tokio::fs::remove_file(&path).await;
            Ok((image_bytes, "Captured iOS Simulator display".to_owned()))
        }
        Platform::Android => {
            let image_bytes = run_adb_bytes(runner, &["exec-out", "screencap", "-p"]).await?;
            Ok((image_bytes, "Captured Android Emulator display".to_owned()))
        }
    }
}

async fn tap_device(
    platform: Platform,
    x: f64,
    y: f64,
    runner: &dyn CommandRunner,
) -> anyhow::Result<String> {
    match platform {
        Platform::Ios => {
            let x = x.to_string();
            let y = y.to_string();
            let arguments = ["tap", x.as_str(), y.as_str()];
            run_serve_sim(runner, &arguments).await
        }
        Platform::Android => {
            let (pixel_x, pixel_y) = android_pixels(x, y, runner).await?;
            run_adb(runner, &["shell", "input", "tap", &pixel_x, &pixel_y]).await
        }
    }
}

async fn swipe_device(
    platform: Platform,
    parameters: SwipeParameters,
    runner: &dyn CommandRunner,
) -> anyhow::Result<String> {
    match platform {
        Platform::Ios => {
            let gestures = [
                gesture_payload("begin", parameters.x1, parameters.y1),
                gesture_payload("move", parameters.x2, parameters.y2),
                gesture_payload("end", parameters.x2, parameters.y2),
            ];
            for gesture in gestures {
                run_serve_sim(runner, &["gesture", &gesture]).await?;
            }
            Ok("Swipe completed".to_owned())
        }
        Platform::Android => {
            let (start_x, start_y) = android_pixels(parameters.x1, parameters.y1, runner).await?;
            let (end_x, end_y) = android_pixels(parameters.x2, parameters.y2, runner).await?;
            run_adb(
                runner,
                &[
                    "shell", "input", "swipe", &start_x, &start_y, &end_x, &end_y, "300",
                ],
            )
            .await
        }
    }
}

async fn type_on_device(
    platform: Platform,
    text: &str,
    runner: &dyn CommandRunner,
) -> anyhow::Result<String> {
    match platform {
        Platform::Ios => run_serve_sim(runner, &["type", text]).await,
        Platform::Android => {
            let escaped_text = escape_android_text(text);
            run_adb(runner, &["shell", "input", "text", &escaped_text]).await
        }
    }
}

fn escape_android_text(text: &str) -> String {
    let mut escaped_text = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            ' ' => escaped_text.push_str("%s"),
            '\\' | '&' | ';' | '|' | '<' | '>' | '(' | ')' | '~' | '*' | '?' | '!' | '#' | '$'
            | '\'' | '"' => {
                escaped_text.push('\\');
                escaped_text.push(character);
            }
            _ => escaped_text.push(character),
        }
    }
    escaped_text
}

async fn android_pixels(
    x: f64,
    y: f64,
    runner: &dyn CommandRunner,
) -> anyhow::Result<(String, String)> {
    let output = run_adb(runner, &["shell", "wm", "size"]).await?;
    let dimensions = output
        .lines()
        .find_map(|line| line.rsplit_once(' ').map(|(_, value)| value.trim()))
        .ok_or_else(|| anyhow::anyhow!("could not determine Android display size"))?;
    let (width, height) = parse_display_size(dimensions)?;
    let pixel_x = (x * width).round().to_string();
    let pixel_y = (y * height).round().to_string();
    Ok((pixel_x, pixel_y))
}

async fn wait_for_preview(url: &str) -> anyhow::Result<()> {
    let address = url
        .strip_prefix("http://")
        .and_then(|value| value.split('/').next())
        .filter(|value| value.contains(':'))
        .ok_or_else(|| anyhow::anyhow!("preview URL must use http://host:port"))?;
    timeout(PREVIEW_TIMEOUT, async {
        loop {
            if tokio::net::TcpStream::connect(address).await.is_ok() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("preview did not become available at {url}"))
}

async fn run_serve_sim(runner: &dyn CommandRunner, arguments: &[&str]) -> anyhow::Result<String> {
    let mut command_arguments = vec!["--yes".to_owned(), "serve-sim".to_owned()];
    let device = std::env::var("IOS_SIMULATOR_UDID").ok();
    if let Some(device) = device {
        if arguments
            .first()
            .is_some_and(|argument| !argument.starts_with('-'))
        {
            command_arguments.push(arguments[0].to_owned());
            command_arguments.extend(["--device".to_owned(), device]);
            command_arguments.extend(arguments[1..].iter().map(|argument| (*argument).to_owned()));
        } else {
            command_arguments.extend(arguments.iter().map(|argument| (*argument).to_owned()));
            command_arguments.push(device);
        }
    } else {
        command_arguments.extend(arguments.iter().map(|argument| (*argument).to_owned()));
    }
    run_command(runner, "npx", &command_arguments)
        .await
        .map(|output| {
            command_output(
                &String::from_utf8_lossy(&output.stdout),
                &String::from_utf8_lossy(&output.stderr),
            )
        })
}

fn ios_device_target() -> String {
    std::env::var("IOS_SIMULATOR_UDID").unwrap_or_else(|_| "booted".to_owned())
}

async fn run_adb(runner: &dyn CommandRunner, arguments: &[&str]) -> anyhow::Result<String> {
    let mut command_arguments = Vec::with_capacity(arguments.len() + 2);
    if let Ok(serial) = std::env::var("ANDROID_SERIAL") {
        command_arguments.extend(["-s".to_owned(), serial]);
    }
    command_arguments.extend(arguments.iter().map(|argument| (*argument).to_owned()));
    run_command(runner, "adb", &command_arguments)
        .await
        .map(|output| {
            command_output(
                &String::from_utf8_lossy(&output.stdout),
                &String::from_utf8_lossy(&output.stderr),
            )
        })
}

async fn run_adb_command(runner: &dyn CommandRunner, arguments: &[&str]) -> anyhow::Result<String> {
    let command_arguments = arguments
        .iter()
        .map(|argument| (*argument).to_owned())
        .collect::<Vec<_>>();
    run_command(runner, "adb", &command_arguments)
        .await
        .map(|output| {
            command_output(
                &String::from_utf8_lossy(&output.stdout),
                &String::from_utf8_lossy(&output.stderr),
            )
        })
}

fn parse_display_size(dimensions: &str) -> anyhow::Result<(f64, f64)> {
    let (width, height) = dimensions
        .split_once('x')
        .ok_or_else(|| anyhow::anyhow!("invalid Android display size: {dimensions}"))?;
    Ok((width.parse::<f64>()?, height.parse::<f64>()?))
}

async fn run_adb_bytes(runner: &dyn CommandRunner, arguments: &[&str]) -> anyhow::Result<Vec<u8>> {
    let mut command_arguments = Vec::with_capacity(arguments.len() + 2);
    if let Ok(serial) = std::env::var("ANDROID_SERIAL") {
        command_arguments.extend(["-s".to_owned(), serial]);
    }
    command_arguments.extend(arguments.iter().map(|argument| (*argument).to_owned()));
    let output = runner.run("adb", &command_arguments).await?;
    if !output.success {
        return Err(anyhow::anyhow!(
            "adb failed with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(output.stdout)
}

async fn run_command(
    runner: &dyn CommandRunner,
    command: &str,
    arguments: &[String],
) -> anyhow::Result<CommandOutput> {
    let output = runner.run(command, arguments).await?;
    let standard_output = String::from_utf8_lossy(&output.stdout);
    let standard_error = String::from_utf8_lossy(&output.stderr);
    if !output.success {
        return Err(anyhow::anyhow!(
            "{command} failed with {}: {}",
            output.status,
            command_output(&standard_output, &standard_error)
        ));
    }
    Ok(output)
}

fn temporary_screenshot_path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("device-simulator-mcp-{name}.png"))
}

fn validate_coordinates(coordinates: &[f64]) -> anyhow::Result<()> {
    if coordinates
        .iter()
        .any(|coordinate| !coordinate.is_finite() || !(0.0..=1.0).contains(coordinate))
    {
        return Err(anyhow::anyhow!(
            "coordinates must be finite normalized numbers between 0 and 1"
        ));
    }
    Ok(())
}

fn validate_screenshot_name(name: &str) -> anyhow::Result<()> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') || name.contains('\\') {
        return Err(anyhow::anyhow!(
            "name must be a non-empty file name without path separators"
        ));
    }
    Ok(())
}

fn gesture_payload(gesture_type: &str, x: f64, y: f64) -> String {
    format!(r#"{{"type":"{gesture_type}","x":{x},"y":{y}}}"#)
}

fn command_output(standard_output: &str, standard_error: &str) -> String {
    let output = standard_output.trim();
    if !output.is_empty() {
        return output.to_owned();
    }
    standard_error.trim().to_owned()
}

fn success(message: String) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(message)])
}

fn failure(error: anyhow::Error) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(error.to_string())])
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    let service = DeviceSimulatorMcp::default().serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex},
    };

    use async_trait::async_trait;
    use rmcp::handler::server::wrapper::Parameters;

    use super::{
        CaptureParameters, CommandOutput, CommandRunner, DeviceSimulatorMcp, Platform,
        SwipeParameters, TapParameters, TypeParameters, command_output, escape_android_text,
        gesture_payload, parse_display_size, parse_platform, status_device, swipe_device,
        tap_device, temporary_screenshot_path, type_on_device, validate_coordinates,
        validate_screenshot_name,
    };

    #[derive(Default)]
    struct FakeCommandRunner {
        outputs: Mutex<VecDeque<CommandOutput>>,
        calls: Mutex<Vec<(String, Vec<String>)>>,
    }

    impl FakeCommandRunner {
        fn with_outputs(outputs: Vec<CommandOutput>) -> Self {
            Self {
                outputs: Mutex::new(outputs.into()),
                calls: Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<(String, Vec<String>)> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl CommandRunner for FakeCommandRunner {
        async fn run(&self, command: &str, arguments: &[String]) -> anyhow::Result<CommandOutput> {
            self.calls
                .lock()
                .unwrap()
                .push((command.to_owned(), arguments.to_vec()));
            self.outputs
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| anyhow::anyhow!("fake command output was not configured"))
        }
    }

    fn successful_output(stdout: &str) -> CommandOutput {
        CommandOutput {
            success: true,
            status: "exit status: 0".to_owned(),
            stdout: stdout.as_bytes().to_vec(),
            stderr: Vec::new(),
        }
    }

    fn successful_bytes(stdout: &[u8]) -> CommandOutput {
        CommandOutput {
            success: true,
            status: "exit status: 0".to_owned(),
            stdout: stdout.to_vec(),
            stderr: Vec::new(),
        }
    }

    #[test]
    fn accepts_normalized_coordinates() {
        assert!(validate_coordinates(&[0.0, 0.5, 1.0]).is_ok());
    }

    #[test]
    fn rejects_coordinates_outside_normalized_range() {
        assert!(validate_coordinates(&[-0.1]).is_err());
        assert!(validate_coordinates(&[1.1]).is_err());
    }

    #[test]
    fn rejects_non_finite_coordinates() {
        assert!(validate_coordinates(&[f64::NAN]).is_err());
        assert!(validate_coordinates(&[f64::INFINITY]).is_err());
        assert!(validate_coordinates(&[f64::NEG_INFINITY]).is_err());
    }

    #[test]
    fn accepts_empty_coordinate_list() {
        assert!(validate_coordinates(&[]).is_ok());
    }

    #[test]
    fn rejects_screenshot_path_traversal() {
        assert!(validate_screenshot_name("../screenshot").is_err());
        assert!(validate_screenshot_name("nested/screenshot").is_err());
    }

    #[test]
    fn rejects_invalid_screenshot_names() {
        assert!(validate_screenshot_name("").is_err());
        assert!(validate_screenshot_name(".").is_err());
        assert!(validate_screenshot_name("..").is_err());
        assert!(validate_screenshot_name("nested\\screenshot").is_err());
    }

    #[test]
    fn accepts_safe_screenshot_names() {
        assert!(validate_screenshot_name("checkout-01").is_ok());
        assert!(validate_screenshot_name("screen_2.final").is_ok());
    }

    #[test]
    fn creates_serve_sim_gesture_payload() {
        assert_eq!(
            gesture_payload("begin", 0.5, 0.25),
            r#"{"type":"begin","x":0.5,"y":0.25}"#
        );
    }

    #[test]
    fn preserves_gesture_payload_precision_and_sign() {
        assert_eq!(
            gesture_payload("move", -0.125, 1.0),
            r#"{"type":"move","x":-0.125,"y":1}"#
        );
    }

    #[test]
    fn parses_android_display_size() {
        assert_eq!(parse_display_size("1080x2400").unwrap(), (1080.0, 2400.0));
    }

    #[test]
    fn rejects_invalid_android_display_sizes() {
        assert!(parse_display_size("1080").is_err());
        assert!(parse_display_size("1080x").is_err());
        assert!(parse_display_size("widthx2400").is_err());
    }

    #[test]
    fn parses_decimal_android_display_size() {
        assert_eq!(
            parse_display_size("1080.5x2400.25").unwrap(),
            (1080.5, 2400.25)
        );
    }

    #[test]
    fn escapes_android_text_for_adb_shell() {
        assert_eq!(escape_android_text("hello world!"), "hello%sworld\\!");
    }

    #[test]
    fn escapes_all_android_shell_sensitive_characters() {
        assert_eq!(
            escape_android_text("\\&;|<>()[~*?!#$'\""),
            "\\\\\\&\\;\\|\\<\\>\\(\\)[\\~\\*\\?\\!\\#\\$\\'\\\""
        );
    }

    #[test]
    fn preserves_android_text_without_sensitive_characters() {
        assert_eq!(escape_android_text("hello-world_42"), "hello-world_42");
        assert_eq!(escape_android_text(""), "");
    }

    #[test]
    fn rejects_unknown_platforms() {
        assert!(parse_platform("windows").is_err());
        assert_eq!(parse_platform("ANDROID").unwrap(), Platform::Android);
    }

    #[test]
    fn parses_platform_case_insensitively() {
        assert_eq!(parse_platform("IOS").unwrap(), Platform::Ios);
        assert_eq!(parse_platform("Android").unwrap(), Platform::Android);
    }

    #[test]
    fn rejects_platform_with_whitespace() {
        assert!(parse_platform(" ios ").is_err());
    }

    #[test]
    fn prefers_standard_output_when_command_succeeds() {
        assert_eq!(command_output("  success  ", "warning"), "success");
    }

    #[test]
    fn uses_standard_error_when_standard_output_is_empty() {
        assert_eq!(command_output("  ", "  warning  "), "warning");
    }

    #[test]
    fn returns_empty_command_output_when_both_streams_are_empty() {
        assert_eq!(command_output("  ", "\n"), "");
    }

    #[test]
    fn creates_temporary_screenshot_path() {
        let path = temporary_screenshot_path("checkout");
        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            Some("device-simulator-mcp-checkout.png")
        );
    }

    #[tokio::test]
    async fn reports_android_device_status_from_runner_output() {
        let runner = FakeCommandRunner::with_outputs(vec![successful_output(
            "List of devices attached\nemulator-5554\tdevice",
        )]);

        let status = status_device(Platform::Android, &runner).await.unwrap();

        assert_eq!(status, "List of devices attached\nemulator-5554\tdevice");
        assert_eq!(
            runner.calls(),
            vec![("adb".to_owned(), vec!["devices".to_owned()])]
        );
    }

    #[tokio::test]
    async fn taps_android_device_using_normalized_pixels() {
        let runner = FakeCommandRunner::with_outputs(vec![
            successful_output("Physical size: 1080x2400"),
            successful_output("tap completed"),
        ]);

        let result = tap_device(Platform::Android, 0.5, 0.25, &runner)
            .await
            .unwrap();

        assert_eq!(result, "tap completed");
        assert_eq!(
            runner.calls(),
            vec![
                (
                    "adb".to_owned(),
                    vec!["shell".to_owned(), "wm".to_owned(), "size".to_owned()]
                ),
                (
                    "adb".to_owned(),
                    vec![
                        "shell".to_owned(),
                        "input".to_owned(),
                        "tap".to_owned(),
                        "540".to_owned(),
                        "600".to_owned()
                    ]
                )
            ]
        );
    }

    #[tokio::test]
    async fn swipes_ios_device_with_begin_move_and_end_events() {
        let runner = FakeCommandRunner::with_outputs(vec![
            successful_output("begin"),
            successful_output("move"),
            successful_output("end"),
        ]);
        let parameters = super::SwipeParameters {
            x1: 0.1,
            y1: 0.2,
            x2: 0.8,
            y2: 0.9,
        };

        let result = swipe_device(Platform::Ios, parameters, &runner)
            .await
            .unwrap();

        assert_eq!(result, "Swipe completed");
        let calls = runner.calls();
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[0].0, "npx");
        assert!(calls[0].1[3].contains(r#""type":"begin""#));
        assert!(calls[1].1[3].contains(r#""type":"move""#));
        assert!(calls[2].1[3].contains(r#""type":"end""#));
    }

    #[tokio::test]
    async fn types_android_text_after_escaping_it() {
        let runner = FakeCommandRunner::with_outputs(vec![successful_output("")]);

        type_on_device(Platform::Android, "hello world!", &runner)
            .await
            .unwrap();

        assert_eq!(
            runner.calls()[0].1,
            vec![
                "shell".to_owned(),
                "input".to_owned(),
                "text".to_owned(),
                "hello%sworld\\!".to_owned()
            ]
        );
    }

    #[tokio::test]
    async fn captures_android_bytes_without_decoding_them() {
        let runner = FakeCommandRunner::with_outputs(vec![successful_bytes(&[0, 1, 2, 255])]);

        let (bytes, description) = super::capture_device(Platform::Android, "screen", &runner)
            .await
            .unwrap();

        assert_eq!(bytes, vec![0, 1, 2, 255]);
        assert_eq!(description, "Captured Android Emulator display");
    }

    #[tokio::test]
    async fn returns_runner_error_for_failed_android_command() {
        let runner = FakeCommandRunner::with_outputs(vec![CommandOutput {
            success: false,
            status: "exit status: 1".to_owned(),
            stdout: Vec::new(),
            stderr: b"device unavailable".to_vec(),
        }]);

        let result = status_device(Platform::Android, &runner).await;

        assert_eq!(
            result.unwrap_err().to_string(),
            "adb failed with exit status: 1: device unavailable"
        );
    }

    #[tokio::test]
    async fn delegates_android_tool_handlers_to_the_command_runner() {
        unsafe {
            std::env::set_var("DEVICE_PLATFORM", "android");
        }
        let runner = Arc::new(FakeCommandRunner::with_outputs(vec![
            successful_output("Android Emulator is ready"),
            successful_output("waited"),
            successful_output("List of devices attached"),
            successful_output("Physical size: 1080x2400"),
            successful_output("tap"),
            successful_output("Physical size: 1080x2400"),
            successful_output("Physical size: 1080x2400"),
            successful_output("swipe"),
            successful_output("type"),
            successful_bytes(b"png"),
        ]));
        let service = DeviceSimulatorMcp {
            runner: runner.clone(),
        };

        let start_result = service.device_start().await;
        let status_result = service.device_status().await;
        let tap_result = service
            .device_tap(Parameters(TapParameters { x: 0.5, y: 0.25 }))
            .await;
        let swipe_result = service
            .device_swipe(Parameters(SwipeParameters {
                x1: 0.1,
                y1: 0.2,
                x2: 0.8,
                y2: 0.9,
            }))
            .await;
        let type_result = service
            .device_type(Parameters(TypeParameters {
                text: "test".to_owned(),
            }))
            .await;
        let capture_result = service
            .device_capture(Parameters(CaptureParameters {
                name: Some("handler".to_owned()),
            }))
            .await;
        let stop_result = service.device_stop().await;

        assert_eq!(start_result.is_error, Some(false));
        assert_eq!(status_result.is_error, Some(false));
        assert_eq!(tap_result.is_error, Some(false));
        assert_eq!(swipe_result.is_error, Some(false));
        assert_eq!(type_result.is_error, Some(false));
        assert_eq!(capture_result.is_error, Some(false));
        assert_eq!(stop_result.is_error, Some(false));
        assert_eq!(runner.calls().len(), 10);
        unsafe {
            std::env::remove_var("DEVICE_PLATFORM");
        }
    }

    #[tokio::test]
    async fn rejects_empty_text_before_calling_external_commands() {
        unsafe {
            std::env::set_var("DEVICE_PLATFORM", "android");
        }
        let runner = Arc::new(FakeCommandRunner::default());
        let service = DeviceSimulatorMcp {
            runner: runner.clone(),
        };

        let result = service
            .device_type(Parameters(TypeParameters {
                text: String::new(),
            }))
            .await;

        assert_eq!(result.is_error, Some(true));
        assert!(runner.calls().is_empty());
        unsafe {
            std::env::remove_var("DEVICE_PLATFORM");
        }
    }
}
