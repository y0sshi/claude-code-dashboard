//! ~/.claude/projects/**/*.jsonl を差分読み込みしてセッション情報・使用量を集める。
//! 1ファイルが数百MBになることもあるため、オフセットを覚えて追記分だけを読む。

use serde::Serialize;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

pub const SUMMARIZER_MARK: &str = "ccdash-summarizer";

#[derive(Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct TimelineEntry {
    pub ts: i64,
    pub role: String,
    pub text: String,
    #[serde(skip)]
    pub msg_id: String,
}

#[derive(Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Tokens {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

#[derive(Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SessionData {
    pub session_id: String,
    pub project_dir: String,
    pub cwd: String,
    pub git_branch: String,
    pub custom_title: Option<String>,
    pub ai_title: Option<String>,
    pub first_ts: i64,
    pub last_ts: i64,
    pub last_user_ts: i64,
    pub last_assistant_ts: i64,
    /// "user" | "assistant" | "tool"
    pub last_role: String,
    pub last_stop_reason: Option<String>,
    pub pending_question: bool,
    pub model: Option<String>,
    pub entrypoint: Option<String>,
    pub files_touched: Vec<String>,
    pub skills: Vec<String>,
    pub user_prompt_count: u32,
    pub first_prompt: String,
    pub last_prompt: String,
    pub last_assistant_text: String,
    pub cost: f64,
    pub tokens: Tokens,
    /// 直近の応答時点でコンテキストに載っていたトークン数（入力 + キャッシュ読み書き）
    pub context_tokens: u64,
    pub file_path: String,
    pub file_size: u64,
    #[serde(skip)]
    pub timeline: Vec<TimelineEntry>,
    #[serde(skip)]
    pub last_user_uuid: String,
    #[serde(skip)]
    pub user_prompts: Vec<String>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageRec {
    pub ts: i64,
    pub cost: f64,
    #[serde(skip)]
    pub project: String,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RateLimitHit {
    pub ts: i64,
    pub kind: String,
    /// epoch 秒
    pub resets_at: i64,
}

struct FileState {
    offset: u64,
    carry: Vec<u8>,
}

#[derive(Default)]
pub struct Index {
    pub sessions: HashMap<String, SessionData>,
    pub usage: Vec<UsageRec>,
    pub hits: Vec<RateLimitHit>,
    seen_msg: HashSet<String>,
    pub version: u64,
}

pub struct Scanner {
    files: HashMap<PathBuf, FileState>,
}

pub struct PendingFile {
    pub path: PathBuf,
    pub size: u64,
    pub is_sub: bool,
    pub session_id: String,
    pub project_dir: String,
}

const CHUNK: usize = 8 * 1024 * 1024;

impl Scanner {
    pub fn new() -> Self {
        Self { files: HashMap::new() }
    }

    /// 追記があるファイルの一覧（未読バイト数つき）
    pub fn pending(&self, root: &Path) -> Vec<(PendingFile, u64)> {
        let mut out = vec![];
        let Ok(projects) = std::fs::read_dir(root) else { return out };
        for p in projects.flatten() {
            let pdir = p.path();
            if !pdir.is_dir() {
                continue;
            }
            let project_dir = p.file_name().to_string_lossy().to_string();
            if project_dir.contains(SUMMARIZER_MARK) {
                continue;
            }
            let Ok(entries) = std::fs::read_dir(&pdir) else { continue };
            for e in entries.flatten() {
                let path = e.path();
                if path.is_file() && path.extension().map(|x| x == "jsonl").unwrap_or(false) {
                    let sid = path.file_stem().unwrap().to_string_lossy().to_string();
                    self.push_if_grown(&mut out, path, false, sid, &project_dir);
                } else if path.is_dir() {
                    let sid = e.file_name().to_string_lossy().to_string();
                    let sub = path.join("subagents");
                    if let Ok(subs) = std::fs::read_dir(&sub) {
                        for s in subs.flatten() {
                            let sp = s.path();
                            if sp.extension().map(|x| x == "jsonl").unwrap_or(false) {
                                self.push_if_grown(&mut out, sp, true, sid.clone(), &project_dir);
                            }
                        }
                    }
                }
            }
        }
        out
    }

    fn push_if_grown(
        &self,
        out: &mut Vec<(PendingFile, u64)>,
        path: PathBuf,
        is_sub: bool,
        session_id: String,
        project_dir: &str,
    ) {
        let Ok(meta) = std::fs::metadata(&path) else { return };
        let size = meta.len();
        let off = self.files.get(&path).map(|f| f.offset).unwrap_or(0);
        if size > off {
            out.push((
                PendingFile { path, size, is_sub, session_id, project_dir: project_dir.to_string() },
                size - off,
            ));
        }
    }

    /// 1チャンク読み、完結した行を返す。読み終えたら None。
    pub fn read_chunk(&mut self, f: &PendingFile) -> Option<Vec<Vec<u8>>> {
        let st = self.files.entry(f.path.clone()).or_insert(FileState { offset: 0, carry: vec![] });
        if st.offset >= f.size {
            return None;
        }
        let mut file = File::open(&f.path).ok()?;
        file.seek(SeekFrom::Start(st.offset)).ok()?;
        let want = ((f.size - st.offset) as usize).min(CHUNK);
        let mut buf = vec![0u8; want];
        let n = file.read(&mut buf).ok()?;
        if n == 0 {
            return None;
        }
        buf.truncate(n);
        st.offset += n as u64;
        let mut data = std::mem::take(&mut st.carry);
        data.extend_from_slice(&buf);
        let mut lines = vec![];
        let mut start = 0;
        for i in 0..data.len() {
            if data[i] == b'\n' {
                if i > start {
                    lines.push(data[start..i].to_vec());
                }
                start = i + 1;
            }
        }
        st.carry = data[start..].to_vec();
        Some(lines)
    }
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

fn parse_ts(v: &Value) -> i64 {
    v.get("timestamp")
        .and_then(|t| t.as_str())
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.timestamp_millis())
        .unwrap_or(0)
}

/// USD / 100万トークン（入力, 出力）。API 換算の目安。
fn price(model: &str) -> (f64, f64) {
    let m = model.to_ascii_lowercase();
    if m.contains("opus") || m.contains("fable") {
        (5.0, 25.0)
    } else if m.contains("haiku") {
        (1.0, 5.0)
    } else {
        (3.0, 15.0)
    }
}

fn u(v: &Value, k: &str) -> u64 {
    v.get(k).and_then(|x| x.as_u64()).unwrap_or(0)
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(n).collect();
        t.push('…');
        t
    }
}

