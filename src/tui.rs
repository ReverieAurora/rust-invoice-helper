use std::io::{self, Stdout, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::cursor;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};
use ratatui::{Frame, Terminal, TerminalOptions, Viewport};
use rfd::FileDialog;

use crate::invoice::process_folder;
use crate::model::{
    ProcessResult, ProcessingMode, ProgressUpdate, APP_VERSION, DEFAULT_MAX_FILES_PER_FOLDER,
};
use crate::runtime_log;

type AppTerminal = Terminal<CrosstermBackend<Stdout>>;

const INLINE_HEIGHT: u16 = 18;
const MAX_CONTENT_WIDTH: u16 = 88;
const DEFAULT_AUTO_SAVE_LOG: bool = true;
const ACCENT: Color = Color::Rgb(217, 119, 87);
const TEXT: Color = Color::Rgb(235, 235, 235);
const MUTED: Color = Color::Rgb(128, 128, 128);
const SUBTLE: Color = Color::Rgb(82, 82, 82);
const SUCCESS: Color = Color::Rgb(94, 180, 120);
const WARNING: Color = Color::Rgb(224, 170, 90);
const ERROR: Color = Color::Rgb(220, 95, 95);

pub fn run() -> Result<()> {
    let mut terminal = TerminalSession::new()?;
    let mut remembered_mode = ProcessingMode::FullValidationExport;
    let mut remembered_batch_limit = Some(DEFAULT_MAX_FILES_PER_FOLDER);
    let mut remembered_auto_log = DEFAULT_AUTO_SAVE_LOG;

    loop {
        let SetupAction::Start {
            mode,
            source_folder,
            max_files_per_folder,
            auto_save_log,
        } = setup_screen(
            &mut terminal,
            remembered_mode,
            remembered_batch_limit,
            remembered_auto_log,
        )?
        else {
            runtime_log::info("程序退出 | interface=tui | reason=user_quit");
            return Ok(());
        };
        remembered_mode = mode;
        remembered_batch_limit = max_files_per_folder;
        remembered_auto_log = auto_save_log;
        let started = Instant::now();
        runtime_log::info(format!(
            "开始处理 | interface=tui | mode={} | source={} | batch_limit={} | auto_save_log={}",
            mode.label(),
            source_folder.display(),
            max_files_per_folder
                .map(|limit| limit.to_string())
                .unwrap_or_else(|| "unlimited".to_owned()),
            auto_save_log
        ));

        match process_with_progress(&mut terminal, &source_folder, mode, max_files_per_folder) {
            Ok(result) => {
                runtime_log::info(format!(
                    "处理完成 | interface=tui | elapsed_ms={} | pdf={} | valid={} | review={} | non_invoice={} | qualified_sellers={} | output={}",
                    started.elapsed().as_millis(),
                    result.pdf_count,
                    result.success_count,
                    result.failed_count,
                    result.skipped_count,
                    result.qualified_count,
                    result.output.display()
                ));
                let log_folder = completed_log_folder(&result, mode, &source_folder);
                let log_notice = auto_log_notice(log_folder, auto_save_log, "处理结果");
                drain_pending_events()?;
                if completion_screen(&mut terminal, &result, log_notice)? == NextAction::Exit {
                    runtime_log::info("程序退出 | interface=tui | reason=completed");
                    return Ok(());
                }
            }
            Err(error) => {
                runtime_log::error(format!(
                    "处理失败 | interface=tui | elapsed_ms={} | mode={} | source={} | error={error:#}",
                    started.elapsed().as_millis(),
                    mode.label(),
                    source_folder.display()
                ));
                let log_notice = auto_log_notice(
                    &source_folder,
                    auto_save_log,
                    "来源目录（处理失败，未生成结果目录）",
                );
                drain_pending_events()?;
                if error_screen(&mut terminal, &format!("{error:#}"), log_notice)?
                    == NextAction::Exit
                {
                    runtime_log::info("程序退出 | interface=tui | reason=error_screen");
                    return Ok(());
                }
            }
        }
    }
}

