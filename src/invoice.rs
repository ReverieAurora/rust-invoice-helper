use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread;

use anyhow::{bail, Context, Result};

use crate::export;
use crate::model::{
    InvoiceRecord, IssueKind, ProcessResult, ProcessingMode, ProgressUpdate, RawInvoiceData,
    RecordState, SellerSummaries, ValidationIssue, EXPECTED_BUYER_NAME, EXPECTED_BUYER_TAX_ID,
};
use crate::parser;

const MAX_PARALLEL_PDFS: usize = 8;

pub fn process_folder<F>(
    folder: &Path,
    output: Option<&Path>,
    mode: ProcessingMode,
    max_files_per_folder: Option<usize>,
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
        bail!("所选文件夹第一层中没有 PDF 文件：{}", folder.display());
    }

    let mut records = process_pdfs(folder, &pdf_files, mode, &mut on_progress);

    let summaries = summarize(&records);
    for record in &mut records {
        record.seller_total = record
            .raw
            .seller_name
            .as_ref()
            .and_then(|seller| summaries.get(seller))
            .map(|summary| summary.total);
        record.high_value_seller = record
            .seller_total
            .is_some_and(|total| mode.qualifies(total));
    }

    let output = match mode {
        ProcessingMode::FullValidationExport => export::export_full(
            folder,
            output,
            &mut records,
            &summaries,
            mode,
            max_files_per_folder,
        )?,
        ProcessingMode::QuickSummary => {
            export::export_quick(folder, output, &records, &summaries, mode)?
        }
    };

    Ok(ProcessResult {
        output,
        pdf_count: records.len(),
        success_count: records
            .iter()
            .filter(|record| record.state == RecordState::Valid)
            .count(),
        failed_count: records
            .iter()
            .filter(|record| record.state == RecordState::Invalid)
            .count(),
        skipped_count: records
            .iter()
            .filter(|record| record.state == RecordState::NonInvoice)
            .count(),
        qualified_count: summaries
            .values()
            .filter(|summary| mode.qualifies(summary.total))
            .count(),
    })
}

fn process_pdfs<F>(
    folder: &Path,
    pdf_files: &[PathBuf],
    mode: ProcessingMode,
    on_progress: &mut F,
) -> Vec<InvoiceRecord>
where
    F: FnMut(ProgressUpdate),
{
    let total = pdf_files.len();
    let worker_count = parallel_worker_count(total);
    let next_index = AtomicUsize::new(0);
    let (sender, receiver) = mpsc::channel();
    let mut records = (0..total).map(|_| None).collect::<Vec<_>>();

    thread::scope(|scope| {
        for _ in 0..worker_count {
            let sender = sender.clone();
            let next_index = &next_index;
            scope.spawn(move || loop {
                let index = next_index.fetch_add(1, Ordering::Relaxed);
                let Some(path) = pdf_files.get(index) else {
                    break;
                };

                let file_name = display_file_name(path);
                let record = process_pdf(folder, path, mode);
                if sender.send((index, file_name, record)).is_err() {
                    break;
                }
            });
        }
        drop(sender);

        for (completed, (index, file_name, record)) in receiver.into_iter().enumerate() {
            records[index] = Some(record);
            on_progress(ProgressUpdate {
                current: completed + 1,
                total,
                file_name,
            });
        }
    });

    records
        .into_iter()
        .map(|record| record.expect("每份 PDF 都应由并行 worker 返回结果"))
        .collect()
}

