# Feature: Multi-device discovery and selection

## Scope

Discover and explicitly select iOS Simulators, physical iPhones, Android ADB
devices, and configured Android gRPC targets. Preserve the existing platform
backends and legacy environment configuration without silently choosing a
device.

## Decisions

- Resolve targets in this order: per-call `device_id`, MCP-instance selection,
  then an explicit legacy environment target.
- `device_list` reports discoverable targets and discovery errors;
  `device_select` stores or clears the instance default without mutating the
  process environment.
- Use stable type-prefixed IDs: `ios-simulator:`, `ios-device:`, `android:`,
  and `android-grpc:`.
- Require an explicit Simulator UDID, iPhone UDID, Android serial, or gRPC
  endpoint for legacy fallback. Never infer the first booted/connected device.
- Resolve a target before entering the shared operation queue. A `device_step`
  uses one immutable target for all actions and its final observation.
- Keep backend and frame-cache state keyed by target; cache invalidation is
  scoped to the affected target.
- Preserve the existing iPhone `devicectl` and WebDriverAgent implementation;
  multi-device support routes to it rather than replacing it.

## Validation Status

- Automated fake-command tests cover discovery, stable IDs, instance versus
  call-level precedence, Android routing, explicit legacy serial selection, and
  target stability while selection changes during a step.
- On 2026-10-22, a selected physical iPhone was validated with the Personal Team
  local-profile flow over `iproxy`: runner startup, visual verification,
  tap/text input, accessibility readback, and orderly MCP-owned runner stop all
  succeeded. The iPhone was left powered on.