enum SetupAction {
    Start {
        mode: ProcessingMode,
        source_folder: PathBuf,
        max_files_per_folder: Option<usize>,
        auto_save_log: bool,
    },
    Quit,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum NextAction {
    Again,
    Exit,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SetupPage {
    Main,
    Settings,
}

struct SetupView<'a> {
    page: SetupPage,
    selected_main: usize,
    selected_setting: usize,
    mode: ProcessingMode,
    source_folder: Option<&'a Path>,
    max_files_per_folder: Option<usize>,
    auto_save_log: bool,
    batch_limit_input: Option<&'a str>,
    notice: &'a str,
}

fn setup_screen(
    terminal: &mut TerminalSession,
    initial_mode: ProcessingMode,
    initial_batch_limit: Option<usize>,
    initial_auto_log: bool,
) -> Result<SetupAction> {
    let mut page = SetupPage::Main;
    let mut selected_main = 0_usize;
    let mut selected_setting = 0_usize;
    let mut mode = initial_mode;
    let mut source_folder = None;
    let mut max_files_per_folder = initial_batch_limit;
    let mut auto_save_log = initial_auto_log;
    let mut batch_limit_input = None::<String>;
    let mut notice = "使用 ↑/↓ 选择，按 Enter 确认".to_owned();

    loop {
        terminal
            .terminal
            .draw(|frame| {
                render_setup(
                    frame,
                    SetupView {
                        page,
                        selected_main,
                        selected_setting,
                        mode,
                        source_folder: source_folder.as_deref(),
                        max_files_per_folder,
                        auto_save_log,
                        batch_limit_input: batch_limit_input.as_deref(),
                        notice: &notice,
                    },
                )
            })
            .context("无法绘制 TUI 设置界面")?;

        let key = read_key()?;
        if batch_limit_input.is_some() {
            match key {
                KeyCode::Char(character) if character.is_ascii_digit() => {
                    let input = batch_limit_input.as_mut().expect("编辑状态应存在输入值");
                    if input.len() < 9 {
                        input.push(character);
                    }
                }
                KeyCode::Backspace => {
                    batch_limit_input
                        .as_mut()
                        .expect("编辑状态应存在输入值")
                        .pop();
                }
                KeyCode::Enter => {
                    let input = batch_limit_input.as_deref().expect("编辑状态应存在输入值");
                    match parse_batch_limit_input(input) {
                        Ok(limit) => {
                            max_files_per_folder = limit;
                            batch_limit_input = None;
                            notice = format!(
                                "分批上限已设为{}",
                                batch_limit_description(max_files_per_folder)
                            );
                        }
                        Err(message) => notice = message.to_owned(),
                    }
                }
                KeyCode::Esc => {
                    batch_limit_input = None;
                    notice = "已取消修改分批上限".to_owned();
                }
                _ => {}
            }
            continue;
        }

        match page {
            SetupPage::Main => match key {
                KeyCode::Up => selected_main = selected_main.saturating_sub(1),
                KeyCode::Down => selected_main = (selected_main + 1).min(3),
                KeyCode::Enter => match selected_main {
                    0 => {
                        let folder = if let Some(folder) = source_folder.clone() {
                            Some(folder)
                        } else {
                            pick_source_folder(terminal)?
                        };
                        if let Some(folder) = folder {
                            return Ok(SetupAction::Start {
                                mode,
                                source_folder: folder,
                                max_files_per_folder,
                                auto_save_log,
                            });
                        }
                        notice = "已取消选择来源目录".to_owned();
                    }
                    1 => {
                        if let Some(folder) = pick_source_folder(terminal)? {
                            source_folder = Some(folder);
                            notice = "来源目录已更新".to_owned();
                        } else {
                            notice = "已取消选择来源目录".to_owned();
                        }
                    }
                    2 => {
                        page = SetupPage::Settings;
                        selected_setting = 0;
                        notice = "选择设置项并按 Enter 修改".to_owned();
                    }
                    _ => return Ok(SetupAction::Quit),
                },
                KeyCode::Esc => return Ok(SetupAction::Quit),
                _ => {}
            },
            SetupPage::Settings => match key {
                KeyCode::Up => selected_setting = selected_setting.saturating_sub(1),
                KeyCode::Down => selected_setting = (selected_setting + 1).min(4),
                KeyCode::Enter => match selected_setting {
                    0 => {
                        mode = match mode {
                            ProcessingMode::FullValidationExport => ProcessingMode::QuickSummary,
                            ProcessingMode::QuickSummary => ProcessingMode::FullValidationExport,
                        };
                        notice = format!("处理模式已切换为：{}", mode.label());
                    }
                    1 => {
                        batch_limit_input = Some(String::new());
                        notice = "输入上限；0 表示不限制，然后按 Enter 保存".to_owned();
                    }
                    2 => {
                        auto_save_log = !auto_save_log;
                        notice = if auto_save_log {
                            "自动日志已开启；成功后保存到处理结果中".to_owned()
                        } else {
                            "自动日志已关闭；仍可在设置中手动另存".to_owned()
                        };
                    }
                    3 => notice = export_log_notice(terminal)?,
                    _ => {
                        page = SetupPage::Main;
                        selected_main = 0;
                        notice = "设置已保留；选择“开始处理”继续".to_owned();
                    }
                },
                KeyCode::Esc => {
                    page = SetupPage::Main;
                    selected_main = 0;
                    notice = "已返回主菜单".to_owned();
                }
                _ => {}
            },
        }
    }
}

fn pick_source_folder(terminal: &mut TerminalSession) -> Result<Option<PathBuf>> {
    terminal.suspend()?;
    let selected = FileDialog::new()
        .set_title("请选择 PDF 发票所在文件夹")
        .pick_folder();
    terminal.resume()?;
    drain_dialog_events()?;
    Ok(selected)
}

fn export_log_notice(terminal: &mut TerminalSession) -> Result<String> {
    terminal.suspend()?;
    let selected = FileDialog::new()
        .set_title("请选择运行日志的保存位置")
        .set_file_name(runtime_log::suggested_file_name())
        .add_filter("运行日志", &["log", "txt"])
        .save_file();
    terminal.resume()?;
    drain_dialog_events()?;

    let Some(path) = selected else {
        return Ok("已取消手动导出".to_owned());
    };
    runtime_log::info(format!("用户选择导出运行日志 | path={}", path.display()));
    match runtime_log::export(&path) {
        Ok(()) => Ok(format!("运行日志已导出：{}", path.display())),
        Err(error) => {
            runtime_log::error(format!("导出运行日志失败 | error={error:#}"));
            Ok(format!("运行日志导出失败：{error:#}"))
        }
    }
}

fn completed_log_folder<'a>(
    result: &'a ProcessResult,
    mode: ProcessingMode,
    source_folder: &'a Path,
) -> &'a Path {
    match mode {
        ProcessingMode::FullValidationExport => &result.output,
        ProcessingMode::QuickSummary => result.output.parent().unwrap_or(source_folder),
    }
}

