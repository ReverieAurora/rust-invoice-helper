mod invoice;
mod tui;

use std::path::PathBuf;

use clap::Parser;
use invoice::{process_folder, ProcessingMode};

#[derive(Debug, Parser)]
#[command(
    name = "invoice-helper",
    version,
    about = "带简易界面的 PDF 发票销售方汇总工具"
)]
struct Cli {
    /// 可选：直接以命令行模式处理该发票目录；省略时启动图形界面
    folder: Option<PathBuf>,

    /// 命令行模式的报告输出路径；省略时输出到发票目录
    #[arg(short, long, value_name = "FILE", requires = "folder")]
    output: Option<PathBuf>,
}

fn main() {
    let cli = Cli::parse();

    if let Some(folder) = cli.folder {
        if let Err(error) = run_cli(folder, cli.output) {
            eprintln!("错误：{error:#}");
        }
        return;
    }

    if let Err(error) = tui::run() {
        eprintln!("\n错误：{error:#}");
        tui::pause_before_exit();
    }
}

fn run_cli(folder: PathBuf, output: Option<PathBuf>) -> anyhow::Result<()> {
    let result = process_folder(
        &folder,
        output.as_deref(),
        ProcessingMode::SellerTotalAtLeast1000,
        |progress| {
            println!(
                "[{}/{}] {}",
                progress.current, progress.total, progress.file_name
            );
        },
    )?;

    println!(
        "处理完成：成功 {} 份，失败 {} 份；符合条件销售方 {} 个。",
        result.success_count, result.failed_count, result.qualified_count
    );
    println!("报告：{}", result.output.display());
    Ok(())
}
