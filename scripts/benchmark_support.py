"""Read-only helpers for equivalent captures and MCP-only resource sampling."""

import os
import queue
import signal
import subprocess
import tempfile
import threading
import time
from pathlib import Path

MAX_BYTES = 24 * 1024 * 1024


def validate_comparison(platform, arguments, environment):
    if arguments:
        raise ValueError("CLI comparison requires default fresh full-resolution PNG arguments")
    target_key = "IOS_SIMULATOR_UDID" if platform == "ios" else "ANDROID_SERIAL"
    if not environment.get(target_key):
        raise ValueError("CLI comparison requires an explicit device target")
    if platform == "android" and environment.get("DEVICE_ANDROID_BACKEND", "adb") != "adb":
        raise ValueError("equivalent Android CLI comparison requires the adb backend")


def run_bounded(arguments, timeout):
    child = subprocess.Popen(arguments, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                             start_new_session=os.name == "posix")
    output = queue.Queue(maxsize=1)
    deadline = time.monotonic() + timeout

    def read():
        try:
            data = child.stdout.read(MAX_BYTES + 1)
            if len(data) > MAX_BYTES:
                output.put(RuntimeError("direct CLI output exceeded byte limit"))
            else:
                output.put(data)
        except OSError:
            output.put(RuntimeError("direct CLI output read failed"))

    reader = threading.Thread(target=read, daemon=True)
    reader.start()
    try:
        data = output.get(timeout=max(0.001, deadline - time.monotonic()))
        if isinstance(data, Exception):
            raise data
        child.wait(timeout=max(0.001, deadline - time.monotonic()))
        if child.returncode:
            raise RuntimeError("direct CLI capture failed")
        return data
    except (queue.Empty, subprocess.TimeoutExpired):
        raise RuntimeError("direct CLI deadline exceeded") from None
    finally:
        if os.name == "posix":
            try:
                os.killpg(child.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
        elif child.poll() is None:
            child.kill()
        child.wait()
        reader.join(timeout=1)
        if not reader.is_alive():
            child.stdout.close()


def direct_capture(platform, environment, timeout, temporary_root=None):
    if platform == "ios":
        with tempfile.TemporaryDirectory(prefix="device-benchmark-", dir=temporary_root) as directory:
            path = Path(directory) / "capture.png"
            run_bounded(["xcrun", "simctl", "io", environment["IOS_SIMULATOR_UDID"],
                         "screenshot", str(path)], timeout)
            if path.stat().st_size > MAX_BYTES:
                raise RuntimeError("direct CLI screenshot exceeded byte limit")
            with path.open("rb") as image:
                data = image.read(MAX_BYTES + 1)
    else:
        data = run_bounded(["adb", "-s", environment["ANDROID_SERIAL"],
                            "exec-out", "screencap", "-p"], timeout)
    if len(data) > MAX_BYTES or not data.startswith(b"\x89PNG\r\n\x1a\n"):
        raise RuntimeError("direct CLI did not return a bounded PNG")
    return len(data)


def cpu_seconds(value):
    fields = value.split(":")
    if len(fields) not in (2, 3):
        raise ValueError("unsupported ps CPU time format")
    result = 0.0
    for field in fields:
        result = result * 60 + float(field)
    return result


def process_usage(pid):
    output = run_bounded(["ps", "-p", str(pid), "-o", "time=,rss="], 3).decode().split()
    if len(output) != 2:
        raise RuntimeError("MCP process resource sampling unavailable")
    return cpu_seconds(output[0]), int(output[1]) * 1024


def idle_usage(pid, seconds):
    before_cpu, before_rss = process_usage(pid)
    started = time.monotonic()
    time.sleep(seconds)
    after_cpu, after_rss = process_usage(pid)
    elapsed = time.monotonic() - started
    delta = max(0, after_cpu - before_cpu)
    return {"scope": "mcp_process_only", "idle_interval_ms": round(elapsed * 1000),
            "cpu_measurement": "coarse_ps_cpu_time_delta",
            "cpu_time_delta_ms": round(delta * 1000, 2),
            "cpu_percent_one_core": round(delta / elapsed * 100, 2),
            "rss_before_bytes": before_rss, "rss_after_bytes": after_rss,
            "backend_resources_included": False}
