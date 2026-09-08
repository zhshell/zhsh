#!/bin/sh
# 校验 Cargo 是唯一版本源，并核对 CLI、man、README 与 Debian 元数据。

set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)
REPO_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)
EXPECTED_ROOT=$(git -C "$REPO_ROOT" rev-parse --show-toplevel)
if [ "$REPO_ROOT" != "$EXPECTED_ROOT" ]; then
    echo "release-metadata: 脚本不在仓库根目录下" >&2
    exit 2
fi
cd "$REPO_ROOT"

WORK_DIR=$(mktemp -d "${TMPDIR:-/tmp}/zhsh-release-metadata.XXXXXX")
cleanup() {
    rm -rf "$WORK_DIR"
}
trap cleanup EXIT HUP INT TERM

BINARY=${1:-target/release/zhsh}
DEB_FILE=${2:-}

python3 "$SCRIPT_DIR/release_metadata.py" "$BINARY" "$DEB_FILE"
