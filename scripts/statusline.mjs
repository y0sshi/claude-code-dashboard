#!/usr/bin/env node
// Claude Code の statusLine コマンド。
// 1) stdin の rate_limits (5時間枠・週枠の使用率) を ~/.claude/dashboard/ratelimits.jsonl に追記
// 2) ステータスライン用の1行を出力
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

const claudeDir = process.env.CLAUDE_CONFIG_DIR || path.join(os.homedir(), ".claude");
const dashDir = path.join(claudeDir, "dashboard");
const logFile = path.join(dashDir, "ratelimits.jsonl");
const stateFile = path.join(dashDir, "statusline-last.json");

let raw = "";
process.stdin.setEncoding("utf8");
process.stdin.on("data", (d) => (raw += d));
process.stdin.on("end", () => {
  let input = {};
  try {
    input = JSON.parse(raw);
  } catch {}
  const rl = input.rate_limits || {};
  const five = rl.five_hour;
  const week = rl.seven_day;

  try {
    if (five || week) {
      fs.mkdirSync(dashDir, { recursive: true });
      const key = JSON.stringify([five?.used_percentage, five?.resets_at, week?.used_percentage, week?.resets_at]);
      let last = {};
      try {
        last = JSON.parse(fs.readFileSync(stateFile, "utf8"));
      } catch {}
      const now = Date.now();
      // 値が変わったとき、または5分ごとに記録
      if (last.key !== key || now - (last.ts || 0) > 5 * 60 * 1000) {
        const rec = { ts: now, session_id: input.session_id, five_hour: five ?? null, seven_day: week ?? null };
        fs.appendFileSync(logFile, JSON.stringify(rec) + "\n");
        fs.writeFileSync(stateFile, JSON.stringify({ key, ts: now }));
      }
    }
  } catch {}

  // セッションごとのコンテキスト窓の大きさ（ダッシュボードのコンテキスト残量表示に使う）
  try {
    const size = input.context_window?.context_window_size;
    if (input.session_id && typeof size === "number" && size > 0) {
      const ctxFile = path.join(dashDir, "context.json");
      let map = {};
      try {
        map = JSON.parse(fs.readFileSync(ctxFile, "utf8"));
      } catch {}
      if (map[input.session_id]?.size !== size) {
        map[input.session_id] = { size, ts: Date.now() };
        // 古いものから捨てて 300 件まで
        const keep = Object.entries(map).sort((a, b) => (b[1].ts || 0) - (a[1].ts || 0)).slice(0, 300);
        fs.mkdirSync(dashDir, { recursive: true });
        fs.writeFileSync(ctxFile, JSON.stringify(Object.fromEntries(keep)));
      }
    }
  } catch {}

  const hhmm = (sec) => {
    if (!sec) return "";
    const d = new Date(sec * 1000);
    return `${String(d.getHours()).padStart(2, "0")}:${String(d.getMinutes()).padStart(2, "0")}`;
  };
  const pct = (p) => (typeof p === "number" ? `${Math.round(p)}%` : "-");
  const parts = [];
  if (input.model?.display_name) parts.push(input.model.display_name);
  const dir = input.workspace?.current_dir || input.cwd;
  if (dir) parts.push(path.basename(dir));
  if (five) parts.push(`5h ${pct(five.used_percentage)} (~${hhmm(five.resets_at)})`);
  if (week) parts.push(`週 ${pct(week.used_percentage)}`);
  process.stdout.write(parts.join(" | "));
});
