# 参与 zhsh 开发

感谢提交问题、修复和改进。zhsh 当前是面向个人 Linux 与 Homelab 环境的 prototype。

## 开发环境

仓库使用 `rust-toolchain.toml` 固定 Rust 工具链。从仓库根目录运行：

```bash
cargo build --quiet
cargo test --quiet
make ci
```

提交前请至少运行 `make ci`。打包相关改动还应运行对应的 `make deb`、`make rpm` 或完整的
`make release-check`。

## 修改边界

- 不要提交 token、私钥、真实 LLM 配置、Shell 历史或 Agent 原始响应；
- 新代码应放入最窄的现有职责模块，避免在 `zhsh/src/` 根目录增加松散源文件；
- 不要恢复已经移除的旧 `zh` 命令形式或环境变量 Provider 配置；
- Safety 分类不是安全证明，涉及自动执行边界的修改必须包含对应回归验证；
- 终端交互修改应说明人工验证环境，并尽可能提供 PTY 回归测试。

## 提交与 Pull Request

提交标题使用简短的祈使式 conventional 前缀，例如 `feat:`、`fix:`、`refactor:`、`docs:` 或
`test:`。Pull Request 请说明行为变化、验证命令和已知限制；交互展示变化请附脱敏终端抄本。

安全漏洞不要提交公开 issue，请按照 `SECURITY.md` 使用私密报告入口。
