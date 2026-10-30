#!/usr/bin/env python3
"""Собрать архив уже скомпилированного клиента; публикации нет."""
import argparse
import hashlib
from pathlib import Path
import tarfile
import json
import subprocess

parser = argparse.ArgumentParser()
parser.add_argument("--target", required=True, choices=["x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu", "x86_64-apple-darwin", "aarch64-apple-darwin"])
args = parser.parse_args()
root = Path(__file__).resolve().parent.parent
metadata = json.loads(subprocess.check_output(["cargo", "metadata", "--no-deps", "--format-version", "1", "--locked", "--offline"], cwd=root, text=True))
version = next(package["version"] for package in metadata["packages"] if package["name"] == "zottion")
binary = root / "target" / args.target / "release" / "zottion"
if not binary.is_file():
    parser.error(f"Сначала cargo build --release --locked --target {args.target}")
out = root / "dist"
out.mkdir(exist_ok=True)
name = f"zottion-v{version}-{args.target}"
archive = out / f"{name}.tar.gz"
with tarfile.open(archive, "w:gz") as tar:
    tar.add(binary, arcname=f"{name}/zottion")
    for file in ["LICENSE", "README.md", "README.ru.md", "CONTRIBUTING.md", "SECURITY.md", "ARCHITECTURE.md", "render.yaml", "assets", "docs"]:
        tar.add(root / file, arcname=f"{name}/{file}")
digest = hashlib.sha256(archive.read_bytes()).hexdigest()
archive.with_suffix(archive.suffix + ".sha256").write_text(f"{digest}  {archive.name}\n")
print(archive)
