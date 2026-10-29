#!/usr/bin/env python3
"""run opt-in renderer regressions inside a nested niri compositor."""

import argparse
import base64
from contextlib import contextmanager
from dataclasses import dataclass
import fcntl
import json
import math
import os
from pathlib import Path
import shutil
import signal
import stat
import subprocess
import sys
import tempfile
import time


ROOT = Path(__file__).resolve().parent.parent
FIXTURES = ROOT / "tests/rendering"
ARTIFACTS = ROOT / "target/render-tests"
CASES = ("image-fit", "stencil", "gradients", "shadows")
# shadows accept a state only after the same passing frame is captured twice.
STABLE_FRAMES = {"shadows": 2}
RELOAD_REJECTED = "reload failed; keeping the previous surfaces"
# the runtime error each panel-invalid.luau must produce, in thing check and in
# the log of the running host.
INVALID_RELOADS = {"shadows": "shadow blur must be between 0 and 32"}
MAX_LOG_BYTES = 1024 * 1024
COMMAND_TIMEOUT = 8.0
INNER_TIMEOUT = 35.0
OUTER_TIMEOUT = 45.0
STOP_TIMEOUT = 3.0
MAX_PPM_BYTES = 64 * 1024 * 1024
PPM_WHITESPACE = b" \t\r\n\v\f"
TERMINATION_SIGNALS = (signal.SIGINT, signal.SIGTERM, signal.SIGHUP)


class FixtureError(RuntimeError):
    pass


class SignalInterrupt(BaseException):
    def __init__(self, number):
        super().__init__(number)
        self.number = number


@dataclass(frozen=True)
class Ppm:
    width: int
    height: int
    pixels: bytes

    @classmethod
    def read(cls, path):
        size = path.stat().st_size
        if size > MAX_PPM_BYTES + 4096:
            raise FixtureError(f"screenshot exceeds {MAX_PPM_BYTES // (1024 * 1024)} MiB")
        data = path.read_bytes()
        index = 0
        tokens = []
        for _ in range(4):
            token, index = ppm_token(data, index)
            tokens.append(token)
        if tokens[0] != b"P6":
            raise FixtureError("screenshot is not an 8-bit binary PPM")
        try:
            width, height, maximum = (int(token) for token in tokens[1:])
        except ValueError as error:
            raise FixtureError("screenshot has an invalid PPM header") from error
        if width <= 0 or height <= 0 or maximum != 255:
            raise FixtureError("screenshot has unsupported PPM dimensions or depth")
        expected = width * height * 3
        if expected > MAX_PPM_BYTES:
            raise FixtureError(f"screenshot exceeds {MAX_PPM_BYTES // (1024 * 1024)} MiB")
        if index >= len(data) or data[index] not in PPM_WHITESPACE:
            raise FixtureError("screenshot has an incomplete PPM header")
        if data[index : index + 2] == b"\r\n":
            index += 2
        else:
            index += 1
        pixels = data[index:]
        if len(pixels) != expected:
            raise FixtureError(
                f"screenshot contains {len(pixels)} pixel bytes; expected {expected}"
            )
        return cls(width, height, pixels)

    def pixel(self, x, y):
        if not 0 <= x < self.width or not 0 <= y < self.height:
            raise FixtureError(
                f"sample ({x}, {y}) lies outside {self.width}x{self.height} screenshot"
            )
        offset = (y * self.width + x) * 3
        return tuple(self.pixels[offset : offset + 3])


def ppm_token(data, index):
    while True:
        while index < len(data) and data[index] in PPM_WHITESPACE:
            index += 1
        if index >= len(data):
            raise FixtureError("screenshot has an incomplete PPM header")
        if data[index] != ord("#"):
            break
        newline = data.find(b"\n", index + 1)
        if newline < 0:
            raise FixtureError("screenshot has an unterminated PPM comment")
        index = newline + 1
    start = index
    while index < len(data) and data[index] not in PPM_WHITESPACE:
        index += 1
    if start == index or index - start > 32:
        raise FixtureError("screenshot has an invalid PPM header")
    return data[start:index], index


