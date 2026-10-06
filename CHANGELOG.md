# Changelog

All notable changes to this project will be documented in this file.

## [Unreleased]

### Features

- Add case-sensitive accessibility filters and configurable bounded output
  limits for inspection and step observations.
- Add opt-in bounded raw-frame cache, fresh visual change/stability waits and
  explicit accessibility observations in `device_step`.
- Reuse the fresh frame that satisfies a visual wait and add bounded iOS
  element-present waits with exact label/identifier selectors.
- Add bounded action batches with optional final capture through `device_step`.
- Add opt-in persistent iOS control, latest-frame capture and accessibility inspection.
- Add optional JPEG, resize and crop with coordinate-transform metadata.
- Add an experimental authenticated Android Emulator gRPC backend; ADB remains the default.
- Add read-only persistent-session latency benchmarks and redacted diagnostics.

### Bug Fixes

- Bound operation queues, subprocess output and deadlines; propagate MCP cancellation.
- Isolate temporary captures and terminate owned subprocess groups on Unix.
- Query Android dimensions once per swipe, respect override size and map edges to valid pixels.
- Track iOS helper ownership and preserve externally managed streams on stop/shutdown.
- Pin serve-sim and support a preinstalled executable for offline CLI use.

## [0.1.5] - 2026-09-26

### Features

- Add AI client MCP installer CLI

## [0.1.4] - 2026-09-26

### Bug Fixes

- Expose iOS simulator input repair


### Chores

- Add release notes and project skills


### Other

- Serialize platform environment tests

## [0.1.3] - 2026-09-13

### Bug Fixes

- Keep public installer executable

- Replace Astro favicon

- Clarify platform setup instructions

- Explain missing device dependencies


### Documentation

- Prepare repository for public release

## [0.1.2] - 2026-09-13

### CI / Build

- Configure cargo-dist action pins


### Features

- Redesign device simulator landing page


### Other

- Add mocked backend and device e2e coverage

## [0.1.1] - 2026-09-12

### Features

- Harden private device distribution

## [0.1.0] - 2026-09-12

### Bug Fixes

- Harden device backends and project guidance

- Make release preparation retryable


### Features

- Add cross-platform device simulator MCP
