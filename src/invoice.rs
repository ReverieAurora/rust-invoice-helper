use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{anyhow, bail, Context, Result};
use chrono::Local;
use regex::Regex;
use rust_decimal::Decimal;

const THOUSAND_YUAN: Decimal = Decimal::from_parts(1000, 0, 0, false, 0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessingMode {
    SellerTotalAtLeast1000,
}

impl ProcessingMode {
    pub const ALL: [Self; 1] = [Self::SellerTotalAtLeast1000];

    pub const fn label(self) -> &'static str {
        match self {
            Self::SellerTotalAtLeast1000 => "销售方累计金额 ≥ 1000 元",
        }
    }

    pub const fn description(self) -> &'static str {
        match self {
            Self::SellerTotalAtLeast1000 => {
                "按销售方名称合并发票价税合计，并列出累计金额大于等于 1000.00 元的销售方。"
            }
        }
    }

    fn qualifies(self, total: Decimal) -> bool {
        match self {
            Self::SellerTotalAtLeast1000 => total >= THOUSAND_YUAN,
        }
    }

    fn ai_prompt(self) -> &'static str {
        match self {
            Self::SellerTotalAtLeast1000 => {
                "请读取本文件“发票明细”中的全部成功记录，以销售方名称进行精确分组，将同一销售方各张发票的“价税合计（元）”相加。请列出累计金额大于等于 1000.00 元的销售方，并给出销售方名称、发票数量、合计金额以及所包含的发票文件名。金额统一保留两位小数。忽略提取失败的记录，不要根据文件名猜测缺失信息。请核对计算结果与“销售方汇总”部分；如有差异，明确指出。如果没有符合条件的销售方，请直接说明“没有符合条件的销售方”。"
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct ProgressUpdate {
    pub current: usize,
    pub total: usize,
    pub file_name: String,
}

#[derive(Debug, Clone)]
pub struct ProcessResult {
    pub output: PathBuf,
    pub pdf_count: usize,
    pub success_count: usize,
    pub failed_count: usize,
    pub qualified_count: usize,
}

#[derive(Debug)]
struct InvoiceRecord {
    file_name: String,
    seller: Option<String>,
    total: Option<Decimal>,
    errors: Vec<String>,
}

impl InvoiceRecord {
    fn is_success(&self) -> bool {
        self.seller.is_some() && self.total.is_some() && self.errors.is_empty()
    }
}

#[derive(Debug, Default)]
struct SellerSummary {
    count: usize,
    total: Decimal,
    files: Vec<String>,
}

pub fn process_folder<F>(
    folder: &Path,
    output: Option<&Path>,
    mode: ProcessingMode,
    mut on_progress: F,
) -> Result<ProcessResult>
where
    F: FnMut(ProgressUpdate),
{
    if !folder.is_dir() {
        bail!("所选路径不是文件夹：{}", folder.display());
    }

    let pdf_files = find_pdf_files(folder)?;
    if pdf_files.is_empty() {
        bail!("所选文件夹中没有 PDF 文件：{}", folder.display());
    }

    let records = pdf_files
        .iter()
        .enumerate()
        .map(|(index, path)| {
            on_progress(ProgressUpdate {
                current: index + 1,
                total: pdf_files.len(),
                file_name: display_file_name(path),
            });
            process_pdf(path)
        })
        .collect::<Vec<_>>();

    let summaries = summarize(&records);
    let output = match output {
        Some(path) => {
            if path.exists() {
                bail!("为避免覆盖已有数据，输出文件已存在：{}", path.display());
            }
            path.to_owned()
        }
        None => unique_default_output(folder),
    };

    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("无法创建输出目录：{}", parent.display()))?;
    }

    let report = render_report(folder, &records, &summaries, mode);
    fs::write(&output, report.as_bytes())
        .with_context(|| format!("无法写入报告：{}", output.display()))?;

    let success_count = records.iter().filter(|record| record.is_success()).count();
    let failed_count = records.len() - success_count;
    let qualified_count = summaries
        .values()
        .filter(|summary| mode.qualifies(summary.total))
        .count();

    Ok(ProcessResult {
        output,
        pdf_count: records.len(),
        success_count,
        failed_count,
        qualified_count,
    })
}

