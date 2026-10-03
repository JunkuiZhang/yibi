use chrono::{Datelike, NaiveDate};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub const DEFAULT_START: &str = "2026-07-01";
pub const MAX_AMOUNT: i64 = 100_000_000_000;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Expense,
    Income,
    Transfer,
    Refund,
}

impl Kind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Expense => "expense",
            Self::Income => "income",
            Self::Transfer => "transfer",
            Self::Refund => "refund",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    #[serde(default)]
    pub id: i64,
    pub date: String,
    pub kind: Kind,
    pub amount_cents: i64,
    #[serde(default)]
    pub refunded_cents: i64,
    #[serde(default)]
    pub refund_of: Option<i64>,
    #[serde(default)]
    pub import_status: String,
    pub merchant: String,
    #[serde(default)]
    pub description: String,
    pub category: String,
    pub account: String,
    #[serde(default)]
    pub note: String,
    pub split_start: Option<String>,
    pub split_months: Option<u32>,
    #[serde(default = "manual_source")]
    pub source: String,
    pub external_id: Option<String>,
}

fn manual_source() -> String {
    "manual".into()
}

impl Entry {
    pub fn validate(&self) -> Result<(), String> {
        if !valid_date(&self.date) {
            return Err("请输入有效日期（YYYY-MM-DD）".into());
        }
        if !(0..=MAX_AMOUNT).contains(&self.amount_cents) {
            return Err("金额不能小于零或超过十亿元".into());
        }
        if self.kind == Kind::Refund && self.amount_cents == 0 {
            return Err("退款金额必须大于零".into());
        }
        if self.kind == Kind::Expense {
            if !(0..=self.amount_cents).contains(&self.refunded_cents) {
                return Err("累计退款金额不能小于零或超过原支出金额".into());
            }
        } else if self.refunded_cents != 0 {
            return Err("只有支出可设置累计退款金额".into());
        }
        if self.refund_of.is_some_and(|id| id <= 0) {
            return Err("关联支出的记录编号必须大于零".into());
        }
        if self.refund_of.is_some() && self.kind != Kind::Refund {
            return Err("只有退款流水可关联原支出".into());
        }
        for (text, max, label) in [
            (&self.merchant, 200, "交易对象"),
            (&self.description, 500, "商品说明"),
            (&self.category, 60, "分类"),
            (&self.account, 60, "账户"),
            (&self.note, 1000, "备注"),
            (&self.import_status, 200, "原始交易状态"),
        ] {
            if text.chars().count() > max || text.chars().any(|c| c == '\0') {
                return Err(format!("{label}过长或包含无效字符"));
            }
        }
        if self.category.trim().is_empty() || self.account.trim().is_empty() {
            return Err("分类和账户不能为空".into());
        }
        match (&self.split_start, self.split_months) {
            (None, None) => (),
            (Some(start), Some(months)) => {
                if self.kind != Kind::Expense || !(2..=120).contains(&months) {
                    return Err("只有支出可按月分摊，期限为 2–120 个月".into());
                }
                if self.amount_cents == 0 {
                    return Err("零元流水无需按月分摊".into());
                }
                month_index(start)?;
                if add_months(start, months - 1)?.as_str() > "2199-12" {
                    return Err("分摊结束时间超出范围".into());
                }
            }
            _ => return Err("请同时填写开始月份和分摊月数".into()),
        }
        Ok(())
    }

    pub fn expense_in(&self, month: &str, allocated: bool) -> i64 {
        if self.kind != Kind::Expense {
            return 0;
        }
        let net = self.amount_cents - self.refunded_cents;
        if allocated && let (Some(start), Some(count)) = (&self.split_start, self.split_months) {
            let offset = month_index(month).unwrap_or(-1) - month_index(start).unwrap_or(-1);
            if offset < 0 || offset >= count as i32 {
                return 0;
            }
            // Give the first remainder months one extra cent: exact sum, at most 1 cent apart.
            return net / count as i64 + i64::from((offset as i64) < net % count as i64);
        }
        if self.date.starts_with(month) { net } else { 0 }
    }
}

/// Exported cumulative refunds and linked refund rows may describe the same money.
/// Use the larger total so importing both representations never deducts it twice.
pub fn refund_totals(entries: &[Entry]) -> HashMap<i64, i64> {
    let mut linked = HashMap::<i64, i64>::new();
    for refund in entries.iter().filter(|e| e.kind == Kind::Refund) {
        if let Some(id) = refund.refund_of {
            let total = linked.entry(id).or_default();
            *total = total.saturating_add(refund.amount_cents.max(0));
        }
    }
    entries
        .iter()
        .filter(|e| e.kind == Kind::Expense)
        .map(|e| {
            let total = e
                .refunded_cents
                .max(linked.get(&e.id).copied().unwrap_or(0))
                .clamp(0, e.amount_cents.max(0));
            (e.id, total)
        })
        .collect()
}

