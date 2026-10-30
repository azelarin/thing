"""test the renderer harness without starting a compositor or graphics backend."""

import json
import math
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


BACKGROUND = check_rendering.SHADOW_BACKGROUND
WHITE = check_rendering.SHADOW_WHITE
GREEN = check_rendering.SHADOW_GREEN
BLACK = (0, 0, 0)
SHADOWS = frozenset("abcefghijk")


class Canvas:
    """a logical 640 by 480 frame painted with axis-aligned rectangles."""

    def __init__(self, scale):
        self.scale = scale
        self.width = check_rendering.physical(640, scale)
        self.height = check_rendering.physical(480, scale)
        self.pixels = bytearray(bytes(BACKGROUND) * (self.width * self.height))

    def rect(self, left, top, right, bottom, color):
        physical = check_rendering.physical
        x0, x1 = physical(left, self.scale), physical(right, self.scale)
        for y in range(physical(top, self.scale), physical(bottom, self.scale)):
            start = (y * self.width + x0) * 3
            self.pixels[start : start + (x1 - x0) * 3] = bytes(color) * (x1 - x0)
        return self

    def image(self):
        return Ppm(self.width, self.height, bytes(self.pixels))

    def ppm(self):
        return f"P6\n{self.width} {self.height}\n255\n".encode() + bytes(self.pixels)


def gallery(scale, *, reload=False, shadows=SHADOWS):
    """a plausible shadows frame. it approximates each case with rectangles
    placed to satisfy the probes; it does not model the renderer."""
    canvas = Canvas(scale)
    rect = canvas.rect
    if "a" in shadows:
        rect(60, 60, 120, 120, BLACK).rect(114, 114, 120, 120, BACKGROUND)
        rect(50, 50, 110, 110, BACKGROUND)
    rect(190, 50, 250, 110, WHITE)
    if "b" in shadows:
        for top, bottom, value in ((110, 120, 30), (120, 126, 110), (126, 134, 170), (134, 140, 210)):
            rect(190, top, 250, bottom, (value,) * 3)
    rect(330, 50, 390, 110, WHITE)
    if "c" in shadows:
        rect(330, 110, 390, 130, BLACK)
    rect(470, 50, 530, 110, WHITE)
    if "e" in shadows:
        rect(58, 198, 118, 218, BLACK).rect(58, 198, 78, 258, BLACK)
    rect(50, 190, 110, 210, GREEN).rect(50, 190, 70, 250, GREEN)
    if "f" in shadows:
        rect(184, 184, 256, 256, BLACK)
        for x, y in ((184, 184), (253, 184), (184, 253), (253, 253)):
            rect(x, y, x + 3, y + 3, BACKGROUND)
    rect(190, 190, 250, 250, GREEN)
    rect(205, 205, 235, 235, BLACK if "f" in shadows else BACKGROUND)
    rect(211, 211, 229, 229, BACKGROUND)
    if "g" in shadows:
        rect(340, 250, 380, 264, BLACK)
    rect(330, 190, 390, 250, GREEN)
    if "h" in shadows:
        rect(458, 190, 470, 250, BLACK)
    rect(470, 190, 530, 250, WHITE)
    rect(50, 330, 110, 390, (240, 240, 240))
    if "i" in shadows:
        rect(50, 390, 110, 410, (112, 112, 112))
    if "j" in shadows:
        rect(170, 330, 190, 390, BLACK)
    rect(190, 330, 250, 390, WHITE)
    rect(330, 330, 390, 390, WHITE)
    if "k" in shadows:
        rect(330, 310 if reload else 390, 390, 330 if reload else 410, BLACK)
    return canvas


