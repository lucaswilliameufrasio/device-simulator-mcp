# Feature: Physical iOS device backend

## Scope

Add opt-in support for connected physical iPhones without changing the default
iOS Simulator target or its existing `serve-sim` transport.

## Decisions

- `DEVICE_IOS_TARGET=device` opts into physical-device routing. Require an
  explicit `IOS_DEVICE_UDID`; never infer a physical target from simulator
  configuration or select the first connected device.
- Use Xcode `devicectl` for device readiness, screenshots and orientation.
  Keep device I/O in `src/ios_device.rs` and keep MCP schemas stable.
- Use an externally provisioned loopback WebDriverAgent URL for touch, swipe,
  text input and accessibility. The MCP does not sign, install, launch or stop
  WebDriverAgent or the target application.
- Without WebDriverAgent, advertise the native capabilities and reject
  unsupported physical input before a `device_step` submits any action.
- `device_stop` must not stop the physical device or externally managed WDA.
- Keep target identifiers and endpoint configuration out of capability output.

## Consequences

- A physical device can be inspected and captured with Xcode alone. Full UI
  interaction requires the user to configure and provision WebDriverAgent.
- Physical device selection is opt-in, so existing Simulator installations and
  their `IOS_SIMULATOR_UDID` behavior remain unchanged.
- Hardware-dependent smoke tests remain manual; automated tests use command
  and HTTP protocol boundaries.
