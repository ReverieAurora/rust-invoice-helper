use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use anyhow::{Context, Result};
use chrono::Local;
use serde_json::json;

use crate::model::{InvoiceRecord, IssueKind, APP_VERSION};
use crate::parser;

static ENTRIES: OnceLock<Mutex<Vec<String>>> = OnceLock::new();

pub fn start_session() {
    let entries = ENTRIES.get_or_init(|| Mutex::new(Vec::new()));
    if let Ok(mut entries) = entries.lock() {
        entries.clear();
    }
    info(format!(
        "程序启动 | version={} | pid={}",
        APP_VERSION,
        std::process::id()
    ));
    let executable = std::env::current_exe().ok();
    let executable_sha256 = executable
        .as_deref()
        .and_then(|path| parser::file_sha256(path).ok())
        .unwrap_or_else(|| "unknown".to_owned());
    info(format!(
        "运行环境 | os={} | arch={} | logical_cpus={} | executable={} | executable_sha256={} | working_directory={}",
        std::env::consts::OS,
        std::env::consts::ARCH,
        std::thread::available_parallelism()
            .map(|count| count.get())
            .unwrap_or(1),
        executable
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "unknown".to_owned()),
        executable_sha256,
        std::env::current_dir()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|_| "unknown".to_owned())
    ));
}

pub fn info(message: impl AsRef<str>) {
    push("INFO", message.as_ref());
}

pub fn error(message: impl AsRef<str>) {
    push("ERROR", message.as_ref());
}

pub fn file_result(completed: usize, total: usize, elapsed_ms: u128, record: &InvoiceRecord) {
    if ENTRIES.get().is_none() {
        return;
    }
    let issues = record
        .issues
        .iter()
        .map(|issue| {
            json!({
                "kind": issue.kind.label(),
                "reason": issue.reason,
                "action": recommended_action(issue.kind),
            })
        })
        .collect::<Vec<_>>();
    let primary_issue = record.primary_issue();
    let filename = parser::analyze_invoice_filename(&record.file_name, record.raw.total);
    let filename_amount_matches_invoice = match (filename.amount, record.raw.total) {
        (Some(file_amount), Some(invoice_total)) => Some(file_amount == invoice_total),
        _ => None,
    };
    let detail = json!({
        "schema_version": 1,
        "app_version": APP_VERSION,
        "progress": format!("{completed}/{total}"),
        "elapsed_ms": u64::try_from(elapsed_ms).unwrap_or(u64::MAX),
        "file_name": record.file_name,
        "source_relative": record.source_relative.display().to_string(),
        "file_size_bytes": fs::metadata(&record.source_path).map(|value| value.len()).ok(),
        "sha256": record.sha256,
        "state": record.state.label(),
        "submitter": record.submitter,
        "filename_analysis": {
            "parsed_amount": filename.amount.map(|value| value.to_string()),
            "invoice_total_matches": filename_amount_matches_invoice,
            "format_error": filename.format_error,
        },
        "is_invoice": record.raw.is_invoice,
        "invoice_number": record.raw.invoice_number,
        "invoice_date": record.raw.invoice_date,
        "buyer_name": record.raw.buyer_name,
        "buyer_tax_id": record.raw.buyer_tax_id,
        "seller_name": record.raw.seller_name,
        "seller_tax_id": record.raw.seller_tax_id,
        "invoice_issuer": record.raw.invoice_issuer,
        "total": record.raw.total.map(|value| value.to_string()),
        "goods": record.raw.goods,
        "is_toy": record.raw.is_toy,
        "is_red": record.raw.is_red,
        "pdf_risks": record.raw.pdf_risks,
        "primary_issue": primary_issue.map(|issue| issue.kind.label()),
        "classification_folder": primary_issue.map(|issue| issue.kind.folder_name()),
        "recommended_action": primary_issue.map(|issue| recommended_action(issue.kind)),
        "issues": issues,
    });
    push("FILE", &detail.to_string());
}

pub fn export(path: &Path) -> Result<()> {
    let content = snapshot()?;
    fs::write(path, content).with_context(|| format!("无法导出运行日志：{}", path.display()))?;
    Ok(())
}

pub fn export_to_folder(folder: &Path) -> Result<std::path::PathBuf> {
    if !folder.is_dir() {
        anyhow::bail!("日志目标不是文件夹：{}", folder.display());
    }

    let content = snapshot()?;
    let timestamp = Local::now().format("%Y-%m-%d_%H%M%S_%3f");
    for suffix in 0..1000_u16 {
        let file_name = if suffix == 0 {
            format!("invoice-helper-{timestamp}.log")
        } else {
            format!("invoice-helper-{timestamp}-{suffix}.log")
        };
        let path = folder.join(file_name);
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                file.write_all(content.as_bytes())
                    .with_context(|| format!("无法写入运行日志：{}", path.display()))?;
                return Ok(path);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("无法创建运行日志：{}", path.display()));
            }
        }
    }
    anyhow::bail!("无法生成不重复的运行日志文件名：{}", folder.display())
}

