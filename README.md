# Device Simulator MCP

Local MCP server for inspecting and controlling booted iOS Simulators and
Android Emulators. It is framework-agnostic: the app can be Flutter, SwiftUI,
UIKit, React Native, or any other app that runs on the target device.

## Requirements

- Rust 1.98 or newer to build from source.
- iOS: macOS, Xcode Command Line Tools, a booted Simulator, Node.js 24.21.0
  or newer, and `npx` for `serve-sim` interactions.
- Android: Android SDK platform tools, `adb`, and a booted Emulator.

## Installation

Release binaries are published for macOS, Linux, and Windows. Download a
specific asset from the public GitHub release page:

```bash
gh release download --repo lucaswilliameufrasio/device-simulator-mcp \
  --pattern 'device-simulator-mcp-*'
```

Install the current release directly:

```bash
./scripts/install.sh
```

Install a specific version:

```bash
./scripts/install.sh --tag v0.1.0
```

Build locally:

```bash
cargo build --release
```

## Register with an AI client

The binary can print or apply an MCP configuration for OpenCode, Claude Code,
Codex CLI, Cursor, or Gemini CLI. Preview first; `--apply` updates only the
`device-simulator` entry and preserves other client settings.

```bash
# Preview the user-wide OpenCode configuration (default scope: user).
device-simulator-mcp install-mcp --client opencode

# Apply it. The executable path defaults to the running binary.
device-simulator-mcp install-mcp --client opencode --apply

# Use Android instead of the default iOS platform.
device-simulator-mcp install-mcp --client codex --platform android --apply

# Run from the project root to add the server only to that project.
device-simulator-mcp install-mcp --client cursor --scope project --apply
```

Use `--config-file PATH` to target a non-default configuration file or
`--binary PATH` if the installed executable should be registered at another
path. JSON/JSONC comments and Codex TOML comments are preserved. Restart the AI
client after applying the configuration. This command registers the stdio MCP
server; it does not install lifecycle hooks.

## MCP Configuration

The server uses stdio. Set `DEVICE_PLATFORM` to `ios` or `android`.

```json
{
  "mcpServers": {
    "device-simulator": {
      "command": "/path/to/device-simulator-mcp",
      "env": {
        "DEVICE_PLATFORM": "ios"
      }
    }
  }
}
```

Set `ANDROID_SERIAL` when more than one Android device is available.

## Tools

- `device_start`: start inspection for the selected platform.
- `device_stop`: stop an iOS helper started by this MCP, or disconnect the
  persistent input socket. Externally started streams are never stopped.
  Android emulator lifecycle remains external to this tool.
- `device_status`: show the selected device status.
- `device_capture`: return the current display as PNG by default, with optional
  JPEG, resize and crop parameters.
- `device_tap`: tap a normalized coordinate between `0` and `1`.
- `device_swipe`: swipe between normalized coordinates.
- `device_type`: type into the focused control.
- `device_step`: execute a bounded, known sequence of actions with an optional
  final screenshot in one MCP call.
- `device_inspect`: inspect a bounded accessibility snapshot on demand through
  the opt-in persistent iOS backend.
- `device_repair_input`: repair iOS Simulator input services. This restarts
  SpringBoard and closes running apps; use only when input is broken.

The server does not build, install, launch, or modify applications. The
explicit iOS input-repair tool is an exception to normal device interaction:
it restarts SpringBoard and closes open apps. Everything runs locally and
screenshots are returned directly to the MCP host.

## Faster agent loops

Prefer `device_step` when the actions are already known. Do not batch actions
that depend on UI you have not observed. All arguments are validated first;
execution stops at the first failure and reports the number of completed
actions. Previously submitted actions are not rolled back or automatically
retried. A backend submission is not acknowledgement of application rendering.

```json
{
  "actions": [
    { "kind": "tap", "x": 0.5, "y": 0.25 },
    { "kind": "type", "text": "sample" }
  ],
  "capture": { "format": "jpeg", "max_dimension": 1280, "quality": 80 },
  "timeout_ms": 10000,
  "settle_ms": 100
}
```

- A step accepts at most 16 actions, a total deadline of 1–20000 ms, and an
  explicit settle delay of 0–1000 ms. Settling is not a visual-idle guarantee.
- Omit `capture` to return no image. A capture-only step with `actions: []` is
  also supported. Screenshots are never collected automatically per action.
