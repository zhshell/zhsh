#!/usr/bin/env python3
"""安装后 Agent 冒烟使用的严格 loopback Mock 与 PTY 驱动器。"""

from __future__ import annotations

import argparse
import fcntl
import http.server
import json
import os
import pathlib
import re
import select
import shutil
import subprocess
import termios
import threading
import time
from dataclasses import dataclass, field
from typing import Any


ANSI = re.compile(r"\x1b\[[0-?]*[ -/]*[@-~]")


@dataclass
class MockState:
    provider: str
    scenario: str
    token: str
    model: str
    requests: list[dict[str, Any]] = field(default_factory=list)
    errors: list[str] = field(default_factory=list)

    def response(self, index: int) -> str:
        if self.scenario == "success" and self.provider == "anthropic":
            if index != 0:
                raise AssertionError("Anthropic smoke 只应发起一轮")
            return json.dumps(
                {"response": {"action": "done", "answer": "ANTHROPIC_SMOKE_OK"}},
                ensure_ascii=False,
            )
        if self.scenario == "success":
            responses = [
                {
                    "action": "run",
                    "purpose": "验证命令反馈中的 token 脱敏",
                    "command": "printenv ZHSH_SMOKE_SECRET",
                },
                {"action": "done", "answer": "AGENT_SMOKE_OK"},
            ]
        elif self.scenario == "path-shadow":
            responses = [
                {
                    "action": "run",
                    "purpose": "验证 PATH shadow 必须确认",
                    "command": "ls",
                }
            ]
        elif self.scenario == "background":
            responses = [
                {
                    "action": "run",
                    "purpose": "验证 Agent 拒绝后台执行",
                    "command": "sleep 29.731 &",
                },
                {"action": "done", "answer": "BACKGROUND_SMOKE_OK"},
            ]
        else:
            raise AssertionError(f"未知 smoke 场景: {self.scenario}")
        if index >= len(responses):
            raise AssertionError(f"收到意外的第 {index + 1} 个请求")
        return json.dumps({"response": responses[index]}, ensure_ascii=False)

    @property
    def expected_answer(self) -> str:
        if self.scenario == "background":
            return "BACKGROUND_SMOKE_OK"
        if self.provider == "anthropic":
            return "ANTHROPIC_SMOKE_OK"
        return "AGENT_SMOKE_OK"

    @property
    def expected_requests(self) -> int:
        if self.scenario == "path-shadow":
            return 1
        return 1 if self.scenario == "success" and self.provider == "anthropic" else 2

    def validate_request(self, path: str, headers: http.client.HTTPMessage, body: bytes) -> None:
        expected_path = "/v1/responses" if self.provider == "openai" else "/v1/messages"
        if path != expected_path:
            raise AssertionError(f"请求路径应为 {expected_path}，实际为 {path}")
        if self.token.encode() in body:
            raise AssertionError("access token 泄漏到 JSON 请求正文")
        payload = json.loads(body)
        if payload.get("model") != self.model:
            raise AssertionError(f"模型不匹配: {payload.get('model')!r}")
        if self.provider == "openai":
            if headers.get("Authorization") != f"Bearer {self.token}":
                raise AssertionError("OpenAI Authorization 不匹配")
            if not isinstance(payload.get("input"), list) or not payload["input"]:
                raise AssertionError("OpenAI Responses 请求缺少 input")
            if not isinstance(payload.get("instructions"), str):
                raise AssertionError("OpenAI Responses 请求缺少 instructions")
            if payload.get("reasoning") != {"effort": "none"}:
                raise AssertionError("OpenAI reasoning.effort 必须显式为 none")
            if payload.get("store") is not False:
                raise AssertionError("OpenAI Responses 必须显式 store=false")
            output_format = payload.get("text", {}).get("format", {})
            if (
                output_format.get("type") != "json_schema"
                or output_format.get("strict") is not True
                or output_format.get("schema", {}).get("properties", {}).get("response")
                is None
            ):
                raise AssertionError("OpenAI 请求缺少 zhsh Structured Outputs schema")
        else:
            if not isinstance(payload.get("messages"), list) or not payload["messages"]:
                raise AssertionError("Anthropic 请求缺少 messages")
            if headers.get("x-api-key") != self.token:
                raise AssertionError("Anthropic x-api-key 不匹配")
            if headers.get("anthropic-version") != "2023-06-01":
                raise AssertionError("Anthropic 版本头不匹配")
            if payload.get("thinking") != {"type": "disabled"}:
                raise AssertionError("Anthropic thinking 必须显式 disabled")
            output_format = payload.get("output_config", {}).get("format", {})
            if (
                output_format.get("type") != "json_schema"
                or output_format.get("schema", {}).get("properties", {}).get("response")
                is None
            ):
                raise AssertionError("Anthropic 请求缺少 zhsh Structured Outputs schema")


