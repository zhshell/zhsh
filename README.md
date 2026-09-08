# zhsh

zhsh 是面向中文用户的 AI 增强交互式 Shell。它保留 Bash 的命令、管道、重定向和脚本能力，
同时允许直接使用中文及其他非 ASCII 文本描述需要完成的任务。

```text
｢zh｣user@server:~$ 检查当前系统负荷
> uptime; free -h; df -h /
系统负荷较低。

运行时间: 3天
可用内存: 12Gi
根分区可用空间: 80Gi
2轮 3.1s
```

当前版本是面向 Homelab 和个人 Linux 环境的 prototype，不建议在保存生产凭据、关键业务状态
或高价值数据的主机上使用。

## 主要能力

- ASCII 首字符输入原样进入 Shell，非 ASCII 首字符输入进入 LLM Agent；
- 每个 Agent 执行阶段最多六次模型请求，单任务最多澄清三次；
- OpenAI Responses API 和 Anthropic Messages API 的声明式 Codec；
- flash、standard、max 三个模型档位；
- `cd`、`history`、`source`、`alias`、`zh` 等会话内建命令；
- 对 Agent 命令执行目标、副作用和数据披露进行宿主侧分析；
- 支持用户安装声明式 Codec 和 Safety 规则，不执行插件代码；
- Ctrl-C 取消当前输入、LLM 请求或 Agent 命令；
- 支持单槽位前台作业暂停和 `fg` 恢复。

## 支持范围

官方二进制包当前验证以下环境：

- Ubuntu 24.04 LTS amd64，DEB；
- Fedora 44 x86_64，RPM；
- glibc 2.39 或更新版本；
- Bash 5.2 或更新版本。

源码构建使用 `rust-toolchain.toml` 固定的 Rust 1.98.0。其他发行版、架构和操作系统尚未验证。

## 安装

Ubuntu：

```bash
sudo apt install ./zhsh_0.1.0-1_amd64.deb
```

Fedora：

```bash
sudo dnf install ./zhsh-0.1.0-1.x86_64.rpm
```

从源码构建和安装：

```bash
make build
sudo make install
```

`make install` 会安装主程序、两个锁定的官方 Codec、README、LICENSE 和 `zhsh(1)` 手册。
应用级 Safety 规则需要用户另行安装。当前不支持使用 `cargo install` 完成完整安装，因为它不会
部署运行时资源。

启动和版本查询：

```bash
zhsh
zhsh --version
zhsh --help
```

## 输入路由

zhsh 使用确定性的二元规则，只检查去除边界空白后的首字符：

```text
ls -la                    → Shell
for f in *.mkv; do ...    → Shell
git commit -m 测试        → Shell
check disk usage          → Shell
检查当前系统负荷          → Agent
Проверить систему         → Agent
```

路由器不会分析意图、查询 PATH、补空格或规范化 Unicode。英文自然语言以 ASCII 开头，因此也会
进入 Shell。非 ASCII 命令名可使用 `./工具` 或 `command 工具` 显式交给 Shell。

## 配置 LLM

创建或修改配置：

```bash
zh llm
zh llm local-qwen
zh llm -m
zh llm -m local-qwen
```

配置向导没有业务超时。在表单中按 Esc 进入 Normal 状态，方向键移动，`i` 恢复输入；输入
`:q` 放弃，输入 `:wq` 可在任意阶段保存当前草稿。不完整配置可以保存，但在修复前 Agent
保持不可用，普通 Shell 不受影响。

配置保存在 `~/.zhsh/llm/<名称>.llm`，活动名称保存在 `~/.zhsh/active-llm`：

```text
NAME=example
URL=https://api.example.com
FORMAT=openai@0.3.0
JSON_SCHEMA=off
ACCESS_TOKEN=
FLASH=fast-model
STANDARD=standard-model
MAX=max-model
TIER=flash
```

字段说明：

- `FORMAT`：精确的 Codec `id@version`；
- `JSON_SCHEMA`：`off` 或 `on`，缺失时按 `off`；
- `URL`：用户授权的 Provider Base URL，可以包含路径前缀；
- `ACCESS_TOKEN`：可为空，是否需要和是否有效由 Provider 决定；
- `FLASH`、`STANDARD`、`MAX`：三个档位的模型名；
- `TIER`：当前档位。

公网和域名形式的 Base URL 必须使用 HTTPS。HTTP 只允许字面 localhost、loopback 和明确的
私有 IP 地址；私网 HTTP 会显示明文传输警告，带非空 token 时会额外提示凭据风险。URL 不能
包含用户信息、查询参数或 fragment。zhsh 会将 Codec endpoint 追加到 Base URL，并拒绝任何
Origin 变化。

常用管理命令：

```bash
zh                  # 当前 LLM、Codec、传输和授信状态
zh ls               # 已保存配置
zh use NAME         # 启用配置
zh tier flash       # 切换模型档位
zh trust            # 查看 Agent 授信策略
zh help             # 查看 zh 内建命令
```