- Alternatively use `"accessibility": true` for a bounded iOS accessibility
  observation instead of `capture`. This requires the persistent iOS backend;
  unsupported requests are rejected before any action is submitted.
- Optional `wait_condition` is `{ "kind": "visual_change" }` or
  `{ "kind": "visual_stability", "stable_samples": 3 }` (2–8 samples).
  Change compares a fresh pre-input baseline to fresh post-input screenshots;
  stability compares consecutive grayscale thumbnails, including source size.
  Both are heuristics, **not application-idle guarantees**. Samples never use
  replayed/latest frames or local cache. Polling waits at least 200 ms between
  captures, collects at most 32 samples, and respects the total step deadline.
  When a screenshot is requested, the step returns the exact fresh sample that
  satisfied the wait, marked `observation_source: visual_wait_sample`, rather
  than acquiring another screenshot. Crop/resize is applied after the wait.
  A wait failure reports completed actions without retrying them.
- Persistent iOS also supports `wait_condition: { "kind": "element_present",
  "label": "Example", "identifier": "sample" }`. Supply at least one exact
  selector; both must match the same element. Search is bounded by the AX
  projection limits, so a truncated tree cannot prove an element is absent.
  With `accessibility: true`, the matched snapshot is returned without a second
  AX request. No screenshot is automatically fetched for this semantic wait.
- `max_dimension` limits the longest image edge (1–4096 px), without upscaling.
  `format` is `png` or `jpeg`; JPEG quality is 1–100 (default 80).
- Optional `crop` uses source pixels: `{ "x": 0, "y": 0, "width": 600,
  "height": 800 }`. Crop happens before resizing. Image metadata includes source
  and output dimensions and the crop. Tap/swipe coordinates still refer to the
  full device, not the cropped image.
- The existing capture parameters/default PNG bytes remain compatible.
- Optional capture `max_age_ms` permits reuse of one locally acquired raw frame
  (1–5000 ms); default 0 always reacquires. Cache reuse requires an explicit
  device target (`IOS_SIMULATOR_UDID`, `ANDROID_SERIAL`, or configured gRPC
  endpoint). Metadata reports `cache_hit`, `local_age_ms`, and the original
  receive timestamp. Local age is **not source age** for serve-sim replay.
  Mutations, repair, start and stop invalidate the cache even if they fail.
  No background capture is started. Visual waits reject final captures requesting
  cache reuse or `latest_frame`; independent captures remain compatible.
- Per server, operations share ordering and a bounded queue of eight calls.
  Normal calls have a 20-second total deadline; startup allows 25 seconds.
  Each subprocess has a 20-second deadline and a 24 MiB output limit per pipe.
- Cancellation terminates subprocesses; Unix additionally terminates their
  process groups. Persistent gestures attempt bounded release of held contacts
  or keys on cancellation. Results can still be uncertain: never blindly retry.

### Persistent iOS backend (opt-in)

The default `cli` backend retains CLI interactions, using pinned
`serve-sim@0.1.47`. `device_start` reuses a running helper or starts an owned
foreground helper that is stopped on explicit stop or graceful MCP shutdown.
It requires an explicit `IOS_SIMULATOR_UDID` or exactly one booted Simulator.
It does not kill or take ownership of an existing external stream.
Set `SERVE_SIM_BINARY` to a preinstalled, compatible `serve-sim` executable to
bypass npm resolution entirely, including in the CLI fallback and helper startup.
Provision it before offline use; the default `npx` path can require network on
first use.

For input without a new Node/npm process per action, provision the pinned
server once in another terminal:

```bash
npx --yes serve-sim@0.1.47 --no-preview --quiet --port 3100 SIMULATOR_UDID
```

Configure the MCP environment:

```json
{
  "DEVICE_PLATFORM": "ios",
  "DEVICE_IOS_BACKEND": "serve-sim",
  "IOS_SIMULATOR_UDID": "SIMULATOR_UDID",
  "SERVE_SIM_URL": "http://127.0.0.1:3100"
}
```

`SERVE_SIM_URL` is the local server origin, optionally including its mount path
(default for the persistent backend: `http://127.0.0.1:3200`). Alternatively,
`SERVE_SIM_HELPER_URL` can specify the full `/helper/SIMULATOR_UDID` endpoint.
Only loopback HTTP URLs without credentials or redirects are accepted. Start
checks health and nonzero capture dimensions, not merely a listening TCP port.
Stop closes only this MCP's input socket; the external server stays running.