fn auto_log_notice(target_folder: &Path, enabled: bool, destination: &str) -> String {
    if !enabled {
        runtime_log::info("自动导出运行日志已关闭");
        return "自动日志已关闭；可在菜单中手动另存".to_owned();
    }

    runtime_log::info(format!(
        "准备自动导出运行日志 | destination={destination} | folder={}",
        target_folder.display()
    ));
    match runtime_log::export_to_folder(target_folder) {
        Ok(path) => format!("运行日志已自动保存：{}", path.display()),
        Err(error) => {
            runtime_log::error(format!("自动导出运行日志失败 | error={error:#}"));
            format!("运行日志自动保存失败：{error:#}；可从菜单另存")
        }
    }
}

fn process_with_progress(
    terminal: &mut TerminalSession,
    source_folder: &Path,
    mode: ProcessingMode,
    max_files_per_folder: Option<usize>,
) -> Result<ProcessResult> {
    terminal
        .terminal
        .draw(|frame| render_progress(frame, mode, source_folder, max_files_per_folder, None))
        .context("无法绘制 TUI 进度界面")?;

    let mut draw_error = None;
    let result = process_folder(
        source_folder,
        None,
        mode,
        max_files_per_folder,
        |progress| {
            if draw_error.is_some() {
                return;
            }
            if let Err(error) = terminal.terminal.draw(|frame| {
                render_progress(
                    frame,
                    mode,
                    source_folder,
                    max_files_per_folder,
                    Some(&progress),
                );
            }) {
                draw_error = Some(error);
            }
        },
    );

    if let Some(error) = draw_error {
        return Err(error).context("处理仍在继续，但无法刷新 TUI 进度");
    }
    result
}

