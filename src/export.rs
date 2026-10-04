use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use chrono::Local;
use rust_decimal::Decimal;
use rust_xlsxwriter::{Workbook, Worksheet};

use crate::model::{InvoiceRecord, ProcessingMode, RecordState, SellerSummaries};
use crate::parser;

pub fn export_full(
    source_folder: &Path,
    output_parent: Option<&Path>,
    records: &mut [InvoiceRecord],
    summaries: &SellerSummaries,
    mode: ProcessingMode,
    max_files_per_folder: Option<usize>,
) -> Result<PathBuf> {
    let parent = output_parent.unwrap_or(source_folder);
    if parent.exists() && !parent.is_dir() {
        bail!("完整模式的输出路径必须是文件夹：{}", parent.display());
    }
    fs::create_dir_all(parent)
        .with_context(|| format!("无法创建输出父目录：{}", parent.display()))?;

    let (staging, final_path) = unique_result_paths(parent);
    fs::create_dir(&staging)
        .with_context(|| format!("无法创建临时结果目录：{}", staging.display()))?;

    copy_classified_files(&staging, records, max_files_per_folder)?;
    write_workbooks(&staging, records, summaries, mode)?;
    write_report(
        &staging.join("处理报告.md"),
        source_folder,
        records,
        summaries,
        mode,
        max_files_per_folder,
    )?;

    fs::rename(&staging, &final_path).with_context(|| {
        format!(
            "结果已生成，但无法将临时目录改名：{} -> {}",
            staging.display(),
            final_path.display()
        )
    })?;
    Ok(final_path)
}

pub fn export_quick(
    source_folder: &Path,
    output: Option<&Path>,
    records: &[InvoiceRecord],
    summaries: &SellerSummaries,
    mode: ProcessingMode,
) -> Result<PathBuf> {
    let path = match output {
        Some(path) => {
            if path.exists() {
                bail!("为避免覆盖已有数据，输出文件已存在：{}", path.display());
            }
            path.to_path_buf()
        }
        None => unique_report_path(source_folder),
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("无法创建输出目录：{}", parent.display()))?;
    }
    write_report(&path, source_folder, records, summaries, mode, None)?;
    Ok(path)
}

fn copy_classified_files(
    root: &Path,
    records: &mut [InvoiceRecord],
    max_files_per_folder: Option<usize>,
) -> Result<()> {
    for (index, batch) in ordinary_assignments(records, max_files_per_folder) {
        let record = &mut records[index];
        let mut relative = PathBuf::from(format!("发票_{batch:02}"));
        if record.high_value_seller {
            relative.push(safe_component(
                record.submitter.as_deref().unwrap_or("提交人待确认"),
            ));
        }
        relative.push(&record.file_name);
        copy_record(root, record, relative)?;
    }

    let mut toy = records
        .iter()
        .enumerate()
        .filter(|(_, record)| record.state == RecordState::Valid && record.raw.is_toy)
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    toy.sort_by_key(|index| record_sort_key(&records[*index]));
    for index in toy {
        let record = &mut records[index];
        let mut relative = PathBuf::from("玩具类发票");
        if record.high_value_seller {
            relative.push(safe_component(
                record.submitter.as_deref().unwrap_or("提交人待确认"),
            ));
        }
        relative.push(&record.file_name);
        copy_record(root, record, relative)?;
    }

    for record in records
        .iter_mut()
        .filter(|record| record.state == RecordState::Invalid)
    {
        let folder = record
            .primary_issue()
            .map(|issue| issue.kind.folder_name())
            .unwrap_or("错误_其他");
        copy_record(root, record, Path::new(folder).join(&record.file_name))?;
    }
    for record in records
        .iter_mut()
        .filter(|record| record.state == RecordState::NonInvoice)
    {
        copy_record(root, record, Path::new("非发票PDF").join(&record.file_name))?;
    }
    Ok(())
}