class ShadowOracleTest(unittest.TestCase):
    def verify(self, canvas, stage="scale-1"):
        check_rendering.verify_capture("shadows", canvas.image(), stage, canvas.scale)

    def test_accepts_plausible_frames_at_both_scales(self):
        for scale in (1.0, 1.25):
            with self.subTest(scale=scale):
                for stage in ("scale-1", "scale-1.25", "rejected"):
                    self.verify(gallery(scale), stage)
                self.verify(gallery(scale, reload=True), "reload")

    def test_rejects_blank_and_unshadowed_frames(self):
        for canvas in (Canvas(1.0), gallery(1.0, shadows=frozenset())):
            with self.assertRaises(FixtureError):
                self.verify(canvas)

    def test_reports_the_probe_each_mistake_breaks(self):
        cases = {
            "f: hole center stays clear": gallery(1.25).rect(211, 211, 229, 229, BLACK),
            "f: grown corner stays round": gallery(1.0).rect(184, 184, 187, 187, BLACK),
            "d: scroll viewport clips the shadow": gallery(1.0).rect(470, 110, 530, 130, BLACK),
            "h: mirrored offset moves the shadow left": gallery(
                1.25, shadows=SHADOWS - {"h"}
            ).rect(530, 190, 542, 250, BLACK),
            "c: shadow overflows a tight container": gallery(1.0, shadows=SHADOWS - {"c"}),
            "a: knockout keeps the transparent center": gallery(1.0).rect(
                50, 50, 110, 110, BLACK
            ),
            # a bounding-box shadow fills the concavity of the path.
            "e: concavity stays clear": gallery(1.0)
            .rect(58, 198, 118, 258, BLACK)
            .rect(50, 190, 110, 210, GREEN)
            .rect(50, 190, 70, 250, GREEN),
            "b: blur falloff": gallery(1.0)
            .rect(190, 110, 250, 122, BLACK)
            .rect(190, 122, 250, 140, BACKGROUND),
            "g: shrinking narrows the shadow": gallery(1.25).rect(330, 250, 390, 274, BLACK),
            "i: shadow uses opacity once": gallery(1.0).rect(50, 390, 110, 410, (168,) * 3),
            "j: rotation turns the offset left": gallery(1.0, shadows=SHADOWS - {"j"}),
        }
        for label, canvas in cases.items():
            with self.subTest(label=label):
                with self.assertRaisesRegex(FixtureError, label):
                    self.verify(canvas)

    def test_stale_pixels_do_not_pass_the_valid_reload(self):
        with self.assertRaisesRegex(FixtureError, "k: reloaded shadow above"):
            self.verify(gallery(1.25), "reload")
        with self.assertRaisesRegex(FixtureError, "k: shadow below"):
            self.verify(gallery(1.25, reload=True), "rejected")


class StableCaptureTest(unittest.TestCase):
    def capture(self, frames, required, case="shadows", scale=1.0):
        calls = []
        sequence = iter(frames)

        def grab(arguments, **_kwargs):
            calls.append(arguments)
            Path(arguments[-1]).write_bytes(next(sequence).ppm())

        class RunningProcess:
            def poll(self):
                return None

        with tempfile.TemporaryDirectory(prefix="thing-render-stable-") as directory:
            with (
                mock.patch.object(check_rendering, "checked", side_effect=grab),
                mock.patch.object(check_rendering.time, "sleep"),
                mock.patch.object(
                    check_rendering.time, "monotonic", side_effect=range(len(frames) + 1)
                ),
            ):
                check_rendering.capture_until(
                    Path("grim"),
                    {},
                    RunningProcess(),
                    "output",
                    Path(directory) / "capture.ppm",
                    case,
                    "scale-1",
                    scale,
                    len(frames),
                    frames=required,
                )
        return len(calls)

    def test_waits_for_two_identical_passing_frames(self):
        first = gallery(1.0)
        # a passing frame that still differs outside every probe.
        second = gallery(1.0).rect(630, 470, 640, 480, BLACK)
        self.assertEqual(self.capture([first, second, second], 2), 3)
        self.assertEqual(self.capture([first], 1), 1)

    def test_a_failing_frame_restarts_the_count(self):
        good = gallery(1.0)
        broken = gallery(1.0).rect(211, 211, 229, 229, BLACK)
        with self.assertRaisesRegex(FixtureError, "waiting for 2 identical frames"):
            self.capture([good, broken, good], 2)
        self.assertEqual(self.capture([good, broken, good, good], 2), 4)


    def test_radial_gradients_need_two_identical_passing_frames(self):
        required = check_rendering.STABLE_FRAMES["radial-gradients"]
        self.assertEqual(required, 2)
        frame = RadialFrame(1.0)
        self.assertEqual(self.capture([frame, frame], required, "radial-gradients"), 2)
        with self.assertRaisesRegex(FixtureError, "waiting for 2 identical frames"):
            self.capture([frame, RadialFrame(1.0, filled_hole=True), frame], required, "radial-gradients")


