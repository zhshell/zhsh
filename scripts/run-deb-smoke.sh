#!/bin/sh
# 只在 packaging/release/smoke.Dockerfile 构建的全新安装容器中运行。

set -eu

WORK_DIR=$(mktemp -d /tmp/zhsh-installed-smoke.XXXXXX)
chown zhsmoke:zhsmoke "$WORK_DIR"
chmod 0700 "$WORK_DIR"
cleanup() {
    if command -v pkill >/dev/null 2>&1; then
        pkill -f 'sleep 29[.]731' >/dev/null 2>&1 || true
    fi
    rm -rf "$WORK_DIR"
}
trap cleanup EXIT HUP INT TERM

fail() {
    echo "deb-smoke: $*" >&2
    exit 1
}

[ "$(id -u)" -eq 0 ] || fail "容器安装检查必须以 root 启动"
[ -x /usr/bin/zhsh ] || fail "缺少 /usr/bin/zhsh"
[ "$(stat -c '%U:%G:%a' /usr/bin/zhsh)" = "root:root:755" ] ||
    fail "binary 所有权或权限错误"
[ "$(stat -c '%U:%G:%a' /usr/lib/zhsh/plugins/llm)" = "root:root:755" ] ||
    fail "Codec 目录所有权或权限错误"
[ ! -e /usr/lib/zhsh/plugins/safety ] ||
    fail "zhsh 软件包不得创建系统级 Safety 规则目录"

codec_count=$(find /usr/lib/zhsh/plugins/llm -maxdepth 1 -type f -name '*.zhcodec' | wc -l)
[ "$codec_count" -eq 2 ] || fail "官方 Codec 应为 2 个，实际为 $codec_count"
find /usr/lib/zhsh/plugins/llm -maxdepth 1 -type f -name '*.zhcodec' -exec sh -c '
    for file do
        [ "$(stat -c "%U:%G:%a" "$file")" = "root:root:644" ] || exit 1
    done
' sh {} + || fail "Codec 所有权或权限错误"
[ -f /usr/share/doc/zhsh/README.md ] || fail "包内缺少 README"
[ -f /usr/share/doc/zhsh/LICENSE ] || fail "包内缺少 LICENSE"
[ -f /usr/share/man/man1/zhsh.1.gz ] || fail "包内缺少压缩 man page"

ASCII_HOME="$WORK_DIR/ascii-home"
install -d -o zhsmoke -g zhsmoke -m700 "$ASCII_HOME"
ascii_output=$(printf '%s\n' "printf 'SHELL_SMOKE_OK\\n'" exit |
    timeout 15 runuser -u zhsmoke -- env -i \
        HOME="$ASCII_HOME" PATH=/usr/bin:/bin TERM=dumb LC_ALL=C.UTF-8 \
        /usr/bin/zhsh 2>&1) || fail "ASCII Shell 冒烟失败"
printf '%s\n' "$ascii_output" | grep -q SHELL_SMOKE_OK || fail "ASCII Shell 未输出标记"
if [ -d "$ASCII_HOME/.zhsh/plugins/llm" ]; then
    [ -z "$(find "$ASCII_HOME/.zhsh/plugins/llm" -type f -print -quit)" ] ||
        fail "测试 HOME 不得复制官方 Codec"
fi

INVALID_CWD="$WORK_DIR/invalid-home"
install -d -o zhsmoke -g zhsmoke -m700 "$INVALID_CWD"
printf '%s\n' "touch \"$INVALID_CWD/rc-ran\"" >"$INVALID_CWD/.zhshrc"
printf '%s\n' "touch \"$INVALID_CWD/bashrc-ran\"" >"$INVALID_CWD/.bashrc"
chmod 644 "$INVALID_CWD/.zhshrc" "$INVALID_CWD/.bashrc"

run_invalid_home() {
    mode=$1
    case "$mode" in
        missing)
            command='env -i PATH=/usr/bin:/bin TERM=dumb LC_ALL=C.UTF-8 /usr/bin/zhsh'
            ;;
        empty)
            command='env -i HOME= PATH=/usr/bin:/bin TERM=dumb LC_ALL=C.UTF-8 /usr/bin/zhsh'
            ;;
        relative)
            command='env -i HOME=. PATH=/usr/bin:/bin TERM=dumb LC_ALL=C.UTF-8 /usr/bin/zhsh'
            ;;
        nonutf8)
            command='bad_home=$(printf "\377"); env -i HOME="$bad_home" PATH=/usr/bin:/bin TERM=dumb LC_ALL=C.UTF-8 /usr/bin/zhsh'
            ;;
        *) fail "未知 HOME smoke: $mode" ;;
    esac
    output=$(cd "$INVALID_CWD" && printf '%s\n' pwd exit |
        timeout 15 runuser -u zhsmoke -- sh -c "$command" 2>&1) ||
        fail "$mode HOME 下普通 Shell 不可用"
    printf '%s\n' "$output" | grep -q "$INVALID_CWD" || fail "$mode HOME 下 pwd 未执行"
}

