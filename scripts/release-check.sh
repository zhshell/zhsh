#!/bin/sh
# zhsh prototype 的唯一源码与制品发布门禁。

set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)
REPO_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)
EXPECTED_ROOT=$(git -C "$REPO_ROOT" rev-parse --show-toplevel)
if [ "$REPO_ROOT" != "$EXPECTED_ROOT" ]; then
    echo "release-check: 脚本不在仓库根目录下" >&2
    exit 2
fi
cd "$REPO_ROOT"

WORK_DIR=$(mktemp -d "${TMPDIR:-/tmp}/zhsh-release-check.XXXXXX")
TOOL_IMAGE=
RUNNER_IMAGE=
RUNNER_CONTAINER=
cleanup() {
    if [ -n "$RUNNER_CONTAINER" ]; then
        docker rm -f "$RUNNER_CONTAINER" >/dev/null 2>&1 || true
    fi
    if [ -n "$RUNNER_IMAGE" ]; then
        docker image rm "$RUNNER_IMAGE" >/dev/null 2>&1 || true
    fi
    if [ -n "$TOOL_IMAGE" ]; then
        docker image rm "$TOOL_IMAGE" >/dev/null 2>&1 || true
    fi
    rm -rf "$WORK_DIR"
}
trap cleanup EXIT HUP INT TERM

is_dirty() {
    [ -n "$(git status --porcelain --untracked-files=all)" ]
}

if is_dirty && [ "${ZHSH_RELEASE_ALLOW_DIRTY:-0}" != 1 ]; then
    echo "release-check: 工作树不干净；发布门禁默认拒绝未提交修改" >&2
    echo "release-check: 仅本地开发验证可显式设置 ZHSH_RELEASE_ALLOW_DIRTY=1" >&2
    exit 1
fi

