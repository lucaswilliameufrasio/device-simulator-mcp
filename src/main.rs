use std::{path::PathBuf, time::Duration};

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

#[derive(Clone, Default)]
struct DeviceSimulatorMcp;

#[tool_router(server_handler)]
impl DeviceSimulatorMcp {
    #[tool(description = "Start inspection for the configured device")]
    async fn device_start(&self) -> CallToolResult {
        match start_device().await {
            Ok(message) => success(message),
            Err(error) => failure(error),
        }
    }

    #[tool(description = "Stop the active iOS inspection stream")]
    async fn device_stop(&self) -> CallToolResult {
        match configured_platform() {
            Platform::Ios => match run_serve_sim(&["--kill"]).await {
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
        match status_device().await {
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

        match capture_device(screenshot_name).await {
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

        match tap_device(parameters.x, parameters.y).await {
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

        match swipe_device(parameters).await {
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

        match type_on_device(&parameters.text).await {
            Ok(message) => success(message),
            Err(error) => failure(error),
        }
    }
}

fn configured_platform() -> Platform {
    match std::env::var("DEVICE_PLATFORM")
        .unwrap_or_else(|_| "ios".to_owned())
        .to_ascii_lowercase()
        .as_str()
    {
        "android" => Platform::Android,
        _ => Platform::Ios,
    }
}

async fn start_device() -> anyhow::Result<String> {
    match configured_platform() {
        Platform::Ios => {
            run_serve_sim(&["--detach", "--quiet", "--fit"]).await?;
            let preview_url =
                std::env::var("SERVE_SIM_URL").unwrap_or_else(|_| DEFAULT_PREVIEW_URL.to_owned());
            wait_for_preview(&preview_url).await?;
            Ok(preview_url)
        }
        Platform::Android => {
            run_adb(&["start-server"]).await?;
            timeout(PREVIEW_TIMEOUT, run_adb(&["wait-for-device"]))
                .await
                .map_err(|_| anyhow::anyhow!("Android Emulator did not become ready"))??;
            Ok("Android Emulator is ready".to_owned())
        }
    }
}

async fn status_device() -> anyhow::Result<String> {
    match configured_platform() {
        Platform::Ios => run_serve_sim(&["--list"]).await,
        Platform::Android => run_adb(&["devices"]).await,
    }
}

async fn capture_device(name: &str) -> anyhow::Result<(Vec<u8>, String)> {
    match configured_platform() {
        Platform::Ios => {
            let path = temporary_screenshot_path(name);
            run_command(
                "xcrun",
                &[
                    "simctl".to_owned(),
                    "io".to_owned(),
                    "booted".to_owned(),
                    "screenshot".to_owned(),
                    path.display().to_string(),
                ],
            )
            .await?;
            let image_bytes = tokio::fs::read(&path).await?;
            let _ = tokio::fs::remove_file(&path).await;
            Ok((image_bytes, "Captured iOS Simulator display".to_owned()))
        }
        Platform::Android => {
            let image_bytes = run_adb_bytes(&["exec-out", "screencap", "-p"]).await?;
            Ok((image_bytes, "Captured Android Emulator display".to_owned()))
        }
    }
}

async fn tap_device(x: f64, y: f64) -> anyhow::Result<String> {
    match configured_platform() {
        Platform::Ios => {
            let x = x.to_string();
            let y = y.to_string();
            let arguments = ["tap", x.as_str(), y.as_str()];
            run_serve_sim(&arguments).await
        }
        Platform::Android => {
            let (pixel_x, pixel_y) = android_pixels(x, y).await?;
            run_adb(&["shell", "input", "tap", &pixel_x, &pixel_y]).await
        }
    }
}

async fn swipe_device(parameters: SwipeParameters) -> anyhow::Result<String> {
    match configured_platform() {
        Platform::Ios => {
            let gestures = [
                gesture_payload("begin", parameters.x1, parameters.y1),
                gesture_payload("move", parameters.x2, parameters.y2),
                gesture_payload("end", parameters.x2, parameters.y2),
            ];
            for gesture in gestures {
                run_serve_sim(&["gesture", &gesture]).await?;
            }
            Ok("Swipe completed".to_owned())
        }
        Platform::Android => {
            let (start_x, start_y) = android_pixels(parameters.x1, parameters.y1).await?;
            let (end_x, end_y) = android_pixels(parameters.x2, parameters.y2).await?;
            run_adb(&[
                "shell", "input", "swipe", &start_x, &start_y, &end_x, &end_y, "300",
            ])
            .await
        }
    }
}

async fn type_on_device(text: &str) -> anyhow::Result<String> {
    match configured_platform() {
        Platform::Ios => run_serve_sim(&["type", text]).await,
        Platform::Android => {
            let escaped_text = text.replace(' ', "%s");
            run_adb(&["shell", "input", "text", &escaped_text]).await
        }
    }
}

async fn android_pixels(x: f64, y: f64) -> anyhow::Result<(String, String)> {
    let output = run_adb(&["shell", "wm", "size"]).await?;
    let dimensions = output
        .lines()
        .find_map(|line| line.rsplit_once(' ').map(|(_, value)| value.trim()))
        .ok_or_else(|| anyhow::anyhow!("could not determine Android display size"))?;
    let (width, height) = dimensions
        .split_once('x')
        .ok_or_else(|| anyhow::anyhow!("invalid Android display size: {dimensions}"))?;
    let pixel_x = (x * width.parse::<f64>()?).round().to_string();
    let pixel_y = (y * height.parse::<f64>()?).round().to_string();
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

async fn run_serve_sim(arguments: &[&str]) -> anyhow::Result<String> {
    let mut command_arguments = vec!["--yes".to_owned(), "serve-sim".to_owned()];
    command_arguments.extend(arguments.iter().map(|argument| (*argument).to_owned()));
    run_command("npx", &command_arguments).await
}

async fn run_adb(arguments: &[&str]) -> anyhow::Result<String> {
    let mut command_arguments = Vec::with_capacity(arguments.len() + 2);
    if let Ok(serial) = std::env::var("ANDROID_SERIAL") {
        command_arguments.extend(["-s".to_owned(), serial]);
    }
    command_arguments.extend(arguments.iter().map(|argument| (*argument).to_owned()));
    run_command("adb", &command_arguments).await
}

async fn run_adb_bytes(arguments: &[&str]) -> anyhow::Result<Vec<u8>> {
    let mut command_arguments = Vec::with_capacity(arguments.len() + 2);
    if let Ok(serial) = std::env::var("ANDROID_SERIAL") {
        command_arguments.extend(["-s".to_owned(), serial]);
    }
    command_arguments.extend(arguments.iter().map(|argument| (*argument).to_owned()));
    let output = Command::new("adb").args(command_arguments).output().await?;
    if !output.status.success() {
        return Err(anyhow::anyhow!(
            "adb failed with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(output.stdout)
}

async fn run_command(command: &str, arguments: &[String]) -> anyhow::Result<String> {
    let output = Command::new(command).args(arguments).output().await?;
    let standard_output = String::from_utf8_lossy(&output.stdout);
    let standard_error = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        return Err(anyhow::anyhow!(
            "{command} failed with {}: {}",
            output.status,
            command_output(&standard_output, &standard_error)
        ));
    }
    Ok(command_output(&standard_output, &standard_error))
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

    let service = DeviceSimulatorMcp.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{gesture_payload, validate_coordinates, validate_screenshot_name};

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
    fn rejects_screenshot_path_traversal() {
        assert!(validate_screenshot_name("../screenshot").is_err());
        assert!(validate_screenshot_name("nested/screenshot").is_err());
    }

    #[test]
    fn creates_serve_sim_gesture_payload() {
        assert_eq!(
            gesture_payload("begin", 0.5, 0.25),
            r#"{"type":"begin","x":0.5,"y":0.25}"#
        );
    }
}
