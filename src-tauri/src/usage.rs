//! 5時間枠・週枠の集計と、上限ラインの推定。
//! - 実測%: statusLine ロガーが ~/.claude/dashboard/ratelimits.jsonl に書く rate_limits
//! - 推定: 会話ログの API 換算コストを、過去に上限 (429) に達した時点のコストで割る

use crate::indexer::{RateLimitHit, UsageRec};
use serde::Serialize;
use serde_json::Value;
use std::path::Path;

pub const H5: i64 = 5 * 3600 * 1000;
pub const D7: i64 = 7 * 24 * 3600 * 1000;

#[derive(Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct RlSample {
    pub ts: i64,
    pub five_pct: Option<f64>,
    pub five_reset: Option<i64>,
    pub week_pct: Option<f64>,
    pub week_reset: Option<i64>,
}

pub fn read_samples(path: &Path) -> Vec<RlSample> {
    let Ok(text) = std::fs::read_to_string(path) else { return vec![] };
    let mut out = vec![];
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        let ms = |x: &Value| x.as_i64().map(|s| s * 1000);
        out.push(RlSample {
            ts: v["ts"].as_i64().unwrap_or(0),
            five_pct: v["five_hour"]["used_percentage"].as_f64(),
            five_reset: ms(&v["five_hour"]["resets_at"]),
            week_pct: v["seven_day"]["used_percentage"].as_f64(),
            week_reset: ms(&v["seven_day"]["resets_at"]),
        });
    }
    out.sort_by_key(|s| s.ts);
    out
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Window {
    pub start: i64,
    pub end: i64,
    pub cost: f64,
    /// [ts, 累積コスト]
    pub cum: Vec<(i64, f64)>,
    /// [ts, 実測%]
    pub pct: Vec<(i64, f64)>,
    /// 上限 (429) に達した時刻
    pub hit_at: Option<i64>,
    /// プロジェクト別コスト (project_dir, cost)
    pub by_project: Vec<(String, f64)>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectInfo {
    pub key: String,
    pub label: String,
    pub cost: f64,
    /// 色の枠 (1..=5)。上位5件以外は None（その他）
    pub slot: Option<u8>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Calibration {
    /// 100% に相当する推定コスト (USD換算)
    pub limit: Option<f64>,
    pub source: String,
    pub from_hits: Vec<f64>,
    pub from_samples: Vec<f64>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageView {
    pub now: i64,
    pub weeks: Vec<Window>,
    pub blocks: Vec<Window>,
    pub week_cal: Calibration,
    pub five_cal: Calibration,
    pub latest: Option<RlSample>,
    pub daily: Vec<(i64, f64)>,
    pub total_samples: usize,
    /// 直近6週のコスト順
    pub projects: Vec<ProjectInfo>,
    /// ドル円レート（円換算の表示用）
    pub fx: Option<crate::fx::Fx>,
}

fn median(v: &mut Vec<f64>) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = v.len();
    Some(if n % 2 == 1 { v[n / 2] } else { (v[n / 2 - 1] + v[n / 2]) / 2.0 })
}

fn cost_between(usage: &[UsageRec], a: i64, b: i64) -> f64 {
    usage.iter().filter(|r| r.ts >= a && r.ts < b).map(|r| r.cost).sum()
}

fn near(a: i64, b: i64) -> bool {
    (a - b).abs() < 120_000
}

fn downsample(v: Vec<(i64, f64)>, max: usize) -> Vec<(i64, f64)> {
    if v.len() <= max {
        return v;
    }
    let step = v.len() as f64 / max as f64;
    let mut out: Vec<(i64, f64)> = (0..max).map(|i| v[(i as f64 * step) as usize]).collect();
    out.push(*v.last().unwrap());
    out
}

fn build_window(usage: &[UsageRec], samples: &[RlSample], hits: &[RateLimitHit], start: i64, end: i64, weekly: bool) -> Window {
    let mut cum = vec![(start, 0.0)];
    let mut acc = 0.0;
    let mut by_project: Vec<(String, f64)> = vec![];
    for r in usage.iter().filter(|r| r.ts >= start && r.ts < end) {
        acc += r.cost;
        cum.push((r.ts, acc));
        match by_project.iter_mut().find(|(k, _)| *k == r.project) {
            Some(e) => e.1 += r.cost,
            None => by_project.push((r.project.clone(), r.cost)),
        }
    }
    let pct = samples
        .iter()
        .filter_map(|s| {
            let (p, rs) = if weekly { (s.week_pct, s.week_reset) } else { (s.five_pct, s.five_reset) };
            match (p, rs) {
                (Some(p), Some(rs)) if near(rs, end) && s.ts >= start && s.ts < end => Some((s.ts, p)),
                _ => None,
            }
        })
        .collect();
    let kind = if weekly { "seven_day" } else { "five_hour" };
    let hit_at = hits
        .iter()
        .filter(|h| h.kind == kind && near(h.resets_at * 1000, end))
        .map(|h| h.ts)
        .min();
    Window { start, end, cost: acc, cum: downsample(cum, 500), pct, hit_at, by_project }
}

pub fn compute(
    usage_in: &[UsageRec],
    hits: &[RateLimitHit],
    samples: &[RlSample],
    now: i64,
    labels: &std::collections::HashMap<String, String>,
) -> UsageView {
    let mut usage: Vec<UsageRec> = usage_in.iter().filter(|r| r.ts > 0).cloned().collect();
    usage.sort_by_key(|r| r.ts);

    // --- 週枠の区切り: 既知のリセット時刻 (固定アンカー) から 7日刻み ---
    let anchor = samples
        .iter()
        .rev()
        .find_map(|s| s.week_reset)
        .or_else(|| hits.iter().filter(|h| h.kind == "seven_day").map(|h| h.resets_at * 1000).max());
    let week_end = match anchor {
        Some(a) => {
            let k = (now - a).div_euclid(D7) + 1;
            a + k * D7
        }
        None => now + 1,
    };
    let weeks: Vec<Window> = (0..6)
        .map(|i| {
            let end = week_end - i * D7;
            build_window(&usage, samples, hits, end - D7, end, true)
        })
        .collect();

    // --- 5時間枠: 既知のリセット時刻を優先し、無ければ最初の利用時刻(時単位切り捨て)+5h ---
    let mut known: Vec<i64> = samples.iter().filter_map(|s| s.five_reset).collect();
    known.extend(hits.iter().filter(|h| h.kind == "five_hour").map(|h| h.resets_at * 1000));
    known.sort();
    known.dedup_by(|a, b| near(*a, *b));
    let since = now - 8 * 24 * 3600 * 1000;
    let mut bounds: Vec<(i64, i64)> = vec![];
    for r in usage.iter().filter(|r| r.ts >= since) {
        if let Some(&(_, e)) = bounds.last() {
            if r.ts < e {
                continue;
            }
        }
        let start = match known.iter().find(|&&k| k - H5 <= r.ts && r.ts < k) {
            Some(&k) => k - H5,
            None => r.ts - r.ts.rem_euclid(3600 * 1000),
        };
        bounds.push((start, start + H5));
    }
    // 現在の枠がまだ無ければ（利用ゼロ）、statusLine の値から作る
    if let Some(k) = known.iter().find(|&&k| k > now && k - H5 <= now) {
        if !bounds.iter().any(|&(_, e)| near(e, *k)) {
            bounds.push((k - H5, *k));
        }
    }
    let blocks: Vec<Window> = bounds
        .iter()
        .map(|&(s, e)| build_window(&usage, samples, hits, s, e, false))
        .collect();

    // --- 上限の推定 ---
    let calibrate = |kind: &str, span: i64| -> Calibration {
        let mut first_hit: Vec<(i64, i64)> = vec![]; // (reset_ms, hit_ts)
        for h in hits.iter().filter(|h| h.kind == kind) {
            let r = h.resets_at * 1000;
            match first_hit.iter_mut().find(|(x, _)| near(*x, r)) {
                Some(e) => e.1 = e.1.min(h.ts),
                None => first_hit.push((r, h.ts)),
            }
        }
        let mut from_hits: Vec<f64> = first_hit
            .iter()
            .map(|&(r, t)| cost_between(&usage, r - span, t))
            .filter(|c| *c > 0.0)
            .collect();
        // statusLine 実測: 各枠の最新サンプル (10%以上) からコスト/割合
        let mut latest_per: Vec<(i64, i64, f64)> = vec![]; // (reset, ts, pct)
        for s in samples {
            let (p, rs) = if kind == "seven_day" { (s.week_pct, s.week_reset) } else { (s.five_pct, s.five_reset) };
            if let (Some(p), Some(rs)) = (p, rs) {
                if p < 10.0 {
                    continue;
                }
                match latest_per.iter_mut().find(|(x, _, _)| near(*x, rs)) {
                    Some(e) => {
                        e.1 = s.ts;
                        e.2 = p
                    }
                    None => latest_per.push((rs, s.ts, p)),
                }
            }
        }
        let mut from_samples: Vec<f64> = latest_per
            .iter()
            .map(|&(rs, ts, p)| cost_between(&usage, rs - span, ts) / (p / 100.0))
            .filter(|c| *c > 0.0)
            .collect();
        let (limit, source) = if from_samples.len() >= 2 {
            (median(&mut from_samples.clone()), "statusline".to_string())
        } else if !from_hits.is_empty() {
            (median(&mut from_hits.clone()), "429".to_string())
        } else if !from_samples.is_empty() {
            (median(&mut from_samples.clone()), "statusline".to_string())
        } else {
            (None, "none".to_string())
        };
        from_hits.sort_by(|a, b| a.partial_cmp(b).unwrap());
        from_samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
        Calibration { limit, source, from_hits, from_samples }
    };
    let week_cal = calibrate("seven_day", D7);
    let five_cal = calibrate("five_hour", H5);

    // --- 日別 (ローカル日付) ---
    let mut daily: Vec<(i64, f64)> = vec![];
    use chrono::{Local, TimeZone};
    for r in usage.iter().filter(|r| r.ts >= now - 42 * 24 * 3600 * 1000) {
        let d = Local.timestamp_millis_opt(r.ts).unwrap().date_naive();
        let key = Local
            .from_local_datetime(&d.and_hms_opt(0, 0, 0).unwrap())
            .earliest()
            .map(|x| x.timestamp_millis())
            .unwrap_or(0);
        match daily.last_mut() {
            Some(l) if l.0 == key => l.1 += r.cost,
            _ => daily.push((key, r.cost)),
        }
    }

    let mut projects: Vec<ProjectInfo> = vec![];
    if let Some(oldest) = weeks.last().map(|w| w.start) {
        for r in usage.iter().filter(|r| r.ts >= oldest) {
            match projects.iter_mut().find(|p| p.key == r.project) {
                Some(p) => p.cost += r.cost,
                None => projects.push(ProjectInfo {
                    key: r.project.clone(),
                    label: labels.get(&r.project).cloned().unwrap_or_else(|| r.project.clone()),
                    cost: r.cost,
                    slot: None,
                }),
            }
        }
    }
    projects.sort_by(|a, b| b.cost.partial_cmp(&a.cost).unwrap());
    assign_slots(&mut projects);

    UsageView {
        projects,
        now,
        weeks,
        blocks,
        week_cal,
        five_cal,
        latest: samples.last().cloned(),
        daily,
        total_samples: samples.len(),
        fx: None,
    }
}

pub const TOP_N: usize = 5;

/// 上位5件に色の枠を割り当てる。一度割り当てた枠は上位にいる限り変えない。
/// アプリとスマホで同じ色になるよう、割り当てはファイルに保存する。
fn assign_slots(projects: &mut [ProjectInfo]) {
    let path = crate::dash_dir().join("project_slots.json");
    let prev: std::collections::HashMap<String, u8> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    let n = projects.len().min(TOP_N);
    let mut used: Vec<u8> = vec![];
    for p in projects[..n].iter_mut() {
        if let Some(&s) = prev.get(&p.key) {
            p.slot = Some(s);
            used.push(s);
        }
    }
    for p in projects[..n].iter_mut() {
        if p.slot.is_none() {
            let s = (1..=TOP_N as u8).find(|s| !used.contains(s)).unwrap_or(1);
            p.slot = Some(s);
            used.push(s);
        }
    }
    let next: std::collections::HashMap<&str, u8> =
        projects[..n].iter().filter_map(|p| p.slot.map(|s| (p.key.as_str(), s))).collect();
    if let Ok(t) = serde_json::to_string(&next) {
        let _ = std::fs::write(&path, t);
    }
}
