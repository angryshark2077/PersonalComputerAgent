from __future__ import annotations

import argparse
import math
import os
import sqlite3
import subprocess
import tempfile
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Sequence


CPU_BUDGET_PERCENT = 1.0
RSS_BUDGET_KIB = 120 * 1024
WORKSPACE_ID = "018f3f4a-2d9b-7d21-a310-2c49d9b43c13"
DEVICE_ID = "018f3f4a-2d9b-7d21-a310-2c49d9b43c14"


class BudgetExceeded(RuntimeError):
    pass


@dataclass(frozen=True)
class SampleSummary:
    sample_count: int
    average_cpu_percent: float
    peak_rss_kib: int


def parse_ps_sample(output: str) -> tuple[float, int]:
    fields = output.split()
    if len(fields) != 2:
        raise ValueError(f"expected CPU and RSS from ps, got: {output!r}")
    cpu_percent = float(fields[0])
    rss_kib = int(fields[1])
    if not math.isfinite(cpu_percent) or cpu_percent < 0 or rss_kib < 0:
        raise ValueError(f"invalid ps sample: {output!r}")
    return cpu_percent, rss_kib


def summarize_samples(samples: Sequence[tuple[float, int]]) -> SampleSummary:
    if not samples:
        raise ValueError("at least one performance sample is required")
    return SampleSummary(
        sample_count=len(samples),
        average_cpu_percent=sum(sample[0] for sample in samples) / len(samples),
        peak_rss_kib=max(sample[1] for sample in samples),
    )


def enforce_budget(summary: SampleSummary) -> None:
    failures = []
    if summary.average_cpu_percent >= CPU_BUDGET_PERCENT:
        failures.append(
            f"average CPU {summary.average_cpu_percent:.3f}% is not below "
            f"{CPU_BUDGET_PERCENT:.1f}%"
        )
    if summary.peak_rss_kib >= RSS_BUDGET_KIB:
        failures.append(
            f"peak RSS {summary.peak_rss_kib} KiB is not below {RSS_BUDGET_KIB} KiB"
        )
    if failures:
        raise BudgetExceeded("; ".join(failures))


def _resolve_agent(path: str) -> Path:
    agent = Path(path).expanduser().resolve(strict=True)
    if not agent.is_file() or not os.access(agent, os.X_OK):
        raise ValueError(f"agent must be an executable file: {agent}")
    return agent


def _wait_for_system_running(
    child: subprocess.Popen[bytes], database: Path, timeout_seconds: float = 10.0
) -> None:
    deadline = time.monotonic() + timeout_seconds
    while time.monotonic() < deadline:
        status = child.poll()
        if status is not None:
            raise RuntimeError(f"agent exited before System became running: {status}")
        if database.exists():
            try:
                with sqlite3.connect(
                    f"file:{database}?mode=ro", uri=True, timeout=0.2
                ) as connection:
                    row = connection.execute(
                        "SELECT status FROM collector_states "
                        "WHERE collector_key = 'system'"
                    ).fetchone()
                if row == ("running",):
                    return
            except sqlite3.Error:
                pass
        time.sleep(0.05)
    raise TimeoutError("System collector did not become running within ten seconds")


def _sample_child(child: subprocess.Popen[bytes]) -> tuple[float, int]:
    if child.poll() is not None:
        raise RuntimeError(f"agent exited during performance sampling: {child.returncode}")
    result = subprocess.run(
        ["/bin/ps", "-p", str(child.pid), "-o", "%cpu=,rss="],
        check=True,
        capture_output=True,
        text=True,
    )
    return parse_ps_sample(result.stdout)


def _stop_exact_child(child: subprocess.Popen[bytes], timeout_seconds: float = 5.0) -> None:
    if child.poll() is not None:
        child.wait()
        return
    child.terminate()
    try:
        child.wait(timeout=timeout_seconds)
    except subprocess.TimeoutExpired:
        child.kill()
        child.wait(timeout=timeout_seconds)


def run_probe(agent_argument: str) -> SampleSummary:
    agent = _resolve_agent(agent_argument)
    with tempfile.TemporaryDirectory(prefix="pca-s2-performance-") as temporary_root:
        root = Path(temporary_root).resolve()
        child = subprocess.Popen(
            [
                str(agent),
                "run",
                "--runtime-root",
                str(root),
                "--process-test-workspace-id",
                WORKSPACE_ID,
                "--process-test-device-id",
                DEVICE_ID,
            ],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        try:
            _wait_for_system_running(child, root / "Data" / "agent.sqlite3")
            time.sleep(10)
            samples = []
            started_at = time.monotonic()
            for sample_number in range(1, 13):
                deadline = started_at + sample_number * 5
                time.sleep(max(0.0, deadline - time.monotonic()))
                samples.append(_sample_child(child))
        finally:
            _stop_exact_child(child)
    return summarize_samples(samples)


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Measure the feature-built System Collector agent process"
    )
    parser.add_argument("--agent", required=True, help="path to the pca-agentd binary")
    arguments = parser.parse_args(argv)

    summary = run_probe(arguments.agent)
    print(f"samples={summary.sample_count}")
    print(f"average_cpu_percent={summary.average_cpu_percent:.3f}")
    print(f"peak_rss_kib={summary.peak_rss_kib}")
    enforce_budget(summary)
    return 0