def physical(value, scale):
    return int(value * scale + 0.5)


def expect_color(image, point, scale, expected, label, tolerance=10):
    location = (physical(point[0], scale), physical(point[1], scale))
    actual = image.pixel(*location)
    if max(abs(channel - target) for channel, target in zip(actual, expected)) > tolerance:
        raise FixtureError(
            f"{label} at logical {point} was {actual}; expected {expected}"
        )


def verify_image_fit(image, stage, scale):
    dark = (32, 32, 32)
    red = (227, 75, 75)
    green = (81, 184, 112)
    blue = (74, 135, 215)
    for row, (top, middle, bottom) in enumerate(((20, 60, 100), (130, 170, 210))):
        prefix = ("svg", "png")[row]
        if stage == "reload":
            for y in (top, middle, bottom):
                expect_color(image, (88, y), scale, green, f"{prefix} reloaded cover")
        else:
            expect_color(image, (88, top), scale, dark, f"{prefix} contain margin")
            expect_color(image, (25, middle), scale, red, f"{prefix} contain red")
            expect_color(image, (88, middle), scale, green, f"{prefix} contain green")
            expect_color(image, (151, middle), scale, blue, f"{prefix} contain blue")
            expect_color(image, (88, bottom), scale, dark, f"{prefix} contain margin")
        for y in (top, middle, bottom):
            for x in (190, 254, 318):
                expect_color(image, (x, y), scale, green, f"{prefix} cover")
            expect_color(image, (357, y), scale, red, f"{prefix} fill red")
            expect_color(image, (420, y), scale, green, f"{prefix} fill green")
            expect_color(image, (483, y), scale, blue, f"{prefix} fill blue")


def verify_stencil(image, _stage, scale):
    green = (0, 160, 0)
    white = (255, 255, 255)
    for index, left in enumerate((10, 146, 282)):
        expect_color(image, (left + 30, 40), scale, green, f"stencil case {index + 1} L top")
        expect_color(image, (left + 30, 110), scale, green, f"stencil case {index + 1} L stem")
        expect_color(
            image,
            (left + 55, 60),
            scale,
            white,
            f"stencil case {index + 1} cleared upper concavity",
        )
        expect_color(
            image,
            (left + 55, 90),
            scale,
            white,
            f"stencil case {index + 1} cleared lower concavity",
        )


def verify_gradients(image, stage, scale):
    dark = (32, 32, 32)
    expect_color(image, (10, 10), scale, dark, "gradient rounded corner")
    for x in (40, 240, 440):
        progress = (x - 10) / 460
        if stage == "reload":
            progress = 1 - progress
        gray = round(255 * (12.92 * progress if progress <= 0.0031308 else 1.055 * progress ** (1 / 2.4) - 0.055))
        expect_color(image, (x, 35), scale, (gray, gray, gray), "linear-light gradient")
    expect_color(image, (120, 95), scale, (255, 0, 0), "far-endpoint hard stop left")
    expect_color(image, (360, 95), scale, (0, 0, 255), "far-endpoint hard stop right")
    expect_color(image, (240, 145), scale, dark, "path contain margin")
    expect_color(image, (15, 170), scale, dark, "path padding")
    for y in (153, 185):
        alpha = 1 - (y - 140) / 60
        expected = (round(255 * alpha + 32 * (1 - alpha)), round(32 * (1 - alpha)), round(32 * (1 - alpha)))
        expect_color(image, (240, y), scale, expected, "path premultiplied fade")
    expect_color(image, (30, 245), scale, (250, 250, 250), "mirrored gradient start")
    expect_color(image, (450, 245), scale, (59, 59, 59), "mirrored gradient end")


SHADOW_BACKGROUND = (224, 224, 224)
SHADOW_WHITE = (255, 255, 255)
SHADOW_GREEN = (0, 160, 0)


def sample(image, point, scale):
    """average the 3x3 physical pixels around a logical point."""
    x, y = physical(point[0], scale), physical(point[1], scale)
    pixels = [image.pixel(x + dx, y + dy) for dy in (-1, 0, 1) for dx in (-1, 0, 1)]
    return tuple(round(sum(pixel[channel] for pixel in pixels) / 9) for channel in range(3))