fn ordinary_assignments(
    records: &[InvoiceRecord],
    max_files_per_folder: Option<usize>,
) -> Vec<(usize, usize)> {
    let limit = max_files_per_folder
        .filter(|limit| *limit > 0)
        .unwrap_or(usize::MAX);
    let mut high_by_seller = BTreeMap::<String, Vec<usize>>::new();
    let mut ordinary = Vec::new();
    for (index, record) in records
        .iter()
        .enumerate()
        .filter(|(_, record)| record.state == RecordState::Valid && !record.raw.is_toy)
    {
        if record.high_value_seller {
            high_by_seller
                .entry(record.raw.seller_name.clone().unwrap_or_default())
                .or_default()
                .push(index);
        } else {
            ordinary.push(index);
        }
    }

    let mut assignments = Vec::new();
    let mut batch = 1;
    let mut used = 0;
    for indices in high_by_seller.values_mut() {
        indices.sort_by_key(|index| record_sort_key(&records[*index]));
        // 若当前批次放不下整家公司，则从新批次开始；只有单家公司自身
        // 超过 30 张时才拆分，避免可避免的跨目录分散。
        if used > 0 && indices.len() > limit - used {
            batch += 1;
            used = 0;
        }
        for index in indices.iter().copied() {
            if used == limit {
                batch += 1;
                used = 0;
            }
            assignments.push((index, batch));
            used += 1;
        }
    }

    ordinary.sort_by_key(|index| records[*index].file_name.to_lowercase());
    for index in ordinary {
        if used == limit {
            batch += 1;
            used = 0;
        }
        assignments.push((index, batch));
        used += 1;
    }
    assignments
}

fn record_sort_key(record: &InvoiceRecord) -> (u8, String, String, String) {
    (
        u8::from(!record.high_value_seller),
        record
            .raw
            .seller_name
            .as_deref()
            .unwrap_or_default()
            .to_lowercase(),
        record
            .submitter
            .as_deref()
            .unwrap_or_default()
            .to_lowercase(),
        record.file_name.to_lowercase(),
    )
}

fn copy_record(root: &Path, record: &mut InvoiceRecord, relative: PathBuf) -> Result<()> {
    let requested = root.join(relative);
    let destination = unique_destination(&requested);
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("无法创建分类目录：{}", parent.display()))?;
    }
    fs::copy(&record.source_path, &destination).with_context(|| {
        format!(
            "无法复制文件：{} -> {}",
            record.source_path.display(),
            destination.display()
        )
    })?;

    let copied_hash = parser::file_sha256(&destination)?;
    if record.sha256.is_empty() {
        record.sha256 = parser::file_sha256(&record.source_path)?;
    }
    if copied_hash != record.sha256 {
        bail!("复制校验失败，源文件与副本不同：{}", record.file_name);
    }
    record.export_relative = Some(
        destination
            .strip_prefix(root)
            .unwrap_or(&destination)
            .to_path_buf(),
    );
    Ok(())
}

fn unique_destination(requested: &Path) -> PathBuf {
    if !requested.exists() {
        return requested.to_path_buf();
    }
    let parent = requested.parent().unwrap_or_else(|| Path::new("."));
    let stem = requested
        .file_stem()
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_else(|| "文件".to_owned());
    let extension = requested.extension().map(|value| value.to_string_lossy());
    for suffix in 2..10_000 {
        let name = match &extension {
            Some(extension) => format!("{stem}_{suffix}.{extension}"),
            None => format!("{stem}_{suffix}"),
        };
        let candidate = parent.join(name);
        if !candidate.exists() {
            return candidate;
        }
    }
    parent.join(format!("{stem}_{}", Local::now().timestamp_millis()))
}

fn safe_component(value: &str) -> String {
    let mut result = value
        .chars()
        .map(|character| {
            if character.is_control() || r#"<>:"/\|?*"#.contains(character) {
                '_'
            } else {
                character
            }
        })
        .collect::<String>();
    result = result.trim_matches([' ', '.']).to_owned();
    if result.is_empty() {
        "未命名".to_owned()
    } else {
        result
    }
}

fn write_workbooks(
    root: &Path,
    records: &[InvoiceRecord],
    summaries: &SellerSummaries,
    mode: ProcessingMode,
) -> Result<()> {
    if has_summary_workbook_data(records, summaries) {
        write_summary_workbook(&root.join("汇总表.xlsx"), records, summaries, mode)?;
    }
    if has_submitter_workbook_data(records) {
        write_submitter_workbook(&root.join("个人开票金额统计.xlsx"), records)?;
    }
    if has_error_workbook_data(records) {
        write_error_workbook(&root.join("错误追查表.xlsx"), records)?;
    }
    if has_payment_workbook_data(records) {
        write_payment_workbook(&root.join("支付截图补充表.xlsx"), records)?;
    }
    Ok(())
}

fn has_summary_workbook_data(records: &[InvoiceRecord], summaries: &SellerSummaries) -> bool {
    !records.is_empty() || !summaries.is_empty()
}

fn has_submitter_workbook_data(records: &[InvoiceRecord]) -> bool {
    records
        .iter()
        .any(|record| record.state == RecordState::Valid && record.raw.total.is_some())
}

