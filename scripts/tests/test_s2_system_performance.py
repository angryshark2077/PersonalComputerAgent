from __future__ import annotations

import unittest

from scripts.s2_system_performance import (
    BudgetExceeded,
    enforce_budget,
    parse_ps_sample,
    summarize_samples,
)


class SystemPerformanceTests(unittest.TestCase):
    def test_ps_sample_is_parsed(self) -> None:
        self.assertEqual(parse_ps_sample("  0.4  81920\n"), (0.4, 81920))

    def test_summary_enforces_existing_budget(self) -> None:
        summary = summarize_samples([(0.4, 80 * 1024), (0.8, 90 * 1024)])

        self.assertEqual(summary.sample_count, 2)
        self.assertAlmostEqual(summary.average_cpu_percent, 0.6)
        self.assertEqual(summary.peak_rss_kib, 90 * 1024)
        enforce_budget(summary)

    def test_over_budget_cpu_sample_fails(self) -> None:
        with self.assertRaises(BudgetExceeded):
            enforce_budget(summarize_samples([(1.2, 90 * 1024)]))

    def test_over_budget_rss_sample_fails(self) -> None:
        with self.assertRaises(BudgetExceeded):
            enforce_budget(summarize_samples([(0.2, 120 * 1024)]))

    def test_budget_is_strict(self) -> None:
        with self.assertRaises(BudgetExceeded):
            enforce_budget(summarize_samples([(1.0, 120 * 1024)]))

    def test_empty_samples_are_rejected(self) -> None:
        with self.assertRaises(ValueError):
            summarize_samples([])


if __name__ == "__main__":
    unittest.main()
