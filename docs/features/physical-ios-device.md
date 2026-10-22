# Feature: Native physical iPhone control

## Goal

Give the selected physical iPhone interactive controls through this MCP, with a
workflow comparable to Argent, without requiring an externally provisioned
WebDriverAgent URL. Keep `serve-sim` as the independent persistent backend for
iOS Simulators.

## Actors and journey

- The MCP client explicitly discovers and selects a physical iPhone.
- The client starts inspection; the MCP builds/signs and starts its own XCTest
  runner, then connects to it over a USB port forward.
- On first setup, the operator may need to enable Developer Mode, trust the
  development certificate, and approve Xcode signing on the device.
- The client sends taps, swipes, text, and accessibility requests to the
  selected runner session. Each operation is routed to the same selected UDID.
- Stop/shutdown closes only MCP-owned runner and `iproxy` processes. It must not
  power off the phone or stop an app the operator launched.

## Accepted decisions (2026-10-11)

- Use a project-owned XCUITest runner, not a runtime dependency on Argent.
- Keep runner I/O behind the iOS-specific module. Rust MCP code owns target
  resolution, validation, queueing, lifecycle, and response shaping; Swift owns
  XCTest UI interaction and the bounded command protocol.
- Keep `devicectl` for discovery, device status, fresh screenshots, and
  orientation. The XCTest runner supplies physical-device touch, swipe, text,
  and accessibility.
- Originally, use a persistent USB connection forwarded by `iproxy`. The
  optional-transport decision below supersedes the mandatory dependency;
  `iproxy` is supplied by `libusbmuxd`. Never route physical input over an
  untrusted network.
- Require explicit physical-device selection. Resolve the target once per
  operation and never retry uncertain input automatically.
- Keep the public CoreDevice ID as the session/selection key. At startup,
  resolve `hardwareProperties.udid` through that device's `devicectl` details
  and use the hardware UDID only for XCTest destinations and iproxy. Never
  substitute the CoreDevice UUID when the hardware UDID is missing.
- Keep first-run signing/trust requirements explicit. Do not silently change
  Apple account, provisioning, or device settings.
- Provisioning updates and destination-device registration require explicit
  opt-in through `IOS_RUNNER_ALLOW_PROVISIONING_UPDATES=true` (default false).
  This permits Xcode to contact Apple and create/update profiles or register
  the selected device. The operator authorized this opt-in for the physical
  validation; it is not enabled by default for other installations. Simulator
  smoke tests must never enable provisioning updates.
- Preserve local-device testing with an Apple Personal Team. Apple manages its
  app IDs, devices, certificates, and provisioning profiles through Xcode, so
  the headless Developer Portal provisioning path is not the Personal Team
  setup path. `prepare-ios-runner` creates a persistent, team-specific project
  for Xcode to build for testing without running the long-lived XCTest server.
  Physical runner startup in `IOS_RUNNER_SIGNING_MODE=local` uses only matching,
  unexpired Xcode-managed profiles for both the app and `.xctrunner`, including
  the selected device. Xcode signs automatically with those existing local
  profiles; the MCP does not pass provisioning-update or device-registration
  flags. Do not force manual signing or a profile UUID: Xcode rejects manual
  signing settings for its managed profiles. Missing or invalid profiles fail
  preflight, with no fallback to profile creation. Keep opt-in provisioning
  updates for enrolled Developer Program teams. Personal Team profiles expire
  after seven days and follow Apple's device/app ID limits.
- `IOS_WDA_URL` is not required by the new runner. Preserve the existing WDA
  route when it is explicitly configured; otherwise use the project-owned
  XCTest runner.

## Lifecycle and failure behavior

### Optional transport (2026-10-21)

