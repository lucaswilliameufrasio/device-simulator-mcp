#!/usr/bin/env python3
"""Read-only latency benchmark using one persistent MCP connection.

Only read-only observations are requested. Images are never printed; direct
iOS CLI comparison uses uniquely named, automatically deleted temporary PNGs.
Run before/after changes on the same idle device.
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

from benchmark_support import direct_capture, idle_usage, validate_comparison


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
    parser.add_argument("--inspect-arguments", default="{}", help="JSON filters/limits for device_inspect")
    parser.add_argument("--step", action="store_true", help="Also benchmark a capture-only device_step (no input)")
    parser.add_argument("--step-wait", choices=["visual_stability"], help="Use fresh visual stability for capture-only steps")
    parser.add_argument("--lifecycle", action="store_true", help="Start inspection first and stop only MCP-owned resources afterward")
    parser.add_argument("--compare-cli", action="store_true", help="Pair default fresh PNG MCP capture with direct CLI capture")
    parser.add_argument("--temporary-root", help="Directory for unique, auto-cleaned direct iOS capture files")
    parser.add_argument("--idle-seconds", type=float, default=0, help="Measure MCP-only idle CPU/RSS after observations (0 disables, 1..30 seconds)")
    args = parser.parse_args()
    if args.samples < 1 or not math.isfinite(args.timeout) or args.timeout <= 0:
        parser.error("samples and timeout must be positive")
    capture_arguments = json.loads(args.capture_arguments)
    environment = dict(os.environ, DEVICE_PLATFORM=args.platform)
    if args.idle_seconds != 0 and not 1 <= args.idle_seconds <= 30:
        parser.error("idle-seconds must be 0 or between 1 and 30")
    if args.compare_cli:
        try:
            validate_comparison(args.platform, capture_arguments, environment)
        except ValueError as error:
            parser.error(str(error))
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
            tools.append(("device_inspect", json.loads(args.inspect_arguments)))
        if args.step:
            step = {"actions": [], "capture": capture_arguments}
            if args.step_wait:
                step["wait_condition"] = {"kind": args.step_wait}
            tools.append(("device_step", step))
        for tool, arguments in tools:
            durations = []
            sizes = []
            errors = 0
            cli_durations = []
            cli_sizes = []

            def measure_cli():
                started = time.monotonic()
                size = direct_capture(args.platform, environment, args.timeout, args.temporary_root)
                cli_durations.append((time.monotonic() - started) * 1000)
                cli_sizes.append(size)

            for sample in range(args.samples):
                compare = args.compare_cli and tool == "device_capture"
                if compare and sample % 2 == 0:
                    measure_cli()
                started = time.monotonic()
                result = request("tools/call", {"name": tool, "arguments": arguments})
                durations.append((time.monotonic() - started) * 1000)
                sizes.append(len(json.dumps(result).encode()))
                errors += bool(result.get("isError"))
                del result
                if compare and sample % 2 == 1:
                    measure_cli()
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
            if cli_durations:
                print(json.dumps({"platform": args.platform, "tool": "direct_cli_capture",
                    "samples": args.samples, "comparison": "paired_fresh_full_resolution_png",
                    "order": "alternating_cli_first_mcp_first", "errors": 0,
                    "first_ms": round(cli_durations[0], 2),
                    "p50_ms": round(statistics.median(cli_durations), 2),
                    "p95_ms": round(percentile(cli_durations, 0.95), 2),
                    "mean_raw_png_bytes": round(statistics.mean(cli_sizes)),
                    "median_mcp_minus_cli_ms": round(statistics.median([
                        mcp - cli for mcp, cli in zip(durations, cli_durations)
                    ]), 2)}))
        if args.idle_seconds:
            print(json.dumps(idle_usage(child.pid, args.idle_seconds)))
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
