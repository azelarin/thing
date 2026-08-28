import os
import subprocess
import unittest

from measure_process import Process, descendants, parse_stat, sample


class MeasurementTest(unittest.TestCase):
    def test_stat_command_can_contain_closing_parentheses(self):
        with open("/proc/self/stat") as source:
            original = source.read()
        suffix = original.rsplit(") ", 1)[1]
        renamed = f"{os.getpid()} (a process ) with spaces) {suffix}"
        self.assertEqual(parse_stat(os.getpid(), original), parse_stat(os.getpid(), renamed))

    def test_keeps_known_orphans_but_rejects_reused_pids(self):
        root = Process(10000000, 0, 1, 0)
        child = Process(10000001, root.pid, 2, 0)
        orphan = Process(10000002, 0, 3, 0)
        reused = Process(10000003, 0, 5, 0)
        table = {p.pid: p for p in (root, child, orphan, reused)}
        known = {orphan.identity, (reused.pid, 4)}
        self.assertEqual(descendants(table, root, known), [root, child, orphan])

    def test_samples_a_disposable_process(self):
        child = subprocess.Popen(["sleep", "10"])
        try:
            result = sample(child.pid, seconds=0.05, interval=0.025)
            self.assertEqual(result["root_pid"], child.pid)
            self.assertGreaterEqual(len(result["samples"]), 2)
            self.assertGreaterEqual(result["cpu_seconds"], 0)
            self.assertGreater(result["duration_seconds"], 0)
        finally:
            child.terminate()
            child.wait()


if __name__ == "__main__":
    unittest.main()
