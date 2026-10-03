use chrono::{Datelike, NaiveDate};
use serde::{Deserialize, Serialize};

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
        if !(1..=MAX_AMOUNT).contains(&self.amount_cents) {
            return Err("金额必须大于零且不超过十亿元".into());
        }
        for (text, max, label) in [
            (&self.merchant, 200, "交易对象"),
            (&self.description, 500, "商品说明"),
            (&self.category, 60, "分类"),
            (&self.account, 60, "账户"),
            (&self.note, 1000, "备注"),
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
        if self.kind == Kind::Refund {
            return if self.date.starts_with(month) {
                -self.amount_cents
            } else {
                0
            };
        }
        if self.kind != Kind::Expense {
            return 0;
        }
        if allocated && let (Some(start), Some(count)) = (&self.split_start, self.split_months) {
            let offset = month_index(month).unwrap_or(-1) - month_index(start).unwrap_or(-1);
            if offset < 0 || offset >= count as i32 {
                return 0;
            }
            // Give the first remainder months one extra cent: exact sum, at most 1 cent apart.
            return self.amount_cents / count as i64
                + i64::from((offset as i64) < self.amount_cents % count as i64);
        }
        if self.date.starts_with(month) {
            self.amount_cents
        } else {
            0
        }
    }
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
    let clean = value
        .trim()
        .trim_start_matches(['¥', '￥'])
        .trim()
        .replace(',', "");
    let parts: Vec<_> = clean.split('.').collect();
    if parts.len() > 2
        || parts[0].is_empty()
        || !parts[0].bytes().all(|b| b.is_ascii_digit())
        || parts
            .get(1)
            .is_some_and(|s| s.len() > 2 || s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()))
    {
        return Err("金额须为正数，最多两位小数".into());
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
    if !(1..=MAX_AMOUNT).contains(&cents) {
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
        for s in [
            "0",
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
}
