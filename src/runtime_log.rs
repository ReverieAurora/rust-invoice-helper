use std::fs;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use anyhow::{Context, Result};
use chrono::Local;

static ENTRIES: OnceLock<Mutex<Vec<String>>> = OnceLock::new();

pub fn start_session() {
    let entries = ENTRIES.get_or_init(|| Mutex::new(Vec::new()));
    if let Ok(mut entries) = entries.lock() {
        entries.clear();
    }
    info(format!(
        "程序启动 | version={} | pid={}",
        env!("CARGO_PKG_VERSION"),
        std::process::id()
    ));
}

pub fn info(message: impl AsRef<str>) {
    push("INFO", message.as_ref());
}

pub fn error(message: impl AsRef<str>) {
    push("ERROR", message.as_ref());
}

pub fn export(path: &Path) -> Result<()> {
    let entries = ENTRIES
        .get()
        .context("当前没有可导出的运行日志")?
        .lock()
        .map_err(|_| anyhow::anyhow!("运行日志内存已损坏"))?;
    let mut content = entries.join("\n");
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
    let entries = ENTRIES.get_or_init(|| Mutex::new(Vec::new()));
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
        assert!(!path.exists());
        export(&path).expect("应能主动导出运行日志");
        let content = fs::read_to_string(&path).expect("应能读取已导出的运行日志");
        assert!(content.contains("程序启动"));
        assert!(content.contains("测试处理完成"));
        fs::remove_file(path).expect("应能清理测试日志");
    }
}
