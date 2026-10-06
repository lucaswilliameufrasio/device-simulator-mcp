use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

struct Client {
    child: std::process::Child,
    input: std::process::ChildStdin,
    output: BufReader<std::process::ChildStdout>,
    next_id: u64,
}

impl Client {
    fn start(command: Command) -> Self {
        Self::start_target(command, None)
    }

    fn start_target(mut command: Command, serial: Option<&str>) -> Self {
        command
            .env("DEVICE_PLATFORM", "android")
            .env("DEVICE_ANDROID_BACKEND", "adb")
            .env("DEVICE_IOS_BACKEND", "cli")
            .env_remove("ANDROID_SERIAL")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        if let Some(serial) = serial {
            command.env("ANDROID_SERIAL", serial);
        }
        let mut child = command.spawn().unwrap();
        let input = child.stdin.take().unwrap();
        let output = BufReader::new(child.stdout.take().unwrap());
        let mut client = Self {
            child,
            input,
            output,
            next_id: 0,
        };
        client.request(
            "initialize",
            serde_json::json!({
                "protocolVersion":"2025-11-25", "capabilities":{},
                "clientInfo":{"name":"persistent-test","version":"1"},
            }),
        );
        client.send(serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized"}));
        client
    }

    fn send(&mut self, value: serde_json::Value) {
        writeln!(self.input, "{value}").unwrap();
        self.input.flush().unwrap();
    }

    fn read_response(&mut self, id: u64) -> serde_json::Value {
        loop {
            let mut line = String::new();
            assert!(
                self.output.read_line(&mut line).unwrap() > 0,
                "MCP closed early"
            );
            let value: serde_json::Value = serde_json::from_str(&line).unwrap();
            if value["id"] == id {
                return value;
            }
        }
    }

    fn request(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
        self.next_id += 1;
        let id = self.next_id;
        self.send(serde_json::json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}));
        self.read_response(id)
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn server_command() -> Command {
    Command::new(env!("CARGO_BIN_EXE_device-simulator-mcp"))
}

#[test]
fn diagnostic_logging_never_traces_mcp_arguments() {
    let mut child = server_command()
        .env("DEVICE_PLATFORM", "unsupported")
        .env("RUST_LOG", "trace,rmcp::service=trace")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    writeln!(input,"{}",serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
        "protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"privacy-test","version":"1"}
    }})).unwrap();
    writeln!(
        input,
        "{}",
        serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized"})
    )
    .unwrap();
    writeln!(
        input,
        "{}",
        serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call",
        "params":{"name":"device_type","arguments":{"text":"PRIVATE_TEST_INPUT"}}})
    )
    .unwrap();
    drop(input);
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    assert!(
        !String::from_utf8(output.stderr)
            .unwrap()
            .contains("PRIVATE_TEST_INPUT")
    );
}

#[test]
fn lists_legacy_and_new_tools_in_one_persistent_session() {
    let mut client = Client::start(server_command());
    let response = client.request("tools/list", serde_json::json!({}));
    let tools = response["result"]["tools"].as_array().unwrap();
    let inspect = tools
        .iter()
        .find(|tool| tool["name"] == "device_inspect")
        .unwrap();
    assert!(inspect["inputSchema"]["properties"]["label_contains"].is_object());
    assert!(inspect["inputSchema"]["properties"]["max_elements"].is_object());
    for name in [
        "device_start",
        "device_stop",
        "device_status",
        "device_tap",
        "device_swipe",
        "device_type",
        "device_rotate",
        "device_multitouch",
        "device_capture",
        "device_repair_input",
        "device_step",
        "device_inspect",
        "device_capabilities",
    ] {
        assert!(tools.iter().any(|tool| tool["name"] == name));
    }
    for _ in 0..3 {
        let response = client.request(
            "tools/call",
            serde_json::json!({"name":"device_step","arguments":{
                "actions":[{"kind":"tap","x":0.5,"y":0.5},{"kind":"type","text":""}],
            }}),
        );
        assert_eq!(response["result"]["isError"], true);
    }
    let response = client.request(
        "tools/call",
        serde_json::json!({"name":"device_rotate","arguments":{"orientation":"diagonal"}}),
    );
    assert_eq!(response["result"]["isError"], true);
    let response = client.request(
        "tools/call",
        serde_json::json!({"name":"device_multitouch","arguments":{"frames":[]}}),
    );
    assert_eq!(response["result"]["isError"], true);
    let response = client.request(
        "tools/call",
        serde_json::json!({"name":"device_inspect",
        "arguments":{"max_elements":0}}),
    );
    assert_eq!(response["result"]["isError"], true);
    assert!(
        response
            .to_string()
            .contains("max_elements must be between")
    );
    let response = client.request(
        "tools/call",
        serde_json::json!({"name":"device_capabilities","arguments":{}}),
    );
    assert_eq!(response["result"]["isError"], false);
    let capabilities: serde_json::Value =
        serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(capabilities["backend"], "adb");
    assert_eq!(capabilities["availability_probed"], false);
}

