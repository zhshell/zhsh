#!/bin/sh
# Export a buildable, history-free GitHub source snapshot using an explicit allowlist.

set -eu

if [ "$#" -ne 1 ]; then
    echo "用法: scripts/export-public-snapshot.sh DESTINATION" >&2
    exit 2
fi

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)
REPO_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)
DESTINATION=$(realpath -m -- "$1")

case "$DESTINATION/" in
    "$REPO_ROOT/"|"$REPO_ROOT/"*)
        echo "public-export: 目标目录不得位于私有源码树内" >&2
        exit 2
        ;;
esac

if [ -e "$DESTINATION" ] && [ -n "$(find "$DESTINATION" -mindepth 1 -maxdepth 1 -print -quit)" ]; then
    echo "public-export: 目标目录必须不存在或为空: $DESTINATION" >&2
    exit 2
fi

mkdir -p "$DESTINATION"

for entry in \
    .github \
    .gitignore \
    CHANGELOG.md \
    CONTRIBUTING.md \
    Cargo.lock \
    Cargo.toml \
    LICENSE \
    Makefile \
    README.md \
    SECURITY.md \
    deny.toml \
    packaging \
    rust-toolchain.toml \
    scripts \
    zhsh; do
    if [ ! -e "$REPO_ROOT/$entry" ]; then
        echo "public-export: allowlist 路径不存在: $entry" >&2
        exit 1
    fi
    cp -a "$REPO_ROOT/$entry" "$DESTINATION/$entry"
done

printf '%s\n' \
    'This marker identifies the history-free public source repository.' \
    >"$DESTINATION/.public-repository"

(cd "$DESTINATION" && scripts/check-public-boundary.sh)
printf 'public-export: 已生成 %s\n' "$DESTINATION"
