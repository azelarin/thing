"""test the renderer harness without starting a compositor or graphics backend."""

import json
import os
from pathlib import Path
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import time
import unittest
from unittest import mock

import check_rendering
from check_rendering import FixtureError, Ppm, atomic_json, stop_process_group


class PpmTest(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="thing-render-test-")
        self.addCleanup(self.directory.cleanup)
        self.path = Path(self.directory.name) / "capture.ppm"

    def test_reads_binary_pixels_with_comments(self):
        pixels = bytes((255, 0, 0, 0, 160, 0))
        self.path.write_bytes(b"P6\n# fixture\n2 1\n255\n" + pixels)

        image = Ppm.read(self.path)

        self.assertEqual((image.width, image.height), (2, 1))
        self.assertEqual(image.pixel(0, 0), (255, 0, 0))
        self.assertEqual(image.pixel(1, 0), (0, 160, 0))

    def test_rejects_malformed_or_truncated_images(self):
        malformed = {
            "text format": b"P3\n1 1\n255\n0 0 0",
            "unsupported depth": b"P6\n1 1\n65535\n\0\0\0",
            "truncated pixels": b"P6\n2 1\n255\n\0\0\0",
            "unbounded dimensions": b"P6\n100000 100000\n255\n",
        }
        for label, contents in malformed.items():
            with self.subTest(label=label):
                self.path.write_bytes(contents)
                with self.assertRaises(FixtureError):
                    Ppm.read(self.path)


class ResultTest(unittest.TestCase):
    def test_replaces_result_atomically(self):
        with tempfile.TemporaryDirectory(prefix="thing-render-result-") as directory:
            path = Path(directory) / "result.json"
            atomic_json(path, {"ok": True, "case": "stencil"})
            self.assertEqual(json.loads(path.read_text())["case"], "stencil")
            self.assertEqual(list(path.parent.iterdir()), [path])

    def test_accepts_a_result_written_during_the_exit_check(self):
        with tempfile.TemporaryDirectory(prefix="thing-render-result-") as directory:
            path = Path(directory) / "result.json"

            class ExitedProcess:
                def poll(self):
                    atomic_json(path, {"ok": True})
                    return 0

            check_rendering.wait_for_result(
                ExitedProcess(), path, time.monotonic() + 1, "stencil"
            )
            self.assertTrue(path.exists())


class AssetTest(unittest.TestCase):
    def test_materializes_the_raster_fixture_in_the_disposable_copy(self):
        with tempfile.TemporaryDirectory(prefix="thing-render-assets-") as directory:
            with mock.patch.object(check_rendering, "ARTIFACTS", Path(directory)):
                run_directory = check_rendering.prepare_run_directory(
                    "image-fit", "opengl"
                )
            fixture = run_directory / "fixture"
            data = (fixture / "pattern.png").read_bytes()
            self.assertEqual(data[:8], b"\x89PNG\r\n\x1a\n")
            self.assertEqual(struct.unpack(">II", data[16:24]), (600, 100))
            self.assertFalse((fixture / "pattern.png.b64").exists())


class EnvironmentTest(unittest.TestCase):
    def test_accepts_only_nested_wayland_niri_and_dbus_endpoints(self):
        with tempfile.TemporaryDirectory(prefix="thing-render-sockets-") as directory:
            root = Path(directory)
            wayland_path = root / "wayland-nested"
            niri_path = root / "niri-nested.sock"
            with socket.socket(socket.AF_UNIX) as wayland:
                with socket.socket(socket.AF_UNIX) as niri:
                    wayland.bind(str(wayland_path))
                    niri.bind(str(niri_path))
                    values = {
                        "XDG_RUNTIME_DIR": directory,
                        "WAYLAND_DISPLAY": wayland_path.name,
                        "NIRI_SOCKET": str(niri_path),
                        "DBUS_SESSION_BUS_ADDRESS": "unix:path=/tmp/nested-bus",
                        "THING_RENDER_PARENT_WAYLAND": str(root / "wayland-parent"),
                        "THING_RENDER_PARENT_NIRI": str(root / "niri-parent.sock"),
                        "THING_RENDER_PARENT_BUS": "unix:path=/tmp/parent-bus",
                    }
                    with mock.patch.dict(os.environ, values, clear=True):
                        environment = check_rendering.private_environment()
                    self.assertEqual(environment["NIRI_SOCKET"], str(niri_path))

                    values["THING_RENDER_PARENT_NIRI"] = str(niri_path)
                    with mock.patch.dict(os.environ, values, clear=True):
                        with self.assertRaises(FixtureError):
                            check_rendering.private_environment()

                    values["THING_RENDER_PARENT_NIRI"] = ""
                    del values["THING_RENDER_PARENT_BUS"]
                    with mock.patch.dict(os.environ, values, clear=True):
                        with self.assertRaisesRegex(FixtureError, "parent endpoint markers"):
                            check_rendering.private_environment()