if [ "${ZHSH_RELEASE_IN_CONTAINER:-0}" != 1 ]; then
    command -v docker >/dev/null 2>&1 || {
        echo "release-check: 需要 Docker 构建固定 Ubuntu 24.04 发布环境" >&2
        exit 1
    }

    SOURCE_DATE_EPOCH=${SOURCE_DATE_EPOCH:-$(git show -s --format=%ct HEAD)}
    case "$SOURCE_DATE_EPOCH" in
        '' | *[!0-9]*)
            echo "release-check: SOURCE_DATE_EPOCH 必须是十进制 Unix 时间戳" >&2
            exit 2
            ;;
    esac

    mkdir -p "$WORK_DIR/toolchain" "$WORK_DIR/runner/source"
    cp packaging/release/Dockerfile "$WORK_DIR/toolchain/Dockerfile"

    SUFFIX=$$
    TOOL_IMAGE="zhsh-release-toolchain:$SUFFIX"
    RUNNER_IMAGE="zhsh-release-runner:$SUFFIX"
    RUNNER_CONTAINER="zhsh-release-check-$SUFFIX"

    docker build --platform linux/amd64 --pull=false --tag "$TOOL_IMAGE" "$WORK_DIR/toolchain"

    git clone --quiet --no-local --no-hardlinks "$REPO_ROOT" "$WORK_DIR/runner/source"
    git -C "$WORK_DIR/runner/source" checkout --quiet --detach HEAD
    if is_dirty; then
        {
            git diff --name-only --diff-filter=ACMRTUXB -z HEAD
            git diff --cached --name-only --diff-filter=ACMRTUXB -z HEAD
            git ls-files --others --exclude-standard -z
        } |
            tar --null -T - -cf - |
            tar -xf - -C "$WORK_DIR/runner/source"
        {
            git diff --name-only --diff-filter=D -z HEAD
            git diff --cached --name-only --diff-filter=D -z HEAD
        } |
            (cd "$WORK_DIR/runner/source" && xargs -0 -r rm -f --)
    fi

    docker build \
        --platform linux/amd64 \
        --build-arg RELEASE_TOOL_IMAGE="$TOOL_IMAGE" \
        --file packaging/release/runner.Dockerfile \
        --tag "$RUNNER_IMAGE" \
        "$WORK_DIR/runner"

    docker run --name "$RUNNER_CONTAINER" \
        --platform linux/amd64 \
        --network bridge \
        --env HOME=/tmp/zhsh-release-home \
        --env LC_ALL=C.UTF-8 \
        --env TZ=UTC \
        --env SOURCE_DATE_EPOCH="$SOURCE_DATE_EPOCH" \
        --env CARGO_INCREMENTAL=0 \
        --env ZHSH_RELEASE_IN_CONTAINER=1 \
        --env ZHSH_RELEASE_ALLOW_DIRTY="${ZHSH_RELEASE_ALLOW_DIRTY:-0}" \
        "$RUNNER_IMAGE" \
        /workspace/scripts/release-check.sh

    FRESH_ARTIFACT_DIR="$WORK_DIR/artifacts"
    mkdir -p "$FRESH_ARTIFACT_DIR"
    docker cp "$RUNNER_CONTAINER:/artifacts/." "$FRESH_ARTIFACT_DIR/"

    DEB_COUNT=$(find "$FRESH_ARTIFACT_DIR" -maxdepth 1 -type f -name 'zhsh_*.deb' | wc -l)
    RPM_COUNT=$(find "$FRESH_ARTIFACT_DIR" -maxdepth 1 -type f -name 'zhsh-*.rpm' | wc -l)
    if [ "$DEB_COUNT" -ne 1 ] || [ "$RPM_COUNT" -ne 1 ]; then
        echo "release-check: 预期各一个 DEB/RPM，实际为 $DEB_COUNT/$RPM_COUNT" >&2
        exit 1
    fi
    DEB_FILE=$(find "$FRESH_ARTIFACT_DIR" -maxdepth 1 -type f -name 'zhsh_*.deb')
    RPM_FILE=$(find "$FRESH_ARTIFACT_DIR" -maxdepth 1 -type f -name 'zhsh-*.rpm')
    scripts/deb-smoke.sh "$DEB_FILE"
    scripts/rpm-smoke.sh "$RPM_FILE"
    (
        cd "$FRESH_ARTIFACT_DIR"
        sha256sum "$(basename "$DEB_FILE")" "$(basename "$RPM_FILE")" >SHA256SUMS
    )
    ARTIFACT_DIR="$REPO_ROOT/target/release-artifacts"
    rm -rf -- "$ARTIFACT_DIR"
    mkdir -p "$ARTIFACT_DIR"
    cp -a "$FRESH_ARTIFACT_DIR/." "$ARTIFACT_DIR/"
    exit 0
fi

# 以下逻辑只允许在固定发布镜像中运行。外层只传递显式白名单环境变量。
[ "$(id -u)" -ne 0 ] || {
    echo "release-check: 发布检查不得以 root 身份执行" >&2
    exit 1
}
mkdir -p "$HOME" /artifacts
chmod 700 "$HOME"
export CARGO_HOME="$HOME/.cargo"
export RUSTUP_HOME=/opt/rustup
export PATH=/opt/cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
export CARGO_NET_GIT_FETCH_WITH_CLI=false
unset HTTP_PROXY HTTPS_PROXY ALL_PROXY NO_PROXY http_proxy https_proxy all_proxy no_proxy || true
unset OPENAI_API_KEY ANTHROPIC_API_KEY OPENAI_BASE_URL ANTHROPIC_BASE_URL || true

if is_dirty && [ "${ZHSH_RELEASE_ALLOW_DIRTY:-0}" != 1 ]; then
    echo "release-check: 容器内源码快照不干净" >&2
    exit 1
fi

git diff --check
git diff --cached --check

require_version() {
    command_name=$1
    expected=$2
    actual=$($command_name --version | awk '{ print $2; exit }')
    if [ "$actual" != "$expected" ]; then
        echo "release-check: $command_name 版本应为 $expected，实际为 $actual" >&2
        exit 1
    fi
}

run_check() {
    cargo check --package zhsh --locked --all-targets --all-features
}

run_tests() {
    cargo test --package zhsh --locked --quiet
    cargo test --package zhsh --locked --doc --quiet
}

