#!/bin/sh
# 对 cargo-deb 制品执行静态、ABI、lintian 与源码安装树一致性检查。

set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)
REPO_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)
EXPECTED_ROOT=$(git -C "$REPO_ROOT" rev-parse --show-toplevel)
if [ "$REPO_ROOT" != "$EXPECTED_ROOT" ]; then
    echo "check-deb: 脚本不在仓库根目录下" >&2
    exit 2
fi
cd "$REPO_ROOT"

WORK_DIR=$(mktemp -d "${TMPDIR:-/tmp}/zhsh-check-deb.XXXXXX")
cleanup() {
    rm -rf "$WORK_DIR"
}
trap cleanup EXIT HUP INT TERM

if [ "$#" -ne 1 ]; then
    echo "用法: scripts/check-deb.sh /absolute/path/to/zhsh_VERSION-REVISION_amd64.deb" >&2
    exit 2
fi
case "$1" in
    /*) DEB_FILE=$1 ;;
    *) DEB_FILE="$REPO_ROOT/$1" ;;
esac
if [ ! -f "$DEB_FILE" ]; then
    echo "check-deb: Debian 包不存在: $DEB_FILE" >&2
    exit 1
fi

command -v dpkg-deb >/dev/null 2>&1 || {
    echo "check-deb: 缺少 dpkg-deb" >&2
    exit 1
}
command -v lintian >/dev/null 2>&1 || {
    echo "check-deb: 缺少 lintian，不能跳过 Debian 策略检查" >&2
    exit 1
}

dpkg-deb --info "$DEB_FILE" >"$WORK_DIR/deb-info.txt"
dpkg-deb --contents "$DEB_FILE" >"$WORK_DIR/deb-contents.txt"
dpkg-deb --extract "$DEB_FILE" "$WORK_DIR/deb-root"

python3 "$SCRIPT_DIR/deb_manifest_check.py" "$DEB_FILE" "$WORK_DIR/deb-contents.txt"

scripts/check-release-metadata.sh "$WORK_DIR/deb-root/usr/bin/zhsh" "$DEB_FILE"

MAX_GLIBC=$(readelf --version-info "$WORK_DIR/deb-root/usr/bin/zhsh" |
    grep -o 'GLIBC_[0-9][0-9.]*' |
    sed 's/^GLIBC_//' |
    sort -Vu |
    tail -n 1)
if [ -z "$MAX_GLIBC" ]; then
    echo "check-deb: 无法从 ELF 提取 GLIBC symbol version" >&2
    exit 1
fi
if ! awk -v version="$MAX_GLIBC" 'BEGIN {
    split(version, part, ".");
    exit !((part[1] + 0 < 2) || (part[1] + 0 == 2 && part[2] + 0 <= 39));
}'; then
    echo "check-deb: ELF 需要 GLIBC_$MAX_GLIBC，高于支持上限 GLIBC_2.39" >&2
    exit 1
fi

# 此包发布到 GitHub prototype channel，而不是 Debian archive；它没有可合法引用的
# Debian BTS 初始上传 bug。仅抑制这一精确 tag，其余 warning/error 仍失败关闭。
if ! lintian --fail-on error,warning \
    --suppress-tags initial-upload-closes-no-bugs \
    "$DEB_FILE" >"$WORK_DIR/lintian.txt" 2>&1; then
    cat "$WORK_DIR/lintian.txt" >&2
    echo "check-deb: lintian error/warning 不允许忽略" >&2
    exit 1
fi

mkdir -p "$WORK_DIR/install-root"
make install DESTDIR="$WORK_DIR/install-root"

find "$WORK_DIR/install-root" -type f -printf '%P\n' | sort >"$WORK_DIR/install-files.txt"
while IFS= read -r relative; do
    source_file="$WORK_DIR/install-root/$relative"
    case "$relative" in
        usr/share/man/man1/zhsh.1)
            packaged="$WORK_DIR/deb-root/${relative}.gz"
            gzip -cd "$packaged" >"$WORK_DIR/uncompressed-man"
            cmp "$source_file" "$WORK_DIR/uncompressed-man"
            ;;
        *)
            packaged="$WORK_DIR/deb-root/$relative"
            [ -f "$packaged" ] || {
                echo "check-deb: .deb 缺少 make install 文件: $relative" >&2
                exit 1
            }
            cmp "$source_file" "$packaged"
            ;;
    esac
    source_mode=$(stat -c '%a' "$source_file")
    packaged_mode=$(stat -c '%a' "$packaged")
    if [ "$source_mode" != "$packaged_mode" ]; then
        echo "check-deb: make install 与 .deb 权限不一致: $relative" >&2
        exit 1
    fi
done <"$WORK_DIR/install-files.txt"

echo "check-deb: 包清单、权限、GLIBC_$MAX_GLIBC、lintian 与安装树检查通过"
