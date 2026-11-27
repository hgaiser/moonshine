"""Hardware-independent regression checks for the Moonshine smoke-test runner."""

import fcntl
import importlib.util
import json
import os
import socket
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

MODULE = Path(__file__).with_name("verify_video.py")
spec = importlib.util.spec_from_file_location("verify_video", MODULE)
verify_video = importlib.util.module_from_spec(spec)
spec.loader.exec_module(verify_video)


class VerifyVideoTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.lock_path = Path(self.temp.name) / "hw.lock"
        env_patch = patch.dict(os.environ, {"MOONSHINE_HW_LOCK_PATH": str(self.lock_path)})
        env_patch.start()
        self.addCleanup(env_patch.stop)

    def test_completed_session_not_intervals(self):
        log = "Interval [3 frames, 40.0 fps, 3.0 Mbps]\nSession [12 frames, 58.0 fps, 10.0 Mbps]"
        self.assertEqual(verify_video.summarize(log)["frames"], 12)
        self.assertEqual(verify_video.summarize(log)["fps"], 58.0)

    def test_no_session_is_not_success(self):
        log = "No frames were encoded during the session.\nDone."
        self.assertEqual(verify_video.summarize(log)["frames"], 0)
        self.assertIsNone(verify_video.summarize(log)["fps"])

    def test_timing_optional(self):
        no_gpu = "Session [5 frames, 60.0 fps, 30.0 Mbps]"
        self.assertIsNone(verify_video.summarize(no_gpu)["gpu_timing_samples"])
        gpu = no_gpu + "\ngpu_encode: samples=5 avg=100us p50=100us p95=101us; bitstream_ready: avg=200us"
        self.assertEqual(verify_video.summarize(gpu)["gpu_timing_samples"], 5)

    def test_zero_frames_fails_smoke_even_on_success_exit(self):
        with tempfile.TemporaryDirectory() as folder:
            checks = {"app": "/bin/true", "bench": "/bin/true", "render_nodes": ["/dev/dri/renderD128"], "problems": []}
            with patch.object(verify_video, "preflight", return_value=checks), patch.object(
                verify_video, "run_benchmark", return_value=(0, False, "No frames were encoded during the session.")
            ):
                result = verify_video.main(["smoke", "--output", folder])
            self.assertEqual(result, 1)
            report = json.loads((Path(folder) / "report.json").read_text())
            self.assertEqual(report["verdict"], "FAIL")
            self.assertEqual(report["results"]["frames"], 0)

    def test_positive_frames_pass_smoke(self):
        with tempfile.TemporaryDirectory() as folder:
            checks = {
                "app": "/bin/true",
                "bench": "/bin/true",
                "bench_provenance": {"path": "/bin/true", "sha256": "abc123", "cargo_target_dir": None},
                "render_nodes": ["/dev/dri/renderD128"],
                "problems": [],
            }
            with patch.object(verify_video, "preflight", return_value=checks), patch.object(
                verify_video, "run_benchmark", return_value=(0, False, "Session [60 frames, 59.8 fps, 10.0 Mbps]")
            ):
                result = verify_video.main(["smoke", "--output", folder, "--min-fps", "50"])
            self.assertEqual(result, 0)
            report = json.loads((Path(folder) / "report.json").read_text())
            self.assertEqual(report["verdict"], "PASS")
            self.assertEqual(report["results"]["frames"], 60)
            self.assertEqual(report["bench_sha256"], "abc123")

    def test_existing_evidence_is_not_overwritten(self):
        with tempfile.TemporaryDirectory() as folder:
            report_path = Path(folder) / "report.json"
            report_path.write_text("original proof")
            checks = {"app": "/bin/true", "bench": "/bin/true", "render_nodes": ["/dev/dri/renderD128"], "problems": []}
            with patch.object(verify_video, "preflight", return_value=checks), patch.object(
                verify_video, "run_benchmark"
            ) as run:
                result = verify_video.main(["smoke", "--output", folder])
            self.assertEqual(result, 2)
            self.assertEqual(report_path.read_text(), "original proof")
            run.assert_not_called()

    def test_preflight_failure_does_not_start_process(self):
        with tempfile.TemporaryDirectory() as folder:
            checks = {"app": None, "bench": "/bin/true", "render_nodes": [], "problems": ["no renderer"]}
            with patch.object(verify_video, "preflight", return_value=checks), patch.object(
                verify_video, "run_benchmark"
            ) as run:
                result = verify_video.main(["smoke", "--output", folder])
            self.assertEqual(result, 2)
            self.assertFalse((Path(folder) / "report.json").exists())
            run.assert_not_called()

    def test_occupied_udp_port_is_rejected(self):
        with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as held:
            held.bind(("127.0.0.1", 0))
            with patch.object(verify_video, "VIDEO_PORT", held.getsockname()[1]):
                with self.assertRaises(OSError):
                    verify_video.check_port_free()

    def test_detect_active_vcn_ignores_static_counter_and_flags_increasing(self):
        first = {(101, "1"): ("helium", 5000), (202, "2"): ("firefox", 9000)}
        second = {(101, "1"): ("helium", 8500), (202, "2"): ("firefox", 9000)}
        with patch.object(verify_video, "sample_vcn_clients", side_effect=[first, second]):
            active = verify_video.detect_active_vcn(sample_seconds=0)
        self.assertEqual(len(active), 1)
        self.assertEqual(active[0]["comm"], "helium")
        self.assertEqual(active[0]["delta_ns"], 3500)

    def test_preflight_blocks_busy_vcn_unless_allowed(self):
        fake_bench = Path(self.temp.name) / "bench"
        fake_bench.write_text("#!/bin/sh\n")
        fake_bench.chmod(0o755)
        with patch.object(verify_video, "resolve_executable", return_value=str(fake_bench)), \
             patch.object(verify_video, "check_port_free"), \
             patch.object(verify_video, "check_moonshine_server_free", return_value=None), \
             patch.object(verify_video, "detect_active_vcn", return_value=[{"pid": 42, "comm": "helium", "client_id": "1", "delta_ns": 1000}]), \
             patch.object(Path, "glob", return_value=[fake_bench]):
            blocked = verify_video.preflight("vkcube", str(fake_bench), allow_vcn_busy=False, lock_path=self.lock_path)
            allowed = verify_video.preflight("vkcube", str(fake_bench), allow_vcn_busy=True, lock_path=self.lock_path)
        self.assertTrue(any("active VCN" in p for p in blocked["problems"]))
        self.assertFalse(any("active VCN" in p for p in allowed["problems"]))

    def test_preflight_rejects_identical_compare_against_binary(self):
        bench_a = Path(self.temp.name) / "bench-a"
        bench_b = Path(self.temp.name) / "bench-b"
        bench_c = Path(self.temp.name) / "bench-c"
        bench_a.write_bytes(b"same-binary")
        bench_b.write_bytes(b"same-binary")
        bench_c.write_bytes(b"different-binary")
        for path in (bench_a, bench_b, bench_c):
            path.chmod(0o755)
        with patch.object(verify_video, "resolve_executable", side_effect=lambda v: str(Path(v).resolve())), \
             patch.object(verify_video, "check_port_free"), \
             patch.object(verify_video, "check_moonshine_server_free", return_value=None), \
             patch.object(verify_video, "detect_active_vcn", return_value=[]), \
             patch.object(Path, "glob", return_value=[bench_a]):
            collided = verify_video.preflight(str(bench_a), str(bench_a), compare_against=str(bench_b), lock_path=self.lock_path)
            distinct = verify_video.preflight(str(bench_a), str(bench_a), compare_against=str(bench_c), lock_path=self.lock_path)
        self.assertTrue(any("CARGO_TARGET_DIR collision" in p for p in collided["problems"]))
        self.assertEqual(distinct["problems"], [])

    def test_preflight_rejects_active_moonshine_server(self):
        fake_bench = Path(self.temp.name) / "bench"
        fake_bench.write_text("#!/bin/sh\n")
        fake_bench.chmod(0o755)
        with patch.object(verify_video, "resolve_executable", return_value=str(fake_bench)), \
             patch.object(verify_video, "check_port_free"), \
             patch.object(verify_video, "check_moonshine_server_free", return_value={"state": "MOONSHINE_SERVER_BUSY", "currentgame": "1"}), \
             patch.object(verify_video, "detect_active_vcn", return_value=[]), \
             patch.object(Path, "glob", return_value=[fake_bench]):
            res = verify_video.preflight("vkcube", str(fake_bench), lock_path=self.lock_path)
        self.assertTrue(any("production Moonshine server is not idle" in p for p in res["problems"]))

    def test_hw_lock_refuses_concurrent_smoke(self):
        with open(self.lock_path, "a") as held:
            fcntl.flock(held, fcntl.LOCK_EX | fcntl.LOCK_NB)
            with patch.object(verify_video, "run_benchmark") as run:
                result = verify_video.main(["smoke", "--output", str(Path(self.temp.name) / "out")])
            self.assertEqual(result, 2)
            run.assert_not_called()


if __name__ == "__main__":
    unittest.main()
