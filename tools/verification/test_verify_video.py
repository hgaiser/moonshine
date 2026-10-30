"""Hardware-independent regression checks for the Moonshine smoke-test runner."""

import importlib.util
import json
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
            checks = {"app": "/bin/true", "bench": "/bin/true", "render_nodes": ["/dev/dri/renderD128"], "problems": []}
            with patch.object(verify_video, "preflight", return_value=checks), patch.object(
                verify_video, "run_benchmark", return_value=(0, False, "Session [60 frames, 59.8 fps, 10.0 Mbps]")
            ):
                result = verify_video.main(["smoke", "--output", folder, "--min-fps", "50"])
            self.assertEqual(result, 0)
            report = json.loads((Path(folder) / "report.json").read_text())
            self.assertEqual(report["verdict"], "PASS")
            self.assertEqual(report["results"]["frames"], 60)


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


if __name__ == "__main__":
    unittest.main()