def brightness(color):
    return sum(color) / 3


def verify_shadows(image, stage, scale):
    def color(label, point, expected, tolerance=14):
        actual = sample(image, point, scale)
        if max(abs(channel - target) for channel, target in zip(actual, expected)) > tolerance:
            raise FixtureError(f"{label} at logical {point} was {actual}; expected {expected}")
        return actual

    def clear(label, point):
        return color(label, point, SHADOW_BACKGROUND)

    def dark(label, point, limit=70):
        actual = sample(image, point, scale)
        if brightness(actual) > limit:
            raise FixtureError(f"{label} at logical {point} was {actual}; expected a dark shadow")
        return actual

    color("panel background", (580, 430), SHADOW_BACKGROUND)
    # a: transparent rounded box with a hard shadow offset by (10, 10).
    clear("a: knockout keeps the transparent center", (80, 80))
    dark("a: hard shadow inside its edge", (112, 62))
    clear("a: hard shadow has no halo", (112, 58))
    dark("a: shadow near its rounded corner", (112, 112))
    clear("a: shadow corner stays rounded", (118, 118))
    # b: blur 16 below a white box. compare interior samples, not a curve.
    color("b: blurred box", (220, 80), SHADOW_WHITE)
    near = brightness(sample(image, (220, 114), scale))
    beyond = brightness(sample(image, (220, 128), scale))
    if not (60 < beyond < SHADOW_BACKGROUND[0] - 25 and near + 40 < beyond):
        raise FixtureError(
            f"b: blur falloff was {near:.0f} near the box and {beyond:.0f} beyond the "
            "hard edge; expected a darker shadow fading past that edge"
        )
    clear("b: blur fades out", (220, 150))
    # c and d: the same shadow escapes a tight column but not a scroll viewport.
    color("c: box in a tight container", (360, 80), SHADOW_WHITE)
    dark("c: shadow overflows a tight container", (360, 120))
    color("d: box in a scroll viewport", (500, 80), SHADOW_WHITE)
    clear("d: scroll viewport clips the shadow", (500, 120))
    # e: concave path offset by (8, 8) follows its silhouette.
    color("e: concave path fill", (60, 230), SHADOW_GREEN)
    dark("e: shadow under the top arm", (95, 214))
    dark("e: shadow beside the top arm", (114, 208))
    dark("e: shadow beside the stem", (74, 240))
    clear("e: concavity stays clear", (95, 235))
    clear("e: no shadow on the far side", (45, 220))
    # f: even-odd ring with spread 6.
    color("f: ring fill", (195, 220), SHADOW_GREEN)
    dark("f: spread grows into the hole", (208, 220))
    clear("f: hole center stays clear", (220, 220))
    dark("f: spread grows outside", (187, 220))
    clear("f: grown corner stays round", (184, 184))
    # g: spread -10 with a downward offset of 24.
    color("g: square fill", (360, 240), SHADOW_GREEN)
    dark("g: shrunk shadow below", (360, 257))
    clear("g: shrinking narrows the shadow", (334, 257))
    clear("g: shrinking shortens the shadow", (360, 268))
    # h: scale_x = -1 mirrors a local offset of (12, 0).
    color("h: mirrored box", (500, 220), SHADOW_WHITE)
    dark("h: mirrored offset moves the shadow left", (464, 220))
    clear("h: no shadow on the unmirrored side", (536, 220))
    # i: a half-opaque parent dims the box and its shadow once.
    color("i: half-opaque box", (80, 360), (240, 240, 240))
    color("i: shadow uses opacity once", (80, 400), (112, 112, 112), tolerance=16)
    # j: rotation 90 turns a downward offset to the left.
    color("j: rotated box", (220, 360), SHADOW_WHITE)
    dark("j: rotation turns the offset left", (180, 360))
    clear("j: no shadow below the rotated box", (220, 400))
    # k: the reload moves this shadow from below the box to above it.
    color("k: reload target box", (360, 360), SHADOW_WHITE)
    if stage == "reload":
        dark("k: reloaded shadow above", (360, 320))
        clear("k: reload removed the shadow below", (360, 400))
    else:
        dark("k: shadow below", (360, 400))
        clear("k: no shadow above", (360, 320))