Input uses a reusable WebSocket, with ordered gesture events and US-keyboard
ASCII typing (at most 512 characters). An uncertain input is never replayed on
reconnection. Fresh captures still use `simctl` by default. To request the
backend's latest JPEG frame without a `simctl` subprocess:

```json
{ "latest_frame": true, "format": "jpeg", "max_dimension": 1280, "quality": 80 }
```

That frame can be replayed by `serve-sim`; **source age is unknown**. It is not
proof of post-action rendering. This MCP reads one frame and closes its stream
subscription rather than forwarding continuous video into the agent context.
Accessibility is fetched only by `device_inspect` (up to 200 elements/depth 16)
and can be unavailable or slower than capture. Select `DEVICE_IOS_BACKEND=cli`
to roll back. No default-backend promotion has been made.

### Android Emulator gRPC prototype (experimental, opt-in)

ADB remains the default, including for physical devices. An experimental
Emulator-only gRPC backend reuses a channel for input and on-demand PNG capture:

- `DEVICE_ANDROID_BACKEND=grpc`
- `ANDROID_GRPC_ENDPOINT`: explicit loopback HTTP endpoint for the intended Emulator.
- `ANDROID_GRPC_TOKEN_FILE`: path to an existing file containing a valid bearer
  token or JWT accepted by that Emulator. No credential value belongs in MCP
  configuration or logs.

The application loads the bounded credential file internally. It never disables
authentication, installs signing keys, logs credentials, or silently switches to
another backend after an uncertain input. The endpoint, not `ANDROID_SERIAL`,
selects the gRPC target; `ANDROID_SERIAL` applies to ADB only. Text is restricted
to supported ASCII. Current gesture dimension resolution uses an on-demand
screenshot, so input speed must be measured before promoting this prototype.
Streaming, JWT provisioning/discovery, real authenticated gRPC validation and
default promotion remain pending. Select `DEVICE_ANDROID_BACKEND=adb` to roll back.

### Measuring performance

```bash
cargo build --release --locked
python3 scripts/benchmark.py --platform ios --samples 20
python3 scripts/benchmark.py --platform android --samples 20 \
  --capture-arguments '{"format":"jpeg","max_dimension":1280,"quality":80}' --step
```

The benchmark uses one persistent MCP connection and discards screenshots. It
reports first-call/p50/p95 latency, response size and failures without printing
screen contents. `--step` tests capture-only batching; `--inspect` adds iOS AX;
`--lifecycle` tests ownership-aware start/stop. None submits input. Compare on
the same device without concurrent builds and distinguish fresh full-resolution
PNG from scaled or cached frames. `RUST_LOG=device_simulator_mcp=debug` enables
redacted operation/process duration and byte-count metrics on stderr; dependency
MCP/HTTP traces are filtered out even if a verbose logging directive is supplied.

## Troubleshooting

If a tool reports that `adb` was not found, install Android SDK
Platform-Tools and add its `platform-tools` directory to `PATH`. Then start an
Android Emulator or connect a device. Set `ANDROID_SERIAL` when more than one
device is available.

If an iOS tool reports that `npx` or `xcrun` was not found, install Node.js
24.21.0 or newer and Xcode Command Line Tools. Run
`xcode-select --install`, ensure `npx` is on `PATH`, and boot an iOS
Simulator.

### iOS keyboard or touch input on Xcode 27

Xcode 27 routes Simulator input through Device Hub. Keep Device Hub open and
make sure the target Simulator window is visible and frontmost. macOS may also
require Accessibility permission for the application that launched this MCP
server (for example, your terminal or editor); check **System Settings →
Privacy & Security → Accessibility**.

If `device_type` or touch input still does not reach the Simulator, call
`device_repair_input` explicitly. It runs `serve-sim repair-input`, which
restarts SpringBoard and closes running apps. Afterward, restart the
`serve-sim` stream with `device_stop` and `device_start`, then reopen your app.
This repair is not run automatically by `device_type`. With the persistent
backend, restart the externally managed `serve-sim` process yourself; MCP
start/stop deliberately do not restart that external process.

## Development

```bash
cargo fmt --all --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-targets
pnpm --dir docs-site install --frozen-lockfile
pnpm --dir docs-site build
```

The landing page lives in [`docs-site/`](docs-site/).
