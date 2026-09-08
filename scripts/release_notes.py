#!/usr/bin/env python3
"""Extract one version section from CHANGELOG.md for a GitHub Release."""

from __future__ import annotations

import pathlib
import re
import sys


def main() -> int:
    if len(sys.argv) != 3:
        raise SystemExit("用法: release_notes.py VERSION OUTPUT")

    version = sys.argv[1]
    output = pathlib.Path(sys.argv[2])
    repository = pathlib.Path(__file__).resolve().parent.parent
    changelog = (repository / "CHANGELOG.md").read_text(encoding="utf-8")

    heading = re.compile(
        rf"^##[ \t]+{re.escape(version)}(?:[ \t]+-[ \t]+[^\r\n]+)?[ \t]*$",
        re.MULTILINE,
    )
    matches = list(heading.finditer(changelog))
    if len(matches) != 1:
        raise SystemExit(
            f"release-notes: CHANGELOG 中版本 {version} 的二级标题应有且只有一个，"
            f"实际为 {len(matches)} 个"
        )

    start = matches[0].end()
    next_section = re.search(r"^##[ \t]+", changelog[start:], re.MULTILINE)
    end = start + next_section.start() if next_section else len(changelog)
    notes = changelog[start:end].strip()
    if not notes:
        raise SystemExit(f"release-notes: CHANGELOG 中版本 {version} 没有发布内容")

    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(f"{notes}\n", encoding="utf-8")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
