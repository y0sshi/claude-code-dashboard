//! 実データで indexer / usage を検証する: cargo run --release --example dump
use ccdash_lib::{claude_dir, dash_dir, indexer::*, now_ms, usage};
fn main() {
    let t0 = std::time::Instant::now();
    let mut sc = Scanner::new();
    let mut idx = Index::default();
    let pend = sc.pending(&claude_dir().join("projects"));
    let total: u64 = pend.iter().map(|x| x.1).sum();
    for (f, _) in &pend {
        while let Some(lines) = sc.read_chunk(f) { idx.apply_lines(f, lines); }
    }
    println!("files={} bytes={}MB time={:?}", pend.len(), total / 1_000_000, t0.elapsed());
    println!("sessions={} usage_recs={} hits={}", idx.sessions.len(), idx.usage.len(), idx.hits.len());
    let mut ss: Vec<_> = idx.sessions.values().collect();
    ss.sort_by_key(|s| -s.last_ts);
    for s in ss.iter().take(8) {
        println!("- {} | {:?}/{:?} | prompts={} tl={} ${:.2} last_role={} q={} | {}", &s.session_id[..8], s.custom_title, s.ai_title, s.user_prompt_count, s.timeline.len(), s.cost, s.last_role, s.pending_question, s.last_prompt.chars().take(40).collect::<String>());
    }
    let samples = usage::read_samples(&dash_dir().join("ratelimits.jsonl"));
    let v = usage::compute(&idx.usage, &idx.hits, &samples, now_ms(), &Default::default());
    for p in v.projects.iter().take(8) { println!("project {} ({}) ${:.1}", p.label, p.key, p.cost); }
    let j = serde_json::to_value(&v).unwrap();
    println!("weekCal={} fiveCal={}", j["weekCal"], j["fiveCal"]);
    for w in &v.weeks { println!("week {} - {} cost={:.1} hit={:?} pct={}", fmt(w.start), fmt(w.end), w.cost, w.hit_at.map(fmt), w.pct.len()); }
    for b in v.blocks.iter().rev().take(6) { println!("block {} - {} cost={:.2} hit={:?}", fmt(b.start), fmt(b.end), b.cost, b.hit_at.map(fmt)); }
    println!("samples={} latest={:?}", samples.len(), j["latest"]);
}
fn fmt(ms: i64) -> String { chrono::DateTime::from_timestamp_millis(ms).unwrap().with_timezone(&chrono::Local).format("%m/%d %a %H:%M").to_string() }