fn has_error_workbook_data(records: &[InvoiceRecord]) -> bool {
    records
        .iter()
        .any(|record| record.state != RecordState::Valid)
}

fn has_payment_workbook_data(records: &[InvoiceRecord]) -> bool {
    records
        .iter()
        .any(|record| record.state == RecordState::Valid && record.high_value_seller)
}

fn generated_workbook_names(
    records: &[InvoiceRecord],
    summaries: &SellerSummaries,
) -> Vec<&'static str> {
    let mut names = Vec::new();
    if has_summary_workbook_data(records, summaries) {
        names.push("汇总表.xlsx");
    }
    if has_submitter_workbook_data(records) {
        names.push("个人开票金额统计.xlsx");
    }
    if has_error_workbook_data(records) {
        names.push("错误追查表.xlsx");
    }
    if has_payment_workbook_data(records) {
        names.push("支付截图补充表.xlsx");
    }
    names
}

fn write_summary_workbook(
    path: &Path,
    records: &[InvoiceRecord],
    summaries: &SellerSummaries,
    mode: ProcessingMode,
) -> Result<()> {
    let mut workbook = Workbook::new();
    {
        let sheet = workbook.add_worksheet();
        sheet.set_name("销售方汇总")?;
        write_row(
            sheet,
            0,
            &["销售方名称", "有效发票数", "累计金额（元）", "是否超1k"],
        )?;
        for (row, (seller, summary)) in summaries.iter().enumerate() {
            write_row(
                sheet,
                row as u32 + 1,
                &[
                    seller,
                    &summary.count.to_string(),
                    &money(summary.total),
                    if mode.qualifies(summary.total) {
                        "是"
                    } else {
                        "否"
                    },
                ],
            )?;
        }
        set_widths(sheet, &[36.0, 14.0, 18.0, 12.0])?;
    }
    {
        let sheet = workbook.add_worksheet();
        sheet.set_name("发票明细")?;
        write_row(
            sheet,
            0,
            &[
                "序号",
                "源相对路径",
                "提交人",
                "票面开票人",
                "销售方名称",
                "销售方税号",
                "价税合计（元）",
                "发票号码",
                "开票日期",
                "玩具类",
                "状态",
                "分类后路径",
                "SHA-256",
            ],
        )?;
        for (index, record) in records.iter().enumerate() {
            write_row(
                sheet,
                index as u32 + 1,
                &[
                    &(index + 1).to_string(),
                    &record.source_relative.display().to_string(),
                    record.submitter.as_deref().unwrap_or("提交人待确认"),
                    record.raw.invoice_issuer.as_deref().unwrap_or(""),
                    record.raw.seller_name.as_deref().unwrap_or(""),
                    record.raw.seller_tax_id.as_deref().unwrap_or(""),
                    &record.raw.total.map(money).unwrap_or_default(),
                    record.raw.invoice_number.as_deref().unwrap_or(""),
                    record.raw.invoice_date.as_deref().unwrap_or(""),
                    if record.raw.is_toy { "是" } else { "否" },
                    record.state.label(),
                    &display_relative(record),
                    &record.sha256,
                ],
            )?;
        }
        set_widths(
            sheet,
            &[
                8.0, 42.0, 15.0, 15.0, 36.0, 24.0, 18.0, 24.0, 14.0, 10.0, 14.0, 48.0, 68.0,
            ],
        )?;
    }
    workbook
        .save(path)
        .with_context(|| format!("无法生成 Excel：{}", path.display()))?;
    Ok(())
}

fn write_submitter_workbook(path: &Path, records: &[InvoiceRecord]) -> Result<()> {
    let mut totals = BTreeMap::<String, (usize, Decimal, usize)>::new();
    for record in records
        .iter()
        .filter(|record| record.state == RecordState::Valid)
    {
        let Some(total) = record.raw.total else {
            continue;
        };
        let submitter = record.submitter.as_deref().unwrap_or("提交人待确认");
        let entry = totals.entry(submitter.to_owned()).or_default();
        entry.0 += 1;
        entry.1 += total;
        entry.2 += usize::from(record.raw.is_toy);
    }
    let mut workbook = Workbook::new();
    let sheet = workbook.add_worksheet();
    sheet.set_name("个人统计")?;
    write_row(
        sheet,
        0,
        &[
            "提交人",
            "有效发票数",
            "有效金额合计（元）",
            "其中玩具发票数",
        ],
    )?;
    for (row, (submitter, (count, total, toy_count))) in totals.iter().enumerate() {
        write_row(
            sheet,
            row as u32 + 1,
            &[
                submitter,
                &count.to_string(),
                &money(*total),
                &toy_count.to_string(),
            ],
        )?;
    }
    set_widths(sheet, &[18.0, 14.0, 22.0, 18.0])?;
    workbook.save(path)?;
    Ok(())
}