def verify_capture(case, image, stage, scale):
    if case == "image-fit":
        verify_image_fit(image, stage, scale)
    elif case == "stencil":
        verify_stencil(image, stage, scale)
    elif case == "gradients":
        verify_gradients(image, stage, scale)
    elif case == "shadows":
        verify_shadows(image, stage, scale)
    else:
        raise FixtureError(f"unknown renderer fixture: {case}")


def command_text(arguments):
    return " ".join(
        Path(argument).name if index == 0 else str(argument)
        for index, argument in enumerate(arguments)
    )


def checked(arguments, *, env=None, deadline=None):
    timeout = COMMAND_TIMEOUT
    if deadline is not None:
        timeout = min(timeout, max(0.1, deadline - time.monotonic()))
    try:
        return subprocess.run(
            [str(argument) for argument in arguments],
            check=True,
            timeout=timeout,
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            env=env,
        )
    except subprocess.TimeoutExpired as error:
        raise FixtureError(f"command timed out: {command_text(arguments)}") from error
    except subprocess.CalledProcessError as error:
        detail = (error.stderr or error.stdout or "").strip()
        suffix = f": {detail}" if detail else ""
        raise FixtureError(f"command failed: {command_text(arguments)}{suffix}") from error


def atomic_json(path, value):
    temporary = path.with_name(f".{path.name}.{os.getpid()}.tmp")
    temporary.write_text(
        json.dumps(value, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    os.replace(temporary, path)


def atomic_copy(source, destination):
    temporary = destination.with_name(f".{destination.name}.{os.getpid()}.tmp")
    shutil.copyfile(source, temporary)
    os.replace(temporary, destination)


def socket_path(value, runtime):
    path = Path(value)
    if not path.is_absolute():
        path = Path(runtime) / path
    return path.resolve()


def require_socket(value, runtime, label):
    if not value:
        raise FixtureError(f"{label} is required")
    path = socket_path(value, runtime)
    try:
        mode = path.stat().st_mode
    except OSError as error:
        raise FixtureError(f"{label} does not name a live socket: {path}") from error
    if not stat.S_ISSOCK(mode):
        raise FixtureError(f"{label} is not a socket: {path}")
    return path


def private_environment():
    environment = os.environ.copy()
    markers = (
        "THING_RENDER_PARENT_WAYLAND",
        "THING_RENDER_PARENT_NIRI",
        "THING_RENDER_PARENT_BUS",
    )
    if any(marker not in environment for marker in markers):
        raise FixtureError("nested fixture is missing its parent endpoint markers")
    runtime = environment.get("XDG_RUNTIME_DIR")
    if not runtime:
        raise FixtureError("XDG_RUNTIME_DIR is required")
    wayland = require_socket(environment.get("WAYLAND_DISPLAY"), runtime, "WAYLAND_DISPLAY")
    niri = require_socket(environment.get("NIRI_SOCKET"), runtime, "NIRI_SOCKET")
    parent_wayland = environment.get("THING_RENDER_PARENT_WAYLAND")
    parent_niri = environment.get("THING_RENDER_PARENT_NIRI")
    if parent_wayland and wayland == Path(parent_wayland):
        raise FixtureError("nested Wayland socket matches the desktop session")
    if parent_niri and niri == Path(parent_niri):
        raise FixtureError("nested niri IPC socket matches the desktop session")
    bus = environment.get("DBUS_SESSION_BUS_ADDRESS")
    if not bus:
        raise FixtureError("dbus-run-session did not provide a private bus")
    if bus == environment.get("THING_RENDER_PARENT_BUS"):
        raise FixtureError("nested D-Bus address matches the desktop session")
    environment["WAYLAND_DISPLAY"] = os.environ["WAYLAND_DISPLAY"]
    environment["NIRI_SOCKET"] = os.environ["NIRI_SOCKET"]
    environment["DBUS_SESSION_BUS_ADDRESS"] = bus
    return environment


def output_snapshot(niri, environment, deadline):
    result = checked([niri, "msg", "--json", "outputs"], env=environment, deadline=deadline)
    try:
        outputs = json.loads(result.stdout)
    except json.JSONDecodeError as error:
        raise FixtureError("niri returned invalid output JSON") from error
    if not isinstance(outputs, dict) or len(outputs) != 1:
        count = len(outputs) if isinstance(outputs, dict) else "invalid"
        raise FixtureError(f"nested compositor exposed {count} outputs")
    name, output = next(iter(outputs.items()))
    try:
        scale = float(output["logical"]["scale"])
    except (KeyError, TypeError, ValueError) as error:
        raise FixtureError("niri output is missing its logical scale") from error
    if not isinstance(name, str) or not name or not math.isfinite(scale) or scale <= 0:
        raise FixtureError("niri returned an invalid output name or scale")
    return name, scale


def wait_for_output(niri, environment, deadline, *, name=None, scale=None):
    last_error = None
    while time.monotonic() < deadline:
        try:
            current_name, current_scale = output_snapshot(niri, environment, deadline)
            if name is not None and current_name != name:
                raise FixtureError(f"nested output changed from {name} to {current_name}")
            if scale is None or abs(current_scale - scale) < 0.001:
                return current_name, current_scale
            last_error = FixtureError(
                f"nested output scale is {current_scale}; expected {scale}"
            )
        except FixtureError as error:
            last_error = error
        time.sleep(0.1)
    raise FixtureError(f"nested output did not become ready: {last_error}")


def ensure_running(process):
    status = process.poll()
    if status is not None:
        raise FixtureError(f"thing exited before the fixture completed (status {status})")


def capture_until(
    grim, environment, process, output, path, case, stage, scale, deadline, *, frames=1
):
    """capture until `frames` consecutive identical frames pass the probes."""
    last_error = None
    temporary = path.with_name(f".{path.name}.tmp")
    accepted = None
    count = 0
    while time.monotonic() < deadline:
        ensure_running(process)
        try:
            temporary.unlink(missing_ok=True)
            checked(
                [grim, "-t", "ppm", "-o", output, temporary],
                env=environment,
                deadline=deadline,
            )
            os.replace(temporary, path)
            image = Ppm.read(path)
            verify_capture(case, image, stage, scale)
        except FixtureError as error:
            last_error = error
            accepted = None
            count = 0
        else:
            if accepted is not None and image == accepted:
                count += 1
            else:
                accepted = image
                count = 1
            if count >= frames:
                ensure_running(process)
                return
            last_error = FixtureError(f"waiting for {frames} identical frames")
        time.sleep(0.15)
    raise FixtureError(f"{stage} frame did not match the fixture: {last_error}")


def wait_for_rejection(log_path, offset, detail, process, deadline):
    """wait for the host to log a rejected reload after `offset` and return it."""
    while time.monotonic() < deadline:
        ensure_running(process)
        with log_path.open("rb") as log:
            log.seek(offset)
            text = log.read(MAX_LOG_BYTES).decode("utf-8", errors="replace")
        for line in text.splitlines():
            if RELOAD_REJECTED in line and detail in line:
                return line.strip()
        time.sleep(0.1)
    raise FixtureError(f"thing did not report the rejected reload: {detail}")


def stop_child(process, timeout=STOP_TIMEOUT):
    if process is None or process.poll() is not None:
        return
    try:
        process.terminate()
    except ProcessLookupError:
        return
    try:
        process.wait(timeout=timeout)
    except subprocess.TimeoutExpired:
        try:
            process.kill()
        except ProcessLookupError:
            pass
        process.wait(timeout=timeout)


def stop_process_group(process, timeout=STOP_TIMEOUT):
    if process is None:
        return
    group = process.pid
    try:
        os.killpg(group, signal.SIGTERM)
    except ProcessLookupError:
        pass
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        process.poll()
        try:
            os.killpg(group, 0)
        except ProcessLookupError:
            break
        time.sleep(0.02)
    try:
        os.killpg(group, 0)
    except ProcessLookupError:
        pass
    else:
        try:
            os.killpg(group, signal.SIGKILL)
        except ProcessLookupError:
            pass
    if process.poll() is None:
        try:
            process.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=timeout)


def run_inner(run_directory, binary, renderer, case, niri, grim):
    environment = private_environment()
    deadline = time.monotonic() + INNER_TIMEOUT
    output, _ = wait_for_output(niri, environment, min(deadline, time.monotonic() + 8))
    checked(
        [niri, "msg", "output", output, "scale", "1"],
        env=environment,
        deadline=deadline,
    )
    wait_for_output(
        niri,
        environment,
        min(deadline, time.monotonic() + 5),
        name=output,
        scale=1.0,
    )

    fixture = run_directory / "fixture"
    panel = fixture / "panel.luau"
    log_path = run_directory / "thing.log"
    log = log_path.open("w", encoding="utf-8")
    frames = STABLE_FRAMES.get(case, 1)
    rejection = None
    process = None
    try:
        process = subprocess.Popen(
            [str(binary), "run", "--renderer", renderer, str(panel)],
            stdout=log,
            stderr=subprocess.STDOUT,
            env=environment,
        )
        capture_until(
            grim,
            environment,
            process,
            output,
            run_directory / "scale-1.ppm",
            case,
            "scale-1",
            1.0,
            min(deadline, time.monotonic() + 10),
            frames=frames,
        )

        checked(
            [niri, "msg", "output", output, "scale", "1.25"],
            env=environment,
            deadline=deadline,
        )
        wait_for_output(
            niri,
            environment,
            min(deadline, time.monotonic() + 5),
            name=output,
            scale=1.25,
        )
        capture_until(
            grim,
            environment,
            process,
            output,
            run_directory / "scale-1.25.ppm",
            case,
            "scale-1.25",
            1.25,
            min(deadline, time.monotonic() + 10),
            frames=frames,
        )

        invalid = fixture / "panel-invalid.luau"
        if invalid.exists():
            # only log written after the replacement counts as evidence.
            offset = log_path.stat().st_size
            atomic_copy(invalid, panel)
            rejection = wait_for_rejection(
                log_path,
                offset,
                INVALID_RELOADS[case],
                process,
                min(deadline, time.monotonic() + 10),
            )
            capture_until(
                grim,
                environment,
                process,
                output,
                run_directory / "rejected.ppm",
                case,
                "rejected",
                1.25,
                min(deadline, time.monotonic() + 10),
                frames=frames,
            )

        reload = fixture / "panel-reload.luau"
        if reload.exists():
            atomic_copy(reload, panel)
            capture_until(
                grim,
                environment,
                process,
                output,
                run_directory / "reload.ppm",
                case,
                "reload",
                1.25,
                min(deadline, time.monotonic() + 10),
                frames=frames,
            )
        result = {
            "ok": True,
            "case": case,
            "output": output,
            "renderer": renderer,
        }
        if rejection is not None:
            result["rejected_reload"] = rejection
        return result
    finally:
        stop_child(process)
        log.close()


def prepare_run_directory(case, renderer):
    ARTIFACTS.mkdir(parents=True, exist_ok=True)
    run_directory = Path(
        tempfile.mkdtemp(prefix=f"{renderer}-{case}-", dir=ARTIFACTS)
    )
    try:
        shutil.copytree(FIXTURES / case, run_directory / "fixture")
        shutil.copy2(FIXTURES / "niri.kdl", run_directory / "niri.kdl")
        for encoded in (run_directory / "fixture").glob("*.b64"):
            payload = "".join(encoded.read_text(encoding="ascii").split())
            try:
                data = base64.b64decode(payload, validate=True)
            except ValueError as error:
                raise FixtureError(f"invalid base64 fixture: {encoded.name}") from error
            destination = encoded.with_suffix("")
            destination.write_bytes(data)
            encoded.unlink()
        return run_directory
    except BaseException:
        shutil.rmtree(run_directory)
        raise


def resolve_program(name):
    path = shutil.which(name)
    if path is None:
        raise FixtureError(f"required program is not installed: {name}")
    return Path(path).resolve()


def validate_binary(path):
    try:
        binary = path.expanduser().resolve(strict=True)
    except OSError as error:
        raise FixtureError(f"thing binary does not exist: {path}") from error
    if not binary.is_file() or not os.access(binary, os.X_OK):
        raise FixtureError(f"thing binary is not executable: {binary}")
    return binary


def parent_environment():
    environment = os.environ.copy()
    runtime = environment.get("XDG_RUNTIME_DIR")
    if not runtime:
        raise FixtureError("XDG_RUNTIME_DIR is required")
    wayland = require_socket(environment.get("WAYLAND_DISPLAY"), runtime, "WAYLAND_DISPLAY")
    parent_niri = environment.pop("NIRI_SOCKET", "")
    if parent_niri:
        parent_niri = str(require_socket(parent_niri, runtime, "NIRI_SOCKET"))
    environment["THING_RENDER_PARENT_WAYLAND"] = str(wayland)
    environment["THING_RENDER_PARENT_NIRI"] = parent_niri
    environment["THING_RENDER_PARENT_BUS"] = environment.pop(
        "DBUS_SESSION_BUS_ADDRESS", ""
    )
    return environment


def validate_configs(binary, fixture, deadline, case):
    for name in ("panel.luau", "panel-reload.luau"):
        path = fixture / name
        if path.exists():
            checked([binary, "check", path], deadline=deadline)
    invalid = fixture / "panel-invalid.luau"
    if invalid.exists():
        expect_rejected_config(binary, invalid, INVALID_RELOADS[case], deadline)


def expect_rejected_config(binary, path, detail, deadline):
    try:
        checked([binary, "check", path], deadline=deadline)
    except FixtureError as error:
        if detail in str(error):
            return
        raise FixtureError(f"{path.name} failed for another reason: {error}") from error
    raise FixtureError(f"{path.name} unexpectedly passed thing check")


def wait_for_result(process, result_path, deadline, case):
    while not result_path.exists():
        status = process.poll()
        if status is not None:
            if result_path.exists():
                return
            raise FixtureError(
                f"nested compositor exited before producing a result (status {status})"
            )
        if time.monotonic() >= deadline:
            raise FixtureError(f"{case} fixture exceeded {OUTER_TIMEOUT:g} seconds")
        time.sleep(0.1)


def run_case(binary, renderer, case, programs, keep_artifacts):
    run_directory = prepare_run_directory(case, renderer)
    process = None
    log = None
    succeeded = False
    try:
        deadline = time.monotonic() + OUTER_TIMEOUT
        validate_configs(binary, run_directory / "fixture", deadline, case)
        checked(
            [programs["niri"], "validate", "-c", run_directory / "niri.kdl"],
            deadline=deadline,
        )
        result_path = run_directory / "result.json"
        environment = parent_environment()
        command = [
            programs["dbus-run-session"],
            "--",
            programs["niri"],
            "-c",
            run_directory / "niri.kdl",
            "--",
            Path(sys.executable).resolve(),
            Path(__file__).resolve(),
            "--inside",
            "--run-directory",
            run_directory,
            "--binary",
            binary,
            "--renderer",
            renderer,
            "--case",
            case,
            "--niri",
            programs["niri"],
            "--grim",
            programs["grim"],
        ]
        log = (run_directory / "niri.log").open("w", encoding="utf-8")
        process = subprocess.Popen(
            [str(argument) for argument in command],
            stdout=log,
            stderr=subprocess.STDOUT,
            env=environment,
            start_new_session=True,
        )
        wait_for_result(process, result_path, deadline, case)
        try:
            result = json.loads(result_path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError) as error:
            raise FixtureError("nested fixture produced an invalid result") from error
        if result.get("ok") is not True:
            raise FixtureError(str(result.get("error", "nested fixture failed")))
        succeeded = True
        print(f"{case} ({renderer}): ok")
    except BaseException:
        print(f"renderer fixture artifacts: {run_directory}", file=sys.stderr)
        raise
    finally:
        with defer_termination_signals():
            stop_process_group(process)
            if log is not None:
                log.close()
            if succeeded and not keep_artifacts:
                shutil.rmtree(run_directory)


@contextmanager
def defer_termination_signals():
    previous = signal.pthread_sigmask(signal.SIG_BLOCK, TERMINATION_SIGNALS)
    try:
        yield
    finally:
        signal.pthread_sigmask(signal.SIG_SETMASK, previous)


@contextmanager
def renderer_lock():
    runtime = os.environ.get("XDG_RUNTIME_DIR")
    if not runtime:
        raise FixtureError("XDG_RUNTIME_DIR is required")
    path = Path(runtime) / "thing-render-tests.lock"
    flags = os.O_CREAT | os.O_RDWR | os.O_CLOEXEC
    if hasattr(os, "O_NOFOLLOW"):
        flags |= os.O_NOFOLLOW
    descriptor = os.open(path, flags, 0o600)
    try:
        metadata = os.fstat(descriptor)
        if not stat.S_ISREG(metadata.st_mode) or metadata.st_uid != os.geteuid():
            raise FixtureError(f"renderer lock is not a user-owned regular file: {path}")
        try:
            fcntl.flock(descriptor, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError as error:
            raise FixtureError("another renderer fixture is already running") from error
        yield
    finally:
        os.close(descriptor)


def inner_arguments(arguments):
    parser = argparse.ArgumentParser(add_help=False)
    parser.add_argument("--run-directory", required=True, type=Path)
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--renderer", required=True, choices=("opengl", "vulkan"))
    parser.add_argument("--case", required=True, choices=CASES)
    parser.add_argument("--niri", required=True, type=Path)
    parser.add_argument("--grim", required=True, type=Path)
    return parser.parse_args(arguments)


def run_inside(arguments):
    options = inner_arguments(arguments)
    result_path = options.run_directory / "result.json"
    status = 0
    try:
        result = run_inner(
            options.run_directory,
            options.binary,
            options.renderer,
            options.case,
            options.niri,
            options.grim,
        )
    except BaseException as error:
        status = 1
        result = {
            "ok": False,
            "case": options.case,
            "renderer": options.renderer,
            "error": str(error) or type(error).__name__,
        }
        print(f"renderer fixture failed: {result['error']}", file=sys.stderr)
    atomic_json(result_path, result)
    return status


def public_arguments(arguments):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--binary",
        required=True,
        type=Path,
        help="exact thing executable to test; the tool never builds it",
    )
    parser.add_argument(
        "--renderer",
        required=True,
        choices=("opengl", "vulkan"),
        help="run one graphics backend",
    )
    parser.add_argument(
        "--keep-artifacts",
        action="store_true",
        help="keep successful screenshots and logs under target/render-tests",
    )
    parser.add_argument("cases", nargs="+", choices=CASES)
    options = parser.parse_args(arguments)
    if len(set(options.cases)) != len(options.cases):
        parser.error("each renderer fixture may be named only once")
    return options


def signal_handler(number, _frame):
    signal.signal(number, signal.SIG_DFL)
    raise SignalInterrupt(number)


def run_public(arguments):
    options = public_arguments(arguments)
    binary = validate_binary(options.binary)
    programs = {
        name: resolve_program(name)
        for name in ("dbus-run-session", "niri", "grim")
    }
    with renderer_lock():
        for case in options.cases:
            run_case(binary, options.renderer, case, programs, options.keep_artifacts)
    return 0


def main(arguments=None):
    arguments = list(sys.argv[1:] if arguments is None else arguments)
    if arguments[:1] == ["--inside"]:
        return run_inside(arguments[1:])
    for number in TERMINATION_SIGNALS:
        signal.signal(number, signal_handler)
    try:
        return run_public(arguments)
    except SignalInterrupt as error:
        return 128 + error.number
    except FixtureError as error:
        print(f"renderer check failed: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