The operator requested that iproxy remain optional. `IOS_RUNNER_TRANSPORT`
selects `xcode` (default, no extra USB utility) or `iproxy` (persistent session).
An initial per-operation XCTest prototype was rejected after a real Simulator
test showed that XCTest terminated the app between commands, losing input focus.
Instead, the Xcode path keeps one XCTest open and exchanges bounded JSON files
through `devicectl device copy` in the runner's app data container. It never
opens a device TCP listener. Both paths still require development signing and
device trust. There is no automatic input fallback/retry after submission. The
Xcode path uses a longer standalone deadline; file transfers cannot guarantee
the bounded 20-second input/AX step deadline, so those steps reject before input.
Simulator production routing is unchanged.

Each session uses a unique mailbox. A separate commit marker is uploaded only
after the payload transfer finishes; the runner requires matching IDs before
input. Requests are claimed monotonically before input, partial uploads are
not executed, responses are written atomically, and timed-out or
cancelled input poisons the session until explicit stop/start. The runner removes
consumed request files and cleans the mailbox on orderly exit. Forced termination
can leave files in its sandbox; no guarantee of secure erasure is made.

Validate real Xcode execution on a disposable Simulator, plus protocol,
transport selection, rejection, and failure tests. Physical signing and USB
validation remain separate and must not be claimed from Simulator results.

The per-device session moves through `stopped -> starting -> awaiting_runner ->
ready -> stopping -> stopped`, or `failed` on build, signing, trust, tunnel, or
protocol errors. A session is keyed by the selected device UDID. Startup must
report actionable, redacted errors; stop and graceful MCP shutdown reap only
processes owned by this MCP. On timeout or uncertain input, report partial
completion and never resubmit the action.

## Implementation slices

1. Define a bounded, versioned request/response protocol between Rust and the
   Swift XCTest runner; test serialization and malformed/oversized messages.
2. Add the runner project and implement coordinate input, text, app-scoped
   accessibility, screenshot response, health, and stop commands.
3. Add an owned per-UDID lifecycle for `xcodebuild test` and `iproxy`; handle
   startup readiness, process exit, timeout, cancellation, and MCP shutdown.
4. Route physical iPhone operations and capabilities through the runner while
   leaving Simulator, Android, and legacy WDA fallback behavior isolated.
5. Add fake-process and local-protocol tests, then validate runner build and
   behavior on a simulator. Only after explicit operator approval, install and
   exercise the runner on the connected physical iPhone.
6. Update README setup and run the Rust checks plus docs-site build.

## Validation status

- Rust runner manager and both transport paths are implemented. Unit/protocol
  tests cover selection, device/container-scoped transfers, bounded responses,
  cancellation poisoning, and step rejection for Xcode-only control.
- Opt-in real Xcode smoke test (only a disposable local Simulator):

  ```bash
  IOS_RUNNER_TEST_SIMULATOR_UDID=<disposable-simulator-udid> cargo test --locked executes_real_xcode_commands_without_iproxy -- --ignored --nocapture
  ```

  The test signs nothing and bypasses neither physical-device signing nor trust.
  It builds and runs the real persistent XCTest and validates launch, coordinate
  tap, text input with focus preserved, AX readback, swipe, and stop. Simulator
  container file access substitutes for physical `devicectl` transfers; those
  Apple file-transfer calls still need a signed physical-device test. The first
  successful persistent-file run completed in 48 seconds on this Mac. Earlier
  per-operation prototypes timed out or lost app state and were not accepted.
- On 2026-10-22, the Personal Team local-profile flow successfully started the
  MCP-owned XCTest runner over the selected physical iPhone's `iproxy` transport.
  The runner screen was visually verified. A `device_step` tap and text entry
  completed, and a fresh capture showed the entered validation text.
- A fresh runner start was used for a read-only physical accessibility check.
  Querying `runner-validation-input` returned the enabled field and its value.
  `device_stop` then stopped the MCP-owned XCTest runner and USB forwarder; a
  second stop confirmed that no MCP-owned session remained. The runner app
  remained visible on the iPhone, which was intentionally left powered on.
- Headless automatic provisioning with explicit provisioning updates previously
  failed with `No Account for Team`; the local Xcode-managed-profile path avoids
  that setup issue. The runner now explicitly requests automatic Xcode signing
  while omitting provisioning-update and device-registration flags.