fn write_error_workbook(path: &Path, records: &[InvoiceRecord]) -> Result<()> {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_worksheet();
    sheet.set_name("错误与非发票")?;
    write_row(
        sheet,
        0,
        &[
            "序号",
            "文件名",
            "分类",
            "主要问题",
            "全部问题",
            "提交人",
            "分类后路径",
            "SHA-256",
        ],
    )?;
    for (row, record) in records
        .iter()
        .filter(|record| record.state != RecordState::Valid)
        .enumerate()
    {
        let all_issues = record
            .issues
            .iter()
            .map(|issue| format!("{}：{}", issue.kind.label(), issue.reason))
            .collect::<Vec<_>>()
            .join("；");
        write_row(
            sheet,
            row as u32 + 1,
            &[
                &(row + 1).to_string(),
                &record.file_name,
                record.state.label(),
                record
                    .primary_issue()
                    .map(|issue| issue.kind.label())
                    .unwrap_or(""),
                &all_issues,
                record.submitter.as_deref().unwrap_or("提交人待确认"),
                &display_relative(record),
                &record.sha256,
            ],
        )?;
    }
    set_widths(sheet, &[8.0, 42.0, 16.0, 24.0, 72.0, 16.0, 48.0, 68.0])?;
    workbook.save(path)?;
    Ok(())
}

fn write_payment_workbook(path: &Path, records: &[InvoiceRecord]) -> Result<()> {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_worksheet();
    sheet.set_name("待补支付截图")?;
    write_row(
        sheet,
        0,
        &[
            "提交人",
            "销售方名称",
            "销售方累计（元）",
            "本张金额（元）",
            "发票文件名",
            "支付截图状态",
            "备注",
        ],
    )?;
    for (row, record) in records
        .iter()
        .filter(|record| record.state == RecordState::Valid && record.high_value_seller)
        .enumerate()
    {
        write_row(
            sheet,
            row as u32 + 1,
            &[
                record.submitter.as_deref().unwrap_or("提交人待确认"),
                record.raw.seller_name.as_deref().unwrap_or(""),
                &record.seller_total.map(money).unwrap_or_default(),
                &record.raw.total.map(money).unwrap_or_default(),
                &record.file_name,
                "待补充",
                "",
            ],
        )?;
    }
    set_widths(sheet, &[18.0, 36.0, 20.0, 18.0, 42.0, 18.0, 36.0])?;
    workbook.save(path)?;
    Ok(())
}

fn write_row(sheet: &mut Worksheet, row: u32, values: &[&str]) -> Result<()> {
    for (column, value) in values.iter().enumerate() {
        sheet.write_string(row, column as u16, *value)?;
    }
    Ok(())
}

fn set_widths(sheet: &mut Worksheet, widths: &[f64]) -> Result<()> {
    for (column, width) in widths.iter().enumerate() {
        sheet.set_column_width(column as u16, *width)?;
    }
    Ok(())
}