fn completion_screen(
    terminal: &mut TerminalSession,
    result: &ProcessResult,
    initial_notice: String,
) -> Result<NextAction> {
    let mut notice = initial_notice;
    let mut selected = 0_usize;
    loop {
        terminal
            .terminal
            .draw(|frame| render_completion(frame, result, &notice, selected))
            .context("无法绘制 TUI 完成界面")?;

        match read_key()? {
            KeyCode::Up => selected = selected.saturating_sub(1),
            KeyCode::Down => selected = (selected + 1).min(2),
            KeyCode::Enter => match selected {
                0 => return Ok(NextAction::Exit),
                1 => return Ok(NextAction::Again),
                _ => notice = export_log_notice(terminal)?,
            },
            KeyCode::Esc => return Ok(NextAction::Exit),
            _ => {}
        }
    }
}

fn error_screen(
    terminal: &mut TerminalSession,
    message: &str,
    initial_notice: String,
) -> Result<NextAction> {
    let mut notice = initial_notice;
    let mut selected = 0_usize;
    loop {
        terminal
            .terminal
            .draw(|frame| render_error(frame, message, &notice, selected))
            .context("无法绘制 TUI 错误界面")?;

        match read_key()? {
            KeyCode::Up => selected = selected.saturating_sub(1),
            KeyCode::Down => selected = (selected + 1).min(2),
            KeyCode::Enter => match selected {
                0 => return Ok(NextAction::Again),
                1 => notice = export_log_notice(terminal)?,
                _ => return Ok(NextAction::Exit),
            },
            KeyCode::Esc => return Ok(NextAction::Exit),
            _ => {}
        }
    }
}