RADIAL_BACKGROUND = check_rendering.RADIAL_BACKGROUND
RED = check_rendering.RADIAL_RED
BLUE = check_rendering.RADIAL_BLUE


class RadialFrame:
    """a plausible radial-gradients frame, evaluated from the gallery's
    geometry at each physical pixel center. `linear` replaces every radial
    cell with a horizontal linear gradient; `filled_hole` paints the path's
    hole; `blank` paints only the panel background."""

    def __init__(self, scale, *, reload=False, linear=False, filled_hole=False, blank=False):
        self.scale = scale
        width = check_rendering.physical(460, scale)
        height = check_rendering.physical(240, scale)
        pixels = bytearray()
        for row in range(height):
            y = (row + 0.5) / scale
            for column in range(width):
                x = (column + 0.5) / scale
                color = RADIAL_BACKGROUND if blank else self.pixel(x, y, reload, linear, filled_hole)
                pixels += bytes(color)
        self.ppm_image = Ppm(width, height, bytes(pixels))

    @staticmethod
    def pixel(x, y, reload, linear, filled_hole):
        def gray(distance):
            return (check_rendering.falloff_gray(distance),) * 3

        def falloff(center, radii, box):
            if linear:
                return gray((x - box[0]) / box[1])
            return gray(math.hypot((x - center[0]) / radii[0], (y - center[1]) / radii[1]))

        top = 10 <= y < 110
        bottom = 130 <= y < 230
        if top and 10 <= x < 210:
            if reload:
                return falloff((60, 60), (50, 50), (10, 200))
            return falloff((110, 60), (100, 50), (10, 200))
        if top and 230 <= x < 330:
            distance = (x - 230) / 100 if linear else math.hypot(x - 280, y - 60) / 50
            alpha = 1 - min(distance, 1.0)
            return (round(255 * alpha + RADIAL_BACKGROUND[0] * (1 - alpha)),) * 3
        if top and 350 <= x < 450:
            distance = (x - 350) / 100 if linear else math.hypot(x - 400, y - 60) / 50
            return RED if distance < 0.5 else BLUE
        if bottom and 10 <= x < 210:
            return falloff((160, 180), (50, 50), (10, 200))
        if bottom and 230 <= x < 330:
            if 260 <= x < 300 and 160 <= y < 200 and not filled_hole:
                return RADIAL_BACKGROUND
            return falloff((280, 180), (50, 50), (230, 100))
        return RADIAL_BACKGROUND

    def image(self):
        return self.ppm_image

    def ppm(self):
        image = self.ppm_image
        return f"P6\n{image.width} {image.height}\n255\n".encode() + image.pixels


