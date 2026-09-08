#!/bin/sh
# 在 Fedora 44 中安装最终 RPM，并验证版本和运行时资源。

set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)
REPO_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)
cd "$REPO_ROOT"

if [ "$#" -ne 1 ]; then
    echo "用法: scripts/rpm-smoke.sh /absolute/path/to/zhsh-VERSION-RELEASE.x86_64.rpm" >&2
    exit 2
fi
case "$1" in
    /*) RPM_FILE=$1 ;;
    *) RPM_FILE="$REPO_ROOT/$1" ;;
esac
[ -f "$RPM_FILE" ] || {
    echo "rpm-smoke: RPM 包不存在: $RPM_FILE" >&2
    exit 1
}
command -v docker >/dev/null 2>&1 || {
    echo "rpm-smoke: 缺少 Docker；制品安装冒烟不得跳过" >&2
    exit 1
}

VERSION=$(python3 -c \
    'import pathlib,tomllib; print(tomllib.loads(pathlib.Path("zhsh/Cargo.toml").read_text())["package"]["version"])')
docker run --rm --platform linux/amd64 \
    --volume "$RPM_FILE:/input/zhsh.rpm:ro" \
    --env "EXPECTED_VERSION=zhsh $VERSION" \
    fedora:44@sha256:43b29f65a41eb9c35e1cd5323e3bdf3b655c2357a9f4f1ff2f9c2798e5045d80 \
    bash -euc '
        dnf install --assumeyes /input/zhsh.rpm >/dev/null
        test "$(/usr/bin/zhsh --version)" = "$EXPECTED_VERSION"
        rpm -ql zhsh | grep -qx /usr/share/man/man1/zhsh.1
        test "$(find /usr/lib/zhsh/plugins/llm -maxdepth 1 -type f -name "*.zhcodec" | wc -l)" -eq 2
        test ! -e /usr/lib/zhsh/plugins/safety
    '

echo "rpm-smoke: Fedora 44 安装、版本与运行时资源检查通过"
