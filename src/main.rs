mod export;
mod invoice;
mod model;
mod parser;
mod runtime_log;
mod tui;

use std::path::PathBuf;

use clap::Parser;
use invoice::process_folder;
use model::{ProcessingMode, DEFAULT_MAX_FILES_PER_FOLDER};

#[derive(Debug, Parser)]
#[command(
    name = "invoice-helper",
    version = model::APP_VERSION,
    about = "带轻量内联 TUI 的 PDF 发票校验、分类与销售方汇总工具"
)]
struct Cli {
    /// 可选：直接以命令行模式处理该发票目录；省略时启动内联 TUI
    folder: Option<PathBuf>,

    /// 输出路径：快速模式为 Markdown 文件，完整模式为结果父目录
    #[arg(short, long, value_name = "PATH", requires = "folder")]
    output: Option<PathBuf>,

    /// 命令行直接运行完整校验与分类模式
    #[arg(long, requires = "folder")]
    full: bool,

    /// 完整模式下每个发票文件夹的最大文件数；0 表示不限制
    #[arg(long, value_name = "COUNT", requires = "full")]
    max_files_per_folder: Option<usize>,

    /// 内部工作进程参数，不供用户直接使用
    #[arg(long, hide = true, conflicts_with = "folder")]
    worker: Option<PathBuf>,
}

fn main() {
    let cli = Cli::parse();

    if let Some(path) = cli.worker {
        match parser::worker_extract(&path) {
            Ok(result) => match serde_json::to_string(&result) {
                Ok(json) => println!("{json}"),
                Err(error) => {
                    eprintln!("工作进程无法序列化结果：{error}");
                    std::process::exit(3);
                }
            },
            Err(error) => {
                eprintln!("工作进程解析失败：{error:#}");
                std::process::exit(2);
            }
        }
        return;
    }

    if let Some(folder) = cli.folder {
        if let Err(error) = run_cli(folder, cli.output, cli.full, cli.max_files_per_folder) {
            eprintln!("错误：{error:#}");
        }
        return;
    }

    runtime_log::start_session();
    if let Err(error) = tui::run() {
        eprintln!("\n错误：{error:#}");
        tui::pause_before_exit();
    }
}

fn run_cli(
    folder: PathBuf,
    output: Option<PathBuf>,
    full: bool,
    max_files_per_folder: Option<usize>,
) -> anyhow::Result<()> {
    let mode = if full {
        ProcessingMode::FullValidationExport
    } else {
        ProcessingMode::QuickSummary
    };
    let configured_limit = max_files_per_folder.unwrap_or(DEFAULT_MAX_FILES_PER_FOLDER);
    let batch_limit = (configured_limit != 0).then_some(configured_limit);
    let result = process_folder(&folder, output.as_deref(), mode, batch_limit, |progress| {
        println!(
            "[{}/{}] {}",
            progress.current, progress.total, progress.file_name
        );
    })?;

    println!(
        "处理完成：有效 {} 份，待核查 {} 份，非发票 {} 份；符合条件销售方 {} 个。",
        result.success_count, result.failed_count, result.skipped_count, result.qualified_count
    );
    println!("输出：{}", result.output.display());
    Ok(())
}
