# 安全策略

## 报告漏洞

请优先使用 GitHub 仓库的 Private Vulnerability Reporting 提交安全报告，并避免在公开 issue、
终端抄本或示例配置中粘贴 token、私钥和个人数据。无法使用该入口时，可联系 Debian 元数据中的
维护者 `jungle <junglelk@foxmail.com>`。报告中请包含受影响版本、最小复现、预期影响和已采取的
临时缓解；不要附带真实凭据。

维护者确认问题前不会要求报告者运行来源不明的程序。修复完成后会在 `CHANGELOG.md` 和发布
说明中记录影响范围；需要撤回制品时会同时撤回 GitHub Release，并发布升级或回滚说明。

## 当前支持范围

首个公开 prototype 为 Ubuntu 24.04 LTS amd64 的 `.deb` 和 Fedora 44 x86_64 的 `.rpm`
提供安全更新，要求 glibc 2.39 或更新版本及 Bash 5.2。其他 Linux 发行版、ARM64 和 macOS
尚未验证。源码构建和正式制品统一使用 Rust 1.98.0。

## 威胁模型与已知边界

- LLM 响应、Provider 响应和第三方 Codec 都是不可信输入。Codec 是经过签名和限制校验的
  声明数据，不加载本地插件代码。
- Provider 会收到用户的 Agent 输入、系统 Prompt、任务 transcript，以及最多 64 KiB 的命令
  反馈。确认提示会在可能读取敏感、越界或无界数据时说明这一点。
- Agent 命令以启动 zhsh 的当前 UID 在真实宿主执行。prototype 没有系统级文件、进程或网络
  沙箱；`0600` 不能阻止同 UID 的已批准命令读取文件。
- 默认 `balanced` 只自动批准身份可信、预期只读、披露范围受限且没有强制确认信号的计划。
  `trusted` 也不能清除破坏、提权、身份不明、敏感/无界披露或网络外传的强制确认。
- 明显后台或 detach 入口会被拒绝，原进程组会被有界清理；没有 cgroup/subreaper 时，无法
  保证检测运行期间自行 `fork+setsid` 逃离原进程组的任意程序。
- 已验证 LLM 配置中的已知 token 会在反馈前精确替换，但编码、拆分、哈希或其他变换后的值
  无法由精确脱敏保证识别。

用户应为 LLM token 采用最小权限、独立配额和可快速轮换的凭据，不应在 Agent 任务中要求读取
SSH 私钥、云凭据或与当前工作无关的个人数据。