fn render_setup(frame: &mut Frame, view: SetupView<'_>) {
    let SetupView {
        page,
        selected_main,
        selected_setting,
        mode,
        source_folder,
        max_files_per_folder,
        auto_save_log,
        batch_limit_input,
        notice,
    } = view;
    frame.render_widget(Clear, frame.area());
    let area = centered_content(frame.area());
    let path = source_folder
        .map(|value| value.display().to_string())
        .unwrap_or_else(|| "尚未选择".to_owned());
    let path = truncate_middle(&path, area.width.saturating_sub(8) as usize);

    let page_name = if page == SetupPage::Main {
        "主页"
    } else {
        "设置"
    };
    let mut lines = vec![brand_line(page_name), subtitle_line(), divider(area.width)];

    if let Some(input) = batch_limit_input {
        lines.extend([
            Line::from(Span::styled(
                "  自定义分批上限",
                Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
            )),
            Line::default(),
            Line::from(vec![
                Span::styled("  数量  ", Style::default().fg(MUTED)),
                Span::styled(
                    format!("{input}▌"),
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::from(Span::styled(
                "  输入 0 表示不限制",
                Style::default().fg(SUBTLE),
            )),
            Line::default(),
        ]);
    } else if page == SetupPage::Main {
        lines.extend([
            status_line("处理模式", mode.label(), TEXT),
            status_line(
                "分批上限",
                &batch_limit_description(max_files_per_folder),
                TEXT,
            ),
            status_line(
                "运行日志",
                if auto_save_log {
                    "开启 · 自动保存到处理结果"
                } else {
                    "关闭"
                },
                if auto_save_log { SUCCESS } else { WARNING },
            ),
            status_line(
                "来源目录",
                &path,
                if source_folder.is_some() {
                    TEXT
                } else {
                    WARNING
                },
            ),
            Line::default(),
            menu_line("开始处理", "选择目录后运行", selected_main == 0),
            menu_line("选择来源目录", "只更换目录", selected_main == 1),
            menu_line("设置", "模式、分批与日志", selected_main == 2),
            menu_line("退出", "关闭程序", selected_main == 3),
        ]);
    } else {
        lines.extend([
            menu_line("处理模式", mode.label(), selected_setting == 0),
            menu_line(
                "分批上限",
                &batch_limit_description(max_files_per_folder),
                selected_setting == 1,
            ),
            menu_line(
                "自动保存日志",
                if auto_save_log { "开启" } else { "关闭" },
                selected_setting == 2,
            ),
            menu_line("手动另存日志", "选择保存位置", selected_setting == 3),
            menu_line("返回", "回到主菜单", selected_setting == 4),
            Line::default(),
        ]);
    }

    lines.extend([
        Line::from(Span::styled(
            format!("  {notice}"),
            log_notice_style(notice),
        )),
        divider(area.width),
        if batch_limit_input.is_some() {
            key_hints(&[
                ("数字", "输入"),
                ("Backspace", "删除"),
                ("Enter", "保存"),
                ("Esc", "取消"),
            ])
        } else {
            navigation_footer()
        },
    ]);

    frame.render_widget(Paragraph::new(lines), area);
}

fn render_progress(
    frame: &mut Frame,
    mode: ProcessingMode,
    source_folder: &Path,
    max_files_per_folder: Option<usize>,
    progress: Option<&ProgressUpdate>,
) {
    frame.render_widget(Clear, frame.area());
    let area = centered_content(frame.area());
    let (current, total) = progress
        .map(|value| (value.current, value.total))
        .unwrap_or((0, 0));
    let ratio = if total == 0 {
        0.0
    } else {
        (current as f64 / total as f64).clamp(0.0, 1.0)
    };
    let bar_width = (area.width as usize).saturating_sub(24).clamp(10, 36);
    let (filled, empty) = progress_segments(ratio, bar_width);
    let current_file = progress
        .map(|value| value.file_name.as_str())
        .unwrap_or("正在扫描目录中的 PDF……");
    let current_file = truncate_middle(current_file, area.width.saturating_sub(6) as usize);
    let source = truncate_middle(
        &source_folder.display().to_string(),
        area.width.saturating_sub(8) as usize,
    );

    let progress_label = if total == 0 {
        "准备中".to_owned()
    } else {
        format!("{:>3}%   {current} / {total}", (ratio * 100.0).round())
    };
    let spinner = spinner_glyph(current);
    let lines = vec![
        Line::from(vec![
            Span::styled(
                format!("{spinner} "),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "正在处理发票",
                Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled("  ", Style::default()),
            Span::styled(mode.label(), Style::default().fg(MUTED)),
        ]),
        Line::from(vec![
            Span::styled("  分批  ", Style::default().fg(SUBTLE)),
            Span::styled(
                batch_limit_description(max_files_per_folder),
                Style::default().fg(MUTED),
            ),
        ]),
        divider(area.width),
        Line::from(vec![
            Span::raw("  "),
            Span::styled(filled, Style::default().fg(ACCENT)),
            Span::styled(empty, Style::default().fg(SUBTLE)),
            Span::raw("  "),
            Span::styled(progress_label, Style::default().fg(TEXT)),
        ]),
        Line::default(),
        Line::from(vec![
            Span::styled("  当前  ", Style::default().fg(MUTED)),
            Span::styled(current_file, Style::default().fg(TEXT)),
        ]),
        Line::from(vec![
            Span::styled("  目录  ", Style::default().fg(MUTED)),
            Span::styled(source, Style::default().fg(MUTED)),
        ]),
        Line::default(),
        Line::from(Span::styled(
            "  有限并行 · 每份 PDF 独立隔离 · 单份异常不会中止整批任务",
            Style::default().fg(SUBTLE),
        )),
    ];

    frame.render_widget(Paragraph::new(lines), area);
}

fn render_completion(frame: &mut Frame, result: &ProcessResult, notice: &str, selected: usize) {
    frame.render_widget(Clear, frame.area());
    let area = centered_content(frame.area());
    let output = truncate_middle(
        &result.output.display().to_string(),
        area.width.saturating_sub(4) as usize,
    );
    let notice = truncate_middle(notice, area.width.saturating_sub(4) as usize);

    let lines = vec![
        Line::from(vec![
            Span::styled(
                "✓ ",
                Style::default().fg(SUCCESS).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "处理完成",
                Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(Span::styled(
            format!("  共处理 {} 份 PDF", result.pdf_count),
            Style::default().fg(MUTED),
        )),
        divider(area.width),
        Line::from(vec![
            metric(result.success_count, "有效发票", SUCCESS),
            separator(),
            metric(result.failed_count, "待核查", WARNING),
            separator(),
            metric(result.skipped_count, "非发票", MUTED),
        ]),
        Line::from(vec![
            Span::styled(
                format!("  {} ", result.qualified_count),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::styled("个销售方累计金额 ≥ 1000 元", Style::default().fg(MUTED)),
        ]),
        Line::default(),
        Line::from(Span::styled("  输出位置", Style::default().fg(MUTED))),
        Line::from(Span::styled(
            format!("  {output}"),
            Style::default().fg(TEXT),
        )),
        Line::default(),
        Line::from(Span::styled(
            format!("  {notice}"),
            log_notice_style(&notice),
        )),
        divider(area.width),
        menu_line("退出", "关闭程序", selected == 0),
        menu_line("再处理一个目录", "返回主菜单", selected == 1),
        menu_line("手动另存日志", "选择保存位置", selected == 2),
        navigation_footer(),
    ];

    frame.render_widget(Paragraph::new(lines), area);
}

fn render_error(frame: &mut Frame, message: &str, notice: &str, selected: usize) {
    frame.render_widget(Clear, frame.area());
    let area = centered_content(frame.area());
    let message = truncate_middle(message, area.width.saturating_sub(4) as usize * 3);
    let notice = truncate_middle(notice, area.width.saturating_sub(4) as usize);
    let lines = vec![
        Line::from(vec![
            Span::styled(
                "× ",
                Style::default().fg(ERROR).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "处理失败",
                Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
            ),
        ]),
        divider(area.width),
        Line::from(Span::styled(
            format!("  {message}"),
            Style::default().fg(ERROR),
        )),
        Line::default(),
        Line::from(Span::styled(
            format!("  {notice}"),
            log_notice_style(&notice),
        )),
        divider(area.width),
        menu_line("返回重新选择", "回到主菜单", selected == 0),
        menu_line("手动另存日志", "选择保存位置", selected == 1),
        menu_line("退出", "关闭程序", selected == 2),
        navigation_footer(),
    ];

    frame.render_widget(Paragraph::new(lines), area);
}

fn brand_line(page: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            "✦ ",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            "Rust 发票小助手",
            Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("  v{APP_VERSION}  ·  {page}"),
            Style::default().fg(MUTED),
        ),
    ])
}

fn subtitle_line() -> Line<'static> {
    Line::from(Span::styled(
        "  本地离线处理 · 文件不会上传 · 原始 PDF 保持不变",
        Style::default().fg(SUBTLE),
    ))
}

fn status_line(label: &str, value: &str, color: Color) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("  {label:<8}"), Style::default().fg(MUTED)),
        Span::styled(value.to_owned(), Style::default().fg(color)),
    ])
}