class MockHandler(http.server.BaseHTTPRequestHandler):
    server: "MockServer"

    def do_POST(self) -> None:  # noqa: N802 - BaseHTTPRequestHandler API
        try:
            length = int(self.headers.get("Content-Length", "-1"))
            if length < 0 or length > 2 * 1024 * 1024:
                raise AssertionError(f"Content-Length 非法: {length}")
            body = self.rfile.read(length)
            self.server.state.validate_request(self.path, self.headers, body)
            payload = json.loads(body)
            self.server.state.requests.append(payload)
            model_output = self.server.state.response(len(self.server.state.requests) - 1)
            if self.server.state.provider == "openai":
                response = {
                    "status": "completed",
                    "output": [
                        {
                            "type": "message",
                            "role": "assistant",
                            "content": [
                                {"type": "output_text", "text": model_output}
                            ],
                        }
                    ],
                    "usage": {"input_tokens": 10, "output_tokens": 10},
                }
            else:
                response = {
                    "content": [{"type": "text", "text": model_output}],
                    "stop_reason": "end_turn",
                }
            encoded = json.dumps(response, ensure_ascii=False).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(encoded)))
            self.end_headers()
            self.wfile.write(encoded)
        except Exception as error:  # fail closed while still unblocking the client
            self.server.state.errors.append(str(error))
            encoded = json.dumps({"error": {"message": "mock assertion failed"}}).encode()
            self.send_response(500)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(encoded)))
            self.end_headers()
            self.wfile.write(encoded)

    def log_message(self, _format: str, *_args: object) -> None:
        return


class MockServer(http.server.ThreadingHTTPServer):
    state: MockState


def write_private(path: pathlib.Path, content: str) -> None:
    path.write_text(content, encoding="utf-8")
    path.chmod(0o600)


def configure_home(home: pathlib.Path, state: MockState, port: int) -> None:
    if home.exists():
        shutil.rmtree(home)
    config_dir = home / ".zhsh/llm"
    config_dir.mkdir(parents=True, mode=0o700)
    home.chmod(0o700)
    (home / ".zhsh").chmod(0o700)
    config_dir.chmod(0o700)
    codec = f"{state.provider}@0.3.0"
    config = (
        "NAME=smoke\n"
        f"URL=http://127.0.0.1:{port}\n"
        f"FORMAT={codec}\n"
        "JSON_SCHEMA=on\n"
        f"ACCESS_TOKEN={state.token}\n"
        f"FLASH={state.model}\n"
        f"STANDARD={state.model}\n"
        f"MAX={state.model}\n"
        "TIER=flash\n"
    )
    write_private(config_dir / "smoke.llm", config)
    write_private(home / ".zhsh/active-llm", "smoke\n")

    if state.scenario == "path-shadow":
        bin_dir = home / "bin"
        bin_dir.mkdir(mode=0o700)
        shadow = bin_dir / "ls"
        write_private(
            shadow,
            "#!/bin/sh\ntouch \"$HOME/shadow-ran\"\nprintf 'SHADOW_RAN\\n'\n",
        )
        shadow.chmod(0o700)


def child_session() -> None:
    os.setsid()
    fcntl.ioctl(0, termios.TIOCSCTTY, 0)


def read_until(master: int, output: bytearray, predicate: Any, timeout: float) -> None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate(bytes(output)):
            return
        readable, _, _ = select.select([master], [], [], 0.1)
        if not readable:
            continue
        try:
            chunk = os.read(master, 65536)
        except OSError as error:
            if error.errno == 5:  # Linux PTY returns EIO after slave close.
                break
            raise
        if not chunk:
            break
        output.extend(chunk)
    if not predicate(bytes(output)):
        rendered = ANSI.sub("", output.decode(errors="replace"))
        raise AssertionError(f"PTY 等待超时；当前输出:\n{rendered}")


