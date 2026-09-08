#!/bin/sh
# 先校验发布版本身份，再复用完整 release-check 生成 tag 制品。

set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)
REPO_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)
EXPECTED_ROOT=$(git -C "$REPO_ROOT" rev-parse --show-toplevel)
if [ "$REPO_ROOT" != "$EXPECTED_ROOT" ]; then
    echo "release-tag: 脚本不在仓库根目录下" >&2
    exit 2
fi
cd "$REPO_ROOT"

WORK_DIR=$(mktemp -d "${TMPDIR:-/tmp}/zhsh-release-tag.XXXXXX")
cleanup() {
    rm -rf "$WORK_DIR"
}
trap cleanup EXIT HUP INT TERM

if [ -n "$(git status --porcelain --untracked-files=all)" ]; then
    echo "release-tag: tag 发布必须来自干净工作树" >&2
    exit 1
fi

VERSION=$(python3 -c \
    'import pathlib,tomllib; d=tomllib.loads(pathlib.Path("zhsh/Cargo.toml").read_text()); print(d["package"]["version"])')
EXPECTED_TAG="v$VERSION"
HEAD_COMMIT=$(git rev-parse HEAD)

TAGS=$(git tag --points-at "$HEAD_COMMIT" --list 'v*')
if [ "$TAGS" != "$EXPECTED_TAG" ]; then
    echo "release-tag: HEAD 必须且只能带有版本 tag $EXPECTED_TAG；实际为: ${TAGS:-无}" >&2
    exit 1
fi

TAG_COMMIT=$(git rev-list -n 1 "$EXPECTED_TAG")
if [ "$TAG_COMMIT" != "$HEAD_COMMIT" ]; then
    echo "release-tag: $EXPECTED_TAG 未指向当前 commit" >&2
    exit 1
fi

# 发布身份先失败关闭，再执行一次完整源码与制品门禁。
make release-check

DEB_FILE="target/release-artifacts/zhsh_${VERSION}-1_amd64.deb"
RPM_FILE="target/release-artifacts/zhsh-${VERSION}-1.x86_64.rpm"
if [ ! -f "$DEB_FILE" ]; then
    echo "release-tag: 缺少版本对应的 Debian 制品: $DEB_FILE" >&2
    exit 1
fi
if [ ! -f "$RPM_FILE" ]; then
    echo "release-tag: 缺少版本对应的 RPM 制品: $RPM_FILE" >&2
    exit 1
fi
mkdir -p "$WORK_DIR/deb-root"
dpkg-deb --extract "$DEB_FILE" "$WORK_DIR/deb-root"
scripts/check-release-metadata.sh "$WORK_DIR/deb-root/usr/bin/zhsh" "$DEB_FILE"

ARCHIVE="target/release-artifacts/zhsh-${VERSION}.tar.gz"
git archive --format=tar.gz --prefix="zhsh-${VERSION}/" \
    --output="$ARCHIVE" "$EXPECTED_TAG"
python3 scripts/release_notes.py \
    "$VERSION" "target/release-artifacts/RELEASE_NOTES.md"
(
    cd target/release-artifacts
    sha256sum "$(basename "$DEB_FILE")" "$(basename "$RPM_FILE")" \
        "$(basename "$ARCHIVE")" >SHA256SUMS
)
