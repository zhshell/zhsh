# 变更记录

本项目遵循语义化版本。

## 0.1.0 - 2026-09-08

首个公开 prototype，面向 Homelab 和个人 Linux 环境的轻量体验。

### Shell 使用体验

- 使用确定性的二元输入路由：去除边界空白后，ASCII 首字符输入原样交给 Shell，非 ASCII 首字符输入交给 LLM Agent；不会分析意图、查询 PATH、补空格或改写 Unicode。
- 普通命令、管道、重定向、通配符、变量展开、命令展开、条件组合和循环继续由 Bash 解释，因此可以直接运行现有命令和包含中文文件名的脚本。
- 启动时继承父进程的当前目录和全部 UTF-8 进程环境；从现有 Bash 启动时，已经导出的 `PATH`、Locale、代理和其他环境变量会直接进入 zhsh，非 UTF-8 环境项会被跳过并集中提示。
- 在 HOME 有效时，启动阶段只运行一次非登录交互式 `bash -ic`，按照当前系统 Bash 的规则读取 `~/.bashrc` 等非登录交互启动文件；zhsh 从中导入别名和实际存在的 `command_not_found_handle`，不会把该子 Bash 的目录变化、普通变量或其他函数整体复制回来。
- 随后单独读取 `~/.zhshrc`，并把其中的当前目录、导出环境、别名、Shell 函数、可重放普通变量和 `PS0` 至 `PS4` 提交为 zhsh 会话状态；需要长期作用于 zhsh 的配置应写在该文件中。
- 作为登录 Shell 直接启动时，zhsh 不会直接读取 Bash 专用的 `~/.bash_profile`、`~/.bash_login` 或 `~/.profile`；它继承登录管理器提供的环境，再执行上述 `.bashrc` 兼容导入和 `.zhshrc` 加载。由现有 Bash 启动时，Bash 已导出的登录环境则自然随父进程继承。
- 每条普通命令使用当前会话状态启动非登录、非交互的 `bash -c`，不会为每条命令重复读取 `.bashrc` 或 Bash 登录文件；若用户显式设置 `BASH_ENV`，则仍遵循 Bash 对非交互 Shell 的标准加载语义。
- 提供 `cd`、`pwd`、`pushd`、`popd`、`dirs`、`export`、`unset`、`umask`、`alias`、`unalias`、`source`、`.`、`type`、`help`、`history`、`fg` 和 `exit` 等会话内建命令。
- `source` 可以把工作目录、导出环境、别名、Shell 函数和可重放普通变量同步回当前会话。
- 支持将输出型 zhsh 内建命令放在管道最左侧，例如 `history | grep cargo` 和 `zh safety | grep java`；管道右侧仍由 Bash 执行。
- Tab 补全覆盖 PATH 命令、内建命令、别名、Shell 函数、文件路径、`zh` 子命令、配置名和模型档位，并自动处理目录后缀及 Shell 特殊字符转义。
- 命令历史保存在 `~/.zh_history`，最多保留 10000 条；空格开头的输入、Agent 内容、命令输出和配置向导字段不会写入历史。
- 支持 Bash 风格的 `PS0` 至 `PS4`，允许在当前会话或 `~/.zhshrc` 中覆盖默认提示符；提示符渲染不会隐式执行命令替换、反引号或算术展开。

### 中文自然语言 Agent

- 中文及其他非 ASCII 首字符任务可以直接输入，由 Agent 生成命令、读取结果并给出纯文本总结。
- 每个执行阶段严格限制为最多六次模型请求；前五轮可以各提出一条命令，第六轮只能完成回答或发起澄清，协议格式修复也占用同一预算。
- 单个任务最多支持三次澄清；模型澄清同时提供候选项和自由文本输入。等待模型时按 Esc 可进入手动澄清，提交后使用一次澄清额度并开启新的六轮阶段；超时或空白输入会无损返回原流程。
- 对系统状态、软件版本和资源观测类任务，只有本任务实际获得的命令输出才能作为回答证据；无证据结论会在剩余轮次内要求修复，预算耗尽后不会展示不可靠结论。
- 每次命令执行前冻结当前目录、PATH、参数和实际目标；同一冻结计划在一个任务中最多尝试一次，防止格式修复或模型重试造成重复执行。
- 首次 Agent 请求携带发行版、内核、架构、权限身份、内核可见总内存、zhsh 版本、当前目录和 Locale 等最小宿主上下文；完整环境、历史、别名、函数和 LLM 凭据不会作为启动上下文发送。
- 终端分别展示模型命令、必要确认、取消或执行结果以及最终轮次耗时；等待模型期间隐藏光标，最终回答使用适合终端阅读的分组和逐行字段格式。
- Ctrl-C 可以取消当前输入、LLM 请求或 Agent 命令而不退出 zhsh；同时到达的迟发模型响应不会进入后续任务。

