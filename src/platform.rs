//! Legacy platform CLI I/O. No MCP framework or presentation dependencies.
use crate::{
    device, ios,
    process::{CommandOutput, CommandRunner},
};
use std::{
    io,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

#[derive(Clone, Copy, Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Orientation {
    Portrait,
    PortraitUpsideDown,
    LandscapeLeft,
    LandscapeRight,
}

impl Orientation {
    pub(crate) fn serve_sim_value(self) -> &'static str {
        match self {
            Self::Portrait => "portrait",
            Self::PortraitUpsideDown => "portrait_upside_down",
            Self::LandscapeLeft => "landscape_left",
            Self::LandscapeRight => "landscape_right",
        }
    }

    fn android_rotation(self) -> u8 {
        match self {
            Self::Portrait => 0,
            Self::LandscapeLeft => 1,
            Self::PortraitUpsideDown => 2,
            Self::LandscapeRight => 3,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Platform {
    Ios,
    Android,
}

pub(crate) fn configured_platform() -> anyhow::Result<Platform> {
    parse_platform(&std::env::var("DEVICE_PLATFORM").unwrap_or_else(|_| "ios".to_owned()))
}

pub(crate) fn parse_platform(platform: &str) -> anyhow::Result<Platform> {
    match platform.to_ascii_lowercase().as_str() {
        "ios" => Ok(Platform::Ios),
        "android" => Ok(Platform::Android),
        platform => anyhow::bail!("unsupported DEVICE_PLATFORM '{platform}'; use ios or android"),
    }
}

pub(crate) async fn discover_ios_simulators(
    runner: &dyn CommandRunner,
) -> anyhow::Result<Vec<device::DeviceTarget>> {
    discover_ios_simulators_with_backend(runner, device::ios_simulator_backend()?).await
}

pub(crate) async fn discover_ios_simulators_with_backend(
    runner: &dyn CommandRunner,
    backend: device::IosSimulatorBackend,
) -> anyhow::Result<Vec<device::DeviceTarget>> {
    let output = run_command(
        runner,
        "xcrun",
        &[
            "simctl".to_owned(),
            "list".to_owned(),
            "devices".to_owned(),
            "--json".to_owned(),
        ],
    )
    .await?;
    let inventory: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    let groups = inventory
        .get("devices")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| anyhow::anyhow!("invalid iOS Simulator inventory"))?;
    let mut devices = Vec::new();
    for runtime in groups.values() {
        let Some(entries) = runtime.as_array() else {
            continue;
        };
        for entry in entries {
            let Some(udid) = entry.get("udid").and_then(serde_json::Value::as_str) else {
                continue;
            };
            if ios::validate_udid(udid).is_err() {
                continue;
            }
            let name = entry
                .get("name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("iOS Simulator")
                .to_owned();
            let state = entry
                .get("state")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown");
            let available = entry
                .get("isAvailable")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(true);
            let availability = if available {
                state.to_ascii_lowercase()
            } else {
                "unavailable".to_owned()
            };
            devices.push(device::DeviceTarget::IosSimulator {
                udid: udid.to_ascii_lowercase(),
                name,
                availability,
                backend: backend.clone(),
            });
        }
    }
    Ok(devices)
}

pub(crate) async fn discover_android_devices(
    runner: &dyn CommandRunner,
) -> anyhow::Result<Vec<device::DeviceTarget>> {
    let output = run_command(runner, "adb", &["devices".to_owned(), "-l".to_owned()]).await?;
    let listing = String::from_utf8_lossy(&output.stdout);
    let mut devices = Vec::new();
    for line in listing.lines().skip(1) {
        let mut columns = line.split_whitespace();
        let Some(serial) = columns.next() else {
            continue;
        };
        let Some(state) = columns.next() else {
            continue;
        };
        let model = columns
            .find_map(|column| column.strip_prefix("model:"))
            .map(|model| model.replace('_', " "));
        let name = model.unwrap_or_else(|| serial.to_owned());
        let availability = match state {
            "device" => "available",
            "offline" => "offline",
            "unauthorized" => "unauthorized",
            other => other,
        };
        devices.push(device::android_adb_target(
            serial.to_owned(),
            name,
            availability.to_owned(),
        ));
    }
    Ok(devices)
}

pub(crate) async fn start_device(
    platform: Platform,
    device_id: &str,
    runner: &dyn CommandRunner,
) -> anyhow::Result<String> {
    match platform {
        Platform::Ios => anyhow::bail!("iOS lifecycle must use the ownership-aware helper"),
        Platform::Android => {
            run_command(runner, "adb", &["start-server".to_owned()])
                .await
                .map(output_text)?;
            tokio::time::timeout(
                Duration::from_secs(15),
                run_adb(runner, device_id, &["wait-for-device"]),
            )
            .await
            .map_err(|_| anyhow::anyhow!("Android Emulator did not become ready"))??;
            Ok("Android Emulator is ready".to_owned())
        }
    }
}

pub(crate) async fn status_device(
    platform: Platform,
    device_id: &str,
    runner: &dyn CommandRunner,
) -> anyhow::Result<String> {
    match platform {
        Platform::Ios => run_serve_sim_for_device(runner, device_id, &["--list"]).await,
        Platform::Android => run_adb(runner, device_id, &["get-state"]).await,
    }
}

pub(crate) async fn capture_device(
    platform: Platform,
    device_id: &str,
    name: &str,
    runner: &dyn CommandRunner,
) -> anyhow::Result<(Vec<u8>, String)> {
    match platform {
        Platform::Ios => {
            let directory = tempfile::tempdir()?;
            let path = directory.path().join(format!("{name}.png"));
            let output = run_command(
                runner,
                "xcrun",
                &[
                    "simctl".to_owned(),
                    "io".to_owned(),
                    device_id.to_owned(),
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
        Platform::Android => Ok((
            run_adb_bytes(runner, device_id, &["exec-out", "screencap", "-p"]).await?,
            "Captured Android Emulator display".to_owned(),
        )),
    }
}

pub(crate) async fn inspect_android_accessibility(
    serial: &str,
    runner: &dyn CommandRunner,
) -> anyhow::Result<serde_json::Value> {
    static NEXT_DUMP: AtomicU64 = AtomicU64::new(0);
    let remote_path = format!(
        "/sdcard/device-simulator-mcp-{}-{}.xml",
        std::process::id(),
        NEXT_DUMP.fetch_add(1, Ordering::Relaxed)
    );
    let script = format!(
        "path={remote_path}; trap 'rm -f \"$path\" >/dev/null 2>&1' EXIT HUP INT TERM; uiautomator dump --compressed \"$path\" >/dev/null 2>&1 || exit $?; cat \"$path\""
    );
    let xml = run_adb_bytes(runner, serial, &["shell", "sh", "-c", &script]).await?;
    crate::android_accessibility::parse(&xml)
}

pub(crate) async fn tap_device(
    platform: Platform,
    device_id: &str,
    x: f64,
    y: f64,
    runner: &dyn CommandRunner,
) -> anyhow::Result<String> {
    match platform {
        Platform::Ios => {
            run_serve_sim_for_device(runner, device_id, &["tap", &x.to_string(), &y.to_string()])
                .await
        }
        Platform::Android => {
            let (x, y) = normalized_pixels(x, y, android_dimensions(runner, device_id).await?);
            run_adb(runner, device_id, &["shell", "input", "tap", &x, &y]).await
        }
    }
}

pub(crate) async fn swipe_device(
    platform: Platform,
    device_id: &str,
    (x1, y1, x2, y2): (f64, f64, f64, f64),
    runner: &dyn CommandRunner,
) -> anyhow::Result<String> {
    match platform {
        Platform::Ios => {
            for gesture in [
                gesture_payload("begin", x1, y1),
                gesture_payload("move", x2, y2),
                gesture_payload("end", x2, y2),
            ] {
                run_serve_sim_for_device(runner, device_id, &["gesture", &gesture]).await?;
            }
            Ok("Swipe completed".to_owned())
        }
        Platform::Android => {
            let dimensions = android_dimensions(runner, device_id).await?;
            let (start_x, start_y) = normalized_pixels(x1, y1, dimensions);
            let (end_x, end_y) = normalized_pixels(x2, y2, dimensions);
            run_adb(
                runner,
                device_id,
                &[
                    "shell", "input", "swipe", &start_x, &start_y, &end_x, &end_y, "300",
                ],
            )
            .await
        }
    }
}

pub(crate) async fn type_on_device(
    platform: Platform,
    device_id: &str,
    text: &str,
    runner: &dyn CommandRunner,
) -> anyhow::Result<String> {
    match platform {
        Platform::Ios => run_serve_sim_for_device(runner, device_id, &["type", text]).await,
        Platform::Android => {
            run_adb(
                runner,
                device_id,
                &["shell", "input", "text", &escape_android_text(text)],
            )
            .await
        }
    }
}

pub(crate) async fn rotate_device(
    platform: Platform,
    device_id: &str,
    orientation: Orientation,
    runner: &dyn CommandRunner,
) -> anyhow::Result<String> {
    match platform {
        Platform::Ios => {
            run_serve_sim_for_device(
                runner,
                device_id,
                &["rotate", orientation.serve_sim_value()],
            )
            .await
        }
        Platform::Android => {
            let rotation = orientation.android_rotation().to_string();
            run_adb(
                runner,
                device_id,
                &[
                    "shell",
                    "settings",
                    "put",
                    "system",
                    "accelerometer_rotation",
                    "0",
                ],
            )
            .await?;
            run_adb(
                runner,
                device_id,
                &[
                    "shell",
                    "settings",
                    "put",
                    "system",
                    "user_rotation",
                    &rotation,
                ],
            )
            .await?;
            Ok(format!(
                "Android Emulator rotated to {}; automatic rotation is disabled",
                orientation.serve_sim_value()
            ))
        }
    }
}

pub(crate) fn escape_android_text(text: &str) -> String {
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

async fn android_dimensions(
    runner: &dyn CommandRunner,
    serial: &str,
) -> anyhow::Result<(f64, f64)> {
    let output = run_adb(runner, serial, &["shell", "wm", "size"]).await?;
    let dimensions = output
        .lines()
        .rev()
        .find_map(|line| line.rsplit_once(' ').map(|(_, value)| value.trim()))
        .ok_or_else(|| anyhow::anyhow!("could not determine Android display size"))?;
    parse_display_size(dimensions)
}

fn normalized_pixels(x: f64, y: f64, (width, height): (f64, f64)) -> (String, String) {
    (
        (x * (width - 1.0)).round().to_string(),
        (y * (height - 1.0)).round().to_string(),
    )
}

pub(crate) async fn run_serve_sim_for_device(
    runner: &dyn CommandRunner,
    device_id: &str,
    arguments: &[&str],
) -> anyhow::Result<String> {
    run_serve_sim_with_device(runner, Some(device_id), arguments).await
}

async fn run_serve_sim_with_device(
    runner: &dyn CommandRunner,
    device_id: Option<&str>,
    arguments: &[&str],
) -> anyhow::Result<String> {
    let mut command_arguments = Vec::new();
    if let Some(device) = device_id {
        if arguments
            .first()
            .is_some_and(|argument| !argument.starts_with('-'))
        {
            command_arguments.push(arguments[0].to_owned());
            command_arguments.extend(["--device".to_owned(), device.to_owned()]);
            command_arguments.extend(arguments[1..].iter().map(|argument| (*argument).to_owned()));
        } else {
            command_arguments.extend(arguments.iter().map(|argument| (*argument).to_owned()));
            command_arguments.push(device.to_owned());
        }
    } else {
        command_arguments.extend(arguments.iter().map(|argument| (*argument).to_owned()));
    }
    let (program, command_arguments) = ios::command(command_arguments)?;
    run_command(runner, &program, &command_arguments)
        .await
        .map(output_text)
}

fn adb_arguments(serial: &str, arguments: &[&str]) -> Vec<String> {
    let mut result = Vec::with_capacity(arguments.len() + 2);
    result.extend(["-s".to_owned(), serial.to_owned()]);
    result.extend(arguments.iter().map(|argument| (*argument).to_owned()));
    result
}

async fn run_adb(
    runner: &dyn CommandRunner,
    serial: &str,
    arguments: &[&str],
) -> anyhow::Result<String> {
    run_command(runner, "adb", &adb_arguments(serial, arguments))
        .await
        .map(output_text)
}

pub(crate) fn parse_display_size(dimensions: &str) -> anyhow::Result<(f64, f64)> {
    let (width, height) = dimensions
        .split_once('x')
        .ok_or_else(|| anyhow::anyhow!("invalid Android display size: {dimensions}"))?;
    let dimensions = (width.parse::<f64>()?, height.parse::<f64>()?);
    anyhow::ensure!(
        dimensions.0.is_finite()
            && dimensions.1.is_finite()
            && dimensions.0 >= 1.0
            && dimensions.1 >= 1.0
            && dimensions.0 <= 8192.0
            && dimensions.1 <= 8192.0,
        "invalid Android display dimensions"
    );
    Ok(dimensions)
}

async fn run_adb_bytes(
    runner: &dyn CommandRunner,
    serial: &str,
    arguments: &[&str],
) -> anyhow::Result<Vec<u8>> {
    run_command(runner, "adb", &adb_arguments(serial, arguments))
        .await
        .map(|output| output.stdout)
}

async fn run_command(
    runner: &dyn CommandRunner,
    command: &str,
    arguments: &[String],
) -> anyhow::Result<CommandOutput> {
    let output = runner
        .run(command, arguments)
        .await
        .map_err(|error| actionable_command_error(command, error))?;
    if !output.success {
        let message = command_output(
            &String::from_utf8_lossy(&output.stdout),
            &String::from_utf8_lossy(&output.stderr),
        );
        if let Some(error) = actionable_device_failure(command, &message) {
            return Err(error);
        }
        anyhow::bail!("{command} failed with {}: {}", output.status, message);
    }
    Ok(output)
}

fn output_text(output: CommandOutput) -> String {
    command_output(
        &String::from_utf8_lossy(&output.stdout),
        &String::from_utf8_lossy(&output.stderr),
    )
}

pub(crate) fn actionable_command_error(command: &str, error: anyhow::Error) -> anyhow::Error {
    if !error
        .downcast_ref::<io::Error>()
        .is_some_and(|error| error.kind() == io::ErrorKind::NotFound)
    {
        return error;
    }
    let message = match command {
        "adb" => "`adb` was not found. Install Android SDK Platform-Tools, add its platform-tools directory to PATH, and retry.".to_owned(),
        "npx" => "`npx` was not found. Install Node.js 24.22.1 or newer, ensure npx is on PATH, and retry.".to_owned(),
        "xcrun" => "`xcrun` was not found. Install Xcode Command Line Tools with `xcode-select --install`, then retry.".to_owned(),
        _ => format!("`{command}` was not found. Install it and ensure it is on PATH, then retry."),
    };
    anyhow::anyhow!(message)
}

pub(crate) fn actionable_device_failure(command: &str, output: &str) -> Option<anyhow::Error> {
    if command == "adb"
        && (output.contains("no devices/emulators found") || output.contains("device offline"))
    {
        return Some(anyhow::anyhow!(
            "No usable Android device was found. Start an Android Emulator or connect a device, then retry. Set ANDROID_SERIAL when more than one device is available."
        ));
    }
    if command == "npx" && output.contains("could not determine executable to run") {
        return Some(anyhow::anyhow!(
            "`serve-sim` could not be started through npx. Install Node.js 24.22.1 or newer, ensure npx is on PATH, and retry."
        ));
    }
    None
}

pub(crate) fn gesture_payload(gesture_type: &str, x: f64, y: f64) -> String {
    format!(r#"{{"type":"{gesture_type}","x":{x},"y":{y}}}"#)
}

pub(crate) fn command_output(standard_output: &str, standard_error: &str) -> String {
    let output = standard_output.trim();
    if !output.is_empty() {
        return output.to_owned();
    }
    standard_error.trim().to_owned()
}
