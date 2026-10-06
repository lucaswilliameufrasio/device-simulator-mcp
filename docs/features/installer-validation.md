# Installer validation evidence

## Isolated offline fixtures

`python3 -m unittest discover -s scripts -p 'test_*.py'` exercises the real
shell installer with fixture `gh` downloads and simulated `uname` responses.
The suite uses real archive extraction, SHA-256 checking and file installation.
All destinations, downloads and temporary files are isolated and cleaned up.

Coverage includes:

- All three supported shell-installer target selections (Apple Silicon macOS,
  Linux ARM64 and Linux x86_64), explicit tags and latest.
- Intel macOS (`x86_64` and `amd64`) is intentionally rejected; the project does
  not support Intel Macs. Existing historical release assets are not withdrawn.
- Destination paths with spaces, executable permissions and temporary cleanup.
- Checksum mismatch, missing binary and offline download failure without
  replacing an existing installation.
- Unsupported hosts and unknown arguments rejected before download.
- Execution of a provisioned fixture while downloads are unavailable.

CI schedules these fixtures on Linux and macOS. Simulated host selection does
not validate foreign-platform release binaries, and fixture downloads do not
validate GitHub network/authentication behavior. Offline installer download is
expected to fail; offline execution after provisioning is a different claim.

## Local evidence — 2026-10-06

All nine Python script tests passed on the current macOS host (five benchmark
helper tests and four installer test methods, including host/failure subcases).
The locally built native release binary was also packaged into a fixture archive
and installed through the real shell installer into a temporary destination.
`--version` and `install-mcp --help` succeeded there with fixture downloads
disabled. Temporary files were cleaned; the real MCP configuration was unchanged.
This verifies native CLI startup after provisioning, not complete offline device
backend operation. Linux/macOS CI jobs have been configured but not yet run for
this local follow-up.

## Remaining acceptance work

Current-build Linux target execution, clean online provisioning, persistent
helper offline provisioning and real-device/harness recovery flows still need
separate validation. Do not change installed MCP configuration or publish a
release merely to run these tests.

## Pull request distribution checks

PRs use cargo-dist `pr-run-mode = "upload"` to build the configured platform
artifacts and attach them to the workflow run. This validates the distribution
matrix without creating a GitHub release; actual hosting and release publication
remain tag-triggered. The default plan-only mode is insufficient evidence that
the configured targets compile. Revisit if PR artifact build time or cargo-dist
semantics change.
