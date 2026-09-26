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
- `device_stop`: stop the iOS `serve-sim` stream. Android emulator lifecycle
  remains external to this tool.
- `device_status`: show the selected device status.
- `device_capture`: return the current display as a PNG image.
- `device_tap`: tap a normalized coordinate between `0` and `1`.
- `device_swipe`: swipe between normalized coordinates.
- `device_type`: type into the focused control.
- `device_repair_input`: repair iOS Simulator input services. This restarts
  SpringBoard and closes running apps; use only when input is broken.

The server does not build, install, launch, or modify applications. The
explicit iOS input-repair tool is an exception to normal device interaction:
it restarts SpringBoard and closes open apps. Everything runs locally and
screenshots are returned directly to the MCP host.

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
This repair is not run automatically by `device_type`.

## Development

```bash
cargo fmt --all --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-targets
pnpm --dir docs-site install --frozen-lockfile
pnpm --dir docs-site build
```

The landing page lives in [`docs-site/`](docs-site/).
