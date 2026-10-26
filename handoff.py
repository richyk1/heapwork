#!/usr/bin/env python3
"""Pack portable session data, or unpack into a new directory. No links or special files."""
import pathlib
import sys
import tarfile

op, source, destination = sys.argv[1:]
source, destination = pathlib.Path(source), pathlib.Path(destination)
if op == "pack":
    with tarfile.open(destination, "w:gz") as archive:
        for path in sorted(source.rglob("*")):
            relative = path.relative_to(source)
            if any(p in ("target", ".git", ".env") or p.startswith(".env.") for p in relative.parts):
                continue
            if path.is_symlink() or not (path.is_file() or path.is_dir()):
                raise SystemExit(f"refusing non-regular session entry: {relative}")
            archive.add(path, arcname=str(relative), recursive=False)
    print(f"  handoff bundle: {destination.stat().st_size} bytes (snapshot + workspace + session metadata)")
elif op == "unpack":
    if destination.exists():
        raise SystemExit("restore destination already exists; refusing overwrite")
    with tarfile.open(source, "r:gz") as archive:
        entries = archive.getmembers()
        for entry in entries:
            path = pathlib.PurePosixPath(entry.name)
            if path.is_absolute() or ".." in path.parts or not (entry.isfile() or entry.isdir()):
                raise SystemExit(f"unsafe archive entry: {entry.name}")
            if any(p == ".env" or p.startswith(".env.") for p in path.parts):
                raise SystemExit("archive contains an environment secret file")
        destination.mkdir(parents=True)
        for entry in entries:
            path = destination / entry.name
            if entry.isdir():
                path.mkdir(parents=True, exist_ok=True)
            else:
                path.parent.mkdir(parents=True, exist_ok=True)
                with archive.extractfile(entry) as src, path.open("xb") as dst:
                    while chunk := src.read(65536):
                        dst.write(chunk)
                path.chmod(entry.mode & 0o777)
else:
    raise SystemExit("usage: handoff.py pack <session-dir> <archive> | unpack <archive> <new-session-dir>")