### 命令执行与交互程序

- 普通 Agent 命令在捕获模式下执行，stdout 与 stderr 合计上限为 1 MiB；每条命令最多向模型反馈 64 KiB，单次任务最多反馈 256 KiB。
- `sudo`、`su -c`、`ssh host command` 等有限行式交互命令使用真实前台终端，认证输入由目标程序直接读取，输出实时显示，并把有界、脱敏的副本交给 Agent 总结。
- `vim`、`top`、`ssh host` 等全屏或持续会话继承真实终端，只向 Agent 提供退出状态，不捕获屏幕内容；进入远端交互式 SSH 后，后续输入不经过 zhsh 路由或 Safety。
- 提示符以及 Agent 工作期间的 Ctrl-Z/Pause 不会暂停 zhsh。用户直接启动的整行前台命令被 Ctrl-Z 暂停后，可以使用无参数 `fg` 恢复最近一个暂停作业。
- Agent 拒绝可识别的后台或脱离执行语法，并在命令结束后有界清理仍位于原进程组的后代进程。

### LLM 配置与模型切换

- 使用 `zh llm` 创建配置，使用 `zh llm -m` 修改配置；交互式表单支持方向键、Tab 补全、Esc Normal 模式、`i` 继续编辑、`:q` 放弃和任意阶段 `:wq` 保存草稿，且没有业务超时。
- 不完整配置可以保存，并可在 `zh use <配置名>` 时进入修复；修复前只禁用 Agent，不影响普通 Shell 命令。
- 使用 `zh` 或 `zh status` 查看当前配置、Codec、JSON Schema、URL、传输方式、模型档位、脱敏 token 和授信等级；使用 `zh ls` 列出配置，使用 `zh use` 切换配置。
- 每个配置提供 `flash`、`standard`、`max` 三个模型档位，可通过 `zh tier` 即时切换并持久化。
- `ACCESS_TOKEN` 允许为空，是否需要以及是否有效由 LLM Provider 决定；已有 token 在表单和状态输出中保持掩码显示。
- `JSON_SCHEMA=off|on` 在同一 Codec 中显式切换普通 JSON 输出和 JSON Schema Profile；Codec 未提供可验证的 Schema Profile 时自动使用默认 Profile 并提示用户。
- 公网或域名形式的 Provider URL 必须使用 HTTPS；HTTP 只允许字面 localhost、回环地址、RFC 1918 IPv4 和 IPv6 ULA。私网 HTTP 明确标记明文及凭据风险，并绕过环境代理。
- Provider Base URL 可以包含路径前缀，但不能包含凭据、query 或 fragment；zhsh 追加 Codec endpoint 后会再次验证 Origin，拒绝把请求或 token 发送到其他 Origin。

### Codec 管理

- 随安装包提供 `openai@0.3.0` 和 `anthropic@0.3.0` 两个签名的声明式官方 Codec，分别支持 OpenAI Responses API 和 Anthropic Messages API。
- Codec 是经过签名并受资源上限约束的声明数据；它不能执行本地代码，也不能读取环境、文件、密钥或网络。
- `zh codec ls` 查看当前已加载制品及来源，`zh codec -t` 无副作用校验外部制品，`zh codec install` 安装，`zh codec uninstall` 卸载用户制品，`zh codec export` 导出可分发的用户制品，`zh codec reload` 重新加载磁盘状态。
- 第三方 Codec 使用单个 `.zhcodec` 文件分发；首次安装未知发布者时显示完整公钥指纹并默认拒绝，用户明确授权后把发布公钥保存为 Publisher Trust Anchor。
- 外部文件名不决定身份；安装时以签名 payload 中的 FORMAT 规范化文件名。同 FORMAT、同内容的系统制品视为幂等成功，同 FORMAT、不同内容的制品会被拒绝。
- 损坏、冲突或权限不安全的 Codec 单项隔离；无关坏制品不影响普通 Shell，活动配置所需 Codec不可用时只禁用 Agent 并集中报告诊断。