#[cfg(unix)]
#[test]
fn fresh_visual_waits_and_cache_work_through_persistent_stdio() {
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir().unwrap();
    let executable = directory.path().join("adb");
    let frame_file = directory.path().join("frame.png");
    let count_file = directory.path().join("captures");
    let mut png = std::io::Cursor::new(Vec::new());
    image::DynamicImage::new_rgb8(16, 16)
        .write_to(&mut png, image::ImageFormat::Png)
        .unwrap();
    std::fs::write(&frame_file, png.into_inner()).unwrap();
    std::fs::write(
        &executable,
        "#!/bin/sh\necho capture >> \"$TEST_COUNT_FILE\"\ncat \"$TEST_FRAME_FILE\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut command = server_command();
    command
        .env(
            "PATH",
            format!(
                "{}:{}",
                directory.path().display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        )
        .env("TEST_COUNT_FILE", &count_file)
        .env("TEST_FRAME_FILE", &frame_file);
    let mut client = Client::start_target(command, Some("fixture-target"));
    // The returned image is the fresh sample that satisfies stability:
    // no redundant capture or replayed cache sample is requested.
    let result = client.request("tools/call",serde_json::json!({"name":"device_step","arguments":{
        "actions":[],"capture":{},"wait_condition":{"kind":"visual_stability","stable_samples":2}
    }}));
    assert_eq!(result["result"]["isError"], false);
    assert_eq!(
        std::fs::read_to_string(&count_file)
            .unwrap()
            .lines()
            .count(),
        2
    );
    for _ in 0..2 {
        let result = client.request(
            "tools/call",
            serde_json::json!({"name":"device_capture",
            "arguments":{"max_age_ms":5000}}),
        );
        assert_eq!(result["result"]["isError"], false);
        let metadata: serde_json::Value =
            serde_json::from_str(result["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(metadata["cache_hit"], true);
        assert_eq!(metadata["freshness"], "local_cache");
    }
    assert_eq!(
        std::fs::read_to_string(&count_file)
            .unwrap()
            .lines()
            .count(),
        2
    );
    let result = client.request(
        "tools/call",
        serde_json::json!({"name":"device_step","arguments":{
            "actions":[],"capture":{},"wait_condition":{"kind":"visual_change"},"timeout_ms":100
        }}),
    );
    assert_eq!(result["result"]["isError"], true);
    assert!(result.to_string().contains("visual_wait_deadline"));
}

#[cfg(unix)]
fn process_is_running(pid: i32) -> bool {
    #[cfg(target_os = "linux")]
    {
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            return false;
        };
        let Some((_, fields)) = stat.rsplit_once(") ") else {
            return true;
        };
        !fields.starts_with('Z')
    }
    #[cfg(not(target_os = "linux"))]
    {
        // SAFETY: signal 0 checks only existence of this test's own process.
        unsafe { libc::kill(pid, 0) == 0 }
    }
}

#[cfg(unix)]
#[test]
fn protocol_cancellation_kills_input_process_and_releases_session() {
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir().unwrap();
    let executable = directory.path().join("adb");
    let pid_file = directory.path().join("pid");
    std::fs::write(&executable, "#!/bin/sh\nif [ \"$1\" = devices ]; then echo ready; exit 0; fi\necho $$ > \"$TEST_PID_FILE\"\nexec sleep 30\n").unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut command = server_command();
    let path = std::env::var("PATH").unwrap_or_default();
    command
        .env("PATH", format!("{}:{path}", directory.path().display()))
        .env("TEST_PID_FILE", &pid_file);
    let mut client = Client::start(command);
    client.send(
        serde_json::json!({"jsonrpc":"2.0","id":10,"method":"tools/call",
        "params":{"name":"device_type","arguments":{"text":"sample"}}}),
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while !pid_file.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let pid: i32 = std::fs::read_to_string(&pid_file)
        .expect("input process must start")
        .trim()
        .parse()
        .unwrap();
    client.send(
        serde_json::json!({"jsonrpc":"2.0","method":"notifications/cancelled",
        "params":{"requestId":10,"reason":"test cancellation"}}),
    );
    // MCP intentionally suppresses responses for cancelled requests. A ping
    // provides an ordering barrier without waiting for a discarded response.
    let response = client.request("ping", serde_json::json!({}));
    assert!(response.get("result").is_some());
    // Reaping can complete asynchronously after kill_on_drop.
    loop {
        if !process_is_running(pid) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "cancelled input process survived"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let response = client.request(
        "tools/call",
        serde_json::json!({"name":"device_status","arguments":{}}),
    );
    assert_eq!(response["result"]["isError"], false);
}

#[test]
fn prints_install_mcp_help_without_starting_the_server() {
    let output = Command::new(env!("CARGO_BIN_EXE_device-simulator-mcp"))
        .args(["install-mcp", "--help"])
        .output()
        .expect("failed to start CLI");

    assert!(output.status.success());
    let help = String::from_utf8(output.stdout).expect("CLI help should be UTF-8");
    assert!(help.contains("--client"));
    assert!(help.contains("--apply"));
}

#[test]
fn rejects_invalid_tool_arguments_through_stdio_protocol() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_device-simulator-mcp"))
        .env("DEVICE_PLATFORM", "unsupported")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("failed to start MCP server");

    let mut input = child.stdin.take().expect("missing MCP server stdin");
    writeln!(
        input,
        r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":"2025-11-25","capabilities":{{}},"clientInfo":{{"name":"integration-test","version":"0.1.1"}}}}}}"#
    )
    .unwrap();
    writeln!(
        input,
        r#"{{"jsonrpc":"2.0","method":"notifications/initialized"}}"#
    )
    .unwrap();
    writeln!(
        input,
        r#"{{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{{"name":"device_tap","arguments":{{"x":2,"y":0.5}}}}}}"#
    )
    .unwrap();
    drop(input);

    let output = child.stdout.take().expect("missing MCP server stdout");
    let responses = BufReader::new(output)
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(&line.unwrap()).unwrap())
        .collect::<Vec<_>>();

    child.wait().expect("failed to wait for MCP server");

    let tool_response = responses
        .iter()
        .find(|response| response.get("id") == Some(&serde_json::json!(2)))
        .expect("missing tool response");
    assert_eq!(tool_response["result"]["isError"], true);
    assert_eq!(
        tool_response["result"]["content"][0]["text"],
        "coordinates must be finite normalized numbers between 0 and 1"
    );
}
