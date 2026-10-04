use std::collections::BTreeMap;
use std::path::PathBuf;

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

pub const APP_VERSION: &str = "0.3.2";
pub const THOUSAND_YUAN: Decimal = Decimal::from_parts(1000, 0, 0, false, 0);
pub const EXPECTED_BUYER_NAME: &str = "广东工业大学";
pub const EXPECTED_BUYER_TAX_ID: &str = "12440000455860226X";
pub const DEFAULT_MAX_FILES_PER_FOLDER: usize = 30;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessingMode {
    FullValidationExport,
    QuickSummary,
}

impl ProcessingMode {
    pub const fn label(self) -> &'static str {
        match self {
            Self::FullValidationExport => "完整校验、分类与 Excel 导出",
            Self::QuickSummary => "快速汇总销售方累计金额 ≥ 1000 元",
        }
    }

    pub fn qualifies(self, total: Decimal) -> bool {
        total >= THOUSAND_YUAN
    }

    pub const fn ai_prompt(self) -> &'static str {
        "请读取本文件“发票明细”中的全部有效记录，以销售方名称进行精确分组，将同一销售方各张发票的“价税合计（元）”相加。请列出累计金额大于等于 1000.00 元的销售方，并给出销售方名称、发票数量、合计金额以及所包含的发票文件名。金额统一保留两位小数。忽略无效发票和非发票 PDF，不要根据文件名猜测缺失信息。请核对计算结果与“销售方汇总”部分；如有差异，明确指出。"
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RawInvoiceData {
    pub text: String,
    pub is_invoice: bool,
    pub invoice_number: Option<String>,
    pub invoice_date: Option<String>,
    pub buyer_name: Option<String>,
    pub buyer_tax_id: Option<String>,
    pub seller_name: Option<String>,
    pub seller_tax_id: Option<String>,
    pub invoice_issuer: Option<String>,
    #[serde(with = "rust_decimal::serde::str_option")]
    pub total: Option<Decimal>,
    pub goods: Vec<String>,
    pub is_toy: bool,
    pub is_red: bool,
    pub pdf_risks: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RecordState {
    Valid,
    Invalid,
    NonInvoice,
}

impl RecordState {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Valid => "有效发票",
            Self::Invalid => "无效发票",
            Self::NonInvoice => "非发票PDF",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum IssueKind {
    RedInvoice,
    BuyerNameMismatch,
    BuyerTaxIdMismatch,
    SuspiciousPdf,
    FilenameFormat,
    SubmitterUnknown,
    MissingField,
    ParseFailure,
    Other,
}

impl IssueKind {
    pub const fn label(self) -> &'static str {
        match self {
            Self::RedInvoice => "红冲发票",
            Self::BuyerNameMismatch => "抬头非广东工业大学",
            Self::BuyerTaxIdMismatch => "税号错误",
            Self::SuspiciousPdf => "PDF疑似非原文件",
            Self::FilenameFormat => "文件名格式错误",
            Self::SubmitterUnknown => "提交人无法识别",
            Self::MissingField => "字段缺失",
            Self::ParseFailure => "解析失败",
            Self::Other => "其他",
        }
    }

    pub const fn folder_name(self) -> &'static str {
        match self {
            Self::RedInvoice => "错误_红冲发票",
            Self::BuyerNameMismatch => "错误_抬头非广东工业大学",
            Self::BuyerTaxIdMismatch => "错误_税号错误",
            Self::SuspiciousPdf => "错误_PDF疑似非原文件",
            Self::FilenameFormat => "错误_文件名格式错误",
            Self::SubmitterUnknown => "错误_提交人无法识别",
            Self::MissingField => "错误_字段缺失",
            Self::ParseFailure => "错误_解析失败",
            Self::Other => "错误_其他",
        }
    }

    pub const fn priority(self) -> u8 {
        match self {
            Self::RedInvoice => 0,
            Self::BuyerNameMismatch => 1,
            Self::BuyerTaxIdMismatch => 2,
            Self::SuspiciousPdf => 3,
            Self::FilenameFormat => 4,
            Self::SubmitterUnknown => 5,
            Self::MissingField => 6,
            Self::ParseFailure => 7,
            Self::Other => 8,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ValidationIssue {
    pub kind: IssueKind,
    pub reason: String,
}

#[derive(Debug, Clone)]
pub struct InvoiceRecord {
    pub source_path: PathBuf,
    pub source_relative: PathBuf,
    pub file_name: String,
    pub sha256: String,
    pub submitter: Option<String>,
    pub raw: RawInvoiceData,
    pub state: RecordState,
    pub issues: Vec<ValidationIssue>,
    pub seller_total: Option<Decimal>,
    pub high_value_seller: bool,
    pub export_relative: Option<PathBuf>,
}

impl InvoiceRecord {
    pub fn primary_issue(&self) -> Option<&ValidationIssue> {
        self.issues.iter().min_by_key(|issue| issue.kind.priority())
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
    pub skipped_count: usize,
    pub qualified_count: usize,
}

#[derive(Debug, Clone, Default)]
pub struct SellerSummary {
    pub count: usize,
    pub total: Decimal,
    pub files: Vec<String>,
}

pub type SellerSummaries = BTreeMap<String, SellerSummary>;
