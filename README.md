# Device Simulator MCP

Local MCP server for inspecting and controlling iOS Simulators, physical
iPhones, and Android devices. It is framework-agnostic: the app can be Flutter,
SwiftUI, UIKit, React Native, or any other app that runs on the target device.

## Requirements

- Rust 1.98 or newer to build from source.
- iOS: macOS and Xcode Command Line Tools. Simulator `serve-sim` interactions
  also require Node.js 24.22.1 or newer and `npx`.
- Android: Android SDK platform tools and `adb`; connect a device or start an
  Emulator.

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

# Persist runner transport and local signing settings without selecting a device.
device-simulator-mcp install-mcp --client opencode \
  --ios-runner-transport iproxy --ios-runner-signing-mode local --apply

# Run from the project root to add the server only to that project.
device-simulator-mcp install-mcp --client cursor --scope project --apply
```

Use `--config-file PATH` to target a non-default configuration file or
`--binary PATH` if the installed executable should be registered at another
path. JSON/JSONC comments and Codex TOML comments are preserved. Restart the AI
client after applying the configuration. This command registers the stdio MCP
server; it does not install lifecycle hooks.
The optional `--ios-runner-transport` (`xcode` or `iproxy`) and
`--ios-runner-signing-mode` (`automatic` or `local`) flags configure the matching
MCP environment variables. They are iOS-only; when omitted, existing values are
preserved. Local signing still requires `IOS_DEVICE_TEAM_ID` to be configured
separately. The installer does not persist a device UDID or team ID: select a
physical iPhone explicitly with `device_select`.

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

### Device discovery and selection

Use `device_list` to discover iOS Simulators, connected physical iPhones, and
ADB Android devices. The server never picks the first available or booted device.
Select a default for this MCP instance with `device_select`:

```json
{ "device_id": "ios-simulator:11111111-1111-4111-8111-111111111111" }
```

Stable IDs are prefixed by target type: `ios-simulator:<UDID>`,
`ios-device:<UDID>`, `android:<serial>`, and `android-grpc:<endpoint>`. Device
operation tools also accept an optional `device_id` to override the instance
default for that call. `device_step` resolves one target for its entire sequence;
its nested `capture` uses that same target and cannot override it. Omitting a
call override and instance default uses the explicit legacy environment target
when configured; otherwise the operation asks for a selection. Existing
single-device configurations remain supported: iOS requires
`IOS_SIMULATOR_UDID` (or `DEVICE_IOS_TARGET=device` with `IOS_DEVICE_UDID`), and
ADB requires `ANDROID_SERIAL`; Android gRPC requires its explicit endpoint and
token-file configuration. Call `device_select` without `device_id` to clear the
instance default. The server does not change environment variables when
selecting a target.

## Tools

- `device_list`: list discoverable targets, their availability, and the current
  selection source.
- `device_select`: set or clear the default target for this MCP instance.
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
- `device_rotate`: set one of the four explicit orientations. Android ADB mode
  disables automatic rotation; experimental Android gRPC mode does not support
  this operation.
- `device_step`: execute a bounded, known sequence of actions with an optional
  final screenshot in one MCP call.
- `device_inspect`: inspect a bounded accessibility snapshot through persistent
  iOS or Android ADB UIAutomator. Password values are redacted.
- `device_multitouch`: submit simultaneous Android Emulator contacts through the
  experimental authenticated gRPC backend. ADB does not support this operation.
- `device_repair_input`: repair iOS Simulator input services. This restarts
  SpringBoard and closes running apps; use only when input is broken.

The server does not build, install, launch, or modify applications. The
explicit iOS input-repair tool is an exception to normal device interaction:
it restarts SpringBoard and closes open apps. Everything runs locally and
screenshots are returned directly to the MCP host.

`device_multitouch` sends each frame's contacts in one Emulator Controller RPC.
Frames use `down`, `move`, and `up` phases with stable contact IDs; every contact
must be released in the final frame. For example, two contacts can move together
and then be released together. The operation is available only with the
experimental authenticated Android gRPC backend; ADB mode reports it as
unsupported rather than converting it to sequential gestures.

## Faster agent loops

Use `device_capabilities` to discover configured backend support and limits
without device I/O. The response explicitly does not probe runtime availability;
use status/start for readiness. Capability output does not include endpoints or
credentials, and Android gRPC is marked experimental.

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
  target from a `device_id` override, instance selection, or legacy environment
  configuration. Cache entries are isolated by device and backend. Metadata
  reports `cache_hit`, `local_age_ms`, and the original
  receive timestamp. Local age is **not source age** for serve-sim replay.
  Mutations, repair, start and stop invalidate the cache even if they fail.
  No background capture is started. Visual waits reject final captures requesting
  cache reuse or `latest_frame`; independent captures remain compatible.
- Per server, operations share ordering and a bounded queue of eight calls.
  Normal calls have a 20-second total deadline; persistent helper startup allows
  25 seconds and physical XCTest runner startup allows up to five minutes.
  Xcode-only standalone input/inspection also allows five minutes per call.
  Short-lived command processes have a 20-second deadline and a 24 MiB output
  limit per pipe.
- Cancellation terminates subprocesses; Unix additionally terminates their
  process groups. Persistent gestures attempt bounded release of held contacts
  or keys on cancellation. Results can still be uncertain: never blindly retry.

### Physical iOS device backend (opt-in)

Physical iPhones are discovered but never selected automatically; choose one
with `device_select` or a per-call `device_id`. The default remains unselected
unless a target is configured. `devicectl` provides device readiness, screenshots
and orientation. Without `IOS_WDA_URL`, `device_start` builds, signs, installs,
and runs this project's XCTest UI runner. `IOS_RUNNER_TRANSPORT` selects:

- `xcode` (default): only Xcode, no `iproxy` installation or device TCP listener.
  One XCTest stays open, exchanging bounded JSON files through `devicectl`
  app-container transfers. This is slower than a socket; use standalone
  `device_tap`, `device_swipe`, `device_type`, and
  `device_inspect`. Input/AX `device_step` requests are rejected before input
  because they cannot meet the 20-second step deadline. Capture-only steps work.
- `iproxy`: a persistent XCTest session over USB. The runner and local forwarder
  listen only on loopback, never Wi-Fi. Supports input/AX steps.

```json
{
  "DEVICE_PLATFORM": "ios",
  "DEVICE_IOS_TARGET": "device",
  "IOS_DEVICE_UDID": "<connected iPhone UDID>",
  "IOS_DEVICE_TEAM_ID": "<10-character Apple Developer Team ID>"
}
```

For the optional persistent transport:

```bash
brew install libusbmuxd
iproxy --version
```

Set `IOS_RUNNER_TRANSPORT=iproxy` in the MCP environment to select it.
`IOS_IPROXY_BINARY` can specify a non-default executable path. No automatic
fallback/retry occurs after an input failure. Both transports need Xcode.
The first `device_start`
requires a paired, unlocked iPhone with Developer Mode enabled and approval of
the signing certificate on the phone. For an enrolled Apple Developer Program
team, Xcode automatic signing uses the supplied Team ID. If profiles do not
already exist, explicitly set `IOS_RUNNER_ALLOW_PROVISIONING_UPDATES=true` to
permit Xcode to contact Apple, create/update provisioning profiles, and register
the selected device with the team. This is disabled by default; no
certificate/private key is exported.

For an Apple Personal Team, use Xcode-managed local profiles instead of
Developer Portal provisioning:

```bash
device-simulator-mcp prepare-ios-runner --team-id YOUR_TEAM_ID
```

Open the printed project in Xcode, select the connected iPhone as the
destination, and choose **Product > Build For > Testing**. Do not choose
**Product > Test**: the XCTest server intentionally remains running during its
test. Set `IOS_RUNNER_SIGNING_MODE=local` in the MCP environment and restart the
MCP. Before startup, the runner validates unexpired local profiles matching the
team, selected device, app bundle ID, and XCTest runner bundle ID. Xcode then
uses automatic signing with those existing Xcode-managed profiles; the MCP does
not pass provisioning-update or device-registration flags. If the profiles are
missing or invalid, startup fails instead of requesting their creation. The
prepared project is kept under the user's Application Support directory and is
not overwritten by a later prepare command. Personal Team profiles expire after
seven days, so repeat the Xcode build-for-testing step when profiles expire.
Apple documents Personal Team limits of 10 App IDs and 3 devices.

`device_start` accepts an optional `app_bundle_id` when using the
XCTest runner; without it, the runner starts against SpringBoard. Starting the
runner installs its test app on the phone. `device_stop` stops only MCP-owned
XCTest and USB-forwarding processes; it does not power off the phone or stop
other apps. A configured
`IOS_WDA_URL` continues to select the existing externally managed WebDriverAgent
path instead. `device_capabilities` reports configured support, not whether the
runner has been built or the phone is ready.

The Xcode-only runner is validated on an isolated iOS Simulator for launch,
tap, typing with focus preserved, accessibility readback, swipe, and stop. The
Simulator test accesses its local container rather than running physical-device
file transfers. The physical iPhone runner has also been validated with an
Xcode-managed Personal Team profile, including startup, USB forwarding, visual
verification, tap/text input, accessibility readback, and stop; see
[`docs/features/physical-ios-device.md`](docs/features/physical-ios-device.md).
Physical use still requires a paired device and an active development profile.

### Persistent iOS backend (opt-in)

The default `cli` backend retains CLI interactions, using pinned
`serve-sim@0.1.47`. `device_start` reuses a running helper or starts an owned
foreground helper that is stopped on explicit stop or graceful MCP shutdown.
It uses the resolved Simulator target; without a selection, the legacy
configuration requires an explicit `IOS_SIMULATOR_UDID`.
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

`device_inspect` accepts optional `max_elements` (1–200), `max_depth` (1–16),
`label_contains`, exact `identifier`, and exact `role` (or `type` if role is
absent). Filters are case-sensitive and all match the same node. Empty arguments
keep the existing defaults. Results report `truncated`; traversal is capped at
4096 values, strings at 512 characters, and serialized output at 256 KiB.
Limits or filters cannot prove absence outside the inspected portion of a tree.
Steps can use the same options in `accessibility_options`, but only with
`accessibility: true`. Element waits search the default bounded projection;
output filters apply after matching, without another AX fetch or screenshot.

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

For an equivalent read-only comparison with direct CLI, use an explicit target
and **default fresh full-resolution PNG** capture arguments:

```bash
IOS_SIMULATOR_UDID=SIMULATOR_UDID python3 scripts/benchmark.py --platform ios \
  --compare-cli --samples 20 --idle-seconds 5