/// <system-reminder> などの付加ブロックを除去
fn clean_user_text(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    loop {
        match rest.find("<system-reminder>") {
            Some(i) => {
                out.push_str(&rest[..i]);
                match rest[i..].find("</system-reminder>") {
                    Some(j) => rest = &rest[i + j + "</system-reminder>".len()..],
                    None => {
                        rest = "";
                    }
                }
            }
            None => {
                out.push_str(rest);
                break;
            }
        }
    }
    let t = out.trim();
    // スラッシュコマンド: <command-name>/foo</command-name> → /foo
    if let Some(i) = t.find("<command-name>") {
        if let Some(j) = t[i..].find("</command-name>") {
            let name = &t[i + 14..i + j];
            let args = t
                .find("<command-args>")
                .and_then(|a| t[a..].find("</command-args>").map(|b| t[a + 14..a + b].to_string()))
                .unwrap_or_default();
            return format!("{} {}", name, args).trim().to_string();
        }
    }
    if t.starts_with("<local-command-stdout>") || t.starts_with("<local-command-caveat>") {
        return String::new();
    }
    t.to_string()
}

impl Index {
    pub fn apply_lines(&mut self, f: &PendingFile, lines: Vec<Vec<u8>>) {
        for line in lines {
            self.apply_line(f, &line);
        }
        self.version += 1;
    }