fn menu_line(label: &str, detail: &str, selected: bool) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            if selected { "  │ " } else { "    " },
            Style::default().fg(if selected { ACCENT } else { SUBTLE }),
        ),
        Span::styled(
            if selected { "❯ " } else { "  " },
            Style::default()
                .fg(if selected { ACCENT } else { SUBTLE })
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            label.to_owned(),
            Style::default()
                .fg(if selected { TEXT } else { MUTED })
                .add_modifier(if selected {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                }),
        ),
        Span::styled(
            format!("  {detail}"),
            Style::default().fg(if selected { MUTED } else { SUBTLE }),
        ),
    ])
}

fn navigation_footer() -> Line<'static> {
    key_hints(&[("↑/↓", "选择"), ("Enter", "确认")])
}

fn log_notice_style(notice: &str) -> Style {
    if notice.contains("失败") {
        Style::default().fg(ERROR)
    } else if notice.contains("已导出") {
        Style::default().fg(SUCCESS)
    } else {
        Style::default().fg(SUBTLE)
    }
}

fn batch_limit_description(limit: Option<usize>) -> String {
    limit
        .map(|value| format!("每文件夹最多 {value} 张"))
        .unwrap_or_else(|| "不限制（全部放入发票_01）".to_owned())
}

fn parse_batch_limit_input(value: &str) -> std::result::Result<Option<usize>, &'static str> {
    if value.is_empty() {
        return Err("请输入数字；0 表示不限制");
    }
    match value.parse::<usize>() {
        Ok(0) => Ok(None),
        Ok(limit) => Ok(Some(limit)),
        Err(_) => Err("数量过大，请输入较小的整数"),
    }
}