def run_smoke(binary: pathlib.Path, home: pathlib.Path, state: MockState) -> str:
    server = MockServer(("127.0.0.1", 0), MockHandler)
    server.state = state
    configure_home(home, state, server.server_address[1])
    server_thread = threading.Thread(target=server.serve_forever, daemon=True)
    server_thread.start()

    master, slave = os.openpty()
    child_env = {
        "HOME": str(home),
        "LANG": "C.UTF-8",
        "LC_ALL": "C.UTF-8",
        "PATH": f"{home / 'bin'}:/usr/bin:/bin",
        "TERM": "xterm-256color",
        "ZHSH_SMOKE_SECRET": state.token,
    }
    child = subprocess.Popen(
        [str(binary)],
        cwd=home,
        env=child_env,
        stdin=slave,
        stdout=slave,
        stderr=slave,
        close_fds=True,
        preexec_fn=child_session,
    )
    os.close(slave)
    output = bytearray()
    try:
        read_until(master, output, lambda data: b"$ " in data, 10)
        os.write(master, "检查发布冒烟\r".encode())

        if state.scenario in {"success", "path-shadow"} and not (
            state.provider == "anthropic" and state.scenario == "success"
        ):
            confirmation = "? 执行？[y/N]".encode()
            read_until(
                master,
                output,
                lambda data: confirmation in data or bool(state.errors),
                15,
            )
            if state.errors:
                raise AssertionError("Mock 请求断言失败: " + "; ".join(state.errors))
            answer = b"n\r" if state.scenario == "path-shadow" else b"y\r"
            os.write(master, answer)

        if state.scenario == "path-shadow":
            marker = "用户拒绝".encode()
        else:
            marker = state.expected_answer.encode()
        read_until(master, output, lambda data: marker in data, 20)
        if state.scenario == "path-shadow":
            read_until(master, output, lambda data: "! 已取消".encode() in data, 5)
            # 显式退出 0，避免把预期的安全取消状态误判为制品冒烟失败。
            os.write(master, b"exit 0\r")
        else:
            summary = re.compile(
                rb"[1-6]\xe8\xbd\xae(?: \xe6\xbe\x84\xe6\xb8\x85 [1-3]\xe6\xac\xa1)? [0-9]+\.[0-9]s"
            )
            read_until(master, output, lambda data: summary.search(data) is not None, 5)
            os.write(master, b"exit\r")
        child.wait(timeout=10)
    finally:
        if child.poll() is None:
            child.kill()
            child.wait(timeout=5)
        os.close(master)
        server.shutdown()
        server.server_close()
        server_thread.join(timeout=5)

    if child.returncode != 0:
        raise AssertionError(f"zhsh 退出码不是 0: {child.returncode}")
    if state.errors:
        raise AssertionError("Mock 请求断言失败: " + "; ".join(state.errors))
    if len(state.requests) != state.expected_requests:
        raise AssertionError(
            f"请求轮数应为 {state.expected_requests}，实际为 {len(state.requests)}"
        )

    terminal = ANSI.sub("", output.decode(errors="replace")).replace("\r", "")
    if state.token in terminal:
        raise AssertionError("access token 出现在终端输出")
    if state.scenario == "path-shadow" and (home / "shadow-ran").exists():
        raise AssertionError("用户拒绝后 PATH shadow 程序仍被执行")
    if state.scenario == "background" and "Agent 不支持后台执行运算符 &" not in terminal:
        raise AssertionError("后台命令没有返回稳定的 unsupported 诊断")
    return terminal


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", required=True, type=pathlib.Path)
    parser.add_argument("--home", required=True, type=pathlib.Path)
    parser.add_argument("--provider", choices=("openai", "anthropic"), required=True)
    parser.add_argument(
        "--scenario", choices=("success", "path-shadow", "background"), default="success"
    )
    args = parser.parse_args()
    if not args.binary.is_file():
        parser.error(f"binary 不存在: {args.binary}")
    if not args.home.is_absolute():
        parser.error("--home 必须是绝对路径")

    token = "zhsh-smoke-token-0123456789"
    model = f"smoke-{args.provider}-model"
    state = MockState(args.provider, args.scenario, token, model)
    terminal = run_smoke(args.binary.resolve(), args.home, state)
    print(terminal, end="")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
