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
CASES = ("image-fit", "stencil")
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


def verify_capture(case, image, stage, scale):
    if case == "image-fit":
        verify_image_fit(image, stage, scale)
    elif case == "stencil":
        verify_stencil(image, stage, scale)
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


def capture_until(grim, environment, process, output, path, case, stage, scale, deadline):
    last_error = None
    temporary = path.with_name(f".{path.name}.tmp")
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
        else:
            ensure_running(process)
            return
        time.sleep(0.15)
    raise FixtureError(f"{stage} frame did not match the fixture: {last_error}")


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
    log = (run_directory / "thing.log").open("w", encoding="utf-8")
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
            )
        return {
            "ok": True,
            "case": case,
            "output": output,
            "renderer": renderer,
        }
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


def validate_configs(binary, fixture, deadline):
    for name in ("panel.luau", "panel-reload.luau"):
        path = fixture / name
        if path.exists():
            checked([binary, "check", path], deadline=deadline)


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
        validate_configs(binary, run_directory / "fixture", deadline)
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
