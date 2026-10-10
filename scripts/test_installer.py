"""Exercise the real shell installer offline with fixture release downloads.

Host selection is simulated; this does not validate foreign-platform binaries.
All files and destinations live in automatically cleaned temporary directories.
"""

import hashlib
import io
import json
import os
import subprocess
import sys
import tarfile
import tempfile
import unittest
from pathlib import Path

INSTALLER = Path(__file__).resolve().parent / "install.sh"


class InstallerTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="installer-test-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.commands = self.root / "commands"
        self.commands.mkdir()
        self.downloads = self.root / "downloads"
        self.downloads.mkdir()
        self.scratch = self.root / "scratch"
        self.scratch.mkdir()
        self.destination = self.root / "installed bin"
        self.environment = dict(os.environ, HOME=str(self.root),
                                PATH=str(self.commands) + os.pathsep + os.defpath,
                                TMPDIR=str(self.scratch),
                                FIXTURE_ROOT=str(self.root), FIXTURE_OS="Darwin",
                                FIXTURE_ARCH="arm64", FIXTURE_MODE="success")
        self.write_command("uname", """import os, sys
print(os.environ['FIXTURE_OS' if sys.argv[1] == '-s' else 'FIXTURE_ARCH'])
""")
        self.write_command("gh", """import json, os, pathlib, shutil, sys
root = pathlib.Path(os.environ['FIXTURE_ROOT'])
arguments = sys.argv[1:]
(root / 'gh-arguments.json').write_text(json.dumps(arguments))
if os.environ['FIXTURE_MODE'] == 'offline':
    sys.exit(1)
destination = pathlib.Path(arguments[arguments.index('--dir') + 1])
for index, argument in enumerate(arguments):
    if argument == '--pattern':
        name = arguments[index + 1]
        shutil.copyfile(root / 'downloads' / name, destination / name)
""")

    def write_command(self, name, body):
        path = self.commands / name
        path.write_text(f"#!{sys.executable}\n" + body)
        path.chmod(0o755)

    def release(self, target, checksum_valid=True, include_binary=True):
        asset = self.downloads / f"device-simulator-mcp-{target}.tar.xz"
        payload = b"#!/bin/sh\nprintf 'fixture binary\\n'\n"
        with tarfile.open(asset, "w:xz") as archive:
            entry = tarfile.TarInfo("release/device-simulator-mcp" if include_binary else "release/other")
            entry.size = len(payload)
            entry.mode = 0o755
            archive.addfile(entry, io.BytesIO(payload))
        checksum = hashlib.sha256(asset.read_bytes()).hexdigest() if checksum_valid else "0" * 64
        Path(str(asset) + ".sha256").write_text(f"{checksum}  {asset.name}\n")
        return payload

    def run_installer(self, *arguments):
        return subprocess.run(["bash", str(INSTALLER), "--bin-dir", str(self.destination), *arguments],
                              env=self.environment, capture_output=True, text=True, timeout=10)

    def assert_clean(self):
        self.assertEqual(list(self.scratch.iterdir()), [])

    def test_clean_install_and_all_supported_host_selections(self):
        hosts = [("Darwin", "arm64", "aarch64-apple-darwin"),
                 ("Linux", "aarch64", "aarch64-unknown-linux-gnu"),
                 ("Linux", "amd64", "x86_64-unknown-linux-gnu")]
        for operating_system, architecture, target in hosts:
            with self.subTest(target=target):
                self.environment.update(FIXTURE_OS=operating_system, FIXTURE_ARCH=architecture)
                expected = self.release(target)
                result = self.run_installer("--tag", "v0.0.0-fixture")
                self.assertEqual(result.returncode, 0, result.stderr)
                installed = self.destination / "device-simulator-mcp"
                self.assertEqual(installed.read_bytes(), expected)
                self.assertEqual(installed.stat().st_mode & 0o777, 0o755)
                arguments = json.loads((self.root / "gh-arguments.json").read_text())
                self.assertEqual(arguments[:3], ["release", "download", "v0.0.0-fixture"])
                self.assertIn(f"device-simulator-mcp-{target}.tar.xz", arguments)
                self.assert_clean()

    def test_latest_omits_tag_and_installed_fixture_runs_without_download(self):
        self.release("aarch64-apple-darwin")
        self.assertEqual(self.run_installer().returncode, 0)
        arguments = json.loads((self.root / "gh-arguments.json").read_text())
        self.assertEqual(arguments[:3], ["release", "download", "--repo"])
        self.environment["FIXTURE_MODE"] = "offline"
        result = subprocess.run([str(self.destination / "device-simulator-mcp")],
                                env=self.environment, capture_output=True, text=True, timeout=3)
        self.assertEqual(result.stdout, "fixture binary\n")
        self.assert_clean()

    def test_checksum_missing_payload_and_offline_failures_preserve_existing_install(self):
        self.destination.mkdir()
        installed = self.destination / "device-simulator-mcp"
        installed.write_bytes(b"existing installation")
        for failure in ["checksum", "missing-binary", "offline"]:
            with self.subTest(failure=failure):
                self.release("aarch64-apple-darwin", checksum_valid=failure != "checksum",
                             include_binary=failure != "missing-binary")
                self.environment["FIXTURE_MODE"] = "offline" if failure == "offline" else "success"
                result = self.run_installer()
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(installed.read_bytes(), b"existing installation")
                self.assert_clean()

    def test_unsupported_host_and_unknown_option_do_not_download(self):
        for operating_system, architecture in [("Unsupported", "arm64"),
                                               ("Darwin", "x86_64"),
                                               ("Darwin", "amd64")]:
            with self.subTest(host=f"{operating_system}/{architecture}"):
                self.environment.update(FIXTURE_OS=operating_system, FIXTURE_ARCH=architecture)
                self.assertNotEqual(self.run_installer().returncode, 0)
        self.assertNotEqual(self.run_installer("--unknown").returncode, 0)
        self.assertFalse((self.root / "gh-arguments.json").exists())
        self.assertFalse(self.destination.exists())
        self.assert_clean()


if __name__ == "__main__":
    unittest.main()
