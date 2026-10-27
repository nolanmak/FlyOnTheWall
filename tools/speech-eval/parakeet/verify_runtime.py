#!/usr/bin/env python3
"""Verify the pinned FluidAudio runtime sources; never fetch anything."""
import hashlib
import json
from pathlib import Path
import sys

REVISION = "a6b826ce9a09028fc685a4b32a8927ec920c3188"
# SHA-256 of sorted compact JSON [relative path, byte count, Git blob SHA-1].
# Includes Sources/** (with declared static resources), both manifests, LICENSE.
SOURCE_PIN = "6308fc39c425a2f9c909fe3c99a6721a5216e3a9efec87db5ca5d1405eee0b55"


def verify(root):
    root = Path(root)
    if root.is_symlink() or not root.is_dir():
        raise ValueError("source directory")
    paths = [root / name for name in ("Package.swift", "Package@swift-6.2.swift", "LICENSE")]
    source = root / "Sources"
    if source.is_symlink() or not source.is_dir():
        raise ValueError("source layout")
    for path in source.rglob("*"):
        if path.is_symlink():
            raise ValueError("source symlink")
        if path.is_dir():
            continue
        paths.append(path)
        if len(paths) > 1000:
            raise ValueError("source entry bound")
    rows, total = [], 0
    for path in paths:
        if path.is_symlink() or not path.is_file() or path.stat().st_size > 8_000_000:
            raise ValueError("source file")
        with path.open("rb") as handle:
            data = handle.read(8_000_001)
        total += len(data)
        if len(data) > 8_000_000 or total > 16_000_000:
            raise ValueError("source byte bound")
        blob = hashlib.sha1(b"blob " + str(len(data)).encode("ascii") + b"\0" + data).hexdigest()
        rows.append([path.relative_to(root).as_posix(), len(data), blob])
    encoded = json.dumps(sorted(rows), separators=(",", ":"), ensure_ascii=True).encode("ascii")
    if hashlib.sha256(encoded).hexdigest() != SOURCE_PIN:
        raise ValueError("source pin mismatch")


if __name__ == "__main__":
    try:
        if len(sys.argv) != 2:
            raise ValueError("one explicit source directory required")
        verify(sys.argv[1])
    except (OSError, ValueError):
        sys.exit("FluidAudio source verification failed")
    print("Verified FluidAudio 0.17.3 runtime sources at " + REVISION)
