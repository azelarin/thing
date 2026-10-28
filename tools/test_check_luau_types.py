"""test the Luau definition harness without running the analyzer."""

import subprocess
import unittest

from check_luau_types import Case, result_failures


class ResultTest(unittest.TestCase):
    def test_accepts_a_clean_positive_result(self):
        result = subprocess.CompletedProcess([], 0, stdout="", stderr="")
        self.assertEqual(result_failures(Case("valid.luau"), "default", result), [])

    def test_requires_every_negative_diagnostic(self):
        case = Case("invalid.luau", ("first diagnostic", "second diagnostic"))
        result = subprocess.CompletedProcess(
            [], 1, stdout="first diagnostic", stderr="analyzer context"
        )

        failures = result_failures(case, "default", result)

        self.assertEqual(len(failures), 1)
        self.assertIn("second diagnostic", failures[0])
        self.assertIn("analyzer context", failures[0])

    def test_rejects_an_unexpected_exit_for_a_negative(self):
        case = Case("invalid.luau", ("expected diagnostic",))
        result = subprocess.CompletedProcess([], 2, stdout="", stderr="bad arguments")

        failures = result_failures(case, "flags-disabled", result)

        self.assertEqual(len(failures), 1)
        self.assertIn("expected exit 1, got 2", failures[0])


if __name__ == "__main__":
    unittest.main()
