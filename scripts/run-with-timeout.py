#!/usr/bin/env python3
"""Run one command inside a bounded, killable process group."""

from __future__ import annotations

import argparse
import os
import signal
import subprocess
import sys
import time


def parse_arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    limit = parser.add_mutually_exclusive_group(required=True)
    limit.add_argument("--timeout", type=float)
    limit.add_argument("--deadline", type=float)
    parser.add_argument("command", nargs=argparse.REMAINDER)
    arguments = parser.parse_args()
    if arguments.command[:1] == ["--"]:
        arguments.command = arguments.command[1:]
    if not arguments.command:
        parser.error("a command is required after --")
    return arguments


def main() -> int:
    arguments = parse_arguments()
    timeout = arguments.timeout
    if arguments.deadline is not None:
        timeout = arguments.deadline - time.monotonic()
    if timeout is None or timeout <= 0:
        return 124

    process = subprocess.Popen(arguments.command, start_new_session=True)
    try:
        return process.wait(timeout=timeout)
    except subprocess.TimeoutExpired:
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        process.wait()
        return 124


if __name__ == "__main__":
    raise SystemExit(main())