    fn apply_line(&mut self, f: &PendingFile, line: &[u8]) {
        let head = &line[..line.len().min(400)];
        if line.starts_with(b"{\"type\":\"") {
            // メタ行: タイトル系のみ必要
            if f.is_sub || !(contains(head, b"\"custom-title\"") || contains(head, b"\"ai-title\"")) {
                return;
            }
        } else if f.is_sub && !contains(head, b"\"type\":\"assistant\"") {
            return;
        }
        let Ok(v) = serde_json::from_slice::<Value>(line) else { return };
        let t = v.get("type").and_then(|x| x.as_str()).unwrap_or("");

        if f.is_sub {
            if t == "assistant" {
                let sid = v.get("sessionId").and_then(|x| x.as_str()).unwrap_or(&f.session_id).to_string();
                self.account_usage(&v, &sid, &f.project_dir);
                self.check_hit(&v);
            }
            return;
        }

        let sid = f.session_id.clone();
        let s = self.sessions.entry(sid.clone()).or_insert_with(|| SessionData {
            session_id: sid.clone(),
            project_dir: f.project_dir.clone(),
            file_path: f.path.to_string_lossy().to_string(),
            last_role: String::new(),
            ..Default::default()
        });
        s.file_size = f.size;

        match t {
            "custom-title" => {
                if let Some(x) = v.get("customTitle").and_then(|x| x.as_str()) {
                    s.custom_title = Some(x.to_string());
                }
                return;
            }
            "ai-title" => {
                if let Some(x) = v.get("aiTitle").and_then(|x| x.as_str()) {
                    s.ai_title = Some(x.to_string());
                }
                return;
            }
            "user" | "assistant" => {}
            _ => return,
        }

        let ts = parse_ts(&v);
        if ts > 0 {
            if s.first_ts == 0 || ts < s.first_ts {
                s.first_ts = ts;
            }
            if ts > s.last_ts {
                s.last_ts = ts;
            }
        }
        // cwd は作業中の cd で変わるので、セッション開始時の場所を使う
        if let (true, Some(c)) = (s.cwd.is_empty(), v.get("cwd").and_then(|x| x.as_str())) {
            s.cwd = c.to_string();
        }
        if let Some(b) = v.get("gitBranch").and_then(|x| x.as_str()) {
            s.git_branch = b.to_string();
        }
        if let Some(e) = v.get("entrypoint").and_then(|x| x.as_str()) {
            s.entrypoint = Some(e.to_string());
        }

        if t == "user" {
            if v.get("isMeta").and_then(|x| x.as_bool()).unwrap_or(false) {
                return;
            }
            let content = &v["message"]["content"];
            let mut texts = vec![];
            let mut has_tool_result = false;
            if let Some(st) = content.as_str() {
                texts.push(st.to_string());
            } else if let Some(arr) = content.as_array() {
                for b in arr {
                    match b.get("type").and_then(|x| x.as_str()) {
                        Some("tool_result") => has_tool_result = true,
                        Some("text") => {
                            if let Some(x) = b.get("text").and_then(|x| x.as_str()) {
                                texts.push(x.to_string());
                            }
                        }
                        _ => {}
                    }
                }
            }
            if has_tool_result {
                s.last_role = "tool".into();
                s.pending_question = false;
                return;
            }
            if v.get("isCompactSummary").and_then(|x| x.as_bool()).unwrap_or(false) {
                // 自動コンパクトの要約: 依頼ではないので区切りとしてだけ残す
                s.timeline.push(TimelineEntry { ts, role: "compact".into(), text: "（会話がコンパクトされました）".into(), msg_id: String::new() });
                return;
            }
            let origin = v["origin"]["kind"].as_str();
            if origin.is_some() && origin != Some("human") {
                // タスク通知などの自動入力: Claude の番になる
                s.last_role = "user".into();
                return;
            }
            let text = clean_user_text(&texts.join("\n"));
            if text.is_empty() || text.starts_with("[Image: source:") {
                return;
            }
            s.pending_question = false;
            s.last_role = "user".into();
            s.last_user_ts = ts;
            s.user_prompt_count += 1;
            s.last_user_uuid = v.get("uuid").and_then(|x| x.as_str()).unwrap_or("").to_string();
            let short = truncate(&text, 300);
            if s.first_prompt.is_empty() {
                s.first_prompt = short.clone();
            }
            s.last_prompt = short.clone();
            s.user_prompts.push(short);
            if s.user_prompts.len() > 40 {
                // 先頭(最初の依頼)は残す
                s.user_prompts.remove(1);
            }
            s.timeline.push(TimelineEntry { ts, role: "user".into(), text: truncate(&text, 4000), msg_id: String::new() });
            return;
        }

        // assistant
        let msg = &v["message"];
        let model = msg.get("model").and_then(|x| x.as_str()).unwrap_or("");
        if model != "<synthetic>" && !model.is_empty() {
            s.model = Some(model.to_string());
            let us = &msg["usage"];
            let ctx = u(us, "input_tokens") + u(us, "cache_read_input_tokens") + u(us, "cache_creation_input_tokens");
            if ctx > 0 {
                s.context_tokens = ctx;
            }
        }
        let msg_id = msg.get("id").and_then(|x| x.as_str()).unwrap_or("").to_string();
        if let Some(sr) = msg.get("stop_reason").and_then(|x| x.as_str()) {
            s.last_stop_reason = Some(sr.to_string());
        }
        s.last_role = "assistant".into();
        s.last_assistant_ts = ts;
        if let Some(arr) = msg.get("content").and_then(|x| x.as_array()) {
            for b in arr {
                match b.get("type").and_then(|x| x.as_str()) {
                    Some("text") => {
                        let x = b.get("text").and_then(|x| x.as_str()).unwrap_or("").trim().to_string();
                        if x.is_empty() {
                            continue;
                        }
                        s.last_assistant_text = truncate(&x, 400);
                        match s.timeline.last_mut() {
                            Some(last) if last.role == "assistant" && last.msg_id == msg_id && !msg_id.is_empty() => {
                                last.text = truncate(&format!("{}\n{}", last.text, x), 4000);
                            }
                            _ => s.timeline.push(TimelineEntry {
                                ts,
                                role: "assistant".into(),
                                text: truncate(&x, 4000),
                                msg_id: msg_id.clone(),
                            }),
                        }
                    }
                    Some("tool_use") => {
                        let name = b.get("name").and_then(|x| x.as_str()).unwrap_or("");
                        let input = &b["input"];
                        match name {
                            "Edit" | "Write" | "MultiEdit" | "NotebookEdit" => {
                                if let Some(p) = input
                                    .get("file_path")
                                    .or_else(|| input.get("notebook_path"))
                                    .and_then(|x| x.as_str())
                                {
                                    s.files_touched.retain(|x| x != p);
                                    s.files_touched.push(p.to_string());
                                    if s.files_touched.len() > 50 {
                                        s.files_touched.remove(0);
                                    }
                                }
                            }
                            "Skill" => {
                                if let Some(k) = input.get("skill").and_then(|x| x.as_str()) {
                                    s.skills.retain(|x| x != k);
                                    s.skills.push(k.to_string());
                                }
                            }
                            "AskUserQuestion" => s.pending_question = true,
                            _ => {}
                        }
                    }
                    _ => {}
                }
            }
        }
        self.account_usage(&v, &sid, &f.project_dir);
        self.check_hit(&v);
    }