fn write_report(
    path: &Path,
    source_folder: &Path,
    records: &[InvoiceRecord],
    summaries: &SellerSummaries,
    mode: ProcessingMode,
    max_files_per_folder: Option<usize>,
) -> Result<()> {
    let valid = records
        .iter()
        .filter(|record| record.state == RecordState::Valid)
        .count();
    let invalid = records
        .iter()
        .filter(|record| record.state == RecordState::Invalid)
        .count();
    let non_invoice = records
        .iter()
        .filter(|record| record.state == RecordState::NonInvoice)
        .count();
    let batch_setting = match (mode, max_files_per_folder) {
        (ProcessingMode::FullValidationExport, Some(limit)) => {
            format!("每个发票文件夹最多 {limit} 张")
        }
        (ProcessingMode::FullValidationExport, None) => {
            "不限制，普通有效发票全部放入发票_01".to_owned()
        }
        (ProcessingMode::QuickSummary, _) => "不适用（快速模式不复制 PDF）".to_owned(),
    };
    let generated_excel = if mode == ProcessingMode::FullValidationExport {
        let names = generated_workbook_names(records, summaries);
        if names.is_empty() {
            "无（没有可写入表格的数据）".to_owned()
        } else {
            names.join("、")
        }
    } else {
        "无（快速汇总模式不生成 Excel）".to_owned()
    };
    let mut output = format!(
        "# 发票处理报告\n\n- 生成时间：{}\n- 来源目录：`{}`\n- 处理模式：{}\n- 分类分批：{}\n- 生成 Excel：{}\n- PDF 总数：{}\n- 有效发票：{}\n- 无效/待核查发票：{}\n- 非发票 PDF：{}\n- 统计阈值：销售方有效发票累计金额 ≥ 1000.00 元\n\n",
        Local::now().format("%Y-%m-%d %H:%M:%S"),
        source_folder.display(),
        mode.label(),
        batch_setting,
        generated_excel,
        records.len(),
        valid,
        invalid,
        non_invoice
    );
    output.push_str("## 销售方汇总\n\n| 销售方名称 | 有效发票数 | 合计金额（元） | 超1k |\n| --- | ---: | ---: | --- |\n");
    for (seller, summary) in summaries {
        output.push_str(&format!(
            "| {} | {} | {} | {} |\n",
            md_cell(seller),
            summary.count,
            money(summary.total),
            if mode.qualifies(summary.total) {
                "是"
            } else {
                "否"
            }
        ));
    }
    if summaries.is_empty() {
        output.push_str("| — | 0 | 0.00 | 否 |\n");
    }

    output.push_str("\n## 发票明细\n\n| 文件名 | 提交人 | 销售方 | 价税合计（元） | 状态 | 问题 |\n| --- | --- | --- | ---: | --- | --- |\n");
    for record in records {
        let issues = record
            .issues
            .iter()
            .map(|issue| issue.reason.as_str())
            .collect::<Vec<_>>()
            .join("；");
        output.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} |\n",
            md_cell(&record.file_name),
            md_cell(record.submitter.as_deref().unwrap_or("提交人待确认")),
            md_cell(record.raw.seller_name.as_deref().unwrap_or("—")),
            record
                .raw
                .total
                .map(money)
                .unwrap_or_else(|| "—".to_owned()),
            record.state.label(),
            md_cell(&issues)
        ));
    }

    output.push_str("\n## 累计金额大于等于 1000 元的销售方\n\n");
    let qualified = summaries
        .iter()
        .filter(|(_, summary)| mode.qualifies(summary.total))
        .collect::<Vec<_>>();
    if qualified.is_empty() {
        output.push_str("没有符合条件的销售方。\n");
    } else {
        for (seller, summary) in qualified {
            output.push_str(&format!(
                "- {}：{} 张，合计 {} 元\n",
                seller,
                summary.count,
                money(summary.total)
            ));
            for file in &summary.files {
                output.push_str(&format!("  - `{}`\n", file.replace('`', "'")));
            }
        }
    }

    output.push_str("\n## 给 AI 的固定提示词\n\n> ");
    output.push_str(mode.ai_prompt());
    output.push('\n');
    fs::write(path, output.as_bytes())
        .with_context(|| format!("无法生成 Markdown 报告：{}", path.display()))?;
    Ok(())
}

fn money(value: Decimal) -> String {
    format!("{value:.2}")
}

fn display_relative(record: &InvoiceRecord) -> String {
    record
        .export_relative
        .as_ref()
        .map(|path| path.display().to_string())
        .unwrap_or_default()
}

fn md_cell(value: &str) -> String {
    value.replace('|', "\\|").replace(['\r', '\n'], " ")
}

fn unique_result_paths(parent: &Path) -> (PathBuf, PathBuf) {
    let timestamp = Local::now().format("%Y-%m-%d_%H%M%S");
    for suffix in 0..10_000 {
        let suffix = if suffix == 0 {
            String::new()
        } else {
            format!("_{suffix}")
        };
        let final_path = parent.join(format!("发票处理结果_{timestamp}{suffix}"));
        let staging = parent.join(format!(".发票处理结果_{timestamp}{suffix}_处理中"));
        if !final_path.exists() && !staging.exists() {
            return (staging, final_path);
        }
    }
    let unique = Local::now().timestamp_millis();
    (
        parent.join(format!(".发票处理结果_{unique}_处理中")),
        parent.join(format!("发票处理结果_{unique}")),
    )
}

