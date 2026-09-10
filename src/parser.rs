use std::env;
use std::fs;
use std::io::Read;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;
use std::process::{Command, Stdio};
use std::str::FromStr;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use regex::Regex;
use rust_decimal::Decimal;
use sha2::{Digest, Sha256};
use wait_timeout::ChildExt;

use crate::model::RawInvoiceData;

const WORKER_TIMEOUT: Duration = Duration::from_secs(45);

pub fn worker_extract(path: &Path) -> Result<RawInvoiceData> {
    let bytes = fs::read(path).with_context(|| format!("无法读取 PDF 文件：{}", path.display()))?;
    let header_end = bytes.len().min(1024);
    if !bytes[..header_end]
        .windows(5)
        .any(|window| window == b"%PDF-")
    {
        bail!("文件不包含有效的 PDF 文件头");
    }

    let text = extract_text_with_fallback(path)?;
    Ok(parse_pdf_text(&text, &bytes))
}

fn extract_text_with_fallback(path: &Path) -> Result<String> {
    // pdf-extract 遇到部分中文字体编码时会 panic。这个异常可以由备用解析器
    // 正常恢复，因此暂时关闭 panic hook，避免把已处理的异常打印到用户窗口。
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let primary = catch_unwind(AssertUnwindSafe(|| pdf_extract::extract_text(path)));
    std::panic::set_hook(previous_hook);
    match primary {
        Ok(Ok(text)) => Ok(text),
        Ok(Err(primary_error)) => extract_text_with_lopdf(path)
            .with_context(|| format!("主解析器失败：{primary_error}；备用解析器也无法提取文字")),
        Err(payload) => {
            let panic_reason = panic_message(payload);
            extract_text_with_lopdf(path)
                .with_context(|| format!("主解析器异常：{panic_reason}；备用解析器也无法提取文字"))
        }
    }
}

fn extract_text_with_lopdf(path: &Path) -> Result<String> {
    let document = lopdf::Document::load(path)
        .with_context(|| format!("备用解析器无法读取PDF：{}", path.display()))?;
    let pages = document.get_pages().keys().copied().collect::<Vec<_>>();
    document
        .extract_text(&pages)
        .with_context(|| format!("备用解析器无法提取PDF文字：{}", path.display()))
}

fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|message| (*message).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "未知panic".to_owned())
}

pub fn extract_isolated(path: &Path) -> std::result::Result<RawInvoiceData, String> {
    let executable = env::current_exe().map_err(|error| format!("无法定位当前程序：{error}"))?;
    let mut command = Command::new(executable);
    command
        .arg("--worker")
        .arg(path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }

    let mut child = command
        .spawn()
        .map_err(|error| format!("无法启动 PDF 隔离解析进程：{error}"))?;

    match child
        .wait_timeout(WORKER_TIMEOUT)
        .map_err(|error| format!("等待 PDF 解析进程失败：{error}"))?
    {
        Some(_) => {}
        None => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!(
                "解析超过 {} 秒，已终止隔离进程",
                WORKER_TIMEOUT.as_secs()
            ));
        }
    }

    let mut stdout = String::new();
    let mut stderr = String::new();
    if let Some(mut pipe) = child.stdout.take() {
        pipe.read_to_string(&mut stdout)
            .map_err(|error| format!("读取解析结果失败：{error}"))?;
    }
    if let Some(mut pipe) = child.stderr.take() {
        pipe.read_to_string(&mut stderr)
            .map_err(|error| format!("读取解析错误信息失败：{error}"))?;
    }

    let status = child
        .try_wait()
        .map_err(|error| format!("获取解析进程状态失败：{error}"))?
        .ok_or_else(|| "解析进程状态异常".to_owned())?;
    if !status.success() {
        let detail = compact_error(&stderr);
        return Err(if detail.is_empty() {
            format!("隔离解析进程异常退出，状态码：{status}")
        } else {
            format!("隔离解析进程异常退出：{detail}")
        });
    }

    serde_json::from_str(stdout.trim()).map_err(|error| {
        format!(
            "无法读取隔离解析结果：{error}；输出摘要：{}",
            compact_error(&stdout)
        )
    })
}