    fn account_usage(&mut self, v: &Value, sid: &str, project: &str) {
        let msg = &v["message"];
        let model = msg.get("model").and_then(|x| x.as_str()).unwrap_or("");
        if model.is_empty() || model == "<synthetic>" {
            return;
        }
        let id = msg
            .get("id")
            .and_then(|x| x.as_str())
            .or_else(|| v.get("requestId").and_then(|x| x.as_str()))
            .unwrap_or("");
        if id.is_empty() || !self.seen_msg.insert(id.to_string()) {
            return;
        }
        let us = &msg["usage"];
        let input = u(us, "input_tokens");
        let output = u(us, "output_tokens");
        let cread = u(us, "cache_read_input_tokens");
        let cw_total = u(us, "cache_creation_input_tokens");
        let cw5 = u(&us["cache_creation"], "ephemeral_5m_input_tokens");
        let cw1h = u(&us["cache_creation"], "ephemeral_1h_input_tokens");
        let (pi, po) = price(model);
        let cw_cost = if cw5 + cw1h > 0 {
            cw5 as f64 * pi * 1.25 + cw1h as f64 * pi * 2.0
        } else {
            cw_total as f64 * pi * 1.25
        };
        let cost = (input as f64 * pi + cw_cost + cread as f64 * pi * 0.1 + output as f64 * po) / 1e6;
        let ts = parse_ts(v);
        self.usage.push(UsageRec { ts, cost, project: project.to_string() });
        if let Some(s) = self.sessions.get_mut(sid) {
            s.cost += cost;
            s.tokens.input += input;
            s.tokens.output += output;
            s.tokens.cache_read += cread;
            s.tokens.cache_write += cw_total;
        }
    }

    fn check_hit(&mut self, v: &Value) {
        let info = &v["apiErrorParams"]["rate_limit_info"];
        let (Some(kind), Some(reset)) =
            (info.get("rateLimitType").and_then(|x| x.as_str()), info.get("resetsAt").and_then(|x| x.as_i64()))
        else {
            return;
        };
        if info.get("status").and_then(|x| x.as_str()) != Some("rejected") {
            return;
        }
        self.hits.push(RateLimitHit { ts: parse_ts(v), kind: kind.to_string(), resets_at: reset });
    }
}
