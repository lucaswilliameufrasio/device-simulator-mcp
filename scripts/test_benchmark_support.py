import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

import benchmark_support as support


class BenchmarkSupportTests(unittest.TestCase):
    def test_comparison_rejects_cache_resize_implicit_targets_and_grpc(self):
        environment = {"ANDROID_SERIAL": "fixture"}
        support.validate_comparison("android", {}, environment)
        for arguments in [{"latest_frame": True}, {"max_dimension": 1280}, {"max_age_ms": 1}]:
            with self.assertRaises(ValueError):
                support.validate_comparison("android", arguments, environment)
        with self.assertRaises(ValueError):
            support.validate_comparison("ios", {}, {})
        with self.assertRaises(ValueError):
            support.validate_comparison("android", {}, dict(environment, DEVICE_ANDROID_BACKEND="grpc"))

    def test_cpu_time_parses_minutes_and_hours_without_arguments(self):
        self.assertEqual(support.cpu_seconds("1:02.50"), 62.5)
        self.assertEqual(support.cpu_seconds("1:02:03"), 3723)
        with self.assertRaises(ValueError):
            support.cpu_seconds("invalid")

    def test_bounded_process_output_and_deadline(self):
        output = support.run_bounded([sys.executable, "-c", "print('fixture')"], 3)
        self.assertEqual(output, b"fixture\n")
        with mock.patch.object(support, "MAX_BYTES", 8):
            with self.assertRaisesRegex(RuntimeError, "byte limit"):
                support.run_bounded([sys.executable, "-c", "print('123456789')"], 3)
        with self.assertRaisesRegex(RuntimeError, "deadline"):
            support.run_bounded([sys.executable, "-c", "import time; time.sleep(30)"], 0.05)

    def test_direct_ios_capture_uses_unique_paths_and_cleans_them(self):
        paths = []

        def capture(arguments, timeout):
            path = Path(arguments[-1])
            paths.append(path)
            path.write_bytes(b"\x89PNG\r\n\x1a\nfixture")
            return b""

        with tempfile.TemporaryDirectory() as root, mock.patch.object(support, "run_bounded", capture):
            for _ in range(2):
                self.assertEqual(support.direct_capture("ios", {"IOS_SIMULATOR_UDID": "fixture"}, 3, root), 15)
            self.assertNotEqual(paths[0], paths[1])
            self.assertEqual(list(Path(root).iterdir()), [])

    def test_resource_sampler_reports_only_mcp_cpu_delta_and_rss(self):
        with mock.patch.object(support, "process_usage", side_effect=[(1, 1024), (1.02, 2048)]), \
             mock.patch.object(support.time, "monotonic", side_effect=[10, 12]), \
             mock.patch.object(support.time, "sleep"):
            result = support.idle_usage(123, 2)
        self.assertEqual(result["scope"], "mcp_process_only")
        self.assertEqual(result["cpu_percent_one_core"], 1)
        self.assertEqual(result["rss_after_bytes"], 2048)
        self.assertFalse(result["backend_resources_included"])


if __name__ == "__main__":
    unittest.main()