fn unique_report_path(folder: &Path) -> PathBuf {
    let timestamp = Local::now().format("%Y-%m-%d_%H%M%S");
    for suffix in 0..10_000 {
        let suffix = if suffix == 0 {
            String::new()
        } else {
            format!("_{suffix}")
        };
        let path = folder.join(format!("发票汇总_{timestamp}{suffix}.md"));
        if !path.exists() {
            return path;
        }
    }
    folder.join(format!("发票汇总_{}.md", Local::now().timestamp_millis()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::RawInvoiceData;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn sanitizes_windows_path_component() {
        assert_eq!(safe_component(" 张<三>: "), "张_三__");
        assert_eq!(safe_component("..."), "未命名");
    }

    #[test]
    fn keeps_high_value_seller_together_when_group_fits_one_batch() {
        let mut records = Vec::new();
        for index in 0..20 {
            records.push(test_record(&format!("甲_{index}.pdf"), "甲公司"));
        }
        for index in 0..20 {
            records.push(test_record(&format!("乙_{index}.pdf"), "乙公司"));
        }

        let assignments = ordinary_assignments(&records, Some(30));
        let first_company = assignments[..20]
            .iter()
            .map(|(_, batch)| *batch)
            .collect::<Vec<_>>();
        let second_company = assignments[20..]
            .iter()
            .map(|(_, batch)| *batch)
            .collect::<Vec<_>>();
        assert!(first_company.iter().all(|batch| *batch == 1));
        assert!(second_company.iter().all(|batch| *batch == 2));
    }

    #[test]
    fn unlimited_batch_keeps_all_ordinary_invoices_together() {
        let mut records = Vec::new();
        for index in 0..40 {
            records.push(test_record(&format!("甲_{index}.pdf"), "甲公司"));
        }
        for index in 0..40 {
            records.push(test_record(&format!("乙_{index}.pdf"), "乙公司"));
        }

        let assignments = ordinary_assignments(&records, None);
        assert_eq!(assignments.len(), 80);
        assert!(assignments.iter().all(|(_, batch)| *batch == 1));
    }

    #[test]
    fn custom_batch_limit_is_respected() {
        let records = (0..5)
            .map(|index| test_record(&format!("甲_{index}.pdf"), "甲公司"))
            .collect::<Vec<_>>();

        let batches = ordinary_assignments(&records, Some(2))
            .into_iter()
            .map(|(_, batch)| batch)
            .collect::<Vec<_>>();
        assert_eq!(batches, vec![1, 1, 2, 2, 3]);
    }

    #[test]
    fn workbooks_are_generated_only_when_they_have_data_rows() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("系统时间应晚于Unix纪元")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "invoice-helper-workbook-test-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir(&root).expect("应能创建测试目录");
        let summaries = SellerSummaries::new();

        write_workbooks(&root, &[], &summaries, ProcessingMode::FullValidationExport)
            .expect("空数据不应生成表格");
        assert_eq!(fs::read_dir(&root).expect("应能读取测试目录").count(), 0);

        let mut valid = test_record("张三_电机_10.pdf", "甲公司");
        valid.high_value_seller = false;
        valid.raw.total = Some(Decimal::TEN);
        write_workbooks(
            &root,
            &[valid],
            &summaries,
            ProcessingMode::FullValidationExport,
        )
        .expect("应能生成有数据的表格");
        assert!(root.join("汇总表.xlsx").is_file());
        assert!(root.join("个人开票金额统计.xlsx").is_file());
        assert!(!root.join("错误追查表.xlsx").exists());
        assert!(!root.join("支付截图补充表.xlsx").exists());

        let mut invalid = test_record("错误.pdf", "");
        invalid.state = RecordState::Invalid;
        invalid.high_value_seller = false;
        write_workbooks(
            &root,
            &[invalid],
            &summaries,
            ProcessingMode::FullValidationExport,
        )
        .expect("存在错误记录时应能生成错误追查表");
        assert!(root.join("错误追查表.xlsx").is_file());

        fs::remove_dir_all(root).expect("应能清理测试目录");
    }

    fn test_record(file_name: &str, seller: &str) -> InvoiceRecord {
        let raw = RawInvoiceData {
            seller_name: Some(seller.to_owned()),
            ..RawInvoiceData::default()
        };
        InvoiceRecord {
            source_path: PathBuf::from(file_name),
            source_relative: PathBuf::from(file_name),
            file_name: file_name.to_owned(),
            sha256: String::new(),
            submitter: Some("张三".to_owned()),
            raw,
            state: RecordState::Valid,
            issues: Vec::new(),
            seller_total: None,
            high_value_seller: true,
            export_relative: None,
        }
    }
}