fn snapshot() -> Result<String> {
    let entries = ENTRIES
        .get()
        .context("当前没有可导出的运行日志")?
        .lock()
        .map_err(|_| anyhow::anyhow!("运行日志内存已损坏"))?;
    let mut content = format!(
        "# Rust 发票小助手 v{} 运行诊断日志\n# 日志由 TUI 生成；FILE 行采用 JSON，包含原因、分类目录和建议操作。\n# 字段值为 null 表示该信息未能取得，不代表值为空字符串。\n\n",
        APP_VERSION
    );
    content.push_str(&entries.join("\n"));
    content.push('\n');
    Ok(content)
}

fn recommended_action(kind: IssueKind) -> &'static str {
    match kind {
        IssueKind::RedInvoice => "核对是否为红冲或负数发票；不要计入普通报销，必要时联系开票方重新开具。",
        IssueKind::BuyerNameMismatch => {
            "核对购买方抬头；若不是广东工业大学，请联系开票方按正确抬头重新开具。"
        }
        IssueKind::BuyerTaxIdMismatch => {
            "核对购买方税号；若不是12440000455860226X，请联系开票方重新开具。"
        }
        IssueKind::SuspiciousPdf => {
            "从开票平台或邮件重新下载原始电子PDF，避免截图、扫描、打印转PDF或办公软件另存。"
        }
        IssueKind::FilenameFormat => {
            "按“姓名 + 物品 + 金额”或“姓名 + 金额 + 物品”重命名，并确保文件名金额与正文价税合计一致后重试。"
        }
        IssueKind::SubmitterUnknown => {
            "将文件名第一段改为真实提交人中文姓名，并用空格、下划线、短横线或加号与其他字段分隔后重试。"
        }
        IssueKind::MissingField => {
            "优先重新下载原始电子PDF；若PDF肉眼可见该字段仍无法识别，请连同本日志和对应文件哈希反馈开发者。"
        }
        IssueKind::ParseFailure => {
            "先确认PDF能正常打开，再从原渠道重新下载；仍失败时请提供本日志、文件名和SHA-256给开发者定位。"
        }
        IssueKind::Other => "根据具体原因核对源文件；无法判断时请提供本日志中的完整FILE记录。",
    }
}

pub fn suggested_file_name() -> String {
    format!(
        "invoice-helper-{}.log",
        Local::now().format("%Y-%m-%d_%H%M%S_%3f")
    )
}

fn push(level: &str, message: &str) {
    let Some(entries) = ENTRIES.get() else {
        return;
    };
    let Ok(mut entries) = entries.lock() else {
        return;
    };
    entries.push(format_entry(level, message));
}