fn find_pdf_files(folder: &Path) -> Result<Vec<PathBuf>> {
    let mut files = fs::read_dir(folder)
        .with_context(|| format!("无法读取文件夹：{}", folder.display()))?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.is_file())
        .filter(|path| {
            path.extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| extension.eq_ignore_ascii_case("pdf"))
        })
        .collect::<Vec<_>>();

    files.sort_by_key(|path| display_file_name(path).to_lowercase());
    Ok(files)
}

fn process_pdf(path: &Path) -> InvoiceRecord {
    let file_name = display_file_name(path);
    let text = match pdf_extract::extract_text(path) {
        Ok(text) if !text.trim().is_empty() => text,
        Ok(_) => {
            return InvoiceRecord {
                file_name,
                seller: None,
                total: None,
                errors: vec!["PDF 没有可提取的文字层，可能是扫描图片".to_owned()],
            };
        }
        Err(error) => {
            return InvoiceRecord {
                file_name,
                seller: None,
                total: None,
                errors: vec![format!("无法读取 PDF 文字：{error}")],
            };
        }
    };

    let seller_result = extract_seller(&text);
    let total_result = extract_total(&text);
    let mut errors = Vec::new();

    let seller = match seller_result {
        Ok(value) => Some(value),
        Err(error) => {
            errors.push(format!("销售方名称提取失败：{error}"));
            None
        }
    };
    let total = match total_result {
        Ok(value) => Some(value),
        Err(error) => {
            errors.push(format!("价税合计提取失败：{error}"));
            None
        }
    };

    InvoiceRecord {
        file_name,
        seller,
        total,
        errors,
    }
}

fn normalized_lines(text: &str) -> Vec<String> {
    text.replace('\r', "\n")
        .replace(['\u{00a0}', '\u{3000}'], " ")
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(collapse_whitespace)
        .collect()
}

