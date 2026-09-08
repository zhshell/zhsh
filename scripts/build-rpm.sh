#!/bin/sh
# 按标准 Name-Version-Release.Arch 命名生成 x86_64 RPM。

set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)
REPO_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)
cd "$REPO_ROOT"

VERSION=$(python3 -c \
    'import pathlib,tomllib; print(tomllib.loads(pathlib.Path("zhsh/Cargo.toml").read_text())["package"]["version"])')
OUTPUT="target/generate-rpm/zhsh-${VERSION}-1.x86_64.rpm"
mkdir -p target/generate-rpm
cargo generate-rpm -p zhsh --auto-req builtin --output "$OUTPUT"
printf '%s\n' "$REPO_ROOT/$OUTPUT"
