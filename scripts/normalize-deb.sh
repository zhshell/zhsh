#!/bin/sh
# 将 cargo-deb 的通用产物收敛为通过 Debian 策略检查的可发布包。

set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)
REPO_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)
EXPECTED_ROOT=$(git -C "$REPO_ROOT" rev-parse --show-toplevel)
if [ "$REPO_ROOT" != "$EXPECTED_ROOT" ]; then
    echo "normalize-deb: 脚本不在仓库根目录下" >&2
    exit 2
fi
cd "$REPO_ROOT"

WORK_DIR=$(mktemp -d "${TMPDIR:-/tmp}/zhsh-normalize-deb.XXXXXX")
cleanup() {
    rm -rf "$WORK_DIR"
}
trap cleanup EXIT HUP INT TERM

if [ "$#" -ne 1 ]; then
    echo "用法: scripts/normalize-deb.sh /absolute/path/to/zhsh_VERSION-REVISION_amd64.deb" >&2
    exit 2
fi
case "$1" in
    /*) DEB_FILE=$1 ;;
    *) DEB_FILE="$REPO_ROOT/$1" ;;
esac
if [ ! -f "$DEB_FILE" ]; then
    echo "normalize-deb: Debian 包不存在: $DEB_FILE" >&2
    exit 1
fi

SOURCE_DATE_EPOCH=${SOURCE_DATE_EPOCH:-$(git show -s --format=%ct HEAD)}
case "$SOURCE_DATE_EPOCH" in
    '' | *[!0-9]*)
        echo "normalize-deb: SOURCE_DATE_EPOCH 必须是十进制 Unix 时间戳" >&2
        exit 2
        ;;
esac

DEB_ROOT="$WORK_DIR/deb-root"
CONTROL="$DEB_ROOT/DEBIAN/control"
dpkg-deb --raw-extract "$DEB_FILE" "$DEB_ROOT"

if ! grep -Eq '^Depends: bash,([[:space:]]|$)' "$CONTROL"; then
    echo "normalize-deb: cargo-deb Depends 不以未版本化 bash 开头，拒绝猜测修改" >&2
    exit 1
fi
sed 's/^Depends: bash,/Depends: bash (>= 5.2),/' "$CONTROL" >"$WORK_DIR/control"
mv "$WORK_DIR/control" "$CONTROL"
chmod 0644 "$CONTROL"

DOC_DIR="$DEB_ROOT/usr/share/doc/zhsh"
mkdir -p "$DOC_DIR"
install -m 0644 packaging/release/copyright "$DOC_DIR/copyright"

VERSION=$(dpkg-deb --field "$DEB_FILE" Version)
CHANGELOG_DATE=$(date --utc --date="@$SOURCE_DATE_EPOCH" --rfc-email)
{
    printf 'zhsh (%s) unstable; urgency=medium\n\n' "$VERSION"
    printf '  * Prototype release.\n\n'
    printf ' -- jungle <junglelk@foxmail.com>  %s\n' "$CHANGELOG_DATE"
} >"$WORK_DIR/changelog.Debian"
gzip -9 -n -c "$WORK_DIR/changelog.Debian" >"$DOC_DIR/changelog.Debian.gz"
chmod 0644 "$DOC_DIR/changelog.Debian.gz"

find "$DEB_ROOT" -exec touch -h --date="@$SOURCE_DATE_EPOCH" {} +
NEW_DEB="$WORK_DIR/$(basename "$DEB_FILE")"
dpkg-deb --root-owner-group --build "$DEB_ROOT" "$NEW_DEB" >/dev/null
mv "$NEW_DEB" "$DEB_FILE"

echo "normalize-deb: 已写入版本化 bash 依赖、Debian copyright 与 changelog"