class RadialOracleTest(unittest.TestCase):
    def verify(self, frame, stage="scale-1"):
        check_rendering.verify_capture("radial-gradients", frame.image(), stage, frame.scale)

    def test_accepts_plausible_frames_at_both_scales(self):
        for scale in (1.0, 1.25):
            with self.subTest(scale=scale):
                self.verify(RadialFrame(scale), "scale-1")
                self.verify(RadialFrame(scale, reload=True), "reload")

    def test_rejects_a_blank_frame(self):
        with self.assertRaises(FixtureError):
            self.verify(RadialFrame(1.0, blank=True))

    def test_rejects_a_linear_gradient_substitute(self):
        with self.assertRaisesRegex(FixtureError, "a: ellipse"):
            self.verify(RadialFrame(1.25, linear=True), "scale-1.25")

    def test_rejects_a_filled_path_hole(self):
        with self.assertRaisesRegex(FixtureError, "e: path hole stays clear"):
            self.verify(RadialFrame(1.25, filled_hole=True), "scale-1.25")

    def test_stale_pixels_do_not_pass_the_valid_reload(self):
        with self.assertRaisesRegex(FixtureError, "a: reloaded circle"):
            self.verify(RadialFrame(1.25), "reload")
        with self.assertRaisesRegex(FixtureError, "a: ellipse"):
            self.verify(RadialFrame(1.25, reload=True), "scale-1.25")


class RejectedReloadTest(unittest.TestCase):
    LINE = (
        "thing: reload failed; keeping the previous surfaces: "
        "runtime error: shadow blur must be between 0 and 32 logical pixels"
    )

    class RunningProcess:
        def __init__(self, status=None):
            self.status = status

        def poll(self):
            return self.status

    def wait(self, contents, offset, process=None, deadline=0.3):
        with tempfile.TemporaryDirectory(prefix="thing-render-log-") as directory:
            log = Path(directory) / "thing.log"
            log.write_text(contents, encoding="utf-8")
            return check_rendering.wait_for_rejection(
                log,
                offset,
                check_rendering.INVALID_RELOADS["shadows"],
                process or self.RunningProcess(),
                time.monotonic() + deadline,
            )

    def test_accepts_only_a_rejection_logged_after_the_replacement(self):
        earlier = "thing: started\n"
        self.assertEqual(self.wait(earlier + self.LINE + "\n", len(earlier)), self.LINE)
        stale = self.LINE + "\n"
        with self.assertRaisesRegex(FixtureError, "did not report the rejected reload"):
            self.wait(stale + "thing: started\n", len(stale))

    def test_ignores_a_rejection_for_another_reason(self):
        other = "thing: reload failed; keeping the previous surfaces: syntax error\n"
        with self.assertRaisesRegex(FixtureError, "did not report"):
            self.wait(other, 0)

    def test_stops_when_thing_exits(self):
        with self.assertRaisesRegex(FixtureError, "thing exited"):
            self.wait("", 0, self.RunningProcess(status=1))

    def test_requires_the_invalid_fixture_to_fail_for_its_stated_reason(self):
        path = Path("panel-invalid.luau")
        detail = check_rendering.INVALID_RELOADS["shadows"]
        failure = FixtureError(f"command failed: thing check: thing: runtime error: {detail} logical pixels")
        with mock.patch.object(check_rendering, "checked", side_effect=failure):
            check_rendering.expect_rejected_config(Path("thing"), path, detail, 0)
        with mock.patch.object(
            check_rendering, "checked", side_effect=FixtureError("command failed: syntax error")
        ):
            with self.assertRaisesRegex(FixtureError, "another reason"):
                check_rendering.expect_rejected_config(Path("thing"), path, detail, 0)
        with mock.patch.object(check_rendering, "checked"):
            with self.assertRaisesRegex(FixtureError, "unexpectedly passed"):
                check_rendering.expect_rejected_config(Path("thing"), path, detail, 0)


