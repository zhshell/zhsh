#!/usr/bin/env python3
"""check-release-metadata.sh 的无第三方依赖实现。"""

from __future__ import annotations

import pathlib
import re
import subprocess
import sys
import tomllib


def main() -> int:
    if len(sys.argv) != 3:
        raise SystemExit("用法: release_metadata.py BINARY [DEB]")

    root = pathlib.Path.cwd()
    manifest_path = root / "zhsh/Cargo.toml"
    manifest_text = manifest_path.read_text(encoding="utf-8")
    manifest = tomllib.loads(manifest_text)
    version = manifest["package"]["version"]
    if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?", version):
        raise SystemExit(f"release-metadata: Cargo 版本不是规范发布 SemVer: {version}")

    package_block = re.search(r"(?ms)^\[package\]\s*$\n(.*?)(?=^\[|\Z)", manifest_text)
    if package_block is None:
        raise SystemExit("release-metadata: zhsh/Cargo.toml 缺少 [package]")
    version_lines = re.findall(
        r'(?m)^version\s*=\s*"([^"]+)"\s*$', package_block.group(1)
    )
    if version_lines != [version]:
        raise SystemExit("release-metadata: [package] 必须有且只有一个字面量 version")

    deb = manifest.get("package", {}).get("metadata", {}).get("deb", {})
    revision = str(deb.get("revision", ""))
    if revision != "1":
        raise SystemExit("release-metadata: Debian revision 必须为 1")
    deb_version = f"{version}-{revision}"
    maintainer = str(deb.get("maintainer", ""))
    if not re.fullmatch(r"[^<>\n]+ <[^<>\s]+@[^<>\s]+>", maintainer):
        raise SystemExit("release-metadata: Maintainer 必须包含姓名与邮箱")
    homepage = str(manifest["package"].get("homepage", ""))
    if not homepage.startswith("https://"):
        raise SystemExit("release-metadata: Homepage 必须是 HTTPS URL")

    depends = {
        item.strip().split()[0] for item in str(deb.get("depends", "")).split(",")
    }
    for required in ("bash", "ca-certificates", "$auto"):
        if required not in depends:
            raise SystemExit(f"release-metadata: Debian depends 缺少 {required}")

    assets = deb.get("assets", [])
    destinations = {
        entry[1] for entry in assets if isinstance(entry, list) and len(entry) >= 2
    }
    for required in (
        "usr/bin/zhsh",
        "usr/share/man/man1/zhsh.1",
        "usr/share/doc/zhsh/README.md",
        "usr/share/doc/zhsh/LICENSE",
    ):
        if required not in destinations:
            raise SystemExit(f"release-metadata: Debian assets 缺少 {required}")
    if any("/plugins/safety/" in destination for destination in destinations):
        raise SystemExit("release-metadata: zhsh 软件包不得包含应用级 Safety 规则")

    man = (root / "packaging/man/zhsh.1").read_text(encoding="utf-8")
    header = re.search(
        r'^\.TH ZHSH 1 "[^"]+" "zhsh ([^"]+)" ', man, re.MULTILINE
    )
    if header is None or header.group(1) != version:
        actual = header.group(1) if header else "缺失"
        raise SystemExit(
            f"release-metadata: man 版本 {actual} 与 Cargo {version} 不一致"
        )

    readme = (root / "README.md").read_text(encoding="utf-8")
    expected_deb_name = f"zhsh_{deb_version}_amd64.deb"
    expected_rpm_name = f"zhsh-{version}-1.x86_64.rpm"
    if expected_deb_name not in readme:
        raise SystemExit(f"release-metadata: README 未包含当前包名 {expected_deb_name}")
    if expected_rpm_name not in readme:
        raise SystemExit(f"release-metadata: README 未包含当前包名 {expected_rpm_name}")

    changelog = (root / "CHANGELOG.md").read_text(encoding="utf-8")
    if re.search(rf"(?m)^## {re.escape(version)}(?:\s|$)", changelog) is None:
        raise SystemExit(f"release-metadata: CHANGELOG 缺少版本 {version}")

    binary = pathlib.Path(sys.argv[1])
    if not binary.is_file():
        raise SystemExit(f"release-metadata: 缺少待校验 CLI: {binary}")
    try:
        cli_result = subprocess.run(
            [str(binary.resolve()), "--version"],
            check=True,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
    except subprocess.CalledProcessError as error:
        raise SystemExit(
            f"release-metadata: CLI --version 失败，退出码 {error.returncode}: "
            f"{error.stderr.strip()}"
        ) from None
    cli_version = cli_result.stdout
    if cli_version != f"zhsh {version}\n":
        raise SystemExit(
            f"release-metadata: CLI 版本输出 {cli_version!r} 与 Cargo {version!r} 不一致"
        )

    deb_arg = sys.argv[2]
    if deb_arg:
        deb_path = pathlib.Path(deb_arg)
        if not deb_path.is_file():
            raise SystemExit(f"release-metadata: Debian 包不存在: {deb_path}")
        if deb_path.name != expected_deb_name:
            raise SystemExit(
                f"release-metadata: Debian 文件名应为 {expected_deb_name}，"
                f"实际为 {deb_path.name}"
            )
        fields = subprocess.run(
            [
                "dpkg-deb",
                "--field",
                str(deb_path.resolve()),
                "Package",
                "Version",
                "Architecture",
            ],
            check=True,
            text=True,
            stdout=subprocess.PIPE,
        ).stdout.splitlines()
        expected = [
            "Package: zhsh",
            f"Version: {deb_version}",
            "Architecture: amd64",
        ]
        if fields != expected:
            raise SystemExit(
                f"release-metadata: Debian control 版本身份不匹配: {fields!r}"
            )

    print(f"release-metadata: zhsh {version} 元数据一致")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