fn parallel_worker_count(file_count: usize) -> usize {
    if file_count == 0 {
        return 0;
    }

    thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(1)
        .min(MAX_PARALLEL_PDFS)
        .min(file_count)
        .max(1)
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

fn process_pdf(source_root: &Path, path: &Path, mode: ProcessingMode) -> InvoiceRecord {
    let file_name = display_file_name(path);
    let source_relative = path.strip_prefix(source_root).unwrap_or(path).to_path_buf();
    let sha256 = parser::file_sha256(path).unwrap_or_default();
    let submitter = parser::extract_submitter(&file_name);

    match parser::extract_isolated(path) {
        Ok(raw) => validate_record(
            path.to_path_buf(),
            source_relative,
            file_name,
            sha256,
            submitter,
            raw,
            mode,
        ),
        Err(reason) => {
            // 用户确认发票文件名应包含可识别的“姓名 + 物品 + 金额”。若正文解析
            // 失败且文件名也完全不符合该规则，则归入非发票 PDF；疑似发票仍进入
            // 解析失败目录，避免把真正有问题的发票藏进普通资料中。
            let resembles_invoice_name = submitter.is_some() || file_name.contains("发票");
            let (state, kind, reason) = if resembles_invoice_name {
                (RecordState::Invalid, IssueKind::ParseFailure, reason)
            } else {
                (
                    RecordState::NonInvoice,
                    IssueKind::Other,
                    format!(
                        "正文解析失败，且文件名不符合已确认的发票命名规则，按非发票 PDF 分流：{reason}"
                    ),
                )
            };
            InvoiceRecord {
                source_path: path.to_path_buf(),
                source_relative,
                file_name,
                sha256,
                submitter,
                raw: RawInvoiceData::default(),
                state,
                issues: vec![ValidationIssue { kind, reason }],
                seller_total: None,
                high_value_seller: false,
                export_relative: None,
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn validate_record(
    source_path: PathBuf,
    source_relative: PathBuf,
    file_name: String,
    sha256: String,
    submitter: Option<String>,
    raw: RawInvoiceData,
    mode: ProcessingMode,
) -> InvoiceRecord {
    let resembles_invoice = submitter.is_some() || invoice_signal_count(&raw) >= 2;
    if !raw.is_invoice && !resembles_invoice {
        return InvoiceRecord {
            source_path,
            source_relative,
            file_name,
            sha256,
            submitter,
            raw,
            state: RecordState::NonInvoice,
            issues: vec![ValidationIssue {
                kind: IssueKind::Other,
                reason: "未检测到足够的发票版式和关键字段".to_owned(),
            }],
            seller_total: None,
            high_value_seller: false,
            export_relative: None,
        };
    }

    let mut issues = Vec::new();
    if raw.is_red {
        issues.push(ValidationIssue {
            kind: IssueKind::RedInvoice,
            reason: "检测到红字、红冲或负数金额特征".to_owned(),
        });
    }

    if mode == ProcessingMode::FullValidationExport {
        match raw.buyer_name.as_deref() {
            Some(name) if normalized(name) == normalized(EXPECTED_BUYER_NAME) => {}
            Some(name) => issues.push(ValidationIssue {
                kind: IssueKind::BuyerNameMismatch,
                reason: format!("购买方名称为“{name}”，应为“{EXPECTED_BUYER_NAME}”"),
            }),
            None => issues.push(missing("购买方名称")),
        }

        match raw.buyer_tax_id.as_deref() {
            Some(tax_id) if normalized_tax_id(tax_id) == EXPECTED_BUYER_TAX_ID => {}
            Some(tax_id) => issues.push(ValidationIssue {
                kind: IssueKind::BuyerTaxIdMismatch,
                reason: format!("购买方税号为“{tax_id}”，应为“{EXPECTED_BUYER_TAX_ID}”"),
            }),
            None => issues.push(missing("购买方税号")),
        }

        if submitter.is_none() {
            issues.push(ValidationIssue {
                kind: IssueKind::SubmitterUnknown,
                reason: "无法按“姓名 + 物品 + 金额”的文件名规则可靠识别提交人".to_owned(),
            });
        }
        if !raw.pdf_risks.is_empty() {
            issues.push(ValidationIssue {
                kind: IssueKind::SuspiciousPdf,
                reason: raw.pdf_risks.join("；"),
            });
        }
        if raw.invoice_number.is_none() {
            issues.push(missing("发票号码"));
        }
        if raw.invoice_date.is_none() {
            issues.push(missing("开票日期"));
        }
        if raw.seller_tax_id.is_none() {
            issues.push(missing("销售方税号"));
        }
    }

    if raw.seller_name.is_none() {
        issues.push(missing("销售方名称"));
    }
    if raw.total.is_none() {
        issues.push(missing("价税合计"));
    }

    InvoiceRecord {
        source_path,
        source_relative,
        file_name,
        sha256,
        submitter,
        raw,
        state: if issues.is_empty() {
            RecordState::Valid
        } else {
            RecordState::Invalid
        },
        issues,
        seller_total: None,
        high_value_seller: false,
        export_relative: None,
    }
}

fn invoice_signal_count(raw: &RawInvoiceData) -> usize {
    usize::from(raw.invoice_number.is_some())
        + usize::from(raw.invoice_date.is_some())
        + usize::from(raw.buyer_name.is_some() || raw.buyer_tax_id.is_some())
        + usize::from(raw.seller_name.is_some() || raw.seller_tax_id.is_some())
        + usize::from(raw.total.is_some())
        + usize::from(!raw.goods.is_empty())
}

fn missing(field: &str) -> ValidationIssue {
    ValidationIssue {
        kind: IssueKind::MissingField,
        reason: format!("未能可靠提取{field}"),
    }
}

fn normalized(value: &str) -> String {
    value
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect()
}

fn normalized_tax_id(value: &str) -> String {
    normalized(value).to_ascii_uppercase()
}

fn summarize(records: &[InvoiceRecord]) -> SellerSummaries {
    let mut summaries = SellerSummaries::new();
    for record in records
        .iter()
        .filter(|record| record.state == RecordState::Valid)
    {
        let (Some(seller), Some(total)) = (&record.raw.seller_name, record.raw.total) else {
            continue;
        };
        let summary = summaries.entry(seller.clone()).or_default();
        summary.count += 1;
        summary.total += total;
        summary.files.push(record.file_name.clone());
    }
    summaries
}

fn display_file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::Decimal;

    #[test]
    fn threshold_is_inclusive() {
        let mode = ProcessingMode::FullValidationExport;
        assert!(mode.qualifies(Decimal::new(100_000, 2)));
        assert!(!mode.qualifies(Decimal::new(99_999, 2)));
    }

    #[test]
    fn parallel_worker_count_is_safe_and_bounded() {
        assert_eq!(parallel_worker_count(0), 0);
        assert_eq!(parallel_worker_count(1), 1);

        let workers = parallel_worker_count(usize::MAX);
        assert!((1..=MAX_PARALLEL_PDFS).contains(&workers));
    }

    #[test]
    fn tax_id_comparison_ignores_case_and_spaces() {
        assert_eq!(
            normalized_tax_id("12440000455860226 x"),
            EXPECTED_BUYER_TAX_ID
        );
    }

    #[test]
    fn image_pdf_with_invoice_filename_is_suspicious_not_non_invoice() {
        let raw = RawInvoiceData {
            pdf_risks: vec!["无文本层，疑似扫描件或图片转换 PDF".to_owned()],
            ..RawInvoiceData::default()
        };
        let record = validate_record(
            PathBuf::from("张三-电机-10.pdf"),
            PathBuf::from("张三-电机-10.pdf"),
            "张三-电机-10.pdf".to_owned(),
            String::new(),
            Some("张三".to_owned()),
            raw,
            ProcessingMode::FullValidationExport,
        );
        assert_eq!(record.state, RecordState::Invalid);
        assert_eq!(
            record.primary_issue().map(|issue| issue.kind),
            Some(IssueKind::SuspiciousPdf)
        );
    }

    #[test]
    fn partial_invoice_fields_are_not_classified_as_non_invoice() {
        let raw = RawInvoiceData {
            invoice_date: Some("2026-04-08".to_owned()),
            total: Some(Decimal::new(1480, 2)),
            goods: vec!["*集成电路*电子元器件".to_owned()],
            ..RawInvoiceData::default()
        };
        let record = validate_record(
            PathBuf::from("张三-芯片-14.8元.pdf"),
            PathBuf::from("张三-芯片-14.8元.pdf"),
            "张三-芯片-14.8元.pdf".to_owned(),
            String::new(),
            Some("张三".to_owned()),
            raw,
            ProcessingMode::FullValidationExport,
        );
        assert_eq!(record.state, RecordState::Invalid);
    }

    #[test]
    fn document_without_filename_or_content_signals_is_non_invoice() {
        let record = validate_record(
            PathBuf::from("课程说明.pdf"),
            PathBuf::from("课程说明.pdf"),
            "课程说明.pdf".to_owned(),
            String::new(),
            None,
            RawInvoiceData::default(),
            ProcessingMode::FullValidationExport,
        );
        assert_eq!(record.state, RecordState::NonInvoice);
    }
}