## Codec

zhsh 主仓库只锁定、校验并分发两个官方 Codec 制品：

- `openai@0.3.0`：OpenAI Responses API；
- `anthropic@0.3.0`：Anthropic Messages API。

每个制品同时包含默认 JSON 输出和可选 JSON Schema Profile，由 LLM 配置中的
`JSON_SCHEMA=off|on` 选择。Codec 是经过签名和大小、复杂度限制的声明数据，不读取文件、环境
或网络，也不执行本地代码。

```bash
zh codec ls
zh codec -t ./provider.zhcodec
zh codec install ./provider.zhcodec
zh codec uninstall example.provider@1.0.0
zh codec export example.provider@1.0.0 -o ./dist
zh codec reload
```

首次安装第三方发布者的 Codec 时，zhsh 会显示完整公钥指纹并要求明确确认。外部文件名不决定
Codec 身份，安装位置由已验签 payload 中的 FORMAT 规范化。官方 Codec 不能导出。

Codec 相关独立仓库：

- <https://github.com/zhshell/zhcodec-sdk>
- <https://github.com/zhshell/zhcodec-openai>
- <https://github.com/zhshell/zhcodec-anthropic>

## Agent 命令与 Safety

默认 `balanced` 策略只自动执行目标身份可信、静态、有限且披露受限的只读计划。写入、删除、
覆盖、提权、身份不明、敏感披露和宿主无法验证的计划需要用户确认或直接拒绝。Safety 分类是
启发式风险判断，不是命令安全证明，也不是沙箱。

```bash
zh trust balanced
zh trust confirm
zh trust trusted
zh trust -w balanced
```

zhsh 只内置 Linux 核心命令的有限只读规则。Java、Git、Docker 等应用规则可从独立仓库选择，
也可由用户自行编写；所有外部规则统一视为本地策略，推荐规则不保证绝对安全、正确或完整。

```bash
zh safety
zh safety -t ./java.zhse.json
zh safety assess 'java -version'
zh safety install ./java.zhse.json
zh safety reload
```

Safety 推荐规则仓库：<https://github.com/zhshell/zhsh-safety>

## 安全边界

- LLM、Provider 响应和第三方 Codec 均视为不可信输入；
- Agent 命令以启动 zhsh 的当前用户身份在真实宿主执行；
- prototype 不提供文件、网络、进程或权限沙箱；
- 用户确认不能证明用户已经理解全部后果；
- 已经发生的外部副作用不能由 zhsh 自动回滚；
- Provider 会收到自然语言任务、系统 Prompt、当前任务记录和受限的命令输出；
- 已知 access-token 会在反馈前精确脱敏，但不能保证识别编码、拆分或变换后的秘密；
- 用户直接输入的 Shell 命令不经过 Agent Safety 策略。

请为 LLM 使用独立、最小权限、限额且可快速轮换的凭据，不要让 Agent 读取 SSH 私钥、云凭据
或与当前任务无关的个人数据。

## 终端与作业控制

zhsh 当前不提供完整 job control。提示符处的 Ctrl-Z/Pause 不会暂停 zhsh；一条用户前台命令
被 Ctrl-Z 暂停后，可以使用无参数 `fg` 恢复。当前没有 `jobs`、`bg`、作业编号、`wait` 或
`disown`，也不单独管理一行 Bash 命令内部产生的多个后台作业。

`sudo`、`su -c` 和 `ssh host command` 等有限行式交互命令使用真实前台终端，同时捕获有界、
脱敏副本供 Agent 总结。`vim`、`top`、`ssh host` 等全屏或持续会话只向 Agent 提供退出状态。
进入远端交互式 SSH 后，输入不再经过 zhsh 路由、Safety 或历史。

## 内建命令与提示符

运行 `help` 查看 Shell 内建命令，运行 `help NAME` 查看具体用法，运行 `man zhsh` 查看完整手册。

zhsh 支持 Bash 风格的 `PS0`–`PS4`。可以在 `~/.zhshrc` 中覆盖默认提示符：

```bash
PS1='\u@\h:\w\$ '
```

`~/.zhshrc`、`~/.zh_history` 和 `~/.zhsh/` 是用户私有运行时状态，不属于安装包。

## 诊断

```bash
zhsh --trace-agent
```

该选项只为当前进程记录无法通过严格协议解析的 Agent 原始响应，输出到
`~/.zhsh/diagnostics/invalid-agent-responses.jsonl`。内容可能包含用户任务和 Provider 响应，
默认不启用，不应作为 issue 附件直接公开。

## 开发与验证

```bash
cargo build --quiet
cargo test --quiet
cargo clippy --package zhsh --quiet --all-targets --all-features
make ci
make deb
make rpm
make release-check
```

DEB/RPM 和源码安装使用同一 `/usr` 布局。应用级 Safety 规则不进入 zhsh 安装包。

## 许可证

zhsh 以 GPL-3.0-or-later 发布，详见 `LICENSE`。
