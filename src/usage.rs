use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;

const WEEK_MINUTES: i64 = 7 * 24 * 60;

#[derive(Debug, Clone)]
pub struct WeeklyUsage {
    pub used_percent: f64,
    pub resets_at: i64,
}

#[derive(Debug, PartialEq)]
pub struct BankedReset {
    pub count: u64,
    pub expires_at: Option<i64>,
    pub incomplete: bool,
}

pub fn banked_due(expiry: Option<i64>, now: i64, hour: u16, last_notice: Option<i64>) -> bool {
    (7..22).contains(&hour)
        && last_notice.is_none_or(|last| now.saturating_sub(last) >= 2 * 60 * 60)
        && expiry.is_some_and(|time| time > now && time - now <= 48 * 60 * 60)
}

pub fn parse_banked(result: &Value, now: i64) -> Option<BankedReset> {
    let resets = result.get("rateLimitResetCredits")?;
    let count = resets.get("availableCount")?.as_u64()?;
    if count == 0 {
        return Some(BankedReset {
            count,
            expires_at: None,
            incomplete: false,
        });
    }
    let Some(credits) = resets.get("credits").and_then(Value::as_array) else {
        return Some(BankedReset {
            count,
            expires_at: None,
            incomplete: true,
        });
    };
    let mut expiries = Vec::new();
    for credit in credits {
        if credit.get("status").and_then(Value::as_str) == Some("available")
            && let Some(expiry) = credit
                .get("expiresAt")
                .and_then(Value::as_i64)
                .filter(|&time| time > now)
        {
            expiries.push(expiry);
        }
    }
    Some(BankedReset {
        count,
        expires_at: expiries.iter().copied().min(),
        incomplete: expiries.len() as u64 != count,
    })
}

pub fn parse_weekly(result: &Value) -> Result<WeeklyUsage> {
    let bucket = result
        .get("rateLimitsByLimitId")
        .and_then(|buckets| buckets.get("codex"))
        .or_else(|| {
            result.get("rateLimits").filter(|bucket| {
                bucket
                    .get("limitId")
                    .is_none_or(|id| id.is_null() || id == "codex")
            })
        })
        .context("服務未提供 Codex 額度資料；請確認 CLI 使用 ChatGPT 帳戶登入")?;

    let mut weekly = ["primary", "secondary"].into_iter().filter_map(|key| {
        bucket.get(key).filter(|window| {
            window.get("windowDurationMins").and_then(Value::as_i64) == Some(WEEK_MINUTES)
        })
    });
    let window = weekly
        .next()
        .context("服務未提供每週（10080 分鐘）額度，無法判斷剩餘用量")?;
    ensure!(
        weekly.next().is_none(),
        "服務回傳多個每週時段，無法確定額度"
    );
    let used_percent = window
        .get("usedPercent")
        .and_then(Value::as_f64)
        .context("每週已用百分比缺失或格式錯誤")?;
    let resets_at = window
        .get("resetsAt")
        .and_then(Value::as_i64)
        .context("每週額度重設時間缺失或格式錯誤")?;
    ensure!(
        used_percent.is_finite() && used_percent >= 0.0,
        "每週已用百分比無效"
    );
    ensure!(resets_at > 0, "每週額度重設時間無效");
    Ok(WeeklyUsage {
        used_percent,
        resets_at,
    })
}

#[derive(Default)]
pub struct Alerts {
    previous: Option<WeeklyUsage>,
    notified_level: u8,
    pending: Option<Notice>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Notice {
    Started,
    Threshold,
    CycleChanged,
    Reset,
}

impl Notice {
    pub fn title(self) -> &'static str {
        match self {
            Self::Started => "Codex 每週額度：開始監察",
            Self::Threshold => "Codex 每週額度提醒",
            Self::CycleChanged => "Codex 每週額度：週期已更新",
            Self::Reset => "Codex 每週額度已重設！",
        }
    }
}

impl Alerts {
    pub fn observe(&mut self, usage: &WeeklyUsage) -> Option<Notice> {
        if let Some(previous) = &self.previous {
            if previous.used_percent > 0.0 && usage.used_percent == 0.0 {
                self.notified_level = 0;
                self.pending = Some(Notice::Reset);
            } else if previous.resets_at != usage.resets_at {
                self.notified_level = 0;
                if self.pending != Some(Notice::Reset) {
                    self.pending = Some(Notice::CycleChanged);
                }
            }
        } else {
            self.pending = Some(Notice::Started);
        }
        if self.pending.is_none() && level(usage.used_percent) > self.notified_level {
            self.pending = Some(Notice::Threshold);
        }
        self.previous = Some(usage.clone());
        self.pending
    }