fn divider(width: u16) -> Line<'static> {
    let line_width = width.saturating_sub(4).min(56) as usize;
    Line::from(Span::styled(
        format!("  {}", "─".repeat(line_width)),
        Style::default().fg(SUBTLE),
    ))
}

fn key_hints(items: &[(&str, &str)]) -> Line<'static> {
    let mut spans = vec![Span::raw("  ")];
    for (index, (key, label)) in items.iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled("  ·  ", Style::default().fg(SUBTLE)));
        }
        spans.push(Span::styled(
            (*key).to_owned(),
            Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(
            format!(" {label}"),
            Style::default().fg(MUTED),
        ));
    }
    Line::from(spans)
}

fn metric(value: usize, label: &'static str, color: Color) -> Span<'static> {
    Span::styled(
        format!("  {value} {label}"),
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    )
}

fn separator() -> Span<'static> {
    Span::styled("  ·", Style::default().fg(SUBTLE))
}

fn centered_content(area: Rect) -> Rect {
    let width = area.width.min(MAX_CONTENT_WIDTH);
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y,
        width,
        height: area.height,
    }
}

fn progress_segments(ratio: f64, width: usize) -> (String, String) {
    let filled = ((ratio.clamp(0.0, 1.0) * width as f64).round() as usize).min(width);
    ("━".repeat(filled), "─".repeat(width - filled))
}

fn spinner_glyph(step: usize) -> &'static str {
    const FRAMES: [&str; 4] = ["◐", "◓", "◑", "◒"];
    FRAMES[step % FRAMES.len()]
}

fn truncate_middle(value: &str, max_chars: usize) -> String {
    let count = value.chars().count();
    if count <= max_chars {
        return value.to_owned();
    }
    if max_chars <= 1 {
        return "…".chars().take(max_chars).collect();
    }

    let available = max_chars - 1;
    let left_count = available.div_ceil(2);
    let right_count = available / 2;
    let left = value.chars().take(left_count).collect::<String>();
    let right = value
        .chars()
        .rev()
        .take(right_count)
        .collect::<String>()
        .chars()
        .rev()
        .collect::<String>();
    format!("{left}…{right}")
}

fn read_key() -> Result<KeyCode> {
    loop {
        if let Event::Key(key) = event::read().context("无法读取键盘输入")? {
            if key.kind == KeyEventKind::Press {
                return Ok(key.code);
            }
        }
    }
}