pub fn effective_entries(entries: &[Entry]) -> Vec<Entry> {
    let totals = refund_totals(entries);
    entries
        .iter()
        .cloned()
        .map(|mut e| {
            if e.kind == Kind::Expense {
                e.refunded_cents = totals.get(&e.id).copied().unwrap_or(0);
            }
            e
        })
        .collect()
}

pub fn valid_date(value: &str) -> bool {
    value.len() == 10
        && NaiveDate::parse_from_str(value, "%Y-%m-%d")
            .is_ok_and(|d| (1900..=2199).contains(&d.year()) && d.to_string() == value)
}

pub fn month_index(value: &str) -> Result<i32, String> {
    if value.len() != 7 {
        return Err("月份格式应为 YYYY-MM".into());
    }
    let date = format!("{value}-01");
    if !valid_date(&date) {
        return Err("月份无效".into());
    }
    let d = NaiveDate::parse_from_str(&date, "%Y-%m-%d").map_err(|_| "月份无效")?;
    Ok(d.year() * 12 + d.month0() as i32)
}

pub fn add_months(start: &str, offset: u32) -> Result<String, String> {
    let i = month_index(start)? + offset as i32;
    Ok(format!("{:04}-{:02}", i / 12, i % 12 + 1))
}

pub fn parse_money(value: &str) -> Result<i64, String> {
    let input = value.trim().trim_start_matches(['¥', '￥']).trim();
    let whole_part = input.split('.').next().unwrap_or("");
    if whole_part.contains(',') {
        let groups: Vec<_> = whole_part.split(',').collect();
        if !(1..=3).contains(&groups[0].len()) || groups.iter().skip(1).any(|g| g.len() != 3) {
            return Err("金额的千位分隔符格式无效".into());
        }
    }
    if input
        .split_once('.')
        .is_some_and(|(_, fraction)| fraction.contains(','))
    {
        return Err("金额的小数部分不能包含分隔符".into());
    }
    let clean = input.replace(',', "");
    let parts: Vec<_> = clean.split('.').collect();
    if parts.len() > 2
        || parts[0].is_empty()
        || !parts[0].bytes().all(|b| b.is_ascii_digit())
        || parts
            .get(1)
            .is_some_and(|s| s.len() > 2 || s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()))
    {
        return Err("金额不能小于零，最多两位小数".into());
    }
    let whole: i64 = parts[0].parse().map_err(|_| "金额过大")?;
    let frac = parts
        .get(1)
        .map(|s| format!("{s:0<2}"))
        .unwrap_or("00".into());
    let cents = whole
        .checked_mul(100)
        .and_then(|n| n.checked_add(frac.parse().ok()?))
        .ok_or("金额过大")?;
    if !(0..=MAX_AMOUNT).contains(&cents) {
        return Err("金额超出范围".into());
    }
    Ok(cents)
}

