use std::io::{self, Write};
use std::path::PathBuf;
use std::process::Command;

use anyhow::{anyhow, Context, Result};
use rfd::FileDialog;

use crate::invoice::{process_folder, ProcessingMode};

pub fn run() -> Result<()> {
    print_banner();
    let mode = select_mode()?;
    let folder = select_folder()?;

    println!();
    println!("已选择目录：{}", folder.display());
    println!("处理模式：{}", mode.label());
    println!("模式说明：{}", mode.description());
    println!();
    println!("开始读取 PDF……");

    let result = process_folder(&folder, None, mode, |progress| {
        println!(
            "[{}/{}] {}",
            progress.current, progress.total, progress.file_name
        );
    })?;

    println!();
    println!("==================== 处理完成 ====================");
    println!("PDF 总数：{}", result.pdf_count);
    println!("成功提取：{}", result.success_count);
    println!("提取失败：{}", result.failed_count);
    println!("符合当前模式的销售方：{}", result.qualified_count);
    println!("报告位置：{}", result.output.display());
    println!("==================================================");

    if confirm("是否在资源管理器中显示报告？", true)? {
        reveal_in_explorer(&result.output)?;
    }

    pause_before_exit();
    Ok(())
}

fn print_banner() {
    println!("==================================================");
    println!("              Rust 发票小助手 v0.2");
    println!("==================================================");
    println!("读取电子发票的销售方名称和价税合计，并生成 Markdown 汇总报告。");
    println!();
}

fn select_mode() -> Result<ProcessingMode> {
    println!("请选择处理模式：");
    for (index, mode) in ProcessingMode::ALL.iter().enumerate() {
        println!("  {}. {}", index + 1, mode.label());
    }

    loop {
        let input = prompt("请输入序号（直接回车默认选择 1）：")?;
        let selected = if input.is_empty() {
            1
        } else {
            match input.parse::<usize>() {
                Ok(value) => value,
                Err(_) => {
                    println!("请输入有效的数字序号。");
                    continue;
                }
            }
        };

        if selected == 0 {
            println!("没有这个模式，请重新选择。");
            continue;
        }

        if let Some(mode) = ProcessingMode::ALL.get(selected - 1) {
            println!("已选择：{}", mode.label());
            return Ok(*mode);
        }
        println!("没有这个模式，请重新选择。");
    }
}

fn select_folder() -> Result<PathBuf> {
    println!();
    println!("即将打开 Windows 文件夹选择窗口。");
    let _ = prompt("按回车继续……")?;

    FileDialog::new()
        .set_title("请选择 PDF 发票所在文件夹")
        .pick_folder()
        .ok_or_else(|| anyhow!("未选择文件夹，操作已取消"))
}

fn confirm(question: &str, default_yes: bool) -> Result<bool> {
    let hint = if default_yes { "[Y/n]" } else { "[y/N]" };
    loop {
        let answer = prompt(&format!("{question} {hint} "))?.to_lowercase();
        match answer.as_str() {
            "" => return Ok(default_yes),
            "y" | "yes" | "是" => return Ok(true),
            "n" | "no" | "否" => return Ok(false),
            _ => println!("请输入 y 或 n。"),
        }
    }
}

fn prompt(message: &str) -> Result<String> {
    print!("{message}");
    io::stdout().flush().context("无法刷新终端输出")?;

    let mut input = String::new();
    io::stdin()
        .read_line(&mut input)
        .context("无法读取终端输入")?;
    Ok(input.trim().to_owned())
}

fn reveal_in_explorer(path: &std::path::Path) -> Result<()> {
    Command::new("explorer.exe")
        .arg(format!("/select,{}", path.display()))
        .spawn()
        .context("无法打开 Windows 资源管理器")?;
    Ok(())
}

pub fn pause_before_exit() {
    let _ = prompt("\n按回车键退出……");
}
