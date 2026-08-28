#!/usr/bin/env python3
"""sample cpu use and memory for a process and its observed descendants."""

import argparse
from dataclasses import dataclass
import json
import math
import os
from pathlib import Path
import statistics
import sys
import time


@dataclass(frozen=True)
class Process:
    pid: int
    parent: int
    started: int
    ticks: int

    @property
    def identity(self):
        return self.pid, self.started


def parse_stat(pid, text):
    # the command field can itself contain spaces and closing parentheses.
    fields = text.rsplit(") ", 1)[1].split()
    return Process(pid, int(fields[1]), int(fields[19]), int(fields[11]) + int(fields[12]))


def process_table():
    result = {}
    for path in Path("/proc").iterdir():
        if path.name.isdecimal():
            try:
                process = parse_stat(int(path.name), (path / "stat").read_text())
                result[process.pid] = process
            except (OSError, ValueError, IndexError):
                # processes can exit between enumeration and reading stat.
                continue
    return result


def descendants(table, root, known):
    included = {root.pid}
    included.update(p.pid for p in table.values() if p.identity in known)
    while True:
        children = {p.pid for p in table.values() if p.parent in included}
        if children <= included:
            break
        included.update(children)
    included.discard(os.getpid())
    return [table[pid] for pid in sorted(included) if pid in table]


def memory(pid):
    try:
        text = Path(f"/proc/{pid}/smaps_rollup").read_text()
    except OSError:
        return None
    values = {}
    for line in text.splitlines():
        fields = line.split()
        if fields and fields[0] in ("Rss:", "Pss:"):
            values[fields[0][:-1].lower() + "_kib"] = int(fields[1])
    return values if len(values) == 2 else None


def sample(pid, seconds, interval):
    table = process_table()
    root = table.get(pid)
    if root is None or pid == os.getpid():
        raise ValueError("select an existing process other than the sampler")
    known = set()
    previous_ticks = {}
    samples = []
    clock_ticks = os.sysconf("SC_CLK_TCK")
    cpu_ticks = 0
    start = time.monotonic()
    end = start + seconds
    while True:
        table = process_table()
        if pid not in table or table[pid].identity != root.identity:
            raise ValueError("the selected process exited or its PID was reused; sample discarded")
        processes = descendants(table, root, known)
        now = time.monotonic()
        elapsed = now - start
        for process in processes:
            if samples:
                # new descendants contribute their cpu time since birth;
                # existing descendants contribute only their interval delta.
                cpu_ticks += max(0, process.ticks - previous_ticks.get(process.identity, 0))
            previous_ticks[process.identity] = process.ticks
            known.add(process.identity)
        readings = [memory(process.pid) for process in processes]
        readable = all(value is not None for value in readings)
        samples.append({
            "elapsed_seconds": round(elapsed, 6),
            "cpu_seconds": cpu_ticks / clock_ticks,
            "pids": [process.pid for process in processes],
            "pss_kib": sum(value["pss_kib"] for value in readings) if readable else None,
            "rss_kib": sum(value["rss_kib"] for value in readings) if readable else None,
        })
        if now >= end:
            break
        time.sleep(min(interval, end - now))
    duration = samples[-1]["elapsed_seconds"] - samples[0]["elapsed_seconds"]
    complete_memory = all(item["pss_kib"] is not None for item in samples)
    return {
        "schema_version": 1,
        "root_pid": pid,
        "root_start_ticks": root.started,
        "duration_seconds": duration,
        "interval_seconds": interval,
        "cpu_seconds": cpu_ticks / clock_ticks,
        "cpu_percent_one_core": 100 * cpu_ticks / clock_ticks / duration if duration else None,
        "median_pss_kib": statistics.median(item["pss_kib"] for item in samples) if complete_memory else None,
        "peak_sampled_pss_kib": max(item["pss_kib"] for item in samples) if complete_memory else None,
        "samples": samples,
        "limitations": [
            "Short-lived children can exit between samples and escape accounting.",
            "Peak memory is sampled, not a kernel high-water mark.",
            "Unreadable memory totals are null, never treated as zero.",
            "Startup, wakeups, GPU allocations, and frame times are not measured.",
        ],
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--pid", type=int, required=True, help="framework process PID; descendants are included")
    parser.add_argument("--seconds", type=float, default=60)
    parser.add_argument("--interval", type=float, default=1)
    args = parser.parse_args()
    if args.pid <= 0 or not all(math.isfinite(v) and v > 0 for v in (args.seconds, args.interval)):
        parser.error("PID, duration, and interval must be positive; durations must be finite")
    try:
        result = sample(args.pid, args.seconds, args.interval)
    except (OSError, ValueError) as error:
        print(f"measurement failed: {error}", file=sys.stderr)
        return 1
    print(json.dumps(result, indent=2, allow_nan=False))
    return 0


if __name__ == "__main__":
    sys.exit(main())
