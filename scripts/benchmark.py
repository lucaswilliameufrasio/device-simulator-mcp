#!/usr/bin/env python3
"""Read-only latency benchmark using one persistent MCP connection.

Only status and screenshots are requested. Screenshots are discarded, never
written to disk or printed. Run before/after changes on the same idle device.
"""

import argparse
import json
import math
import os
import queue
import statistics
import subprocess
import threading
import time


def percentile(values, fraction):
    return sorted(values)[max(0, math.ceil(len(values) * fraction) - 1)]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", default="target/release/device-simulator-mcp")
    parser.add_argument("--platform", choices=["ios", "android"], required=True)
    parser.add_argument("--samples", type=int, default=10)
    parser.add_argument("--timeout", type=float, default=30)
    parser.add_argument("--capture-arguments", default="{}")
    parser.add_argument("--inspect", action="store_true", help="Also benchmark on-demand iOS accessibility")
    parser.add_argument("--step", action="store_true", help="Also benchmark a capture-only device_step (no input)")
    parser.add_argument("--step-wait", choices=["visual_stability"], help="Use fresh visual stability for capture-only steps")
    parser.add_argument("--lifecycle", action="store_true", help="Start inspection first and stop only MCP-owned resources afterward")
    args = parser.parse_args()
    if args.samples < 1 or args.timeout <= 0:
        parser.error("samples and timeout must be positive")
    capture_arguments = json.loads(args.capture_arguments)
    environment = dict(os.environ, DEVICE_PLATFORM=args.platform)
    messages = queue.Queue()
    child = subprocess.Popen(
        [args.binary], env=environment, stdin=subprocess.PIPE,
        stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True,
    )

    def read_messages():
        for line in child.stdout:
            messages.put(json.loads(line))
        messages.put(None)

    threading.Thread(target=read_messages, daemon=True).start()
    request_id = 0

    def request(method, params):
        nonlocal request_id
        request_id += 1
        child.stdin.write(json.dumps({
            "jsonrpc": "2.0", "id": request_id,
            "method": method, "params": params,
        }) + "\n")
        child.stdin.flush()
        deadline = time.monotonic() + args.timeout
        while True:
            message = messages.get(timeout=max(0.001, deadline - time.monotonic()))
            if message is None:
                raise RuntimeError("MCP process closed its output")
            if message.get("id") == request_id:
                if "error" in message:
                    raise RuntimeError("MCP protocol error")
                return message["result"]

    try:
        started = time.monotonic()
        request("initialize", {
            "protocolVersion": "2025-11-25", "capabilities": {},
            "clientInfo": {"name": "device-benchmark", "version": "1"},
        })
        child.stdin.write(json.dumps({
            "jsonrpc": "2.0", "method": "notifications/initialized",
        }) + "\n")
        child.stdin.flush()
        print(json.dumps({"initialize_ms": round((time.monotonic() - started) * 1000, 2)}))
        if args.lifecycle:
            started = time.monotonic()
            result = request("tools/call", {"name": "device_start", "arguments": {}})
            if result.get("isError"):
                raise RuntimeError("device_start failed")
            print(json.dumps({"device_start_ms": round((time.monotonic() - started) * 1000, 2)}))
        tools = [
            ("device_status", {}), ("device_capture", capture_arguments),
        ]
        if args.inspect:
            tools.append(("device_inspect", {}))
        if args.step:
            step = {"actions": [], "capture": capture_arguments}
            if args.step_wait:
                step["wait_condition"] = {"kind": args.step_wait}
            tools.append(("device_step", step))
        for tool, arguments in tools:
            durations = []
            sizes = []
            errors = 0
            for _ in range(args.samples):
                started = time.monotonic()
                result = request("tools/call", {"name": tool, "arguments": arguments})
                durations.append((time.monotonic() - started) * 1000)
                sizes.append(len(json.dumps(result).encode()))
                errors += bool(result.get("isError"))
                del result
            print(json.dumps({
                "platform": args.platform, "tool": tool,
                "samples": args.samples, "errors": errors,
                "first_ms": round(durations[0], 2),
                "p50_ms": round(statistics.median(durations), 2),
                "p95_ms": round(percentile(durations, 0.95), 2),
                "mean_response_bytes": round(statistics.mean(sizes)),
            }))
            if errors:
                raise RuntimeError(f"{tool} failed {errors} times; timings are not successful-operation measurements")
    finally:
        try:
            if args.lifecycle and child.poll() is None:
                request("tools/call", {"name": "device_stop", "arguments": {}})
        except (RuntimeError, queue.Empty, BrokenPipeError):
            # Closing stdin still exercises graceful server-owned cleanup.
            pass
        child.stdin.close()
        try:
            child.wait(timeout=3)
        except subprocess.TimeoutExpired:
            child.kill()
            child.wait()


if __name__ == "__main__":
    main()