### Agent Safety 与确认

- 默认 `balanced` 授信策略只自动执行目标身份可信、预期只读、披露受限且可静态绑定的计划；写入、删除、覆盖、提权、未知目标、敏感披露和无法验证的命令需要确认或直接拒绝。
- 使用 `zh trust balanced|confirm|trusted` 修改当前会话策略，添加 `-w` 后写入 `~/.zhshrc`。`confirm` 确认所有 Agent 命令；`trusted` 只额外放行 Linux 核心规则确认的任务根内普通修改，不绕过提权、明确破坏、身份不明、敏感披露或网络外传确认。
- Safety 将命令副作用、数据披露、真实执行目标、Shell 结构、重定向和进程监督作为独立维度组合，用户拒绝或确认超时会以“已取消、命令未执行”结束，而不是报告技术失败。
- 内置有限的 Linux 核心只读规则；Java、Git、Docker 等应用规则可以由用户另行安装，所有外部规则都作为本地策略，不被宣传为绝对安全、正确或完整。
- `zh safety` 查看内存中的规则和覆盖状态，`zh safety -t` 无副作用校验单文件或 gzip 规则包，`zh safety assess` 解释指定命令的匹配与最终决策，`zh safety install` 安装一个或多个规则，`zh safety reload` 原子发布新的规则 generation。
- Safety 规则是严格校验的声明式 JSON，不执行插件程序；本地规则可以覆盖内置程序语义，但不能降低宿主持有的目标身份、Shell 结构、重定向、披露和监督约束。
- 用户直接输入的 Shell 命令不会经过 Agent Safety 判断或确认。

### 本地状态、诊断与帮助

- 启动时从 `~/.zhshrc` 恢复会话状态，从 `~/.zhsh/active-llm` 恢复活动 LLM 配置；配置目录使用0700，配置文件和活动标记使用 0600。
- `HOME` 必须是非空 UTF-8 绝对路径；无效 HOME 会禁用用户配置、历史、Codec 和 Safety 规则，但普通 Shell 仍可使用，也不会从当前目录误加载同名状态文件。
- 提供 `zhsh --help`、`zhsh --version`、内建 `help`、`zh help` 和中文 `man zhsh`。
- `zhsh --trace-agent` 可以按需记录无法通过严格协议解析的 Agent 原始响应；默认关闭，诊断内容写入 `~/.zhsh/diagnostics/invalid-agent-responses.jsonl`。

### 安装与支持范围

- 提供 Ubuntu 24.04 LTS amd64 DEB、Fedora 44 x86_64 RPM 和源码安装方式，统一使用 `/usr`文件布局并安装主程序、官方 Codec、README、LICENSE 和中文 man page。
- 官方二进制基线为 glibc 2.39 或更新版本及 Bash 5.2 或更新版本；源码构建固定使用 Rust 1.98.0。
- DEB、RPM、源码归档和校验和由签名版本 Tag 的 GitHub Release 流程生成。

### Prototype 边界

- 当前版本面向个人 Linux 和 Homelab 体验，不建议在保存生产凭据、关键业务状态或高价值数据的主机上使用。
- Agent 命令以启动 zhsh 的当前用户身份在真实宿主执行；当前不提供文件、网络、进程或权限沙箱，也不提供事务和自动回滚。
- 当前只提供单槽位 `fg`，没有 `jobs`、`bg`、作业编号、`wait`、`disown` 或完整 job control。
- zhsh 不实现完整 Shell parser；交给一次性 Bash 的复合命令不会把其中的 `cd`、环境、别名或`zh` 状态变化同步回当前会话。
- Agent 命令没有固定运行时限；主动逃离原进程组的恶意进程不在当前 prototype 的完整监督范围内。
- Provider 会收到自然语言任务、系统 Prompt、最小宿主上下文、当前任务记录和有界命令反馈；已知 LLM token 会在反馈前精确脱敏，但无法保证识别编码、拆分或变换后的秘密。
