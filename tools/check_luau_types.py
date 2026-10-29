#!/usr/bin/env python3
"""check the shipped Luau definitions against small consumer fixtures."""

import argparse
from dataclasses import dataclass
from pathlib import Path
import shutil
import subprocess
import sys


ROOT = Path(__file__).resolve().parent.parent
DEFINITIONS = ROOT / "crates/thing/types/thing.d.luau"
FIXTURES = ROOT / "tests/luau-types"
ANALYZER_VERSION = "1.70.1"
COMMAND_TIMEOUT = 15
SOLVERS = (
    ("default", ()),
    ("flags-disabled", ("--no-flags-enabled",)),
)


@dataclass(frozen=True)
class Case:
    name: str
    diagnostics: tuple[str, ...] = ()


CASES = (
    Case("valid.luau"),
    Case("valid-timers.luau"),
    Case("valid-brushes.luau"),
    Case(
        "invalid-brushes.luau",
        ("Property 'kind' is not compatible.", "Property 'position' is not compatible."),
    ),
    Case(
        "invalid-timers.luau",
        (
            "Property 'interval' is not compatible.",
            "Property 'on_timeout' is not compatible.",
            "missing field 'on_timeout'",
            "Function 'timer.start' expects 1 argument, but 2 are specified",
            "Function 'timer.restart' expects 1 argument, but none are specified",
        ),
    ),
    Case("valid-lock.luau"),
    Case(
        "invalid-text-input.luau",
        ("missing field 'value'", "missing field 'on_change'"),
    ),
    Case(
        "invalid-transitions.luau",
        (
            "Property 'width' is not compatible.",
            "Property 'background' is not compatible.",
            "Property 'placeholder_color' is not compatible.",
        ),
    ),
    Case("invalid-springs.luau", ("Property 'background' is not compatible.",)),
    Case(
        "invalid-clipboard.luau",
        (
            "Expected this to be 'string', but got 'number'",
            "Function 'services.clipboard.clear' expects 0 arguments",
            "Key 'set' not found in table 'ThingSignal<string?>'",
        ),
    ),
    Case("valid-media-timeline.luau"),
    Case(
        "invalid-media-timeline.luau",
        (
            "Expected this to be 'number', but got 'string'",
            "Function 'services.media.set_position' expects 1 to 2 arguments",
            "Expected this to be 'number', but got 'number?'",
        ),
    ),
    Case("valid-applications.luau"),
    Case("valid-application-launch.luau"),
    Case(
        "invalid-application-launch.luau",
        (
            "Expected this to be '\"file\" | \"files\"', but got '\"none\"'",
            "Expected this to be 'string', but got 'ThingDesktopApplication'",
            "Expected this to be exactly '{string}?', but got 'string'",
        ),
    ),
    Case(
        "invalid-applications.luau",
        (
            "Expected this to be 'string', but got 'string?'",
            "Expected this to be 'number', but got 'string'",
            "Key 'set' not found in table 'ThingSignal<ThingReadonlyArray<ThingDesktopApplication>>'",
        ),
    ),
    Case("valid-notification-history.luau"),
    Case(
        "invalid-notification-history.luau",
        (
            "Expected this to be 'number', but got 'number?'",
            "Function 'services.notifications.reply' expects 2 arguments",
            "Function 'services.notifications.clear_history' expects 0 arguments",
        ),
    ),
    Case(
        "invalid-image-source.luau",
        (
            "Property 'icon' is not compatible.",
            "Property 'app_id' is not compatible.",
        ),
    ),
    Case(
        "invalid-input-region.luau",
        (
            "Property 'x' is not compatible.",
            "Property 'width' is not compatible.",
            "Expected this to be 'ThingInputRectangle', but got 'boolean'",
        ),
    ),
    Case(
        "invalid-idle-inhibit.luau",
        (
            "Property 'idle_inhibit' is not compatible.",
            "Cannot add property 'idle_inhibit' to table 'ThingLockProperties'",
            "Expected this to be 'number', but got 'boolean'",
        ),
    ),
    Case(
        "invalid-secret-input.luau",
        (
            "Cannot add property 'value' to table 'ThingSecretInputProperties'",
            "Cannot add property 'on_change' to table 'ThingSecretInputProperties'",
            "Cannot add property 'on_submit' to table 'ThingSecretInputProperties'",
        ),
    ),
    Case(
        "invalid-system-tray-menu.luau",
        (
            "Expected this to be 'string', but got 'number'",
            "Expected this to be 'string', but got 'boolean'",
            "Expected this to be 'number', but got 'string'",
            "Expected this to be 'number', but got 'boolean'",
        ),
    ),
    Case(
        "invalid-ui-actions.luau",
        (
            "Property 'autofocus' is not compatible.",
            "Expected this to be 'ThingWidget', but got 'string'",
            "Expected this to be 'ThingWidget', but got 'boolean'",
        ),
    ),
    Case(
        "invalid-key-events.luau",
        (
            "Expected this to be 'string', but got 'boolean'",
            "Expected this to be 'boolean', but got 'string'",
        ),
    ),
    Case(
        "invalid-managed-process.luau",
        (
            "Other overloads are also not viable",
            "Expected this to be 'string', but got 'number'",
            "Function 'query.stop' expects 0 arguments",
            "Key 'refresh' not found in external type 'ThingManagedProcess'",
            "Key 'start' not found in external type 'ThingScheduledProcess'",
            "Function 'scheduled.cancel' expects 0 arguments",
            "Property 'text' is not compatible.",
        ),
    ),
    Case(
        "invalid-pointer-area.luau",
        (
            "Expected this to be 'boolean?', but got 'number'",
            "Expected this to be 'number', but got",
        ),
    ),
    Case(
        "invalid-transforms.luau",
        (
            "Property 'translate_x' is not compatible.",
            "Property 'rotation' is not compatible.",
            "Property 'x' is not compatible.",
            "Property 'offset_y' is not compatible.",
        ),
    ),
)


