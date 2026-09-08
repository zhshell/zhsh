#!/bin/sh
# 在全新固定 Ubuntu 24.04 镜像中安装并验证最终 .deb。

set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)
REPO_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)
EXPECTED_ROOT=$(git -C "$REPO_ROOT" rev-parse --show-toplevel)
if [ "$REPO_ROOT" != "$EXPECTED_ROOT" ]; then
    echo "deb-smoke: 脚本不在仓库根目录下" >&2
    exit 2
fi
cd "$REPO_ROOT"

WORK_DIR=$(mktemp -d "${TMPDIR:-/tmp}/zhsh-deb-smoke.XXXXXX")
SMOKE_IMAGE=
SMOKE_CONTAINER=
cleanup() {
    if [ -n "$SMOKE_CONTAINER" ]; then
        docker rm -f "$SMOKE_CONTAINER" >/dev/null 2>&1 || true
    fi
    if [ -n "$SMOKE_IMAGE" ]; then
        docker image rm "$SMOKE_IMAGE" >/dev/null 2>&1 || true
    fi
    rm -rf "$WORK_DIR"
}
trap cleanup EXIT HUP INT TERM

if [ "$#" -ne 1 ]; then
    echo "用法: scripts/deb-smoke.sh /absolute/path/to/zhsh_VERSION-REVISION_amd64.deb" >&2
    exit 2
fi
case "$1" in
    /*) DEB_FILE=$1 ;;
    *) DEB_FILE="$REPO_ROOT/$1" ;;
esac
if [ ! -f "$DEB_FILE" ]; then
    echo "deb-smoke: Debian 包不存在: $DEB_FILE" >&2
    exit 1
fi
command -v docker >/dev/null 2>&1 || {
    echo "deb-smoke: 缺少 Docker；制品冒烟不得跳过" >&2
    exit 1
}

cp "$DEB_FILE" "$WORK_DIR/package.deb"
cp scripts/mock-llm.py "$WORK_DIR/mock-llm.py"
cp scripts/run-deb-smoke.sh "$WORK_DIR/run-deb-smoke.sh"
cp packaging/release/smoke-dpkg.cfg "$WORK_DIR/smoke-dpkg.cfg"

SUFFIX=$$
SMOKE_IMAGE="zhsh-deb-smoke:$SUFFIX"
SMOKE_CONTAINER="zhsh-deb-smoke-$SUFFIX"
docker build \
    --platform linux/amd64 \
    --pull=false \
    --no-cache \
    --file packaging/release/smoke.Dockerfile \
    --tag "$SMOKE_IMAGE" \
    "$WORK_DIR"
docker run --name "$SMOKE_CONTAINER" --platform linux/amd64 --network bridge "$SMOKE_IMAGE"
