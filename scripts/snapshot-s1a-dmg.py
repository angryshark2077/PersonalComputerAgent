#!/usr/bin/env python3
"""Create, validate, and remove one private immutable-by-identity DMG snapshot."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import shutil
import stat
import sys
import tempfile
from pathlib import Path
from typing import Any, Dict


def fail(message: str) -> "None":
    raise ValueError(message)


def reject_symlink_components(path: Path) -> None:
    current = Path(path.anchor)
    for part in path.parts[1:]:
        current /= part
        metadata = current.lstat()
        if stat.S_ISLNK(metadata.st_mode):
            fail(f"symbolic-link path component is forbidden: {current}")


def directory_identity(path: Path, uid: int) -> Dict[str, Any]:
    metadata = path.lstat()
    if not stat.S_ISDIR(metadata.st_mode) or stat.S_ISLNK(metadata.st_mode):
        fail("snapshot directory is not a regular directory")
    if metadata.st_uid != uid or stat.S_IMODE(metadata.st_mode) != 0o700:
        fail("snapshot directory must be owned by the current UID with mode 0700")
    return {
        "dir_path": str(path), "dir_dev": metadata.st_dev, "dir_ino": metadata.st_ino,
        "dir_uid": metadata.st_uid, "dir_mode": stat.S_IMODE(metadata.st_mode),
    }


def file_identity(path: Path, uid: int) -> Dict[str, Any]:
    flags = os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0)
    descriptor = os.open(path, flags)
    try:
        metadata = os.fstat(descriptor)
        if not stat.S_ISREG(metadata.st_mode):
            fail("snapshot is not a regular file")
        if metadata.st_uid != uid or stat.S_IMODE(metadata.st_mode) != 0o600:
            fail("snapshot must be owned by the current UID with mode 0600")
        digest = hashlib.sha256()
        size = 0
        while True:
            chunk = os.read(descriptor, 1024 * 1024)
            if not chunk:
                break
            digest.update(chunk)
            size += len(chunk)
    finally:
        os.close(descriptor)
    return {
        "file_path": str(path), "file_dev": metadata.st_dev, "file_ino": metadata.st_ino,
        "file_uid": metadata.st_uid, "file_mode": stat.S_IMODE(metadata.st_mode),
        "file_size": size, "file_sha256": digest.hexdigest(),
    }


def create_snapshot(source: Path, parent: Path) -> Dict[str, Any]:
    uid = os.geteuid()
    source = Path(os.path.abspath(source))
    parent = Path(os.path.abspath(parent))
    reject_symlink_components(source)
    reject_symlink_components(parent)
    parent_metadata = parent.lstat()
    if not stat.S_ISDIR(parent_metadata.st_mode) or stat.S_ISLNK(parent_metadata.st_mode):
        fail("snapshot parent is not a regular directory")
    directory = Path(tempfile.mkdtemp(prefix="pca-s1a-dmg.", dir=parent))
    snapshot = directory / "candidate.dmg"
    try:
        os.chmod(directory, 0o700)
        source_fd = os.open(source, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0))
        try:
            before = os.fstat(source_fd)
            if not stat.S_ISREG(before.st_mode):
                fail("DMG source must be a regular file")
            output_fd = os.open(
                snapshot,
                os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0),
                0o600,
            )
            try:
                while True:
                    chunk = os.read(source_fd, 1024 * 1024)
                    if not chunk:
                        break
                    view = memoryview(chunk)
                    while view:
                        written = os.write(output_fd, view)
                        view = view[written:]
                os.fsync(output_fd)
            finally:
                os.close(output_fd)
            after = os.fstat(source_fd)
            path_metadata = source.lstat()
            if (
                before.st_dev, before.st_ino, before.st_size, before.st_mtime_ns, before.st_ctime_ns,
            ) != (
                after.st_dev, after.st_ino, after.st_size, after.st_mtime_ns, after.st_ctime_ns,
            ):
                fail("DMG source changed while it was being snapshotted")
            if (after.st_dev, after.st_ino) != (path_metadata.st_dev, path_metadata.st_ino):
                fail("DMG source path was replaced while it was being snapshotted")
        finally:
            os.close(source_fd)
        identity = directory_identity(directory, uid)
        identity.update(file_identity(snapshot, uid))
        return identity
    except Exception as original_error:
        try:
            shutil.rmtree(directory)
        except OSError as cleanup_error:
            fail(f"snapshot creation failed ({original_error}); partial cleanup also failed ({cleanup_error})")
        raise


def validate_snapshot(identity: Dict[str, Any]) -> None:
    uid = os.geteuid()
    directory = Path(identity["dir_path"])
    snapshot = Path(identity["file_path"])
    if snapshot.parent != directory or snapshot.name != "candidate.dmg":
        fail("snapshot path is not the fixed file inside its private directory")
    actual = directory_identity(directory, uid)
    actual.update(file_identity(snapshot, uid))
    for key in (
        "dir_dev", "dir_ino", "dir_uid", "dir_mode", "file_dev", "file_ino",
        "file_uid", "file_mode", "file_size", "file_sha256",
    ):
        if actual[key] != identity.get(key):
            fail(f"snapshot identity changed: {key}")


def cleanup_snapshot(identity: Dict[str, Any]) -> None:
    validate_snapshot(identity)
    snapshot = Path(identity["file_path"])
    directory = Path(identity["dir_path"])
    snapshot.unlink()
    directory.rmdir()


def main() -> int:
    parser = argparse.ArgumentParser()
    subparsers = parser.add_subparsers(dest="command", required=True)
    create = subparsers.add_parser("create")
    create.add_argument("--source", type=Path, required=True)
    create.add_argument("--parent", type=Path)
    for command in ("validate", "cleanup"):
        child = subparsers.add_parser(command)
        child.add_argument("--identity-json", required=True)
    arguments = parser.parse_args()
    try:
        if arguments.command == "create":
            parent = arguments.parent or Path(os.path.realpath(tempfile.gettempdir()))
            print(json.dumps(create_snapshot(arguments.source, parent), sort_keys=True))
        else:
            identity = json.loads(arguments.identity_json)
            if not isinstance(identity, dict):
                fail("snapshot identity must be an object")
            if arguments.command == "validate":
                validate_snapshot(identity)
            else:
                cleanup_snapshot(identity)
    except (KeyError, OSError, ValueError, json.JSONDecodeError) as error:
        print(f"S1A DMG snapshot failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
