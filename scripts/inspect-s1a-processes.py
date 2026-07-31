#!/usr/bin/env python3
"""Inspect one macOS Agent process and its exact Bridge child via libproc."""

from __future__ import annotations

import argparse
import ctypes
import json
import os
import sys
from pathlib import Path
from typing import Any, Dict, List


PROC_PIDTBSDINFO = 3
PATH_BUFFER_SIZE = 4096
CHILD_CAPACITY = 4096


class ProcBSDInfo(ctypes.Structure):
    _pack_ = 4
    _fields_ = [
        ("pbi_flags", ctypes.c_uint32), ("pbi_status", ctypes.c_uint32),
        ("pbi_xstatus", ctypes.c_uint32), ("pbi_pid", ctypes.c_uint32),
        ("pbi_ppid", ctypes.c_uint32), ("pbi_uid", ctypes.c_uint32),
        ("pbi_gid", ctypes.c_uint32), ("pbi_ruid", ctypes.c_uint32),
        ("pbi_rgid", ctypes.c_uint32), ("pbi_svuid", ctypes.c_uint32),
        ("pbi_svgid", ctypes.c_uint32), ("pbi_rfu_1", ctypes.c_uint32),
        ("pbi_comm", ctypes.c_char * 16), ("pbi_name", ctypes.c_char * 32),
        ("pbi_nfiles", ctypes.c_uint32), ("pbi_pgid", ctypes.c_uint32),
        ("pbi_pjobc", ctypes.c_uint32), ("e_tdev", ctypes.c_uint32),
        ("e_tpgid", ctypes.c_uint32), ("pbi_nice", ctypes.c_int32),
        ("pbi_start_tvsec", ctypes.c_uint64),
        ("pbi_start_tvusec", ctypes.c_uint64),
    ]


def fail(message: str) -> "None":
    raise ValueError(message)


def process_info(libproc: Any, pid: int) -> Dict[str, Any]:
    path_buffer = ctypes.create_string_buffer(PATH_BUFFER_SIZE)
    path_length = libproc.proc_pidpath(pid, path_buffer, len(path_buffer))
    if path_length <= 0:
        fail(f"could not resolve executable path for PID {pid}")
    info = ProcBSDInfo()
    result = libproc.proc_pidinfo(pid, PROC_PIDTBSDINFO, 0, ctypes.byref(info), ctypes.sizeof(info))
    if result != ctypes.sizeof(info) or info.pbi_pid != pid:
        fail(f"could not read BSD process identity for PID {pid}")
    return {
        "pid": pid, "ppid": int(info.pbi_ppid), "uid": int(info.pbi_uid),
        "path": os.fsdecode(path_buffer.value),
        "start_time": float(info.pbi_start_tvsec) + float(info.pbi_start_tvusec) / 1_000_000,
    }


def child_pids(libproc: Any, pid: int) -> List[int]:
    values = (ctypes.c_int * CHILD_CAPACITY)()
    count = libproc.proc_listchildpids(pid, values, ctypes.sizeof(values))
    if count < 0 or count >= CHILD_CAPACITY:
        fail("could not enumerate the bounded direct child process set")
    return [int(values[index]) for index in range(count)]


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--agent-pid", type=int, required=True)
    parser.add_argument("--uid", type=int, required=True)
    parser.add_argument("--agent-path", type=Path, required=True)
    parser.add_argument("--bridge-path", type=Path, required=True)
    arguments = parser.parse_args()
    try:
        if sys.platform != "darwin":
            fail("libproc process inspection requires macOS")
        libproc = ctypes.CDLL("/usr/lib/libproc.dylib", use_errno=True)
        agent = process_info(libproc, arguments.agent_pid)
        if arguments.uid == 0 or agent["uid"] != arguments.uid or agent["path"] != str(arguments.agent_path):
            fail("Agent PID is not the exact current-user installed executable")
        candidates = []
        for child_pid in child_pids(libproc, arguments.agent_pid):
            try:
                child = process_info(libproc, child_pid)
            except ValueError:
                continue
            if child["path"] == str(arguments.bridge_path):
                candidates.append(child)
        if len(candidates) != 1:
            fail("expected exactly one direct Bridge child with the installed executable path")
        bridge = candidates[0]
        if bridge["uid"] != arguments.uid or bridge["ppid"] != agent["pid"]:
            fail("Bridge is not a current-user direct child of the exact Agent")
        if bridge["start_time"] < agent["start_time"]:
            fail("Bridge process predates its Agent parent")
        print(json.dumps({"agent": agent, "bridge": bridge}, sort_keys=True))
    except (OSError, ValueError) as error:
        print(f"S1A process inspection failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