def output_text(result):
    return "\n".join(
        value.strip() for value in (result.stdout, result.stderr) if value.strip()
    )


def result_failures(case, solver, result):
    label = f"{case.name} ({solver})"
    output = output_text(result)
    if not case.diagnostics:
        if result.returncode == 0:
            return []
        detail = output or "analyzer produced no output"
        return [f"{label}: expected success, got exit {result.returncode}\n{detail}"]

    if result.returncode == 0:
        return [f"{label}: expected type errors, but analysis succeeded"]
    if result.returncode != 1:
        detail = output or "analyzer produced no output"
        return [f"{label}: expected exit 1, got {result.returncode}\n{detail}"]
    missing = [
        diagnostic for diagnostic in case.diagnostics if diagnostic not in output
    ]
    if not missing:
        return []
    expected = "\n".join(f"  {diagnostic}" for diagnostic in missing)
    detail = output or "analyzer produced no output"
    return [f"{label}: missing expected diagnostics:\n{expected}\n{detail}"]


def invoke(arguments):
    try:
        return subprocess.run(
            arguments,
            cwd=ROOT,
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            timeout=COMMAND_TIMEOUT,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        raise RuntimeError(str(error)) from error


def check_version(analyzer):
    result = invoke([analyzer, "--version"])
    version = output_text(result)
    if result.returncode != 0:
        raise RuntimeError(f"cannot read analyzer version: {version or 'no output'}")
    if version != ANALYZER_VERSION:
        raise RuntimeError(
            f"expected luau-lsp {ANALYZER_VERSION}, found {version or 'unknown'}"
        )


def analyze(analyzer, case, flags):
    return invoke(
        [
            analyzer,
            "analyze",
            "--platform=standard",
            f"--definitions=@thing={DEFINITIONS}",
            *flags,
            str(FIXTURES / case.name),
        ]
    )


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--analyzer",
        required=True,
        help=f"path to the luau-lsp {ANALYZER_VERSION} executable",
    )
    args = parser.parse_args(argv)
    analyzer = shutil.which(args.analyzer)
    if analyzer is None:
        parser.error(f"analyzer is not executable: {args.analyzer}")

    try:
        check_version(analyzer)
        failures = []
        for case in CASES:
            path = FIXTURES / case.name
            if not path.is_file():
                failures.append(f"missing fixture: {path.relative_to(ROOT)}")
                continue
            for solver, flags in SOLVERS:
                result = analyze(analyzer, case, flags)
                failures.extend(result_failures(case, solver, result))
    except RuntimeError as error:
        print(f"luau definition check failed: {error}", file=sys.stderr)
        return 1

    if failures:
        for failure in failures:
            print(f"luau definition check failed: {failure}", file=sys.stderr)
        return 1
    print(
        f"checked {len(CASES)} Luau fixtures with "
        f"{len(SOLVERS)} solver configurations"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
