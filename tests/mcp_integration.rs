use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

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