    pub fn delivered(&mut self, usage: &WeeklyUsage) {
        self.notified_level = self.notified_level.max(level(usage.used_percent));
        self.pending = None;
    }
}

fn level(percent: f64) -> u8 {
    [80, 90, 100]
        .into_iter()
        .filter(|&threshold| percent >= f64::from(threshold))
        .max()
        .unwrap_or(0)
}

pub fn check_fresh(usage: &WeeklyUsage, now: i64) -> Result<()> {
    if usage.used_percent > 0.0 && usage.resets_at <= now {
        bail!("服務回傳的每週額度已過重設時間，等待下次更新");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn banked_resets_distinguish_unknown_empty_and_incomplete_details() {
        assert_eq!(parse_banked(&json!({}), 100), None);
        assert_eq!(
            parse_banked(
                &json!({"rateLimitResetCredits": {"availableCount": 0}}),
                100
            ),
            Some(BankedReset {
                count: 0,
                expires_at: None,
                incomplete: false
            })
        );
        let mut result = json!({"rateLimitResetCredits": {"availableCount": 2, "credits": [
            {"status": "available", "expiresAt": 300},
            {"status": "available", "expiresAt": 200}
        ]}});
        assert_eq!(
            parse_banked(&result, 100),
            Some(BankedReset {
                count: 2,
                expires_at: Some(200),
                incomplete: false
            })
        );
        result["rateLimitResetCredits"]["credits"][1]["expiresAt"] = Value::Null;
        assert_eq!(
            parse_banked(&result, 100),
            Some(BankedReset {
                count: 2,
                expires_at: Some(300),
                incomplete: true
            })
        );
        result["rateLimitResetCredits"]["credits"] = Value::Null;
        assert_eq!(
            parse_banked(&result, 100),
            Some(BankedReset {
                count: 2,
                expires_at: None,
                incomplete: true
            })
        );
    }

    #[test]
    fn banked_reminder_is_limited_to_48_hours_and_daytime() {
        let expiry = 1_000_000;
        assert!(!banked_due(Some(expiry), expiry - 48 * 3600 - 1, 7, None));
        assert!(banked_due(Some(expiry), expiry - 48 * 3600, 7, None));
        assert!(banked_due(Some(expiry), expiry - 1, 21, None));
        assert!(!banked_due(Some(expiry), expiry - 1, 22, None));
        assert!(!banked_due(Some(expiry), expiry - 1, 6, None));
        assert!(!banked_due(Some(expiry), expiry, 7, None));
        assert!(!banked_due(None, expiry - 1, 7, None));
        assert!(!banked_due(
            Some(expiry),
            expiry - 1,
            9,
            Some(expiry - 7200)
        ));
        assert!(banked_due(Some(expiry), expiry - 1, 9, Some(expiry - 7201)));
    }

    fn sample(percent: f64, reset: i64) -> WeeklyUsage {
        WeeklyUsage {
            used_percent: percent,
            resets_at: reset,
        }
    }

    #[test]
    fn identifies_week_by_duration_and_prefers_codex_bucket() {
        let value = json!({
            "rateLimits": {"secondary": {"usedPercent": 99, "windowDurationMins": 10080, "resetsAt": 9}},
            "rateLimitsByLimitId": {"codex": {
                "primary": {"usedPercent": 42.5, "windowDurationMins": 10080, "resetsAt": 99},
                "secondary": {"usedPercent": 80, "windowDurationMins": 300, "resetsAt": 88}
            }}
        });
        let usage = parse_weekly(&value).unwrap();
        assert_eq!(usage.used_percent, 42.5);
        assert_eq!(usage.resets_at, 99);
    }

    #[test]
    fn accepts_legacy_weekly_window_but_rejects_unknown_or_invalid_usage() {
        let mut value = json!({"rateLimits": {"secondary": {
            "usedPercent": 0, "windowDurationMins": 10080, "resetsAt": 99
        }}});
        assert_eq!(parse_weekly(&value).unwrap().used_percent, 0.0);
        value["rateLimits"]["secondary"]["usedPercent"] = Value::Null;
        assert!(parse_weekly(&value).is_err());
        value["rateLimits"]["secondary"]["usedPercent"] = json!(-1);
        assert!(parse_weekly(&value).is_err());
        value["rateLimits"]["secondary"]["usedPercent"] = json!(1);
        value["rateLimits"]["secondary"]["windowDurationMins"] = json!(300);
        assert!(parse_weekly(&value).is_err());
        assert!(parse_weekly(&json!({"rateLimits": null})).is_err());
    }

    #[test]
    fn alerts_once_per_level_and_rearms_after_reset() {
        let mut alerts = Alerts::default();
        let initial = sample(45.0, 1000);
        assert_eq!(alerts.observe(&initial), Some(Notice::Started));
        alerts.delivered(&initial);
        assert!(alerts.observe(&sample(79.9, 1000)).is_none());
        for percent in [80.0, 90.0, 100.0] {
            let usage = sample(percent, 1000);
            assert_eq!(alerts.observe(&usage), Some(Notice::Threshold));
            alerts.delivered(&usage);
            assert!(alerts.observe(&usage).is_none());
        }
        assert!(alerts.observe(&sample(85.0, 1000)).is_none());
        let reset = sample(0.0, 2000);
        assert_eq!(alerts.observe(&reset), Some(Notice::Reset));
        alerts.delivered(&reset);
        assert_eq!(alerts.observe(&sample(80.0, 2000)), Some(Notice::Threshold));
    }

    #[test]
    fn failed_delivery_retries_and_large_jump_only_needs_one_notice() {
        let mut alerts = Alerts::default();
        let usage = sample(95.0, 1000);
        assert!(alerts.observe(&usage).is_some());
        assert!(alerts.observe(&usage).is_some());
        alerts.delivered(&usage);
        assert!(alerts.observe(&sample(94.0, 1000)).is_none());
        assert!(alerts.observe(&sample(100.0, 1000)).is_some());
    }

    #[test]
    fn full_reset_notifies_even_when_reset_timestamp_does_not_change() {
        let mut alerts = Alerts::default();
        let initial = sample(0.0, 1000);
        alerts.observe(&initial);
        alerts.delivered(&initial);
        assert!(alerts.observe(&sample(12.0, 1000)).is_none());
        let reset = sample(0.0, 1000);
        assert_eq!(alerts.observe(&reset), Some(Notice::Reset));
        assert_eq!(alerts.observe(&reset), Some(Notice::Reset));
        // 通知失敗後即使又開始使用，仍保留重設事件。
        assert_eq!(alerts.observe(&sample(1.0, 1000)), Some(Notice::Reset));
        alerts.delivered(&sample(1.0, 1000));
        assert!(alerts.observe(&sample(1.0, 1000)).is_none());
    }

    #[test]
    fn changed_reset_date_without_full_recovery_is_only_a_cycle_update() {
        let mut alerts = Alerts::default();
        let initial = sample(80.0, 1000);
        alerts.observe(&initial);
        alerts.delivered(&initial);
        assert_eq!(
            alerts.observe(&sample(10.0, 2000)),
            Some(Notice::CycleChanged)
        );
        assert_eq!(alerts.observe(&sample(0.0, 2000)), Some(Notice::Reset));
    }

    #[test]
    fn stale_data_is_not_treated_as_current_usage() {
        assert!(check_fresh(&sample(99.0, 1000), 1000).is_err());
        assert!(check_fresh(&sample(99.0, 1000), 999).is_ok());
        // 回滿時服務可能尚未更新下一次重設時間，仍需讓重設偵測收到這筆資料。
        assert!(check_fresh(&sample(0.0, 1000), 1001).is_ok());
    }

    #[test]
    fn repeated_full_samples_do_not_notify_but_another_refill_does() {
        let mut alerts = Alerts::default();
        let used = sample(12.0, 1000);
        alerts.observe(&used);
        alerts.delivered(&used);
        let full = sample(0.0, 1000);
        assert_eq!(alerts.observe(&full), Some(Notice::Reset));
        alerts.delivered(&full);
        assert!(alerts.observe(&full).is_none());
        assert!(alerts.observe(&used).is_none());
        assert_eq!(alerts.observe(&full), Some(Notice::Reset));
    }
}