class ProcessCleanupTest(unittest.TestCase):
    def test_stops_the_owned_process_group_and_its_child(self):
        source = r'''
import signal
import subprocess
import sys
import time

child = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(60)"])

def stop(_number, _frame):
    if child.poll() is None:
        child.terminate()
    child.wait(timeout=2)
    raise SystemExit(0)

signal.signal(signal.SIGTERM, stop)
print(child.pid, flush=True)
time.sleep(60)
'''
        process = subprocess.Popen(
            [sys.executable, "-c", source],
            stdout=subprocess.PIPE,
            text=True,
            start_new_session=True,
        )
        try:
            child = int(process.stdout.readline())
            stop_process_group(process, timeout=1)
            self.assertIsNotNone(process.returncode)
            deadline = time.monotonic() + 2
            while Path(f"/proc/{child}").exists() and time.monotonic() < deadline:
                time.sleep(0.02)
            self.assertFalse(Path(f"/proc/{child}").exists())
        finally:
            stop_process_group(process, timeout=0.1)
            if process.stdout is not None:
                process.stdout.close()

    def test_defers_termination_until_owned_cleanup_finishes(self):
        source = r'''
import os
import signal
import check_rendering

for number in check_rendering.TERMINATION_SIGNALS:
    signal.signal(number, check_rendering.signal_handler)

try:
    with check_rendering.defer_termination_signals():
        os.kill(os.getpid(), signal.SIGTERM)
        print("group stopped", flush=True)
        print("logs closed", flush=True)
except check_rendering.SignalInterrupt as error:
    print(f"caught {error.number}", flush=True)
'''
        environment = os.environ.copy()
        tools = str(Path(__file__).resolve().parent)
        if environment.get("PYTHONPATH"):
            tools += os.pathsep + environment["PYTHONPATH"]
        environment["PYTHONPATH"] = tools
        result = subprocess.run(
            [sys.executable, "-c", source],
            capture_output=True,
            text=True,
            timeout=3,
            env=environment,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            result.stdout.splitlines(),
            ["group stopped", "logs closed", f"caught {signal.SIGTERM}"],
        )

    def test_escalates_when_a_process_ignores_termination(self):
        source = (
            "import signal, time; "
            "signal.signal(signal.SIGTERM, signal.SIG_IGN); "
            "print('ready', flush=True); time.sleep(60)"
        )
        process = subprocess.Popen(
            [sys.executable, "-c", source],
            stdout=subprocess.PIPE,
            text=True,
            start_new_session=True,
        )
        try:
            self.assertEqual(process.stdout.readline().strip(), "ready")
            stop_process_group(process, timeout=0.05)
            self.assertEqual(process.returncode, -signal.SIGKILL)
        finally:
            stop_process_group(process, timeout=0.1)
            if process.stdout is not None:
                process.stdout.close()


class CaptureTest(unittest.TestCase):
    def test_rejects_a_matching_frame_if_thing_exits_during_capture(self):
        class ExitingProcess:
            def __init__(self):
                self.polls = 0

            def poll(self):
                self.polls += 1
                return None if self.polls == 1 else 7

        process = ExitingProcess()
        with tempfile.TemporaryDirectory(prefix="thing-render-capture-") as directory:
            path = Path(directory) / "capture.ppm"

            def capture(arguments, **_kwargs):
                Path(arguments[-1]).write_bytes(b"P6\n1 1\n255\n\0\0\0")

            with mock.patch.object(check_rendering, "checked", side_effect=capture):
                with mock.patch.object(check_rendering, "verify_capture"):
                    with self.assertRaisesRegex(FixtureError, "thing exited"):
                        check_rendering.capture_until(
                            Path("grim"),
                            {},
                            process,
                            "output",
                            path,
                            "stencil",
                            "scale-1",
                            1.0,
                            time.monotonic() + 1,
                        )
        self.assertEqual(process.polls, 2)


if __name__ == "__main__":
    unittest.main()