fn drain_pending_events() -> Result<()> {
    while event::poll(Duration::ZERO).context("无法检查终端输入")? {
        let _ = event::read().context("无法清理终端输入")?;
    }
    Ok(())
}

fn drain_dialog_events() -> Result<()> {
    const QUIET_PERIOD: Duration = Duration::from_millis(150);
    while event::poll(QUIET_PERIOD).context("无法等待原生窗口按键释放")? {
        let _ = event::read().context("无法清理原生窗口残留输入")?;
    }
    Ok(())
}

struct TerminalSession {
    terminal: AppTerminal,
    active: bool,
}

impl TerminalSession {
    fn new() -> Result<Self> {
        enable_raw_mode().context("无法启用终端原始输入模式")?;

        let mut stdout = io::stdout();
        if let Err(error) = execute!(stdout, cursor::Hide) {
            let _ = disable_raw_mode();
            return Err(error).context("无法隐藏终端光标");
        }

        let backend = CrosstermBackend::new(stdout);
        let options = TerminalOptions {
            viewport: Viewport::Inline(INLINE_HEIGHT),
        };
        let terminal = match Terminal::with_options(backend, options) {
            Ok(terminal) => terminal,
            Err(error) => {
                let _ = disable_raw_mode();
                let _ = execute!(io::stdout(), cursor::Show);
                return Err(error).context("无法初始化内联 TUI 终端");
            }
        };

        Ok(Self {
            terminal,
            active: true,
        })
    }

    fn suspend(&mut self) -> Result<()> {
        disable_raw_mode().context("无法暂停终端原始输入模式")?;
        self.terminal.show_cursor().context("无法显示终端光标")?;
        self.active = false;
        Ok(())
    }

    fn resume(&mut self) -> Result<()> {
        enable_raw_mode().context("无法恢复终端原始输入模式")?;
        self.active = true;
        self.terminal.hide_cursor().context("无法隐藏终端光标")?;
        self.terminal.clear().context("无法刷新内联 TUI")?;
        Ok(())
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let _ = disable_raw_mode();
        let _ = self.terminal.show_cursor();
    }
}

pub fn pause_before_exit() {
    print!("\n按回车键退出……");
    let _ = io::stdout().flush();
    let mut input = String::new();
    let _ = io::stdin().read_line(&mut input);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn middle_truncation_keeps_both_path_ends() {
        assert_eq!(truncate_middle("1234567890", 7), "123…890");
        assert_eq!(truncate_middle("短路径", 10), "短路径");
    }

    #[test]
    fn progress_segments_keep_requested_width() {
        let (filled, empty) = progress_segments(0.5, 10);
        assert_eq!(filled.chars().count(), 5);
        assert_eq!(empty.chars().count(), 5);
    }

    #[test]
    fn batch_limit_input_supports_custom_and_unlimited_values() {
        assert_eq!(parse_batch_limit_input("30"), Ok(Some(30)));
        assert_eq!(parse_batch_limit_input("1"), Ok(Some(1)));
        assert_eq!(parse_batch_limit_input("0"), Ok(None));
        assert!(parse_batch_limit_input("").is_err());
    }

    #[test]
    fn completed_log_uses_the_processing_result_location() {
        let source = Path::new(r"C:\发票");
        let full_result = ProcessResult {
            output: PathBuf::from(r"C:\发票\发票处理结果_20261004"),
            pdf_count: 1,
            success_count: 1,
            failed_count: 0,
            skipped_count: 0,
            qualified_count: 0,
        };
        assert_eq!(
            completed_log_folder(&full_result, ProcessingMode::FullValidationExport, source),
            full_result.output
        );

        let quick_result = ProcessResult {
            output: PathBuf::from(r"C:\发票\发票汇总_20261004.md"),
            ..full_result
        };
        assert_eq!(
            completed_log_folder(&quick_result, ProcessingMode::QuickSummary, source),
            Path::new(r"C:\发票")
        );
    }
}
