#!/usr/bin/env python3
"""检查 .deb control 与 dpkg-deb --contents 的严格文件清单。"""

from __future__ import annotations

import json
import pathlib
import re
import subprocess
import sys


def main() -> int:
    if len(sys.argv) != 3:
        raise SystemExit("用法: deb_manifest_check.py DEB CONTENTS")
    deb = pathlib.Path(sys.argv[1])
    listing = pathlib.Path(sys.argv[2]).read_text(encoding="utf-8")
    control_text = subprocess.run(
        ["dpkg-deb", "--field", str(deb)],
        check=True,
        text=True,
        stdout=subprocess.PIPE,
    ).stdout
    control: dict[str, str] = {}
    current: str | None = None
    for line in control_text.splitlines():
        if line[:1].isspace() and current is not None:
            control[current] += " " + line.strip()
            continue
        if ":" in line:
            current, value = line.split(":", 1)
            control[current] = value.strip()

    if control.get("Package") != "zhsh":
        raise SystemExit("check-deb: Package 必须是 zhsh")
    if control.get("Architecture") != "amd64":
        raise SystemExit("check-deb: Architecture 必须是 amd64")
    depends = control.get("Depends", "")
    for pattern, label in (
        (r"(?:^|,\s*)bash(?:\s|,|$)", "bash"),
        (r"(?:^|,\s*)ca-certificates(?:\s|,|$)", "ca-certificates"),
        (r"(?:^|,\s*)libc6\s*\(>=\s*[0-9]", "libc6 下限"),
    ):
        if re.search(pattern, depends) is None:
            raise SystemExit(f"check-deb: Depends 缺少 {label}: {depends}")

    expected_files = {
        "usr/bin/zhsh",
        "usr/share/doc/zhsh/LICENSE",
        "usr/share/doc/zhsh/README.md",
        "usr/share/man/man1/zhsh.1.gz",
    }
    lock = json.loads(
        pathlib.Path("zhsh/assets/llm-codecs/official-codecs.lock").read_text(
            encoding="utf-8"
        )
    )
    for artifact in lock["artifacts"]:
        expected_files.add("usr/lib/zhsh/plugins/llm/" + artifact["file"])

    seen_files: set[str] = set()
    for line in listing.splitlines():
        parts = line.split(maxsplit=5)
        if len(parts) < 6:
            raise SystemExit(f"check-deb: 无法解析包清单行: {line!r}")
        mode, owner, path = parts[0], parts[1], parts[5]
        path = path.removeprefix("./").removesuffix("/")
        if owner not in {"root/root", "0/0"}:
            raise SystemExit(f"check-deb: 非 root/root 所有权: {path}: {owner}")
        if mode.startswith("d"):
            if mode != "drwxr-xr-x":
                raise SystemExit(f"check-deb: 目录权限不是 0755: {path}: {mode}")
            continue
        if not mode.startswith("-"):
            raise SystemExit(f"check-deb: 不允许符号链接或特殊文件: {path}: {mode}")
        expected_mode = (
            "-rwxr-xr-x"
            if path == "usr/bin/zhsh"
            else "-rw-r--r--"
        )
        if mode != expected_mode:
            raise SystemExit(f"check-deb: 文件权限不匹配: {path}: {mode}")
        seen_files.add(path)

    if any(path.startswith("usr/lib/zhsh/plugins/safety/") for path in seen_files):
        raise SystemExit("check-deb: 软件包不得包含应用级 Safety 规则")

    allowed_extra = {
        "usr/share/doc/zhsh/changelog.Debian.gz",
        "usr/share/doc/zhsh/copyright",
    }
    missing = sorted(expected_files - seen_files)
    unexpected = sorted(seen_files - expected_files - allowed_extra)
    if missing or unexpected:
        raise SystemExit(
            f"check-deb: 文件清单不一致；缺少={missing!r}，意外={unexpected!r}"
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
