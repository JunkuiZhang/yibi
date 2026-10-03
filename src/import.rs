use crate::{
    auth::digest,
    ledger::{Entry, Kind, parse_money, valid_date},
};
use calamine::{Data, Reader, Xlsx};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    io::{Cursor, Read},
};

const MAX_ROWS: usize = 10_000;
const MAX_EXPANDED: u64 = 32 * 1024 * 1024;

#[derive(Clone, Serialize, Deserialize)]
pub struct Rule {
    pub id: i64,
    pub pattern: String,
    pub category: String,
    pub kind: Kind,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct PreviewRow {
    pub line: usize,
    pub entry: Entry,
    pub duplicate: bool,
    pub warnings: Vec<String>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Preview {
    pub source: String,
    pub rows: Vec<PreviewRow>,
    pub skipped: usize,
    pub errors: Vec<String>,
}

pub fn classify(kind: &Kind, merchant: &str, description: &str, rules: &[Rule]) -> String {
    let name = merchant.to_lowercase();
    for rule in rules {
        if rule.kind == *kind && name.contains(&rule.pattern.to_lowercase()) {
            return rule.category.clone();
        }
    }
    let text = format!("{merchant} {description}").to_lowercase();
    let patterns: &[(&str, &[&str])] = match kind {
        Kind::Transfer => return "账户转账".into(),
        Kind::Income => &[
            ("年终奖", &["年终奖", "年度奖金"]),
            ("工资", &["工资", "薪资", "薪酬"]),
        ],
        _ => &[
            (
                "会员订阅",
                &["会员", "订阅", "icloud", "netflix", "spotify"],
            ),
            (
                "交通出行",
                &[
                    "滴滴",
                    "地铁",
                    "公交",
                    "铁路",
                    "12306",
                    "高德打车",
                    "单车",
                    "骑行",
                ],
            ),
            (
                "餐饮",
                &[
                    "餐厅",
                    "食堂",
                    "咖啡",
                    "瑞幸",
                    "星巴克",
                    "肯德基",
                    "麦当劳",
                    "饿了么",
                ],
            ),
            ("日用购物", &["超市", "便利店", "盒马", "山姆", "拼多多"]),
            ("居家生活", &["电费", "水费", "燃气", "房租", "物业"]),
            ("医疗健康", &["医院", "药房", "药店", "诊所"]),
            (
                "通讯网络",
                &["中国移动", "中国联通", "中国电信", "话费", "宽带"],
            ),
        ],
    };
    patterns
        .iter()
        .find(|(_, words)| words.iter().any(|w| text.contains(w)))
        .map(|(category, _)| (*category).to_owned())
        .unwrap_or_else(|| "待分类".into())
}

/// Monthly ride counts cover usage, not purchases of a pass or cycling equipment.
pub fn is_bike_ride(merchant: &str, description: &str) -> bool {
    let text = format!("{merchant} {description}");
    (text.contains("单车") || text.contains("骑行"))
        && ![
            "会员",
            "月卡",
            "季卡",
            "年卡",
            "周卡",
            "次卡",
            "骑行卡",
            "套餐",
            "购车",
            "配件",
            "头盔",
        ]
        .iter()
        .any(|word| text.contains(word))
}

pub fn parse(filename: &str, bytes: &[u8], rules: &[Rule]) -> Result<Preview, String> {
    if bytes.len() > 8 * 1024 * 1024 {
        return Err("文件不能超过 8MB".into());
    }
    let lower = filename.to_lowercase();
    let table: Vec<Vec<String>> = if lower.ends_with(".xlsx") {
        // Inspect the ZIP directory before decompression to bound spreadsheet memory use.
        let mut archive = zip::ZipArchive::new(Cursor::new(bytes))
            .map_err(|_| "无法读取 XLSX 文件，请先解压账单下载包")?;
        if archive.len() > 256 {
            return Err("工作簿内容过多，请使用原始账单明细文件".into());
        }
        let mut total = 0u64;
        for i in 0..archive.len() {
            let f = archive.by_index(i).map_err(|_| "表格压缩内容无效")?;
            total = total.checked_add(f.size()).ok_or("表格过大")?;
            if total > MAX_EXPANDED {
                return Err("解压后的表格超过 32MB，请分批导出账单".into());
            }
        }
        for i in 0..archive.len() {
            let mut file = archive.by_index(i).map_err(|_| "表格压缩内容无效")?;
            if file.name().starts_with("xl/worksheets/") && file.name().ends_with(".xml") {
                let mut xml = Vec::new();
                file.read_to_end(&mut xml).map_err(|_| "工作表读取失败")?;
                check_sheet_bounds(&xml)?;
            }
        }
        let mut book: Xlsx<_> = Xlsx::new(Cursor::new(bytes)).map_err(|_| "无法读取 XLSX 文件")?;
        let sheet = book
            .worksheet_range_at(0)
            .ok_or("表格中没有工作表")?
            .map_err(|_| "工作表损坏或无法解析")?;
        if sheet.height() > MAX_ROWS + 100 || sheet.width() > 50 {
            return Err("表格过大，请分批导入（每批最多一万笔）".into());
        }
        sheet
            .rows()
            .map(|r| r.iter().map(spreadsheet_cell).collect())
            .collect()
    } else if lower.ends_with(".csv") {
        let text = match std::str::from_utf8(bytes) {
            Ok(s) => s.to_owned(),
            Err(_) => {
                let (s, _, bad) = encoding_rs::GBK.decode(bytes);
                if bad {
                    return Err("账单编码无法识别，请使用原始 UTF-8 或 GBK CSV 文件".into());
                }
                s.into_owned()
            }
        };
        let mut reader = csv::ReaderBuilder::new()
            .has_headers(false)
            .flexible(true)
            .from_reader(text.trim_start_matches('\u{feff}').as_bytes());
        let mut rows = Vec::new();
        for record in reader.records() {
            let record = record.map_err(|_| "CSV 格式损坏，请使用未修改的原始账单")?;
            if record.len() > 50 || rows.len() >= MAX_ROWS + 100 {
                return Err("账单过大，请每批导入不超过一万笔".into());
            }
            rows.push(record.iter().map(str::to_owned).collect());
        }
        rows
    } else {
        return Err("支持 CSV 和 XLSX；ZIP 下载包请先在本地解压".into());
    };
    parse_table(table, rules)
}

fn spreadsheet_cell(cell: &Data) -> String {
    match cell {
        // WeChat XLSX exports use styled Excel serial numbers for transaction
        // times. Display would return the serial rather than a calendar date.
        Data::DateTime(value)
            if value.is_datetime()
                && value.as_f64().is_finite()
                && (0.0..200_000.0).contains(&value.as_f64()) =>
        {
            let (year, month, day, hour, minute, second, _) = value.to_ymd_hms_milli();
            format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}")
        }
        Data::DateTimeIso(value) => value.replace('T', " "),
        _ => cell.to_string(),
    }
}

fn check_sheet_bounds(bytes: &[u8]) -> Result<(), String> {
    use quick_xml::events::Event;
    let mut xml = quick_xml::Reader::from_reader(bytes);
    let mut rows = 0;
    let mut cells = 0;
    loop {
        match xml.read_event().map_err(|_| "工作表 XML 无效")? {
            Event::Eof => return Ok(()),
            Event::Start(element) | Event::Empty(element) => {
                let name = element.local_name();
                if name.as_ref() == b"row" {
                    rows += 1;
                    cells = 0;
                }
                if name.as_ref() == b"c" {
                    cells += 1;
                }
                if rows > MAX_ROWS + 100 || cells > 50 {
                    return Err("工作表范围过大，请分批导出账单".into());
                }
                for attribute in element.attributes() {
                    let attribute = attribute.map_err(|_| "工作表属性无效")?;
                    let key = attribute.key.as_ref();
                    if (name.as_ref() == b"c" && key == b"r")
                        || (name.as_ref() == b"dimension" && key == b"ref")
                    {
                        let reference =
                            std::str::from_utf8(&attribute.value).map_err(|_| "单元格地址无效")?;
                        for cell in reference.split(':') {
                            let letters = cell.bytes().take_while(u8::is_ascii_alphabetic).count();
                            if letters == 0 || letters > 2 {
                                return Err("工作表范围过大或地址无效".into());
                            }
                            let col = cell[..letters].bytes().fold(0usize, |n, b| {
                                n * 26 + (b.to_ascii_uppercase() - b'A' + 1) as usize
                            });
                            let row: usize =
                                cell[letters..].parse().map_err(|_| "单元格行号无效")?;
                            if col > 50 || row == 0 || row > MAX_ROWS + 100 {
                                return Err("工作表范围过大，请分批导出账单".into());
                            }
                        }
                    }
                    if name.as_ref() == b"row" && key == b"r" {
                        let row: usize = std::str::from_utf8(&attribute.value)
                            .map_err(|_| "行号无效")?
                            .parse()
                            .map_err(|_| "行号无效")?;
                        if row > MAX_ROWS + 100 {
                            return Err("工作表范围过大，请分批导出账单".into());
                        }
                    }
                }
            }
            _ => (),
        }
    }
}

fn parse_table(table: Vec<Vec<String>>, rules: &[Rule]) -> Result<Preview, String> {
    let header_index = table
        .iter()
        .position(|row| {
            row.iter()
                .any(|v| matches!(clean(v), "交易时间" | "交易创建时间" | "交易成功时间"))
                && row
                    .iter()
                    .any(|v| matches!(clean(v), "金额(元)" | "金额（元）" | "金额"))
        })
        .ok_or("没有识别出微信/支付宝账单表头，请上传原始明细文件")?;
    let header: HashMap<String, usize> = table[header_index]
        .iter()
        .enumerate()
        .map(|(i, s)| (clean(s).to_owned(), i))
        .collect();
    let source = if header.contains_key("交易订单号")
        || table[..=header_index]
            .iter()
            .flatten()
            .any(|s| s.contains("支付宝"))
    {
        "alipay"
    } else {
        "wechat"
    };
    let mut result = Preview {
        source: source.into(),
        rows: Vec::new(),
        skipped: 0,
        errors: Vec::new(),
    };
    for (index, row) in table.into_iter().enumerate().skip(header_index + 1) {
        if row.iter().all(|v| v.trim().is_empty()) {
            continue;
        }
        if result.rows.len() >= MAX_ROWS {
            return Err("单次最多导入一万笔".into());
        }
        let get = |keys: &[&str]| -> String {
            keys.iter()
                .find_map(|k| header.get(*k).and_then(|i| row.get(*i)))
                .map(|s| clean(s).to_owned())
                .unwrap_or_default()
        };
        let timestamp = get(&["交易时间", "交易创建时间", "交易成功时间"]);
        // Export footers contain summaries and separators rather than a transaction date.
        if timestamp.is_empty() || timestamp.starts_with(['-', '*', '共']) {
            continue;
        }
        let status = get(&["当前状态", "交易状态", "状态"]);
        if ["交易关闭", "支付失败", "已关闭", "未支付", "已取消"]
            .iter()
            .any(|s| status.contains(s))
        {
            result.skipped += 1;
            continue;
        }
        let date = timestamp.get(..10).unwrap_or(&timestamp).replace('/', "-");
        let direction = get(&["收/支", "收/支类型", "资金流向"]);
        let trade_type = get(&["交易类型", "交易分类"]);
        let description = get(&["商品", "商品说明", "商品名称"]);
        let merchant = get(&["交易对方", "对方名称"]);
        let detail = format!("{trade_type} {description} {status}");
        let kind = match direction.as_str() {
            "收入" if detail.contains("退款") || detail.contains("退货") => Kind::Refund,
            "收入" => Kind::Income,
            "支出" => Kind::Expense,
            "不计收支" | "不计收入支出" | "其他" | "/" | "-"
                if trade_type.contains("退款") || trade_type.contains("退货") =>
            {
                Kind::Refund
            }
            "不计收支" | "不计收入支出" | "其他" | "/" | "-" => Kind::Transfer,
            _ => {
                result
                    .errors
                    .push(format!("第 {} 行：无法识别收支方向", index + 1));
                continue;
            }
        };
        let kind = if ["充值", "提现", "信用卡还款"].contains(&trade_type.as_str()) {
            Kind::Transfer
        } else {
            kind
        };
        let amount = match parse_money(&get(&["金额(元)", "金额（元）", "金额"])) {
            Ok(n) => n,
            Err(e) => {
                result.errors.push(format!("第 {} 行：{e}", index + 1));
                continue;
            }
        };
        if !valid_date(&date) {
            result
                .errors
                .push(format!("第 {} 行：日期格式无效", index + 1));
            continue;
        }
        let mut warnings = Vec::new();
        let id = get(&["交易单号", "交易订单号"]);
        let external_id = if id.is_empty() || id == "/" || id == "-" {
            warnings.push("缺少交易号，按时间、对象、商品和金额去重；相同交易请核对".into());
            format!(
                "fallback:{}",
                digest(&format!(
                    "{timestamp}|{merchant}|{description}|{amount}|{}",
                    kind.as_str()
                ))
            )
        } else {
            format!("{id}:{}", kind.as_str())
        };
        let refunded_cents = imported_refund(&kind, &status, amount, &mut warnings);
        if kind == Kind::Refund {
            warnings.push(
                "退款流水仅留档，不另计收支；若原支出未注明退款，可入账后手动关联原支出".into(),
            );
        }
        let category = classify(&kind, &merchant, &description, rules);
        if category == "待分类"
            && amount > 0
            && (kind == Kind::Income || (kind == Kind::Expense && refunded_cents < amount))
        {
            warnings.push("尚未匹配分类，入账后可修改并保存商户规则".into());
        }
        let method = get(&["支付方式", "付款方式", "收/付款方式"]);
        let account = if method.is_empty() || method == "/" || method == "-" {
            if source == "wechat" {
                "微信".into()
            } else {
                "支付宝".into()
            }
        } else {
            method
        };
        let entry = Entry {
            id: 0,
            date,
            kind,
            amount_cents: amount,
            refunded_cents,
            refund_of: None,
            import_status: status,
            merchant,
            description,
            category,
            account,
            note: String::new(),
            split_start: None,
            split_months: None,
            source: source.into(),
            external_id: Some(external_id.clone()),
        };
        if let Err(e) = entry.validate() {
            result.errors.push(format!("第 {} 行：{e}", index + 1));
            continue;
        }
        result.rows.push(PreviewRow {
            line: index + 1,
            entry,
            duplicate: false,
            warnings,
        });
    }
    // Repeated exports can contain older and newer statuses for one payment.
    // Keep its largest cumulative refund regardless of row order.
    let mut canonical = HashMap::<(String, String), usize>::new();
    for index in 0..result.rows.len() {
        let entry = &result.rows[index].entry;
        let Some(id) = &entry.external_id else {
            continue;
        };
        let key = (entry.source.clone(), id.clone());
        if let Some(previous) = canonical.get(&key).copied() {
            if entry.refunded_cents > result.rows[previous].entry.refunded_cents {
                result.rows[previous].duplicate = true;
                canonical.insert(key, index);
            } else {
                result.rows[index].duplicate = true;
            }
        } else {
            canonical.insert(key, index);
        }
    }
    if result.rows.is_empty() && result.errors.is_empty() {
        return Err("文件中没有可导入的成功交易".into());
    }
    Ok(result)
}

fn imported_refund(kind: &Kind, status: &str, amount: i64, warnings: &mut Vec<String>) -> i64 {
    if *kind != Kind::Expense || amount == 0 || !status.contains('退') {
        return 0;
    }
    let refunded = if status == "已全额退款" {
        Some(amount)
    } else {
        status.strip_prefix("已退款").and_then(|value| {
            let value = value.trim();
            let value = value
                .strip_prefix('(')
                .and_then(|v| v.strip_suffix(')'))
                .or_else(|| value.strip_prefix('（').and_then(|v| v.strip_suffix('）')))
                .unwrap_or(value)
                .trim();
            let value = value.strip_suffix('元').unwrap_or(value).trim();
            parse_money(value).ok().filter(|n| *n <= amount)
        })
    };
    if let Some(refunded) = refunded {
        warnings.push(format!(
            "原支出累计退款 ¥{}.{:02} 将抵扣原支出月份；独立退款流水不再重复扣减",
            refunded / 100,
            refunded % 100
        ));
        refunded
    } else {
        warnings.push("原支出含退款状态，但无法确认有效退款金额；暂按原额记账，请核对后补填累计退款或关联退款流水".into());
        0
    }
}

fn clean(s: &str) -> &str {
    s.trim()
        .trim_start_matches('\u{feff}')
        .trim_start_matches('\'')
        .trim()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn alipay_gbk_zero_rides_remain_individual_deduplicated_records() {
        let csv = "支付宝交易明细\n交易时间,交易分类,交易对方,商品说明,收/支,金额,收/付款方式,交易状态,交易订单号\n2026-09-01 10:00:00,交通出行,测试出行,共享单车骑行,支出,0.00,支付宝,交易成功,bike-1\n2026-09-01 11:00:00,交通出行,测试出行,共享单车骑行,支出,0,支付宝,交易成功,bike-2\n2026-09-01 10:00:00,交通出行,测试出行,共享单车骑行,支出,0.00,支付宝,交易成功,bike-1\n2026-09-01 12:00:00,交通出行,测试出行,共享单车骑行,支出,1.50,支付宝,交易成功,bike-paid\n2026-09-01 13:00:00,其他,测试对象,测试零元收入,收入,0.00,支付宝,交易成功,zero-income\n2026-09-01 14:00:00,退款,测试对象,退款,收入,0.00,支付宝,退款成功,zero-refund\n";
        let (bytes, _, errors) = encoding_rs::GBK.encode(csv);
        assert!(!errors);
        let preview = parse("alipay.csv", &bytes, &[]).unwrap();
        assert_eq!(preview.source, "alipay");
        assert_eq!(preview.rows.len(), 5);
        assert_eq!(preview.errors.len(), 1); // A refund must represent actual money.
        assert_eq!(preview.rows[0].entry.amount_cents, 0);
        assert_eq!(preview.rows[0].entry.category, "交通出行");
        assert!(!preview.rows[0].duplicate);
        assert!(!preview.rows[1].duplicate);
        assert!(preview.rows[2].duplicate);
        assert!(preview.rows[4].warnings.is_empty());
        assert!(is_bike_ride("测试出行", "共享单车骑行"));
        for purchase in ["单车月卡", "骑行会员", "骑行卡套餐", "骑行头盔"] {
            assert!(!is_bike_ride("测试出行", purchase));
            assert!(!is_bike_ride(purchase, ""));
        }
    }

    fn dated_workbook(date_1904: bool) -> Vec<u8> {
        let epoch = if date_1904 {
            (1904, 1, 1)
        } else {
            (1899, 12, 30)
        };
        let date = chrono::NaiveDate::from_ymd_opt(2026, 7, 1).unwrap();
        let epoch = chrono::NaiveDate::from_ymd_opt(epoch.0, epoch.1, epoch.2).unwrap();
        let serial = date.signed_duration_since(epoch).num_days() as f64 + 0.5;
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default();
        let strings = [
            "微信支付账单明细",
            "交易时间",
            "交易类型",
            "交易对方",
            "商品",
            "收/支",
            "金额(元)",
            "支付方式",
            "当前状态",
            "交易单号",
            "商户单号",
            "备注",
            "商户消费",
            "合成测试商户",
            "合成商品",
            "支出",
            "零钱",
            "支付成功",
            "synthetic-order",
        ];
        let shared = strings
            .iter()
            .map(|s| format!("<si><t>{s}</t></si>"))
            .collect::<String>();
        let header = (0..11)
            .map(|i| {
                format!(
                    "<c r=\"{}18\" t=\"s\"><v>{}</v></c>",
                    char::from(b'A' + i),
                    i + 1
                )
            })
            .collect::<String>();
        let files = [
            ("[Content_Types].xml", r#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/></Types>"#.to_owned()),
            ("_rels/.rels", r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#.to_owned()),
            ("xl/workbook.xml", format!(r#"<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><workbookPr date1904="{}"/><sheets><sheet name="账单" sheetId="1" r:id="rId1"/></sheets></workbook>"#, u8::from(date_1904))),
            ("xl/_rels/workbook.xml.rels", r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/><Relationship Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/sharedStrings" Target="sharedStrings.xml"/></Relationships>"#.to_owned()),
            ("xl/styles.xml", r#"<styleSheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><numFmts count="2"><numFmt numFmtId="164" formatCode="yyyy-mm-dd hh:mm:ss"/><numFmt numFmtId="165" formatCode="¥#,##0.00"/></numFmts><cellXfs count="3"><xf numFmtId="0"/><xf numFmtId="164"/><xf numFmtId="165"/></cellXfs></styleSheet>"#.to_owned()),
            ("xl/sharedStrings.xml", format!(r#"<sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">{shared}</sst>"#)),
            ("xl/worksheets/sheet1.xml", format!(r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><dimension ref="A1:K19"/><sheetData><row r="1"><c r="A1" t="s"><v>0</v></c></row><row r="18">{header}</row><row r="19"><c r="A19" s="1"><v>{serial}</v></c><c r="B19" t="s"><v>12</v></c><c r="C19" t="s"><v>13</v></c><c r="D19" t="s"><v>14</v></c><c r="E19" t="s"><v>15</v></c><c r="F19" s="2"><v>148.00</v></c><c r="G19" t="s"><v>16</v></c><c r="H19" t="s"><v>17</v></c><c r="I19" t="s"><v>18</v></c></row></sheetData></worksheet>"#)),
        ];
        for (name, contents) in files {
            zip.start_file(name, options).unwrap();
            zip.write_all(contents.as_bytes()).unwrap();
        }
        zip.finish().unwrap().into_inner()
    }

    #[test]
    fn wechat_xlsx_numeric_dates_with_styles_and_shared_strings() {
        for epoch_1904 in [false, true] {
            let preview = parse("wechat.xlsx", &dated_workbook(epoch_1904), &[]).unwrap();
            assert!(preview.errors.is_empty(), "{:?}", preview.errors);
            assert_eq!(preview.rows.len(), 1);
            let row = &preview.rows[0];
            assert_eq!(row.line, 19);
            assert_eq!(row.entry.date, "2026-07-01");
            assert_eq!(row.entry.amount_cents, 14800);
            assert_eq!(
                row.entry.external_id.as_deref(),
                Some("synthetic-order:expense")
            );
        }
        assert_eq!(
            spreadsheet_cell(&Data::DateTimeIso("2026-07-01T12:00:00".into())),
            "2026-07-01 12:00:00"
        );
        assert_eq!(spreadsheet_cell(&Data::Float(148.0)), "148");
    }

    #[test]
    fn oversized_sparse_sheet_rejected_before_range_allocation() {
        assert!(
            check_sheet_bounds(br#"<worksheet><dimension ref="A1:XFD1048576"/></worksheet>"#)
                .is_err()
        );
        assert!(
            check_sheet_bounds(
                br#"<worksheet><row r="999999999"><c r="A999999999"/></row></worksheet>"#
            )
            .is_err()
        );
        assert!(
            check_sheet_bounds(
                br#"<worksheet><row r="1"><c r="A1"/><c r="J1"/></row></worksheet>"#
            )
            .is_ok()
        );
        assert!(parse_money("1,,2.00").is_err());
        assert!(parse_money("1,23.00").is_err());
        assert!(parse_money("1234.5,0").is_err());
    }
    #[test]
    fn wechat_preamble_duplicates_refunds_transfers_and_failures() {
        let csv = "微信支付账单明细\n导出账户,测试\n交易时间,交易类型,交易对方,商品,收/支,金额(元),支付方式,当前状态,交易单号\n2026-07-01 12:00:00,商户消费,咖啡店,拿铁,支出,18.50,零钱,支付成功,wx1\n2026-07-01 12:00:00,商户消费,咖啡店,拿铁,支出,18.50,零钱,支付成功,wx1\n2026-07-02 12:00:00,商户退款,咖啡店,退款,收入,18.50,零钱,退款成功,wx1\n2026-07-03 12:00:00,提现,本人,零钱提现,不计收支,100.00,银行卡,成功,wx2\n2026-07-04 12:00:00,商户消费,店,商品,支出,5.00,零钱,交易关闭,wx3\n";
        let p = parse("wechat.csv", csv.as_bytes(), &[]).unwrap();
        assert_eq!(p.rows.len(), 4);
        assert_eq!(p.skipped, 1);
        assert_eq!(p.rows[0].entry.category, "餐饮");
        assert!(p.rows[1].duplicate);
        assert_eq!(p.rows[2].entry.kind, Kind::Refund);
        assert_ne!(p.rows[0].entry.external_id, p.rows[2].entry.external_id);
        assert_eq!(p.rows[3].entry.kind, Kind::Transfer);
    }

    #[test]
    fn refund_statuses_reduce_expenses_and_refund_rows_remain_audit_records() {
        let cases = [
            ("支出", "商户消费", "已全额退款", Kind::Expense, 10000),
            ("支出", "商户消费", "已退款(¥20.01)", Kind::Expense, 2001),
            ("支出", "商户消费", "已退款¥20.01", Kind::Expense, 2001),
            (
                "支出",
                "商户消费",
                "已退款（￥20.01元）",
                Kind::Expense,
                2001,
            ),
            ("支出", "商户消费", "退款处理中", Kind::Expense, 0),
            ("支出", "商户消费", "已退款(¥101.00)", Kind::Expense, 0),
            ("支出", "商户消费", "已退款(¥错误)", Kind::Expense, 0),
            ("收入", "商户退款", "已全额退款", Kind::Refund, 0),
            ("不计收支", "商户退款", "退款成功", Kind::Refund, 0),
            ("/", "商户退款", "退款成功", Kind::Refund, 0),
            ("不计收支", "提现", "成功", Kind::Transfer, 0),
        ];
        let mut csv = "交易时间,交易类型,交易对方,商品,收/支,金额(元),支付方式,当前状态,交易单号\n"
            .to_owned();
        for (index, (direction, trade_type, status, _, _)) in cases.iter().enumerate() {
            csv.push_str(&format!("2026-07-01 12:00:00,{trade_type},合成商户,合成商品,{direction},100.00,零钱,{status},synthetic-{index}\n"));
        }
        let preview = parse("wechat.csv", csv.as_bytes(), &[]).unwrap();
        assert!(preview.errors.is_empty());
        assert_eq!(preview.rows.len(), cases.len());
        for (row, (_, _, status, kind, refunded)) in preview.rows.iter().zip(cases) {
            assert_eq!(row.entry.kind, kind);
            assert_eq!(row.entry.amount_cents, 10000);
            assert_eq!(row.entry.refunded_cents, refunded);
            assert_eq!(row.entry.import_status, status);
            assert_eq!(row.entry.refund_of, None);
            if kind == Kind::Expense && refunded == 0 {
                assert!(row.warnings.iter().any(|s| s.contains("无法确认")));
            }
            if kind == Kind::Refund {
                assert!(row.warnings.iter().any(|s| s.contains("仅留档")));
            }
            if kind == Kind::Refund || refunded == row.entry.amount_cents {
                assert!(row.warnings.iter().all(|s| !s.contains("尚未匹配分类")));
            }
        }
    }

    #[test]
    fn alipay_gbk_and_rules() {
        let csv = "支付宝交易记录明细\n交易时间,交易分类,交易对方,商品说明,收/支,金额,收/付款方式,交易状态,交易订单号\n2026-07-02 10:20:30,购物,我的小店,商品,支出,148.00,花呗,交易成功,ali1\n";
        let (bytes, _, _) = encoding_rs::GBK.encode(csv);
        let rules = vec![Rule {
            id: 1,
            pattern: "小店".into(),
            category: "兴趣爱好".into(),
            kind: Kind::Expense,
        }];
        let p = parse("alipay.csv", &bytes, &rules).unwrap();
        assert_eq!(p.source, "alipay");
        assert_eq!(p.rows[0].entry.category, "兴趣爱好");
        assert_eq!(p.rows[0].entry.amount_cents, 14800);
    }
}