class StageTest(unittest.TestCase):
    def run_stages(self, case):
        events = []

        class Process:
            def __init__(self, *_arguments, **_kwargs):
                self.stopped = False

            def poll(self):
                return 0 if self.stopped else None

            def terminate(self):
                self.stopped = True

            def wait(self, timeout=None):
                return 0

        with tempfile.TemporaryDirectory(prefix="thing-render-stages-") as directory:
            with mock.patch.object(check_rendering, "ARTIFACTS", Path(directory)):
                run_directory = check_rendering.prepare_run_directory(case, "opengl")
            panel = run_directory / "fixture" / "panel.luau"

            def capture(*arguments, frames=1):
                events.append((arguments[6], panel.read_text(), frames))

            def rejection(log_path, offset, detail, _process, _deadline):
                events.append(("witness", panel.read_text(), offset <= log_path.stat().st_size))
                return "logged rejection"

            patches = (
                mock.patch.object(check_rendering, "private_environment", return_value={}),
                mock.patch.object(check_rendering, "wait_for_output", return_value=("out", 1.0)),
                mock.patch.object(check_rendering, "checked"),
                mock.patch.object(check_rendering.subprocess, "Popen", Process),
                mock.patch.object(check_rendering, "capture_until", side_effect=capture),
                mock.patch.object(check_rendering, "wait_for_rejection", side_effect=rejection),
            )
            for patch in patches:
                patch.start()
                self.addCleanup(patch.stop)
            result = check_rendering.run_inner(
                run_directory, Path("thing"), "opengl", case, Path("niri"), Path("grim")
            )
            fixture = check_rendering.FIXTURES / case
            texts = {
                name: (fixture / f"{name}.luau").read_text()
                for name in ("panel", "panel-invalid", "panel-reload")
                if (fixture / f"{name}.luau").exists()
            }
        return events, result, texts

    def test_shadows_reject_a_reload_before_recovering(self):
        events, result, texts = self.run_stages("shadows")
        self.assertEqual(
            events,
            [
                ("scale-1", texts["panel"], 2),
                ("scale-1.25", texts["panel"], 2),
                ("witness", texts["panel-invalid"], True),
                ("rejected", texts["panel-invalid"], 2),
                ("reload", texts["panel-reload"], 2),
            ],
        )
        self.assertEqual(result["rejected_reload"], "logged rejection")

    def test_other_cases_keep_their_single_frame_stages(self):
        events, result, texts = self.run_stages("gradients")
        self.assertEqual(
            events,
            [
                ("scale-1", texts["panel"], 1),
                ("scale-1.25", texts["panel"], 1),
                ("reload", texts["panel-reload"], 1),
            ],
        )
        self.assertNotIn("rejected_reload", result)


class SharedMotionProbes(unittest.TestCase):
    def frame(self, scale, *, reload=False, marker_offset=0, moving=False):
        canvas = Canvas(scale)
        canvas.rect(0, 0, 640, 260, (13, 17, 23))
        fill = (121, 151, 255) if reload else (40, 179, 161)
        targets = (180, 140, 20) if reload else (120, 80, 20)
        for top, target in zip((25, 95, 165), targets):
            canvas.rect(160, top, 160 + target, top + 40, fill)
            left = 160 + target + 8 + marker_offset
            color = (232, 75, 181) if moving else (71, 209, 108)
            canvas.rect(left, top, left + 8, top + 40, color)
        return canvas.image()

    def test_settled_samples_pass_at_both_scales_and_after_reload(self):
        for scale in (1.0, 1.25):
            for reload in (False, True):
                with self.subTest(scale=scale, reload=reload):
                    check_rendering.verify_capture(
                        "shared-motion", self.frame(scale, reload=reload),
                        "reload" if reload else "scale-1", scale
                    )

    def test_a_marker_from_another_sample_is_rejected(self):
        with self.assertRaises(check_rendering.FixtureError):
            check_rendering.verify_capture(
                "shared-motion", self.frame(1, marker_offset=8), "scale-1", 1
            )

    def test_a_nonzero_velocity_is_not_a_settled_frame(self):
        with self.assertRaises(check_rendering.FixtureError):
            check_rendering.verify_capture(
                "shared-motion", self.frame(1, moving=True), "scale-1", 1
            )


if __name__ == "__main__":
    unittest.main()