fn format_entry(level: &str, message: &str) -> String {
    let message = message.replace(['\r', '\n'], " ");
    format!(
        "{} [{level}] {message}",
        Local::now().format("%Y-%m-%dT%H:%M:%S%.3f%:z")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{InvoiceRecord, IssueKind, RawInvoiceData, RecordState, ValidationIssue};
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn log_entry_is_kept_on_one_line() {
        let entry = format_entry("ERROR", "第一行\r\n第二行");
        assert!(entry.contains("[ERROR] 第一行  第二行"));
        assert_eq!(entry.lines().count(), 1);
    }

    #[test]
    fn every_issue_kind_has_a_concrete_recommended_action() {
        let kinds = [
            IssueKind::RedInvoice,
            IssueKind::BuyerNameMismatch,
            IssueKind::BuyerTaxIdMismatch,
            IssueKind::SuspiciousPdf,
            IssueKind::FilenameFormat,
            IssueKind::SubmitterUnknown,
            IssueKind::MissingField,
            IssueKind::ParseFailure,
            IssueKind::Other,
        ];
        for kind in kinds {
            let action = recommended_action(kind);
            assert!(action.chars().count() >= 15, "{}缺少具体建议", kind.label());
        }
    }

    #[test]
    fn session_log_is_written_only_after_explicit_export() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("系统时间应晚于Unix纪元")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "invoice-helper-log-test-{}-{unique}.log",
            std::process::id()
        ));

        start_session();
        info("测试处理完成");
        let record = InvoiceRecord {
            source_path: PathBuf::from("张三_电机_10.pdf"),
            source_relative: PathBuf::from("张三_电机_10.pdf"),
            file_name: "张三_电机_10.pdf".to_owned(),
            sha256: "abc123".to_owned(),
            submitter: Some("张三".to_owned()),
            raw: RawInvoiceData {
                is_invoice: true,
                seller_name: Some("测试销售方".to_owned()),
                pdf_risks: vec!["测试风险".to_owned()],
                ..RawInvoiceData::default()
            },
            state: RecordState::Invalid,
            issues: vec![ValidationIssue {
                kind: IssueKind::MissingField,
                reason: "未能可靠提取价税合计".to_owned(),
            }],
            seller_total: None,
            high_value_seller: false,
            export_relative: None,
        };
        file_result(1, 1, 12, &record);
        let format_record = InvoiceRecord {
            source_path: PathBuf::from("张三_电机_99.pdf"),
            source_relative: PathBuf::from("张三_电机_99.pdf"),
            file_name: "张三_电机_99.pdf".to_owned(),
            sha256: "format-hash".to_owned(),
            submitter: Some("张三".to_owned()),
            raw: RawInvoiceData {
                is_invoice: true,
                total: Some(rust_decimal::Decimal::TEN),
                ..RawInvoiceData::default()
            },
            state: RecordState::Invalid,
            issues: vec![ValidationIssue {
                kind: IssueKind::FilenameFormat,
                reason: "文件名金额 99.00 元与发票正文价税合计 10.00 元不一致".to_owned(),
            }],
            seller_total: None,
            high_value_seller: false,
            export_relative: None,
        };
        file_result(2, 3, 25, &format_record);
        let parse_record = InvoiceRecord {
            source_path: PathBuf::from("损坏.pdf"),
            source_relative: PathBuf::from("损坏.pdf"),
            file_name: "损坏.pdf".to_owned(),
            sha256: "parse-hash".to_owned(),
            submitter: None,
            raw: RawInvoiceData::default(),
            state: RecordState::Invalid,
            issues: vec![ValidationIssue {
                kind: IssueKind::ParseFailure,
                reason: "隔离解析进程异常退出".to_owned(),
            }],
            seller_total: None,
            high_value_seller: false,
            export_relative: None,
        };
        file_result(3, 3, 40, &parse_record);
        assert!(!path.exists());
        export(&path).expect("应能主动导出运行日志");
        let content = fs::read_to_string(&path).expect("应能读取已导出的运行日志");
        assert!(content.contains("程序启动"));
        assert!(content.contains("运行环境"));
        assert!(content.contains("executable_sha256="));
        assert!(content.contains("测试处理完成"));
        assert!(content.contains("[FILE]"));
        assert!(content.contains("\"file_name\":\"张三_电机_10.pdf\""));
        assert!(content.contains("\"seller_name\":\"测试销售方\""));
        assert!(content.contains("未能可靠提取价税合计"));
        assert!(content.contains("错误_文件名格式错误"));
        assert!(content.contains("重新下载原始电子PDF"));
        assert!(content.contains("SHA-256给开发者定位"));

        let file_entries = content
            .lines()
            .filter_map(|line| line.split_once(" [FILE] ").map(|(_, json)| json))
            .map(|json| serde_json::from_str::<serde_json::Value>(json).expect("FILE应为JSON"))
            .collect::<Vec<_>>();
        assert_eq!(file_entries.len(), 3);
        let format_entry = file_entries
            .iter()
            .find(|entry| entry["file_name"] == "张三_电机_99.pdf")
            .expect("应存在文件名金额错误记录");
        assert_eq!(format_entry["schema_version"], 1);
        assert_eq!(format_entry["elapsed_ms"], 25);
        assert_eq!(format_entry["filename_analysis"]["parsed_amount"], "99");
        assert_eq!(
            format_entry["filename_analysis"]["invoice_total_matches"],
            false
        );
        assert_eq!(format_entry["primary_issue"], "文件名格式错误");
        assert_eq!(format_entry["classification_folder"], "错误_文件名格式错误");
        assert!(format_entry["recommended_action"]
            .as_str()
            .is_some_and(|action| action.contains("重命名")));
        assert!(format_entry["issues"][0]["action"]
            .as_str()
            .is_some_and(|action| action.contains("正文价税合计")));

        let auto_folder = std::env::temp_dir().join(format!(
            "invoice-helper-auto-log-test-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir(&auto_folder).expect("应能创建自动日志测试目录");
        let first_auto = export_to_folder(&auto_folder).expect("应能自动导出到目标目录");
        let second_auto = export_to_folder(&auto_folder).expect("重复导出应使用不重复文件名");
        assert_eq!(first_auto.parent(), Some(auto_folder.as_path()));
        assert_eq!(second_auto.parent(), Some(auto_folder.as_path()));
        assert_ne!(first_auto, second_auto);
        assert!(first_auto.exists());
        assert!(second_auto.exists());
        assert!(fs::read_to_string(&first_auto)
            .expect("应能读取自动日志")
            .contains("错误_文件名格式错误"));

        fs::remove_file(path).expect("应能清理测试日志");
        fs::remove_dir_all(auto_folder).expect("应能清理自动日志测试目录");
    }
}
