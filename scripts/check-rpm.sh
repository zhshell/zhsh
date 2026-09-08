#!/bin/sh
# 校验 RPM 身份、依赖和安装文件与源码安装布局一致。

set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)
REPO_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)
cd "$REPO_ROOT"

if [ "$#" -ne 1 ]; then
    echo "用法: scripts/check-rpm.sh /absolute/path/to/zhsh-VERSION-RELEASE.x86_64.rpm" >&2
    exit 2
fi
case "$1" in
    /*) RPM_FILE=$1 ;;
    *) RPM_FILE="$REPO_ROOT/$1" ;;
esac
[ -f "$RPM_FILE" ] || {
    echo "check-rpm: RPM 包不存在: $RPM_FILE" >&2
    exit 1
}

command -v rpm >/dev/null 2>&1 || {
    echo "check-rpm: 缺少 rpm" >&2
    exit 1
}
command -v rpm2cpio >/dev/null 2>&1 || {
    echo "check-rpm: 缺少 rpm2cpio" >&2
    exit 1
}

WORK_DIR=$(mktemp -d "${TMPDIR:-/tmp}/zhsh-check-rpm.XXXXXX")
cleanup() {
    rm -rf "$WORK_DIR"
}
trap cleanup EXIT HUP INT TERM

VERSION=$(python3 -c \
    'import pathlib,tomllib; print(tomllib.loads(pathlib.Path("zhsh/Cargo.toml").read_text())["package"]["version"])')
EXPECTED_FILE="zhsh-${VERSION}-1.x86_64.rpm"
[ "$(basename "$RPM_FILE")" = "$EXPECTED_FILE" ] || {
    echo "check-rpm: 文件名应为 $EXPECTED_FILE" >&2
    exit 1
}

IDENTITY=$(rpm -qp --queryformat '%{NAME}\n%{VERSION}\n%{RELEASE}\n%{ARCH}\n%{LICENSE}\n' "$RPM_FILE")
EXPECTED_IDENTITY=$(printf 'zhsh\n%s\n1\nx86_64\nGPL-3.0-or-later' "$VERSION")
[ "$IDENTITY" = "$EXPECTED_IDENTITY" ] || {
    echo "check-rpm: 包身份不匹配" >&2
    printf '%s\n' "$IDENTITY" >&2
    exit 1
}

rpm -qp --requires "$RPM_FILE" >"$WORK_DIR/requires.txt"
grep -qx 'bash >= 5.2' "$WORK_DIR/requires.txt" || {
    echo "check-rpm: 缺少 bash >= 5.2 依赖" >&2
    exit 1
}
grep -qx 'ca-certificates' "$WORK_DIR/requires.txt" || {
    echo "check-rpm: 缺少 ca-certificates 依赖" >&2
    exit 1
}

mkdir -p "$WORK_DIR/rpm-root" "$WORK_DIR/install-root"
(cd "$WORK_DIR/rpm-root" && rpm2cpio "$RPM_FILE" | cpio -idm --quiet)
make install DESTDIR="$WORK_DIR/install-root"

find "$WORK_DIR/install-root" -type f -printf '%P\n' | sort >"$WORK_DIR/install-files.txt"
find "$WORK_DIR/rpm-root" -type f -printf '%P\n' | sort >"$WORK_DIR/rpm-files.txt"
cmp "$WORK_DIR/install-files.txt" "$WORK_DIR/rpm-files.txt" || {
    echo "check-rpm: RPM 文件清单与 make install 不一致" >&2
    exit 1
}

while IFS= read -r relative; do
    source_file="$WORK_DIR/install-root/$relative"
    packaged_file="$WORK_DIR/rpm-root/$relative"
    cmp "$source_file" "$packaged_file"
    [ "$(stat -c '%a' "$source_file")" = "$(stat -c '%a' "$packaged_file")" ] || {
        echo "check-rpm: 文件权限不一致: $relative" >&2
        exit 1
    }
done <"$WORK_DIR/install-files.txt"

scripts/check-release-metadata.sh "$WORK_DIR/rpm-root/usr/bin/zhsh"
echo "check-rpm: 包身份、依赖、内容与安装布局检查通过"