run_invalid_home missing
run_invalid_home empty
run_invalid_home relative
run_invalid_home nonutf8
[ ! -e "$INVALID_CWD/rc-ran" ] || fail "非法 HOME 加载了 cwd/.zhshrc"
[ ! -e "$INVALID_CWD/bashrc-ran" ] || fail "非法 HOME 间接加载了 cwd/.bashrc"
[ ! -e "$INVALID_CWD/.zhsh" ] || fail "非法 HOME 创建了 cwd/.zhsh"
[ ! -e "$INVALID_CWD/.zh_history" ] || fail "非法 HOME 创建了 cwd/.zh_history"

BROKEN_HOME="$WORK_DIR/broken-codec-home"
install -d -o zhsmoke -g zhsmoke -m700 "$BROKEN_HOME/.zhsh/plugins/llm"
printf 'not-a-codec\n' >"$BROKEN_HOME/.zhsh/plugins/llm/broken.zhcodec"
chown -R zhsmoke:zhsmoke "$BROKEN_HOME"
chmod 700 "$BROKEN_HOME" "$BROKEN_HOME/.zhsh" "$BROKEN_HOME/.zhsh/plugins" \
    "$BROKEN_HOME/.zhsh/plugins/llm"
chmod 600 "$BROKEN_HOME/.zhsh/plugins/llm/broken.zhcodec"
broken_output=$(printf '%s\n' "printf 'BROKEN_CODEC_SHELL_OK\\n'" exit |
    timeout 15 runuser -u zhsmoke -- env -i \
        HOME="$BROKEN_HOME" PATH=/usr/bin:/bin TERM=dumb LC_ALL=C.UTF-8 \
        /usr/bin/zhsh 2>&1) || fail "无关坏 Codec 阻止了 Shell"
printf '%s\n' "$broken_output" | grep -q BROKEN_CODEC_SHELL_OK ||
    fail "坏 Codec Shell 标记缺失"

ACTIVE_HOME="$WORK_DIR/broken-active-home"
install -d -o zhsmoke -g zhsmoke -m700 "$ACTIVE_HOME/.zhsh/llm"
printf '%s\n' \
    'NAME=broken' \
    'URL=http://127.0.0.1:9' \
    'FORMAT=missing@1.0.0' \
    'ACCESS_TOKEN=not-a-real-token' \
    'FLASH=missing' \
    'STANDARD=missing' \
    'MAX=missing' \
    'TIER=flash' >"$ACTIVE_HOME/.zhsh/llm/broken.llm"
printf 'broken\n' >"$ACTIVE_HOME/.zhsh/active-llm"
chown -R zhsmoke:zhsmoke "$ACTIVE_HOME"
chmod 700 "$ACTIVE_HOME" "$ACTIVE_HOME/.zhsh" "$ACTIVE_HOME/.zhsh/llm"
chmod 600 "$ACTIVE_HOME/.zhsh/llm/broken.llm" "$ACTIVE_HOME/.zhsh/active-llm"
active_output=$(printf '%s\n' "printf 'BROKEN_ACTIVE_SHELL_OK\\n'" exit |
    timeout 15 runuser -u zhsmoke -- env -i \
        HOME="$ACTIVE_HOME" PATH=/usr/bin:/bin TERM=dumb LC_ALL=C.UTF-8 \
        /usr/bin/zhsh 2>&1) || fail "损坏 active Codec 阻止了 Shell"
printf '%s\n' "$active_output" | grep -q BROKEN_ACTIVE_SHELL_OK ||
    fail "损坏 active Shell 标记缺失"

run_mock() {
    provider=$1
    scenario=$2
    home="$WORK_DIR/mock-$provider-$scenario"
    timeout 35 runuser -u zhsmoke -- env -i \
        HOME=/home/zhsmoke PATH=/usr/bin:/bin TERM=xterm-256color LC_ALL=C.UTF-8 \
        python3 /input/mock-llm.py \
            --binary /usr/bin/zhsh \
            --home "$home" \
            --provider "$provider" \
            --scenario "$scenario"
}

openai_output=$(run_mock openai success) || fail "OpenAI PTY/loopback Mock 冒烟失败"
printf '%s\n' "$openai_output" | grep -q AGENT_SMOKE_OK || fail "OpenAI Agent 标记缺失"
printf '%s\n' "$openai_output" | grep -Eq '[1-6]轮( 澄清 [1-3]次)? [0-9]+\.[0-9]s' ||
    fail "OpenAI Agent 轮次摘要错误"

anthropic_output=$(run_mock anthropic success) || fail "Anthropic PTY/loopback Mock 冒烟失败"
printf '%s\n' "$anthropic_output" | grep -q ANTHROPIC_SMOKE_OK ||
    fail "Anthropic Agent 标记缺失"

shadow_output=$(run_mock openai path-shadow) || fail "PATH shadow 安装后回归失败"
printf '%s\n' "$shadow_output" | grep -q '? 执行？\[y/N\]' || fail "PATH shadow 未触发确认"
printf '%s\n' "$shadow_output" | grep -q '用户拒绝' || fail "PATH shadow 拒绝结果缺失"

background_output=$(run_mock openai background) || fail "Agent 后台命令安装后回归失败"
printf '%s\n' "$background_output" | grep -q BACKGROUND_SMOKE_OK || fail "后台拒绝标记缺失"
if pgrep -f 'sleep 29[.]731' >/dev/null 2>&1; then
    fail "Agent 后台 smoke 留下了 sleep 进程"
fi

echo "deb-smoke: 全新 HOME、ASCII、OpenAI、Anthropic、安全与失效域冒烟通过"
