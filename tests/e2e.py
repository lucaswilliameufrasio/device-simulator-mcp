#!/usr/bin/env python3
"""Run a bounded, read-only MCP smoke against an isolated simulator."""

import json
import os
import selectors
import subprocess
import sys
import tempfile
import time


REQUEST_TIMEOUT_SECONDS = 90


class McpClient:
    def __init__(self, binary: str) -> None:
        self.diagnostics = tempfile.TemporaryFile(mode="w+t")
        self.process = subprocess.Popen(
            [binary],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=self.diagnostics,
            text=True,
            bufsize=1,
        )
        if self.process.stdin is None or self.process.stdout is None:
            raise RuntimeError("Could not open MCP stdio pipes")
        self.input = self.process.stdin
        self.output = self.process.stdout
        self.selector = selectors.DefaultSelector()
        self.selector.register(self.output, selectors.EVENT_READ)
        self.next_id = 0

    def send(self, message: dict[str, object]) -> None:
        self.input.write(json.dumps(message, separators=(",", ":")) + "\n")
        self.input.flush()

    def read_response(self, request_id: int) -> dict[str, object]:
        deadline = time.monotonic() + REQUEST_TIMEOUT_SECONDS
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError(f"Timed out waiting for MCP response {request_id}")
            if not self.selector.select(remaining):
                raise TimeoutError(f"Timed out waiting for MCP response {request_id}")
            line = self.output.readline()
            if not line:
                raise RuntimeError(
                    f"MCP process exited before response {request_id} "
                    f"(exit code {self.process.poll()})"
                )
            response = json.loads(line)
            if response.get("id") == request_id:
                return response

    def request(self, method: str, params: dict[str, object]) -> dict[str, object]:
        self.next_id += 1
        request_id = self.next_id
        self.send(
            {
                "jsonrpc": "2.0",
                "id": request_id,
                "method": method,
                "params": params,
            }
        )
        response = self.read_response(request_id)
        if "error" in response:
            raise RuntimeError(f"MCP {method} returned a JSON-RPC error")
        return response

    def call_tool(self, name: str, arguments: dict[str, object]) -> None:
        response = self.request(
            "tools/call", {"name": name, "arguments": arguments}
        )
        result = response.get("result")
        if not isinstance(result, dict) or result.get("isError") is not False:
            print(f"E2E tool {name} did not succeed.", file=sys.stderr)
            if isinstance(result, dict):
                for item in result.get("content", []):
                    if isinstance(item, dict) and item.get("type") == "text":
                        print(item.get("text", ""), file=sys.stderr)
            raise RuntimeError(f"MCP tool {name} failed")

    def close(self, show_diagnostics: bool = False) -> None:
        self.selector.close()
        self.input.close()
        self.output.close()
        if self.process.poll() is None:
            self.process.terminate()
        try:
            self.process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.process.kill()
            self.process.wait()
        if show_diagnostics:
            self.diagnostics.seek(0)
            log = self.diagnostics.read(16_384)
            if log:
                print("Redacted MCP diagnostics:", file=sys.stderr)
                print(log, file=sys.stderr, end="")
        self.diagnostics.close()


def main() -> int:
    platform = os.environ.get("DEVICE_PLATFORM", "")
    if platform not in {"ios", "android"}:
        print("DEVICE_PLATFORM must be ios or android", file=sys.stderr)
        return 2

    binary = sys.argv[1] if len(sys.argv) > 1 else "target/release/device-simulator-mcp"
    client = McpClient(binary)
    started = False
    failed = False
    try:
        client.request(
            "initialize",
            {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {"name": "device-simulator-e2e", "version": "0.1.1"},
            },
        )
        client.send({"jsonrpc": "2.0", "method": "notifications/initialized"})
        try:
            started = True
            client.call_tool("device_start", {})
            client.call_tool("device_status", {})
            client.call_tool("device_capture", {"name": "e2e"})
        finally:
            if started:
                client.call_tool("device_stop", {})
    except (OSError, RuntimeError, TimeoutError, json.JSONDecodeError) as error:
        failed = True
        print(f"E2E failed: {error}", file=sys.stderr)
        return 1
    finally:
        client.close(show_diagnostics=failed)

    print(f"E2E passed for {platform}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
