//! `zhsh` 可执行文件入口。
//!
//! 实际启动生命周期由库函数 [`zhsh::run`] 统一实现；此处只校验进程级参数。

fn main() {
    let arguments = std::env::args_os().skip(1).collect::<Vec<_>>();
    let status = match arguments.as_slice() {
        [] => zhsh::run(),
        [argument] if argument == "--native" => {
            zhsh::run_with_options(zhsh::RunOptions::default().with_native(true))
        }
        [first, second]
            if (first == "--native" && second == "--norc")
                || (first == "--norc" && second == "--native") =>
        {
            zhsh::run_with_options(
                zhsh::RunOptions::default()
                    .with_native(true)
                    .with_native_no_rc(true),
            )
        }
        [argument] if argument == "--trace-agent" => {
            zhsh::run_with_options(zhsh::RunOptions::default().with_agent_trace(true))
        }
        [argument] if argument == "--version" || argument == "-V" => {
            println!("zhsh {}", env!("CARGO_PKG_VERSION"));
            0
        }
        [argument] if argument == "--help" || argument == "-h" => {
            println!(
                "zhsh {}\n\n用法: zhsh [--help | --version | --trace-agent | --native]\n     zhsh --native --norc | zhsh --norc --native\n\n不带参数时启动交互式 Shell。\n--trace-agent  临时记录无法解析的 Agent 原始响应；日志可能包含任务内容。\n--native  用户与模型命令经 PATH 或显式路径直接执行程序；支持现有内建、字面参数、注释和续行。Native 启动按本模式规则加载 ~/.zhshrc。\n--native --norc  跳过 Native 自动加载 ~/.zhshrc；不影响之后显式 source。\nNative 当前只支持已实现的命令形式，不支持的语法不会交给 Bash 重试。使用 exit 或 EOF 退出。\n\nAgent 授信：balanced 自动执行可信只读命令；confirm 逐条确认；trusted 额外自动执行核心规则识别的任务根内普通修改。\ntrusted 仍确认会话修改、破坏性操作、网络/提权和敏感披露；不支持的执行形式仍拒绝。\n进入 Shell 后运行 `zh trust -h` 查看等级摘要、示例及持久化选项；完整手册：man zhsh。",
                env!("CARGO_PKG_VERSION")
            );
            0
        }
        [argument, ..] => {
            eprintln!(
                "zhsh: 未知参数或不支持的参数组合: {}\n用法: zhsh [--help | --version | --trace-agent | --native]\n     zhsh --native --norc | zhsh --norc --native",
                argument.to_string_lossy()
            );
            2
        }
    };
    std::process::exit(status);
}