pub fn file_sha256(path: &Path) -> Result<String> {
    let mut file =
        fs::File::open(path).with_context(|| format!("无法读取文件：{}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .with_context(|| format!("计算文件哈希失败：{}", path.display()))?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

pub fn extract_submitter(file_name: &str) -> Option<String> {
    let stem = Path::new(file_name).file_stem()?.to_string_lossy();
    let copy_suffix =
        Regex::new(r"(?:\s*[（(]\d+[）)])+$").expect("valid filename copy suffix regex");
    let stem = copy_suffix.replace(stem.trim(), "");
    let amount_at_end = Regex::new(r"[¥￥]?\d[\d,]*(?:\.\d{1,2})?(?:元|圆)?$")
        .expect("valid filename trailing amount regex");
    if !amount_at_end.is_match(stem.trim()) {
        return None;
    }

    let separator = Regex::new(r"[\s_\-—–+]+").expect("valid filename separator regex");
    let parts = separator
        .split(stem.trim())
        .filter(|part| !part.trim().is_empty())
        .collect::<Vec<_>>();
    let candidate = parts.first()?.trim_matches(|character: char| {
        matches!(
            character,
            '“' | '”' | '‘' | '’' | '"' | '\'' | '(' | ')' | '（' | '）'
        )
    });
    if valid_submitter(candidate) {
        return Some(candidate.to_owned());
    }

    // 无姓名分隔符时只接受“2～4 个中文姓名字符后直接接英文/数字”的
    // 明确边界，例如“林滔tps40345 34.00元”。全中文连写仍不猜测。
    let name_before_ascii =
        Regex::new(r"^([\p{Han}·]{2,4})[A-Za-z0-9]").expect("valid compact submitter regex");
    let candidate = name_before_ascii.captures(stem.trim())?.get(1)?.as_str();
    valid_submitter(candidate).then(|| candidate.to_owned())
}

fn valid_submitter(candidate: &str) -> bool {
    let length = candidate.chars().count();
    let all_cjk = candidate
        .chars()
        .all(|character| ('\u{3400}'..='\u{9fff}').contains(&character) || character == '·');
    let forbidden = ["发票", "电子发票", "增值税发票", "附件", "文件"];
    all_cjk && (2..=8).contains(&length) && !forbidden.contains(&candidate)
}

fn parse_pdf_text(text: &str, bytes: &[u8]) -> RawInvoiceData {
    let lines = normalized_lines(text);
    let compacted = compact(text);
    let tax_entries = tax_id_entries(&lines);
    let buyer_tax_id = tax_entries
        .first()
        .map(|(_, value)| value.clone())
        .or_else(|| {
            compacted
                .contains(crate::model::EXPECTED_BUYER_TAX_ID)
                .then(|| crate::model::EXPECTED_BUYER_TAX_ID.to_owned())
        });
    let seller_tax_id = if tax_entries.len() >= 2 {
        tax_entries.last().map(|(_, value)| value.clone())
    } else {
        buyer_tax_id
            .as_deref()
            .and_then(|buyer_tax_id| seller_tax_id_near_buyer(&compacted, buyer_tax_id))
    };
    let buyer_name = tax_entries
        .first()
        .and_then(|(index, _)| find_name_before(&lines, *index))
        .or_else(|| {
            compacted
                .contains(crate::model::EXPECTED_BUYER_NAME)
                .then(|| crate::model::EXPECTED_BUYER_NAME.to_owned())
        })
        .map(|name| normalize_party_name(&name));
    let seller_name = (tax_entries.len() >= 2)
        .then(|| {
            tax_entries
                .last()
                .and_then(|(index, _)| find_name_before(&lines, *index))
        })
        .flatten()
        .or_else(|| {
            buyer_tax_id.as_deref().and_then(|buyer_tax_id| {
                seller_between_buyer_name_and_tax_id(&compacted, buyer_tax_id)
            })
        })
        .or_else(|| labeled_other_party_name(&lines, buyer_name.as_deref()))
        .map(|name| normalize_party_name(&name));
    let total = extract_total(text).ok();
    let invoice_issuer = extract_invoice_issuer(&lines, total);
    let goods = extract_goods(&lines);
    let is_red = detect_red_invoice(&compacted, total);
    let pdf_risks = detect_pdf_risks(text, bytes);

    RawInvoiceData {
        // 解析结果通过子进程标准输出传回。正文不参与后续统计，避免大 PDF
        // 填满管道导致父进程等待超时。
        text: if env::var_os("INVOICE_HELPER_DEBUG_TEXT").is_some() {
            text.to_owned()
        } else {
            String::new()
        },
        is_invoice: looks_like_invoice(
            &compacted,
            usize::from(buyer_tax_id.is_some()) + usize::from(seller_tax_id.is_some()),
        ),
        invoice_number: extract_invoice_number(&lines),
        invoice_date: extract_invoice_date(text),
        buyer_name,
        buyer_tax_id,
        seller_name,
        seller_tax_id,
        invoice_issuer,
        total,
        goods,
        is_toy: compacted.contains("玩具"),
        is_red,
        pdf_risks,
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

fn normalize_party_name(value: &str) -> String {
    let chars = value.trim().chars().collect::<Vec<_>>();
    let mut output = String::with_capacity(value.len());
    for (index, character) in chars.iter().copied().enumerate() {
        if !character.is_whitespace() {
            output.push(character);
            continue;
        }

        let previous = output.chars().next_back();
        let next = chars[index + 1..]
            .iter()
            .copied()
            .find(|candidate| !candidate.is_whitespace());
        if previous.is_some_and(is_han) && next.is_some_and(is_han) {
            continue;
        }
        if !output.ends_with(' ') {
            output.push(' ');
        }
    }
    output.trim().to_owned()
}

fn is_han(character: char) -> bool {
    ('\u{3400}'..='\u{9fff}').contains(&character)
}

fn compact(value: &str) -> String {
    value
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect()
}

fn tax_id_entries(lines: &[String]) -> Vec<(usize, String)> {
    let tax_id = Regex::new(r"^(?:[0-9A-Z]{18}|[0-9]{15})$").expect("valid tax id regex");
    let inline_tax_id =
        Regex::new(r"(?:统一社会信用代码/)?纳税人识别号[:：]?([0-9A-Z]{18}|[0-9]{15})")
            .expect("valid inline tax id regex");
    let first_money_line = lines
        .iter()
        .position(|line| line.contains('¥') || line.contains('￥'))
        .unwrap_or(lines.len());

    lines
        .iter()
        .enumerate()
        .filter_map(|(index, line)| {
            let value = compact(line).to_ascii_uppercase();
            // 旧版发票把销售方信息排在商品和金额区域之后。带明确字段标签的
            // 税号可以在全文读取；无标签的独立号码仍只在金额区之前接受，避免
            // 把备注、支付流水号等误当作税号。
            inline_tax_id
                .captures(&value)
                .and_then(|capture| capture.get(1))
                .map(|matched| (index, matched.as_str().to_owned()))
                .or_else(|| {
                    (index < first_money_line && tax_id.is_match(&value)).then_some((index, value))
                })
        })
        .collect()
}

fn find_name_before(lines: &[String], position: usize) -> Option<String> {
    lines[..position].iter().rev().take(8).find_map(|line| {
        let value = line.trim();
        let inline = value
            .find("名称：")
            .map(|index| &value[index + "名称：".len()..])
            .or_else(|| {
                value
                    .find("名称:")
                    .map(|index| &value[index + "名称:".len()..])
            })
            .map(str::trim)
            .filter(|candidate| is_possible_name(candidate));
        inline
            .map(str::to_owned)
            .or_else(|| is_possible_name(value).then(|| value.to_owned()))
    })
}

fn tax_id_after(compacted: &str, buyer_tax_id: &str) -> Option<String> {
    let buyer_end = compacted.find(buyer_tax_id)? + buyer_tax_id.len();
    let candidate = compacted[buyer_end..].chars().take(18).collect::<String>();
    Regex::new(r"^[0-9A-Z]{18}$")
        .expect("valid seller tax id regex")
        .is_match(&candidate)
        .then_some(candidate)
}

fn seller_tax_id_near_buyer(compacted: &str, buyer_tax_id: &str) -> Option<String> {
    tax_id_after(compacted, buyer_tax_id).or_else(|| {
        // 少数数电票的文字层会把销售方税号和购买方税号倒序粘成一行，
        // 例如“9144...69Y1244...226X”。购买方税号固定且已确认，
        // 因此只接受它正前方连续18位的税号候选，避免从其他号码猜测。
        let buyer_start = compacted.find(buyer_tax_id)?;
        let candidate = compacted[..buyer_start]
            .chars()
            .rev()
            .take(18)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<String>();
        Regex::new(r"^[0-9A-Z]{18}$")
            .expect("valid seller tax id regex")
            .is_match(&candidate)
            .then_some(candidate)
    })
}

fn labeled_other_party_name(lines: &[String], buyer_name: Option<&str>) -> Option<String> {
    let labeled_name =
        Regex::new(r"(?:^|\s)名称[:：]\s*(.+)$").expect("valid labeled party name regex");
    lines.iter().find_map(|line| {
        let candidate = labeled_name
            .captures(line)
            .and_then(|capture| capture.get(1))
            .map(|matched| matched.as_str().trim())?;
        if !is_possible_name(candidate)
            || buyer_name.is_some_and(|buyer| normalize_party_name(candidate) == buyer)
        {
            return None;
        }
        Some(candidate.to_owned())
    })
}

fn seller_between_buyer_name_and_tax_id(compacted: &str, buyer_tax_id: &str) -> Option<String> {
    let buyer_end = compacted.find(crate::model::EXPECTED_BUYER_NAME)?
        + crate::model::EXPECTED_BUYER_NAME.len();
    let relative_tax_start = compacted[buyer_end..].find(buyer_tax_id)?;
    let candidate = compacted[buyer_end..buyer_end + relative_tax_start].trim();
    is_possible_name(candidate).then(|| candidate.to_owned())
}

fn is_possible_name(line: &str) -> bool {
    let value = compact(line);
    let has_cjk = value
        .chars()
        .any(|character| ('\u{3400}'..='\u{9fff}').contains(&character));
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
        && (2..=100).contains(&value.chars().count())
        && !forbidden.iter().any(|word| value.contains(word))
        && !value.contains('¥')
        && !value.contains('￥')
}

fn extract_total(text: &str) -> Result<Decimal> {
    let currency =
        Regex::new(r"[¥￥]\s*([-−]?[0-9][0-9,]*\.[0-9]{2})").expect("valid currency regex");
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
        bail!("未找到带货币符号且保留两位小数的金额")
    }

    for window in amounts.windows(3) {
        if window[0].1 + window[1].1 == window[2].1 {
            return Ok(window[2].1);
        }
    }

    let uppercase_money = Regex::new(
        r"[零〇一二三四五六七八九壹贰叁肆伍陆柒捌玖拾佰仟万亿圆元角分负]+(?:整|正|角|分)",
    )
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

    bail!("找到多个金额，但无法确认价税合计")
}

fn parse_amount(value: &str) -> Result<Decimal> {
    let cleaned = value.replace(',', "").replace('−', "-");
    Decimal::from_str(&cleaned).with_context(|| format!("无效金额：{value}"))
}

fn extract_invoice_number(lines: &[String]) -> Option<String> {
    let number = Regex::new(r"^\d{8,20}$").expect("valid invoice number regex");
    let labeled = Regex::new(r"发票号码[:：]?(\d{8,20})").expect("valid labeled invoice regex");
    for line in lines {
        let value = compact(line);
        if let Some(invoice_number) = labeled.captures(&value).and_then(|capture| capture.get(1)) {
            return Some(invoice_number.as_str().to_owned());
        }
    }
    lines
        .iter()
        .map(|line| compact(line))
        .find(|line| number.is_match(line) && line.len() != 15 && line.len() != 18)
        .or_else(|| {
            let joined = lines.iter().map(|line| compact(line)).collect::<String>();
            Regex::new(r"(?:^|[^0-9])(\d{20})(?:[^0-9]|$)")
                .expect("valid embedded invoice number regex")
                .captures(&joined)
                .and_then(|capture| capture.get(1))
                .map(|matched| matched.as_str().to_owned())
                .or_else(|| {
                    // 部分 UniGB-UCS2-H 发票的备用文字层会把“发票号”和
                    // “2026年…”日期直接粘在一起。
                    Regex::new(r"(\d{20})20\d{2}年")
                        .expect("valid invoice number before date regex")
                        .captures(&joined)
                        .and_then(|capture| capture.get(1))
                        .map(|matched| matched.as_str().to_owned())
                })
        })
}

fn extract_invoice_date(text: &str) -> Option<String> {
    let date = Regex::new(r"(20\d{2})\s*年\s*(\d{1,2})\s*月\s*(\d{1,2})\s*日")
        .expect("valid invoice date regex");
    let capture = date.captures(text)?;
    Some(format!(
        "{}-{:0>2}-{:0>2}",
        capture.get(1)?.as_str(),
        capture.get(2)?.as_str(),
        capture.get(3)?.as_str()
    ))
}

fn extract_invoice_issuer(lines: &[String], total: Option<Decimal>) -> Option<String> {
    let inline_issuer = Regex::new(r"开票人[:：]([\p{Han}·]{2,12}?)(?:销售方|收款人|复核|[（(]|$)")
        .expect("valid inline invoice issuer regex");
    for line in lines {
        let value = compact(line);
        if let Some(issuer) = inline_issuer
            .captures(&value)
            .and_then(|capture| capture.get(1))
        {
            return Some(issuer.as_str().to_owned());
        }
        if let Some(rest) = value
            .strip_prefix("开票人：")
            .or_else(|| value.strip_prefix("开票人:"))
        {
            if is_short_person_name(rest) {
                return Some(rest.to_owned());
            }
        }
    }

    let total = total?;
    let candidates = [
        format!("¥{total:.2}"),
        format!("￥{total:.2}"),
        format!("¥{:0.2}", total.abs()),
        format!("￥{:0.2}", total.abs()),
    ];
    let total_line = lines.iter().rposition(|line| {
        candidates
            .iter()
            .any(|candidate| compact(line) == *candidate)
    })?;

    lines
        .iter()
        .skip(total_line + 1)
        .take(6)
        .map(|line| compact(line))
        .find(|line| is_short_person_name(line))
}

fn is_short_person_name(value: &str) -> bool {
    let count = value.chars().count();
    (2..=12).contains(&count)
        && value
            .chars()
            .all(|character| ('\u{3400}'..='\u{9fff}').contains(&character) || character == '·')
}

fn extract_goods(lines: &[String]) -> Vec<String> {
    let mut goods = lines
        .iter()
        .filter(|line| {
            let value = compact(line);
            (value.starts_with('*') && value.matches('*').count() >= 2)
                || (value.contains("玩具") && !value.contains("发票"))
        })
        .cloned()
        .collect::<Vec<_>>();
    goods.sort();
    goods.dedup();
    goods
}

fn detect_red_invoice(compacted: &str, total: Option<Decimal>) -> bool {
    let markers = [
        "红冲",
        "红字发票",
        "红字信息表",
        "销项负数",
        "负数发票",
        "已冲红",
        "部分红冲",
    ];
    markers.iter().any(|marker| compacted.contains(marker))
        || total.is_some_and(|amount| amount.is_sign_negative())
}

fn looks_like_invoice(compacted: &str, tax_id_count: usize) -> bool {
    let mut score = 0;
    if compacted.contains("电子发票")
        || (compacted.contains("增值税") && compacted.contains("发票"))
    {
        score += 2;
    }
    if compacted.contains("发票号码") {
        score += 1;
    }
    if compacted.contains("价税合计") {
        score += 1;
    }
    if compacted.contains("购买方信息") || compacted.contains("购买方名称") {
        score += 1;
    }
    if compacted.contains("销售方信息") || compacted.contains("销售方名称") {
        score += 1;
    }
    if tax_id_count >= 1 {
        score += 1;
    }
    score >= 4
}

fn detect_pdf_risks(text: &str, bytes: &[u8]) -> Vec<String> {
    let mut risks = Vec::new();
    if text.trim().is_empty() {
        risks.push("无文本层，疑似扫描件或图片转换 PDF".to_owned());
    }

    let metadata_tools = lopdf::Document::load_metadata_mem(bytes)
        .map(|metadata| {
            [metadata.creator, metadata.producer]
                .into_iter()
                .flatten()
                .filter(|value| !value.trim().is_empty())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let suspicious_tools = [
        ("microsoft® word", "Microsoft Word"),
        ("microsoft word", "Microsoft Word"),
        ("wps", "WPS"),
        ("kingsoft", "WPS/Kingsoft"),
        ("libreoffice", "LibreOffice"),
        ("ghostscript", "Ghostscript"),
        ("microsoft print to pdf", "Microsoft Print to PDF"),
        ("pdfcreator", "PDFCreator"),
    ];
    for tool in metadata_tools {
        let normalized = tool.to_lowercase();
        for (needle, label) in suspicious_tools {
            if normalized.contains(needle) {
                let reason = format!("PDF 生成器元数据“{tool}”显示由 {label} 生成或处理");
                if !risks.iter().any(|risk| risk == &reason) {
                    risks.push(reason);
                }
            }
        }
    }
    risks
}

fn compact_error(value: &str) -> String {
    let one_line = value.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut output = one_line.chars().take(500).collect::<String>();
    if one_line.chars().count() > 500 {
        output.push('…');
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_TEXT: &str = r#"
电子发票（普通发票）
发票号码：
开票日期：
购买方信息
统一社会信用代码/纳税人识别号：
销售方信息
统一社会信用代码/纳税人识别号：
名称：
名称：
价税合计（大写）
（小写）
开票人：
26442000004577882986
2026年04月27日
广东工业大学
12440000455860226X
轮趣科技（东莞）有限公司
91441900MA518CH52Y
¥32.65
¥4.25
叁拾陆圆玖角整
¥36.90
刘付国周
*电子元件*电机驱动模块
"#;

    #[test]
    fn parses_verified_invoice_fields() {
        let result = parse_pdf_text(SAMPLE_TEXT, b"%PDF-1.7");
        assert!(result.is_invoice);
        assert_eq!(result.buyer_name.as_deref(), Some("广东工业大学"));
        assert_eq!(
            result.seller_name.as_deref(),
            Some("轮趣科技（东莞）有限公司")
        );
        assert_eq!(result.total, Some(Decimal::new(3690, 2)));
        assert_eq!(result.invoice_issuer.as_deref(), Some("刘付国周"));
    }

    #[test]
    fn parses_legacy_labeled_fields_after_amount_section() {
        let text = r#"
发票号码: 09725957
开票日期: 2026 年 04 月 08 日
名称: 广东工业大学
纳税人识别号: 12440000455860226X
*集成电路*电子元器件 ¥14.80
价税合计(大写) 壹拾肆元捌角 (小写) ￥14.80
名称: 深圳市利鸿达科技有限公司
纳税人识别号: 91440300MA5HNNWY1N
收款人: 庄宏利 复核: 庄宏利 开票人: 庄贤滨 销售方:(章)
"#;
        let result = parse_pdf_text(text, b"%PDF-1.7");

        assert_eq!(result.invoice_number.as_deref(), Some("09725957"));
        assert_eq!(result.buyer_name.as_deref(), Some("广东工业大学"));
        assert_eq!(result.buyer_tax_id.as_deref(), Some("12440000455860226X"));
        assert_eq!(
            result.seller_name.as_deref(),
            Some("深圳市利鸿达科技有限公司")
        );
        assert_eq!(result.seller_tax_id.as_deref(), Some("91440300MA5HNNWY1N"));
        assert_eq!(result.invoice_issuer.as_deref(), Some("庄贤滨"));
    }

    #[test]
    fn parses_compact_invoice_number_and_parties() {
        let text = concat!(
            "电子发票（普通发票）发票号码：开票日期：价税合计（大写）（小写）",
            "邱春燕264320000009704748912026年04月30日",
            "广东工业大学资兴市极脉控模型店（个体工商户）",
            "12440000455860226X92431081MAEU38DR1M¥121.77",
        );
        let result = parse_pdf_text(text, b"%PDF-1.7");

        assert_eq!(
            result.invoice_number.as_deref(),
            Some("26432000000970474891")
        );
        assert_eq!(result.buyer_name.as_deref(), Some("广东工业大学"));
        assert_eq!(
            result.seller_name.as_deref(),
            Some("资兴市极脉控模型店（个体工商户）")
        );
        assert_eq!(result.seller_tax_id.as_deref(), Some("92431081MAEU38DR1M"));
    }

    #[test]
    fn parses_reversed_adjacent_tax_ids_and_labeled_names() {
        let text = r#"
电子发票（普通发票） 发票号码: 26957000000114799448
开票日期: 2026年04月29日
销售方信息
价税合计(大写) 壹佰伍拾柒圆陆角捌分 (小写) ¥157.68
购买方信息 91440300795432869Y12440000455860226X
名称:深圳嘉立创科技集团股份有限公司
统一社会信用代码/纳税人识别号:
名称:广东工业大学
统一社会信用代码/纳税人识别号:
"#;
        let result = parse_pdf_text(text, b"%PDF-1.7");

        assert_eq!(result.buyer_name.as_deref(), Some("广东工业大学"));
        assert_eq!(result.buyer_tax_id.as_deref(), Some("12440000455860226X"));
        assert_eq!(
            result.seller_name.as_deref(),
            Some("深圳嘉立创科技集团股份有限公司")
        );
        assert_eq!(result.seller_tax_id.as_deref(), Some("91440300795432869Y"));
    }

    #[test]
    fn amount_in_chinese_ending_with_cents_anchors_total() {
        let total = extract_total("价税合计（大写）肆圆捌角贰分 ¥4.82\n¥4.78\n¥0.04")
            .expect("total should be parsed");
        assert_eq!(total, Decimal::new(482, 2));
    }

    #[test]
    fn party_names_drop_only_whitespace_between_chinese_characters() {
        assert_eq!(
            normalize_party_name("新郑市芯纳半导体科技 有 限公司"),
            "新郑市芯纳半导体科技有限公司"
        );
        assert_eq!(
            normalize_party_name("ABC Technology 有限公司"),
            "ABC Technology 有限公司"
        );
    }

    #[test]
    fn submitter_supports_common_separators() {
        assert_eq!(
            extract_submitter("陈子涵_电机驱动_36.90.pdf").as_deref(),
            Some("陈子涵")
        );
        assert_eq!(
            extract_submitter("陈子涵 电机驱动 36.90.pdf").as_deref(),
            Some("陈子涵")
        );
        assert_eq!(
            extract_submitter("陈子涵-电机驱动-36.90.pdf").as_deref(),
            Some("陈子涵")
        );
        assert_eq!(
            extract_submitter("王振—板材加工—150元.pdf").as_deref(),
            Some("王振")
        );
        assert_eq!(
            extract_submitter("刘子羲+传感器+￥38.41元.pdf").as_deref(),
            Some("刘子羲")
        );
        assert_eq!(
            extract_submitter("叶飞南_步进电机_12圆.pdf").as_deref(),
            Some("叶飞南")
        );
    }

    #[test]
    fn submitter_is_not_guessed_without_separator() {
        assert_eq!(extract_submitter("陈子涵电机驱动36.90.pdf"), None);
    }

    #[test]
    fn submitter_uses_clear_chinese_to_ascii_boundary() {
        assert_eq!(
            extract_submitter("欧阳兆祺MicroHDMI线23.9元.pdf").as_deref(),
            Some("欧阳兆祺")
        );
        assert_eq!(extract_submitter("欧阳兆祺转接线23.9元.pdf"), None);
    }

    #[test]
    fn submitter_supports_copy_suffixes_and_glued_amounts() {
        assert_eq!(
            extract_submitter("刘栩 PS2手柄 49(1).pdf").as_deref(),
            Some("刘栩")
        );
        assert_eq!(
            extract_submitter("刘栩 ps2接收模块 57.98(1)(1).pdf").as_deref(),
            Some("刘栩")
        );
        assert_eq!(
            extract_submitter("林滔 锂电池68.25元.pdf").as_deref(),
            Some("林滔")
        );
        assert_eq!(
            extract_submitter("林滔tps40345 34.00元.pdf").as_deref(),
            Some("林滔")
        );
    }

    #[test]
    fn detects_toy_and_red_invoice() {
        let text = format!("{SAMPLE_TEXT}\n备注：儿童玩具\n红字发票");
        let result = parse_pdf_text(&text, b"%PDF-1.7");
        assert!(result.is_toy);
        assert!(result.is_red);
    }

    #[test]
    fn ordinary_document_is_not_invoice() {
        let result = parse_pdf_text("课程教学安排和比赛加分证明", b"%PDF-1.7");
        assert!(!result.is_invoice);
    }

    #[test]
    fn pdf_risk_only_uses_generator_metadata() {
        let ordinary = test_pdf_with_metadata("iTextSharp", Some("CourierNewPSMT"));
        assert!(detect_pdf_risks("有文字层", &ordinary).is_empty());

        let wps = test_pdf_with_metadata("WPS Office", None);
        let risks = detect_pdf_risks("有文字层", &wps);
        assert_eq!(risks.len(), 1);
        assert!(risks[0].contains("WPS Office"));
    }

    fn test_pdf_with_metadata(producer: &str, extra_font: Option<&str>) -> Vec<u8> {
        use lopdf::{dictionary, Document, Object};

        let mut document = Document::with_version("1.7");
        let pages_id = document.add_object(dictionary! {
            "Type" => "Pages",
            "Kids" => Vec::<Object>::new(),
            "Count" => 0,
        });
        let catalog_id = document.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        let info_id = document.add_object(dictionary! {
            "Producer" => Object::string_literal(producer),
        });
        if let Some(font) = extra_font {
            document.add_object(dictionary! {
                "Type" => "Font",
                "BaseFont" => font,
            });
        }
        document.trailer.set("Root", catalog_id);
        document.trailer.set("Info", info_id);

        let mut bytes = Vec::new();
        document.save_to(&mut bytes).expect("test PDF is writable");
        bytes
    }
}
