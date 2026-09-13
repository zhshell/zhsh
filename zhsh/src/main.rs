//! `zhsh` 可执行文件入口。
//!
//! 实际启动生命周期由库函数 [`zhsh::run`] 统一实现；此处只处理不启动 REPL 的
//! 进程级 `--help` / `--version` / `--trace-agent` / `--native` 参数。

fn main() {
    let mut arguments = std::env::args_os().skip(1);
    let first = arguments.next();
    let extra = arguments.next();
    let status = match (first.as_deref(), extra) {
        (None, None) => zhsh::run(),
        (Some(argument), None) if argument == "--native" => {
            zhsh::run_with_options(zhsh::RunOptions::default().with_native(true))
        }
        (Some(argument), None) if argument == "--trace-agent" => {
            zhsh::run_with_options(zhsh::RunOptions::default().with_agent_trace(true))
        }
        (Some(argument), None) if argument == "--version" || argument == "-V" => {
            println!("zhsh {}", env!("CARGO_PKG_VERSION"));
            0
        }
        (Some(argument), None) if argument == "--help" || argument == "-h" => {
            println!(
                "zhsh {}\n\n用法: zhsh [--help | --version | --trace-agent | --native]\n\n不带参数时启动交互式 Shell。\n--trace-agent  临时记录无法解析的 Agent 原始响应；日志可能包含任务内容。\n--native  用户与模型命令经 PATH 直接执行 ELF 二进制；支持现有内建与字面参数，不支持通用 Shell 展开。使用 exit 或 EOF 退出。",
                env!("CARGO_PKG_VERSION")
            );
            0
        }
        (Some(argument), _) => {
            eprintln!(
                "zhsh: 未知参数: {}\n用法: zhsh [--help | --version | --trace-agent | --native]",
                argument.to_string_lossy()
            );
            2
        }
        (None, Some(_)) => unreachable!("额外参数不可能没有首参数"),
    };
    std::process::exit(status);
}
