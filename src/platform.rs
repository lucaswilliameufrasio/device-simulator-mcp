//! Legacy platform CLI I/O. No MCP framework or presentation dependencies.
use crate::{
    ios,
    process::{CommandOutput, CommandRunner},
};
use std::{io, time::Duration};

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

pub(crate) async fn start_device(
    platform: Platform,
    runner: &dyn CommandRunner,
) -> anyhow::Result<String> {
    match platform {
        Platform::Ios => anyhow::bail!("iOS lifecycle must use the ownership-aware helper"),
        Platform::Android => {
            run_adb_command(runner, &["start-server"]).await?;
            tokio::time::timeout(
                Duration::from_secs(15),
                run_adb(runner, &["wait-for-device"]),
            )
            .await
            .map_err(|_| anyhow::anyhow!("Android Emulator did not become ready"))??;
            Ok("Android Emulator is ready".to_owned())
        }
    }
}

pub(crate) async fn status_device(
    platform: Platform,
    runner: &dyn CommandRunner,
) -> anyhow::Result<String> {
    match platform {
        Platform::Ios => run_serve_sim(runner, &["--list"]).await,
        Platform::Android => run_adb(runner, &["devices"]).await,
    }
}

pub(crate) async fn capture_device(
    platform: Platform,
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
        Platform::Android => Ok((
            run_adb_bytes(runner, &["exec-out", "screencap", "-p"]).await?,
            "Captured Android Emulator display".to_owned(),
        )),
    }
}

pub(crate) async fn tap_device(
    platform: Platform,
    x: f64,
    y: f64,
    runner: &dyn CommandRunner,
) -> anyhow::Result<String> {
    match platform {
        Platform::Ios => run_serve_sim(runner, &["tap", &x.to_string(), &y.to_string()]).await,
        Platform::Android => {
            let (x, y) = normalized_pixels(x, y, android_dimensions(runner).await?);
            run_adb(runner, &["shell", "input", "tap", &x, &y]).await
        }
    }
}

pub(crate) async fn swipe_device(
    platform: Platform,
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
                run_serve_sim(runner, &["gesture", &gesture]).await?;
            }
            Ok("Swipe completed".to_owned())
        }
        Platform::Android => {
            let dimensions = android_dimensions(runner).await?;
            let (start_x, start_y) = normalized_pixels(x1, y1, dimensions);
            let (end_x, end_y) = normalized_pixels(x2, y2, dimensions);
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

pub(crate) async fn type_on_device(
    platform: Platform,
    text: &str,
    runner: &dyn CommandRunner,
) -> anyhow::Result<String> {
    match platform {
        Platform::Ios => run_serve_sim(runner, &["type", text]).await,
        Platform::Android => {
            run_adb(
                runner,
                &["shell", "input", "text", &escape_android_text(text)],
            )
            .await
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

async fn android_dimensions(runner: &dyn CommandRunner) -> anyhow::Result<(f64, f64)> {
    let output = run_adb(runner, &["shell", "wm", "size"]).await?;
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

pub(crate) async fn run_serve_sim(
    runner: &dyn CommandRunner,
    arguments: &[&str],
) -> anyhow::Result<String> {
    let mut command_arguments = Vec::new();
    if let Ok(device) = std::env::var("IOS_SIMULATOR_UDID") {
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
    let (program, command_arguments) = ios::command(command_arguments)?;
    run_command(runner, &program, &command_arguments)
        .await
        .map(output_text)
}

fn ios_device_target() -> String {
    std::env::var("IOS_SIMULATOR_UDID").unwrap_or_else(|_| "booted".to_owned())
}

fn adb_arguments(arguments: &[&str]) -> Vec<String> {
    let mut result = Vec::with_capacity(arguments.len() + 2);
    if let Ok(serial) = std::env::var("ANDROID_SERIAL") {
        result.extend(["-s".to_owned(), serial]);
    }
    result.extend(arguments.iter().map(|argument| (*argument).to_owned()));
    result
}

async fn run_adb(runner: &dyn CommandRunner, arguments: &[&str]) -> anyhow::Result<String> {
    run_command(runner, "adb", &adb_arguments(arguments))
        .await
        .map(output_text)
}

async fn run_adb_command(runner: &dyn CommandRunner, arguments: &[&str]) -> anyhow::Result<String> {
    let arguments = arguments
        .iter()
        .map(|argument| (*argument).to_owned())
        .collect::<Vec<_>>();
    run_command(runner, "adb", &arguments)
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

async fn run_adb_bytes(runner: &dyn CommandRunner, arguments: &[&str]) -> anyhow::Result<Vec<u8>> {
    run_command(runner, "adb", &adb_arguments(arguments))
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
        "npx" => "`npx` was not found. Install Node.js 24.21.0 or newer, ensure npx is on PATH, and retry.".to_owned(),
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
            "`serve-sim` could not be started through npx. Install Node.js 24.21.0 or newer, ensure npx is on PATH, and retry."
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
