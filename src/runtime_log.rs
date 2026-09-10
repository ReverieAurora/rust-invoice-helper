use std::fs;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use anyhow::{Context, Result};
use chrono::Local;
use serde_json::json;

use crate::model::{InvoiceRecord, APP_VERSION};

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
    info(format!(
        "运行环境 | os={} | arch={} | logical_cpus={} | executable={} | working_directory={}",
        std::env::consts::OS,
        std::env::consts::ARCH,
        std::thread::available_parallelism()
            .map(|count| count.get())
            .unwrap_or(1),
        std::env::current_exe()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|_| "unknown".to_owned()),
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
            })
        })
        .collect::<Vec<_>>();
    let detail = json!({
        "progress": format!("{completed}/{total}"),
        "elapsed_ms": elapsed_ms.to_string(),
        "file_name": record.file_name,
        "source_relative": record.source_relative.display().to_string(),
        "file_size_bytes": fs::metadata(&record.source_path).map(|value| value.len()).ok(),
        "sha256": record.sha256,
        "state": record.state.label(),
        "submitter": record.submitter,
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
        "issues": issues,
    });
    push("FILE", &detail.to_string());
}

pub fn export(path: &Path) -> Result<()> {
    let entries = ENTRIES
        .get()
        .context("当前没有可导出的运行日志")?
        .lock()
        .map_err(|_| anyhow::anyhow!("运行日志内存已损坏"))?;
    let mut content = format!(
        "# Rust 发票小助手 v{} 运行诊断日志\n# 日志由用户在 TUI 中主动导出；FILE 行的诊断详情采用 JSON。\n\n",
        APP_VERSION
    );
    content.push_str(&entries.join("\n"));
    content.push('\n');
    fs::write(path, content).with_context(|| format!("无法导出运行日志：{}", path.display()))?;
    Ok(())
}

pub fn suggested_file_name() -> String {
    format!(
        "invoice-helper-{}.log",
        Local::now().format("%Y-%m-%d_%H%M%S")
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
            source_path: PathBuf::from("测试.pdf"),
            source_relative: PathBuf::from("测试.pdf"),
            file_name: "测试.pdf".to_owned(),
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
        assert!(!path.exists());
        export(&path).expect("应能主动导出运行日志");
        let content = fs::read_to_string(&path).expect("应能读取已导出的运行日志");
        assert!(content.contains("程序启动"));
        assert!(content.contains("测试处理完成"));
        assert!(content.contains("[FILE]"));
        assert!(content.contains("\"file_name\":\"测试.pdf\""));
        assert!(content.contains("\"seller_name\":\"测试销售方\""));
        assert!(content.contains("未能可靠提取价税合计"));
        fs::remove_file(path).expect("应能清理测试日志");
    }
}
