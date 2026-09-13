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

The server does not build, install, launch, reset, or modify applications.
Everything runs locally and screenshots are returned directly to the MCP host.

## Development

```bash
cargo fmt --all --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-targets
pnpm --dir docs-site install --frozen-lockfile
pnpm --dir docs-site build
```

The landing page lives in [`docs-site/`](docs-site/).