ANDROID_SERIAL=emulator-5554 python3 scripts/benchmark.py --platform android \
  --compare-cli --samples 20 --idle-seconds 5
python3 -m unittest discover -s scripts -p 'test_benchmark*.py'
```

The comparison alternates CLI-first/MCP-first capture pairs. It refuses resize,
JPEG/cache/latest-frame arguments and Android gRPC, avoiding non-equivalent
speed claims. CLI results report raw PNG bytes; MCP reports JSON response bytes
including base64, protocol encoding/delivery and client JSON decoding. Direct iOS
capture uses a unique temporary PNG and deletes it; `--temporary-root` can select
the parent directory. No pixels are printed. Idle measurements report CPU-time
delta and RSS for the MCP process only, **not Simulator/Emulator or external
backend resources**. Small local samples are not a universal performance SLA.
The CPU counter is coarse: a zero measured delta does not prove zero CPU usage.

Run all script fixtures (including the isolated offline shell installer tests)
with `python3 -m unittest discover -s scripts -p 'test_*.py'`. Installer fixtures
cover release target selection, checksum enforcement, cleanup and preservation
of existing installs on failure. They do not download GitHub assets or validate
foreign-platform binaries; see `docs/features/installer-validation.md`.

## Troubleshooting

If a tool reports that `adb` was not found, install Android SDK
Platform-Tools and add its `platform-tools` directory to `PATH`. Then start an
Android Emulator or connect a device. Set `ANDROID_SERIAL` when more than one
device is available.

If an iOS tool reports that `npx` or `xcrun` was not found, install Node.js
24.22.1 or newer and Xcode Command Line Tools. Run
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
