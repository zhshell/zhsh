#!/bin/sh
# Fail closed when the GitHub-facing snapshot contains private design or local state.

set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)
REPO_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)
cd "$REPO_ROOT"

if [ ! -f .public-repository ]; then
    echo "public-boundary: 缺少 .public-repository 标记；不得从私有工作树发布" >&2
    exit 1
fi

for required in Cargo.toml Cargo.lock README.md LICENSE SECURITY.md CHANGELOG.md \
    CONTRIBUTING.md Makefile packaging/man/zhsh.1 zhsh/Cargo.toml; do
    if [ ! -e "$required" ]; then
        echo "public-boundary: 缺少公开构建必需文件: $required" >&2
        exit 1
    fi
done

for forbidden in docs AGENTS.md .claude .idea .zhcodec-keys target; do
    if git rev-parse --is-inside-work-tree >/dev/null 2>&1; then
        if git ls-files -- "$forbidden" "$forbidden/**" | grep -q .; then
            echo "public-boundary: Git 跟踪了禁止公开的路径: $forbidden" >&2
            exit 1
        fi
    elif [ -e "$forbidden" ]; then
        echo "public-boundary: 快照包含禁止公开的路径: $forbidden" >&2
        exit 1
    fi
done

if git rev-parse --is-inside-work-tree >/dev/null 2>&1; then
    if git ls-files | grep -E '(^|/)(\.env|[^/]+\.(pk8|pem|key))$' >/dev/null; then
        echo "public-boundary: Git 跟踪了私钥或环境凭据文件" >&2
        exit 1
    fi
    if git grep -n -I -E \
        '(^|[^A-Za-z])(-----BEGIN ([A-Z ]+ )?PRIVATE KEY-----|gh[pousr]_[A-Za-z0-9_]{30,}|sk-[A-Za-z0-9_-]{24,})' \
        -- . ':(exclude)scripts/check-public-boundary.sh'; then
        echo "public-boundary: Git 跟踪内容包含疑似私钥或高置信度凭据" >&2
        exit 1
    fi
else
    if find . -type f \
        \( -name '*.pk8' -o -name '*.pem' -o -name '*.key' -o -name '.env' \) \
        -print | grep -q .; then
        echo "public-boundary: 快照包含私钥或环境凭据文件" >&2
        exit 1
    fi
    if grep -RInE --exclude='*.zhcodec' --exclude='check-public-boundary.sh' \
        '(^|[^A-Za-z])(-----BEGIN ([A-Z ]+ )?PRIVATE KEY-----|gh[pousr]_[A-Za-z0-9_]{30,}|sk-[A-Za-z0-9_-]{24,})' .; then
        echo "public-boundary: 快照包含疑似私钥或高置信度凭据" >&2
        exit 1
    fi
fi

echo "public-boundary: 公开快照边界有效"