fn collapse_whitespace(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn compact(value: &str) -> String {
    value
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect()
}

fn extract_seller(text: &str) -> Result<String> {
    let lines = normalized_lines(text);
    let first_money_line = lines
        .iter()
        .position(|line| line.contains('¥') || line.contains('￥'))
        .unwrap_or(lines.len());
    let tax_id = Regex::new(r"^(?:[0-9A-Z]{18}|[0-9]{15})$").expect("valid tax id regex");
    let tax_id_lines = lines
        .iter()
        .enumerate()
        .take(first_money_line)
        .filter_map(|(index, line)| tax_id.is_match(&compact(line)).then_some(index))
        .collect::<Vec<_>>();

    let seller_tax_id_line = tax_id_lines
        .last()
        .copied()
        .ok_or_else(|| anyhow!("未找到销售方纳税人识别号，无法可靠定位名称"))?;

    for line in lines[..seller_tax_id_line].iter().rev().take(6) {
        if is_possible_seller_name(line) {
            return Ok(line.trim().to_owned());
        }
    }

    bail!("已找到销售方纳税人识别号，但其前方没有可信的名称")
}

fn is_possible_seller_name(line: &str) -> bool {
    let compacted = compact(line);
    let has_cjk = compacted
        .chars()
        .any(|character| ('\u{4e00}'..='\u{9fff}').contains(&character));
    let forbidden = [
        "名称",
        "统一社会信用代码",
        "纳税人识别号",
        "发票号码",
        "开票日期",
        "购买方",
        "销售方",
        "信息",
        "电子发票",
        "项目名称",
        "规格型号",
        "价税合计",
    ];

    has_cjk
        && (2..=100).contains(&compacted.chars().count())
        && !forbidden.iter().any(|word| compacted.contains(word))
        && !compacted.contains('¥')
        && !compacted.contains('￥')
        && !compacted
            .chars()
            .all(|character| character.is_ascii_alphanumeric())
}

fn extract_total(text: &str) -> Result<Decimal> {
    let currency = Regex::new(r"[¥￥]\s*([0-9][0-9,]*\.[0-9]{2})").expect("valid currency regex");
    let amounts = currency
        .captures_iter(text)
        .filter_map(|capture| {
            let matched = capture.get(1)?;
            parse_amount(matched.as_str())
                .ok()
                .map(|amount| (matched.start(), amount))
        })
        .collect::<Vec<_>>();

    if amounts.is_empty() {
        bail!("未找到带 ¥ 或 ￥ 符号且保留两位小数的金额")
    }

    for window in amounts.windows(3) {
        if window[0].1 + window[1].1 == window[2].1 {
            return Ok(window[2].1);
        }
    }

    let uppercase_money =
        Regex::new(r"[零〇一二三四五六七八九壹贰叁肆伍陆柒捌玖拾佰仟万亿圆元角分]+(?:整|正)")
            .expect("valid uppercase money regex");
    if let Some(anchor) = uppercase_money.find(text) {
        if let Some((_, amount)) = amounts
            .iter()
            .find(|(position, _)| *position >= anchor.end())
        {
            return Ok(*amount);
        }
    }

    if amounts.len() == 1 {
        return Ok(amounts[0].1);
    }

    bail!("找到多个金额，但无法通过合计等式或大写金额位置确认价税合计")
}

fn parse_amount(value: &str) -> Result<Decimal> {
    let cleaned = value.replace(',', "");
    Decimal::from_str(&cleaned).with_context(|| format!("无效金额：{value}"))
}

fn summarize(records: &[InvoiceRecord]) -> BTreeMap<String, SellerSummary> {
    let mut summaries = BTreeMap::<String, SellerSummary>::new();

    for record in records.iter().filter(|record| record.is_success()) {
        let seller = record
            .seller
            .as_ref()
            .expect("successful record has seller");
        let total = record.total.expect("successful record has total");
        let summary = summaries.entry(seller.clone()).or_default();
        summary.count += 1;
        summary.total += total;
        summary.files.push(record.file_name.clone());
    }

    summaries
}

fn render_report(
    source_folder: &Path,
    records: &[InvoiceRecord],
    summaries: &BTreeMap<String, SellerSummary>,
    mode: ProcessingMode,
) -> String {
    let generated_at = Local::now().format("%Y-%m-%d %H:%M:%S");
    let success_count = records.iter().filter(|record| record.is_success()).count();
    let failed_count = records.len() - success_count;
    let mut output = String::new();

    output.push_str("# 发票提取报告\n\n");
    output.push_str(&format!("- 生成时间：{generated_at}\n"));
    output.push_str(&format!("- 来源目录：`{}`\n", source_folder.display()));
    output.push_str(&format!("- 处理模式：{}\n", mode.label()));
    output.push_str(&format!("- PDF 数量：{}\n", records.len()));
    output.push_str(&format!("- 成功：{success_count}\n"));
    output.push_str(&format!("- 失败：{failed_count}\n\n"));

    output.push_str("## 发票明细\n\n");
    output.push_str("| 序号 | 文件名 | 销售方名称 | 价税合计（元） | 状态 |\n");
    output.push_str("| ---: | --- | --- | ---: | --- |\n");
    for (index, record) in records.iter().enumerate() {
        let seller = record.seller.as_deref().unwrap_or("—");
        let total = record
            .total
            .map(format_money)
            .unwrap_or_else(|| "—".to_owned());
        let status = if record.is_success() {
            "成功".to_owned()
        } else {
            format!("失败：{}", record.errors.join("；"))
        };
        output.push_str(&format!(
            "| {} | {} | {} | {} | {} |\n",
            index + 1,
            markdown_cell(&record.file_name),
            markdown_cell(seller),
            total,
            markdown_cell(&status)
        ));
    }

    output.push_str("\n## 销售方汇总\n\n");
    if summaries.is_empty() {
        output.push_str("没有可用于汇总的有效记录。\n");
    } else {
        output.push_str("| 销售方名称 | 发票数量 | 合计金额（元） | 是否符合当前模式 |\n");
        output.push_str("| --- | ---: | ---: | --- |\n");
        for (seller, summary) in summaries {
            let qualified = if mode.qualifies(summary.total) {
                "是"
            } else {
                "否"
            };
            output.push_str(&format!(
                "| {} | {} | {} | {} |\n",
                markdown_cell(seller),
                summary.count,
                format_money(summary.total),
                qualified
            ));
        }
    }

    output.push_str(&format!("\n## {}\n\n", mode.label()));
    let qualified = summaries
        .iter()
        .filter(|(_, summary)| mode.qualifies(summary.total))
        .collect::<Vec<_>>();
    if qualified.is_empty() {
        output.push_str("没有符合条件的销售方。\n");
    } else {
        for (seller, summary) in qualified {
            output.push_str(&format!(
                "- **{}**：{} 张发票，合计 **{} 元**\n",
                markdown_inline(seller),
                summary.count,
                format_money(summary.total)
            ));
            for file in &summary.files {
                output.push_str(&format!("  - `{}`\n", markdown_code(file)));
            }
        }
    }

    output.push_str("\n## 提取失败的文件\n\n");
    let failed = records
        .iter()
        .filter(|record| !record.is_success())
        .collect::<Vec<_>>();
    if failed.is_empty() {
        output.push_str("无。\n");
    } else {
        for record in failed {
            output.push_str(&format!(
                "- `{}`：{}\n",
                markdown_code(&record.file_name),
                markdown_inline(&record.errors.join("；"))
            ));
        }
    }

    output.push_str("\n## 给 AI 的固定提示词\n\n");
    output.push_str("> ");
    output.push_str(mode.ai_prompt());
    output.push('\n');
    output
}

fn format_money(value: Decimal) -> String {
    format!("{:.2}", value)
}

fn markdown_cell(value: &str) -> String {
    value.replace('|', "\\|").replace(['\r', '\n'], " ")
}

fn markdown_inline(value: &str) -> String {
    value
        .replace('*', "\\*")
        .replace('_', "\\_")
        .replace(['\r', '\n'], " ")
}

fn markdown_code(value: &str) -> String {
    value.replace('`', "'").replace(['\r', '\n'], " ")
}

fn display_file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

fn unique_default_output(folder: &Path) -> PathBuf {
    let timestamp = Local::now().format("%Y-%m-%d_%H%M%S");
    let base = format!("发票汇总_{timestamp}");
    let first = folder.join(format!("{base}.md"));
    if !first.exists() {
        return first;
    }

    for suffix in 1..10_000 {
        let candidate = folder.join(format!("{base}_{suffix}.md"));
        if !candidate.exists() {
            return candidate;
        }
    }

    folder.join(format!("{base}_{}.md", Local::now().timestamp_millis()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_TEXT: &str = r#"
电子发票（普通发票）
发票号码：26442000004577882986
名称：
名称：
示例大学
12345678901234567X
轮趣科技（东莞）有限公司
91310110MA1G8ABCDX
¥32.65
¥4.25
叁拾陆圆玖角整
¥36.90
"#;

    #[test]
    fn extracts_seller_name_from_last_tax_id() {
        assert_eq!(
            extract_seller(SAMPLE_TEXT).unwrap(),
            "轮趣科技（东莞）有限公司"
        );
    }

    #[test]
    fn extracts_tax_inclusive_total_using_equation() {
        assert_eq!(extract_total(SAMPLE_TEXT).unwrap(), Decimal::new(3690, 2));
    }

    #[test]
    fn uses_amount_after_uppercase_money_as_fallback() {
        let text = "价税合计（大写） 壹佰圆整 （小写） ￥100.00";
        assert_eq!(extract_total(text).unwrap(), Decimal::new(10000, 2));
    }

    #[test]
    fn threshold_is_inclusive() {
        let mode = ProcessingMode::SellerTotalAtLeast1000;
        assert!(mode.qualifies(Decimal::new(100000, 2)));
        assert!(!mode.qualifies(Decimal::new(99999, 2)));
    }
}