run_lint() {
    cargo fmt --all -- --check
    cargo clippy --package zhsh --locked --all-targets --all-features -- -D warnings
    RUSTDOCFLAGS='-D warnings' cargo doc \
        --package zhsh --locked --all-features --no-deps

    MAN_STDERR="$WORK_DIR/man.stderr"
    # GNU troff does not provide CJK line-breaking for unspaced Chinese prose. Suppress only its
    # `break` category; every other parser/typesetting diagnostic remains release-blocking.
    groff -Wbreak -man -Tutf8 packaging/man/zhsh.1 >"$WORK_DIR/zhsh.1.txt" 2>"$MAN_STDERR"
    if [ -s "$MAN_STDERR" ]; then
        echo "release-check: groff 产生未列入白名单的警告" >&2
        cat "$MAN_STDERR" >&2
        exit 1
    fi
}

run_audit() {
    require_version cargo-audit 0.22.0
    require_version cargo-deny 0.19.0
    cargo audit --deny warnings
    cargo deny check advisories bans licenses sources
    cargo tree --package zhsh --locked --duplicates > /artifacts/cargo-tree-duplicates.txt
}

check_package_list() {
    package=$1
    list_file=$2
    dirty_arg=
    if [ "${ZHSH_RELEASE_ALLOW_DIRTY:-0}" = 1 ]; then
        dirty_arg=--allow-dirty
    fi
    # shellcheck disable=SC2086
    cargo package --package "$package" --locked $dirty_arg --list >"$list_file"
    if grep -E '(^|/)(\.env|\.idea|\.claude|\.zhcodec-keys|docs)(/|$)|\.(pk8|pem|key)$' "$list_file"; then
        echo "release-check: $package 源码包包含禁止发布的内部或密钥文件" >&2
        exit 1
    fi
    if ! grep -qx 'LICENSE' "$list_file"; then
        echo "release-check: $package 源码包缺少 LICENSE" >&2
        exit 1
    fi
}

run_package() {
    require_version cargo-deb 3.7.0
    require_version cargo-generate-rpm 0.21.0
    dirty_arg=
    if [ "${ZHSH_RELEASE_ALLOW_DIRTY:-0}" = 1 ]; then
        dirty_arg=--allow-dirty
    fi

    check_package_list zhsh "$WORK_DIR/zhsh-package-files.txt"
    # shellcheck disable=SC2086
    cargo package --package zhsh --locked $dirty_arg

    cargo build --package zhsh --release --locked
    scripts/check-release-metadata.sh target/release/zhsh
    cargo deb --package zhsh --no-build --locked

    DEB_COUNT=$(find target/debian -maxdepth 1 -type f -name 'zhsh_*.deb' | wc -l)
    if [ "$DEB_COUNT" -ne 1 ]; then
        echo "release-check: cargo-deb 应生成恰好一个 zhsh 包，实际为 $DEB_COUNT" >&2
        exit 1
    fi
    DEB_FILE=$(find target/debian -maxdepth 1 -type f -name 'zhsh_*.deb')
    scripts/normalize-deb.sh "$DEB_FILE"
    scripts/check-deb.sh "$DEB_FILE"

    scripts/build-rpm.sh
    RPM_COUNT=$(find target/generate-rpm -maxdepth 1 -type f -name 'zhsh-*.rpm' | wc -l)
    if [ "$RPM_COUNT" -ne 1 ]; then
        echo "release-check: cargo-generate-rpm 应生成恰好一个 zhsh 包，实际为 $RPM_COUNT" >&2
        exit 1
    fi
    RPM_FILE=$(find target/generate-rpm -maxdepth 1 -type f -name 'zhsh-*.rpm')
    scripts/check-rpm.sh "$RPM_FILE"

    cp "$DEB_FILE" /artifacts/
    cp "$RPM_FILE" /artifacts/
    cp "$WORK_DIR/zhsh-package-files.txt" /artifacts/
}

require_version rustc 1.98.0
require_version cargo 1.98.0

run_check
run_tests
run_lint
run_audit
run_package