#[cfg(test)]
pub(crate) fn sample() -> Entry {
    Entry {
        id: 0,
        date: "2026-10-04".into(),
        kind: Kind::Expense,
        amount_cents: 14800,
        refunded_cents: 0,
        refund_of: None,
        import_status: String::new(),
        merchant: "视频会员".into(),
        description: String::new(),
        category: "会员订阅".into(),
        account: "支付宝".into(),
        note: String::new(),
        split_start: Some("2027-02".into()),
        split_months: Some(12),
        source: "manual".into(),
        external_id: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn prepaid_membership_preserves_payment_and_exact_sum() {
        let e = sample();
        e.validate().unwrap();
        assert_eq!(e.expense_in("2026-10", false), 14800);
        assert_eq!(e.expense_in("2026-10", true), 0);
        assert_eq!(e.expense_in("2027-01", true), 0);
        assert_eq!(e.expense_in("2028-02", true), 0);
        let parts: Vec<_> = (0..12)
            .map(|i| e.expense_in(&add_months("2027-02", i).unwrap(), true))
            .collect();
        assert_eq!(parts.iter().sum::<i64>(), 14800);
        assert_eq!(parts.iter().max().unwrap() - parts.iter().min().unwrap(), 1);
        assert_eq!(add_months("2027-02", 11).unwrap(), "2028-01");
    }
    #[test]
    fn historical_membership_is_not_respread_at_ledger_start() {
        let mut e = sample();
        e.date = "2026-02-01".into();
        e.split_start = Some("2026-02".into());
        assert_eq!(e.expense_in("2026-07", false), 0);
        assert_eq!(e.expense_in("2026-07", true), 1233);
    }
    #[test]
    fn cents_are_exact_and_invalid_values_rejected() {
        assert_eq!(parse_money("￥1,234.5").unwrap(), 123450);
        assert_eq!(parse_money("0.01").unwrap(), 1);
        assert_eq!(parse_money("0").unwrap(), 0);
        assert_eq!(parse_money("￥0.00").unwrap(), 0);
        for s in [
            "-1",
            "1.001",
            "NaN",
            "1e3",
            "999999999999999999999999",
            "1.",
        ] {
            assert!(parse_money(s).is_err(), "{s}");
        }
        assert!(!valid_date("2026-02-30"));
        assert!(month_index("2027-13").is_err());
    }

    #[test]
    fn zero_amount_records_are_valid_but_cannot_be_refunds_or_memberships() {
        let mut e = sample();
        e.amount_cents = 0;
        assert!(e.validate().unwrap_err().contains("分摊"));
        e.split_start = None;
        e.split_months = None;
        for kind in [Kind::Expense, Kind::Income, Kind::Transfer] {
            e.kind = kind;
            e.validate().unwrap();
            assert_eq!(e.expense_in("2026-10", false), 0);
            assert_eq!(e.expense_in("2026-10", true), 0);
        }
        e.kind = Kind::Expense;
        e.refunded_cents = 1;
        assert!(e.validate().unwrap_err().contains("累计退款"));
        e.refunded_cents = 0;
        e.kind = Kind::Refund;
        assert!(e.validate().unwrap_err().contains("退款金额"));
        e.amount_cents = -1;
        assert!(e.validate().is_err());
    }

    fn refund(id: i64, parent: Option<i64>, date: &str, amount: i64) -> Entry {
        let mut e = sample();
        e.id = id;
        e.kind = Kind::Refund;
        e.date = date.into();
        e.amount_cents = amount;
        e.refund_of = parent;
        e.split_start = None;
        e.split_months = None;
        e
    }

    #[test]
    fn refunds_adjust_original_month_without_double_counting_export_status() {
        let mut expense = sample();
        expense.id = 1;
        expense.refunded_cents = 3000;
        let entries = vec![
            expense,
            refund(2, Some(1), "2026-11-01", 2000),
            refund(3, Some(1), "2026-12-01", 1000),
            refund(4, None, "2026-10-05", 9999),
        ];
        let effective = effective_entries(&entries);
        assert_eq!(refund_totals(&entries)[&1], 3000);
        assert_eq!(effective[0].expense_in("2026-10", false), 11800);
        for month in ["2026-11", "2026-12"] {
            assert_eq!(
                effective
                    .iter()
                    .map(|e| e.expense_in(month, false))
                    .sum::<i64>(),
                0
            );
        }
        assert_eq!(effective[3].expense_in("2026-10", false), 0);
        let mut adjusted = entries.clone();
        adjusted[0].refunded_cents = 0;
        assert_eq!(refund_totals(&adjusted)[&1], 3000);
        assert_eq!(entries[0].refunded_cents, 3000);
        assert_eq!(adjusted[0].refunded_cents, 0);
        adjusted.push(refund(5, Some(1), "2027-01-01", 11800));
        let effective = effective_entries(&adjusted);
        assert_eq!(effective[0].expense_in("2026-10", false), 0);
        assert_eq!(effective[0].expense_in("2027-02", true), 0);
    }

    #[test]
    fn refunded_membership_preserves_original_schedule_and_exact_net_sum() {
        let mut expense = sample();
        expense.id = 1;
        let entries = vec![expense, refund(2, Some(1), "2027-06-01", 4001)];
        let e = &effective_entries(&entries)[0];
        assert_eq!(e.expense_in("2026-10", false), 10799);
        assert_eq!(e.expense_in("2027-01", true), 0);
        assert_eq!(e.expense_in("2028-02", true), 0);
        let parts: Vec<_> = (0..12)
            .map(|i| e.expense_in(&add_months("2027-02", i).unwrap(), true))
            .collect();
        assert_eq!(parts.iter().sum::<i64>(), 10799);
        assert_eq!(parts.iter().max().unwrap() - parts.iter().min().unwrap(), 1);
    }

    #[test]
    fn old_payload_defaults_and_refund_metadata_validation() {
        let mut payload = serde_json::to_value(sample()).unwrap();
        for field in ["refunded_cents", "refund_of", "import_status"] {
            payload.as_object_mut().unwrap().remove(field);
        }
        let old: Entry = serde_json::from_value(payload).unwrap();
        assert_eq!(old.refunded_cents, 0);
        assert_eq!(old.refund_of, None);
        assert!(old.import_status.is_empty());
        old.validate().unwrap();

        let mut invalid = old.clone();
        invalid.refunded_cents = -1;
        assert!(invalid.validate().is_err());
        invalid.refunded_cents = invalid.amount_cents + 1;
        assert!(invalid.validate().is_err());
        invalid.refunded_cents = invalid.amount_cents;
        invalid.validate().unwrap();
        invalid.refund_of = Some(1);
        assert!(invalid.validate().is_err());
        invalid = refund(2, Some(0), "2026-11-01", 100);
        assert!(invalid.validate().is_err());
        invalid.refund_of = None;
        invalid.refunded_cents = 1;
        assert!(invalid.validate().is_err());
        invalid = old;
        invalid.import_status = "x".repeat(201);
        assert!(invalid.validate().is_err());
        invalid.import_status = "已退款\0".into();
        assert!(invalid.validate().is_err());
    }
}
