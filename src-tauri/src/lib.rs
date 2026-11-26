pub mod fx;
pub mod indexer;
mod server;
mod summarizer;
pub mod usage;

use indexer::{Index, Scanner, SessionData, TimelineEntry};
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::{Emitter, Manager};

pub fn claude_dir() -> PathBuf {
    if let Ok(d) = std::env::var("CLAUDE_CONFIG_DIR") {
        return PathBuf::from(d);
    }
    dirs::home_dir().unwrap_or_default().join(".claude")
}

pub fn dash_dir() -> PathBuf {
    let d = claude_dir().join("dashboard");
    let _ = std::fs::create_dir_all(&d);
    d
}

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

#[derive(Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct LiveSession {
    pub pid: u32,
    pub session_id: String,
    pub cwd: String,
    pub name: Option<String>,
    pub status: Option<String>,
    pub status_updated_at: i64,
    pub started_at: i64,
    pub entrypoint: Option<String>,
    pub kind: Option<String>,
    /// status が "waiting" のときの理由（"approve Bash: …" / "answer question" / "approve plan" など）
    pub waiting_for: Option<String>,
}

#[derive(Default)]
pub struct Progress {
    pub total: u64,
    pub done: u64,
    pub ready: bool,
}

pub struct AppState {
    pub index: Mutex<Index>,
    pub live: Mutex<Vec<LiveSession>>,
    pub samples: Mutex<Vec<usage::RlSample>>,
    pub progress: Mutex<Progress>,
    pub summaries: Mutex<summarizer::Store>,
    pub remote: Mutex<Option<Arc<tiny_http::Server>>>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionView {
    #[serde(flatten)]
    pub data: SessionData,
    pub title: String,
    pub live: bool,
    pub pid: Option<u32>,
    pub live_status: Option<String>,
    /// working | stalled | waiting | closed
    pub state: String,
    pub state_since: i64,
    /// waiting のときの種類: permission | question | plan | input | reply
    pub wait_kind: Option<String>,
    /// 許可待ちの対象（"Bash: npm run build" など）
    pub wait_detail: Option<String>,
    /// コンテキスト窓の大きさ（statusLine の記録があればその値、無ければモデルからの推定）
    pub context_limit: u64,
    pub context_limit_known: bool,
    pub summary: Option<String>,
}

fn read_live(sys: &mut sysinfo::System) -> Vec<LiveSession> {
    let dir = claude_dir().join("sessions");
    let mut out = vec![];
    let Ok(rd) = std::fs::read_dir(&dir) else { return out };
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().map(|x| x != "json").unwrap_or(true) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&p) else { continue };
        let Ok(v) = serde_json::from_str::<Value>(&text) else { continue };
        let s = |k: &str| v.get(k).and_then(|x| x.as_str()).map(|x| x.to_string());
        let i = |k: &str| v.get(k).and_then(|x| x.as_i64()).unwrap_or(0);
        let ls = LiveSession {
            pid: v["pid"].as_u64().unwrap_or(0) as u32,
            session_id: s("sessionId").unwrap_or_default(),
            cwd: s("cwd").unwrap_or_default(),
            name: s("name"),
            status: s("status"),
            status_updated_at: i("statusUpdatedAt").max(i("updatedAt")),
            started_at: i("startedAt"),
            entrypoint: s("entrypoint"),
            kind: s("kind"),
            waiting_for: s("waitingFor"),
        };
        if ls.cwd.contains(indexer::SUMMARIZER_MARK) || ls.session_id.is_empty() {
            continue;
        }
        out.push(ls);
    }
    let pids: Vec<sysinfo::Pid> = out.iter().map(|l| sysinfo::Pid::from_u32(l.pid)).collect();
    sys.refresh_processes(sysinfo::ProcessesToUpdate::Some(&pids), true);
    out.retain(|l| sys.process(sysinfo::Pid::from_u32(l.pid)).is_some());
    out
}

fn build_views(state: &AppState) -> Vec<SessionView> {
    let idx = state.index.lock().unwrap();
    let live = state.live.lock().unwrap().clone();
    let sums = state.summaries.lock().unwrap();
    let live_map: HashMap<&str, &LiveSession> = live.iter().map(|l| (l.session_id.as_str(), l)).collect();
    let now = now_ms();
    let ctx = read_context_sizes();
    let mut out = vec![];
    for s in idx.sessions.values() {
        let l = live_map.get(s.session_id.as_str()).copied();
        let mut v = make_view(s, l, now, sums.get(&s.session_id));
        if let Some(n) = ctx.get(&s.session_id) {
            v.context_limit = *n;
            v.context_limit_known = true;
        }
        out.push(v);
    }
    // 会話ログがまだ無いライブセッション
    for l in live.iter() {
        if !idx.sessions.contains_key(&l.session_id) {
            let d = SessionData {
                session_id: l.session_id.clone(),
                cwd: l.cwd.clone(),
                first_ts: l.started_at,
                last_ts: l.started_at,
                ..Default::default()
            };
            out.push(make_view(&d, Some(l), now, None));
        }
    }
    out.sort_by(|a, b| b.data.last_ts.cmp(&a.data.last_ts));
    out
}

fn make_view(s: &SessionData, l: Option<&LiveSession>, now: i64, summary: Option<String>) -> SessionView {
    let title = s
        .custom_title
        .clone()
        .or_else(|| l.and_then(|l| l.name.clone()))
        .or_else(|| s.ai_title.clone())
        .unwrap_or_else(|| {
            if s.first_prompt.is_empty() {
                "(無題)".into()
            } else {
                s.first_prompt.chars().take(40).collect()
            }
        });
    let last_activity = s.last_ts.max(l.map(|l| l.status_updated_at).unwrap_or(0));
    let mut wait_kind: Option<&str> = None;
    let mut wait_detail: Option<String> = None;
    let (state, since) = match l {
        None => ("closed", s.last_ts),
        Some(l) => {
            let status = l.status.as_deref().unwrap_or("");
            let wf = l.waiting_for.as_deref().unwrap_or("");
            if status == "waiting" {
                // Claude Code 本体が画面のダイアログ待ちを記録している
                wait_kind = Some(if wf == "answer question" {
                    "question"
                } else if wf == "approve plan" || wf.starts_with("approve plan ") {
                    "plan"
                } else if let Some(rest) = wf.strip_prefix("approve ") {
                    wait_detail = Some(rest.to_string());
                    "permission"
                } else {
                    wait_detail = (!wf.is_empty()).then(|| wf.to_string());
                    "input"
                });
                ("waiting", l.status_updated_at)
            } else if s.pending_question && s.last_role == "assistant" {
                wait_kind = Some("question");
                ("waiting", s.last_assistant_ts.max(l.status_updated_at))
            } else if status == "busy" {
                if now - last_activity > 3 * 60 * 1000 {
                    ("stalled", last_activity)
                } else {
                    ("working", l.status_updated_at)
                }
            } else {
                wait_kind = Some("reply");
                ("waiting", l.status_updated_at.max(s.last_assistant_ts))
            }
        }
    };
    // モデル名からの推定（statusLine の記録があれば build_views で上書き）
    let model = s.model.as_deref().unwrap_or("").to_ascii_lowercase();
    let context_limit = if model.contains("haiku") { 200_000 } else { 1_000_000 };
    SessionView {
        data: s.clone(),
        title,
        live: l.is_some(),
        pid: l.map(|l| l.pid),
        live_status: l.and_then(|l| l.status.clone()),
        state: state.into(),
        state_since: since,
        wait_kind: wait_kind.map(|x| x.to_string()),
        wait_detail,
        context_limit,
        context_limit_known: false,
        summary,
    }
}

/// scripts/statusline.mjs が記録するセッションごとのコンテキスト窓の大きさ
fn read_context_sizes() -> HashMap<String, u64> {
    let Ok(text) = std::fs::read_to_string(dash_dir().join("context.json")) else { return HashMap::new() };
    let Ok(v) = serde_json::from_str::<Value>(&text) else { return HashMap::new() };
    v.as_object()
        .map(|m| {
            m.iter()
                .filter_map(|(k, x)| x.get("size").and_then(|n| n.as_u64()).filter(|n| *n > 0).map(|n| (k.clone(), n)))
                .collect()
        })
        .unwrap_or_default()
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Overview {
    sessions: Vec<SessionView>,
    progress_total: u64,
    progress_done: u64,
    ready: bool,
}

#[tauri::command]
fn get_overview(state: tauri::State<'_, Arc<AppState>>) -> Overview {
    overview_of(&state)
}

pub(crate) fn overview_of(state: &AppState) -> Overview {
    let sessions = build_views(&state);
    let p = state.progress.lock().unwrap();
    Overview { sessions, progress_total: p.total, progress_done: p.done, ready: p.ready }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Detail {
    timeline: Vec<TimelineEntry>,
    files_touched: Vec<String>,
    skills: Vec<String>,
}

#[tauri::command]
fn get_detail(state: tauri::State<'_, Arc<AppState>>, session_id: String) -> Option<Detail> {
    detail_of(&state, &session_id)
}

pub(crate) fn detail_of(state: &AppState, session_id: &str) -> Option<Detail> {
    let idx = state.index.lock().unwrap();
    idx.sessions.get(session_id).map(|s| Detail {
        timeline: s.timeline.clone(),
        files_touched: s.files_touched.clone(),
        skills: s.skills.clone(),
    })
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Hit {
    session_id: String,
    title: String,
    ts: i64,
    role: String,
    snippet: String,
}

#[tauri::command]
fn search(state: tauri::State<'_, Arc<AppState>>, query: String) -> Vec<Hit> {
    search_of(&state, &query)
}

pub(crate) fn search_of(state: &AppState, query: &str) -> Vec<Hit> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return vec![];
    }
    let idx = state.index.lock().unwrap();
    let sums = state.summaries.lock().unwrap();
    let mut out = vec![];
    for s in idx.sessions.values() {
        let title = s.custom_title.clone().or_else(|| s.ai_title.clone()).unwrap_or_else(|| s.first_prompt.chars().take(40).collect());
        let mut texts: Vec<(i64, &str, String)> = vec![(s.last_ts, "title", title.clone())];
        if let Some(sm) = sums.get(&s.session_id) {
            texts.push((s.last_ts, "summary", sm));
        }
        for e in &s.timeline {
            texts.push((e.ts, e.role.as_str(), e.text.clone()));
        }
        for (ts, role, text) in texts {
            let lower = text.to_lowercase();
            if let Some(pos) = lower.find(&q) {
                // pos はバイト位置: 文字境界に合わせて前後を切り出す
                let chars: Vec<char> = text.chars().collect();
                let cpos = lower[..pos].chars().count();
                let a = cpos.saturating_sub(40);
                let b = (cpos + q.chars().count() + 80).min(chars.len());
                out.push(Hit {
                    session_id: s.session_id.clone(),
                    title: title.clone(),
                    ts,
                    role: role.to_string(),
                    snippet: chars[a..b].iter().collect(),
                });
                if out.len() >= 300 {
                    break;
                }
            }
        }
    }
    out.sort_by(|a, b| b.ts.cmp(&a.ts));
    out
}

#[tauri::command]
fn get_usage(state: tauri::State<'_, Arc<AppState>>) -> usage::UsageView {
    usage_of(&state)
}

pub(crate) fn usage_of(state: &AppState) -> usage::UsageView {
    let (u, h, labels) = {
        let idx = state.index.lock().unwrap();
        // project_dir → 最新セッションの cwd のフォルダ名
        let mut latest: HashMap<String, (i64, String)> = HashMap::new();
        for s in idx.sessions.values() {
            let name = s.cwd.split(['\\', '/']).filter(|x| !x.is_empty()).last().unwrap_or("").to_string();
            if name.is_empty() {
                continue;
            }
            let e = latest.entry(s.project_dir.clone()).or_insert((0, String::new()));
            if s.last_ts >= e.0 {
                *e = (s.last_ts, name);
            }
        }
        let labels: HashMap<String, String> = latest.into_iter().map(|(k, (_, v))| (k, v)).collect();
        (idx.usage.clone(), idx.hits.clone(), labels)
    };
    let samples = state.samples.lock().unwrap().clone();
    let mut view = usage::compute(&u, &h, &samples, now_ms(), &labels);
    view.fx = fx::current();
    view
}

#[tauri::command]
fn open_in_vscode(path: String) -> Result<(), String> {
    let mut cmd = if cfg!(windows) {
        let mut c = std::process::Command::new("cmd");
        c.args(["/C", "code", &path]);
        c
    } else {
        let mut c = std::process::Command::new("code");
        c.arg(&path);
        c
    };
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    cmd.spawn().map(|_| ()).map_err(|e| e.to_string())
}

#[tauri::command]
fn get_summary_enabled(state: tauri::State<'_, Arc<AppState>>) -> bool {
    state.summaries.lock().unwrap().enabled
}

#[tauri::command]
fn set_summary_enabled(state: tauri::State<'_, Arc<AppState>>, enabled: bool) {
    let mut s = state.summaries.lock().unwrap();
    s.enabled = enabled;
    s.save();
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RemoteStatus {
    enabled: bool,
    running: bool,
    local_url: String,
    tailscale_url: Option<String>,
    error: Option<String>,
}

fn remote_status(state: &AppState, error: Option<String>) -> RemoteStatus {
    let running = state.remote.lock().unwrap().is_some();
    RemoteStatus {
        enabled: server::load_enabled(),
        running,
        local_url: format!("http://127.0.0.1:{}", server::PORT),
        tailscale_url: if running { server::tailscale_url() } else { None },
        error,
    }
}

#[tauri::command]
fn get_remote_status(state: tauri::State<'_, Arc<AppState>>) -> RemoteStatus {
    remote_status(&state, None)
}

#[tauri::command]
fn set_remote_enabled(app: tauri::AppHandle, state: tauri::State<'_, Arc<AppState>>, enabled: bool) -> RemoteStatus {
    server::save_enabled(enabled);
    let mut err = None;
    {
        let mut slot = state.remote.lock().unwrap();
        if let Some(srv) = slot.take() {
            srv.unblock();
        }
        if enabled {
            // 直前に止めたサーバーのポート解放を少し待つ
            for i in 0..10 {
                match server::start(app.clone(), state.inner().clone()) {
                    Ok(srv) => {
                        *slot = Some(srv);
                        err = None;
                        break;
                    }
                    Err(e) => {
                        err = Some(e);
                        std::thread::sleep(std::time::Duration::from_millis(100 * (i + 1)));
                    }
                }
            }
        }
    }
    remote_status(&state, err)
}

fn watcher(app: tauri::AppHandle, state: Arc<AppState>) {
    let root = claude_dir().join("projects");
    let samples_path = dash_dir().join("ratelimits.jsonl");
    let mut scanner = Scanner::new();
    let mut sys = sysinfo::System::new();
    let mut samples_len = 0u64;
    let mut last_live_sig = String::new();
    loop {
        let pending = scanner.pending(&root);
        let mut changed = !pending.is_empty();
        {
            let mut p = state.progress.lock().unwrap();
            if !p.ready {
                p.total = pending.iter().map(|x| x.1).sum();
                p.done = 0;
            }
        }
        for (f, _) in pending {
            while let Some(lines) = scanner.read_chunk(&f) {
                let bytes: u64 = lines.iter().map(|l| l.len() as u64 + 1).sum();
                state.index.lock().unwrap().apply_lines(&f, lines);
                let mut p = state.progress.lock().unwrap();
                if !p.ready {
                    p.done += bytes;
                    let _ = app.emit("progress", (p.done, p.total));
                }
            }
        }
        {
            let mut p = state.progress.lock().unwrap();
            if !p.ready {
                p.ready = true;
                changed = true;
            }
        }
        let live = read_live(&mut sys);
        let sig: String = live.iter().map(|l| format!("{}:{:?}:{};", l.session_id, l.status, l.status_updated_at)).collect();
        if sig != last_live_sig {
            last_live_sig = sig;
            changed = true;
        }
        *state.live.lock().unwrap() = live;

        let len = std::fs::metadata(&samples_path).map(|m| m.len()).unwrap_or(0);
        if len != samples_len {
            samples_len = len;
            *state.samples.lock().unwrap() = usage::read_samples(&samples_path);
            let _ = app.emit("usage-changed", ());
        }
        if changed {
            let _ = app.emit("changed", ());
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let state = Arc::new(AppState {
        index: Mutex::new(Index::default()),
        live: Mutex::new(vec![]),
        samples: Mutex::new(vec![]),
        progress: Mutex::new(Progress::default()),
        summaries: Mutex::new(summarizer::Store::load()),
        remote: Mutex::new(None),
    });
    tauri::Builder::default()
        // 二重起動すると通知も二重になるので、既存ウィンドウを前面に出す
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.unminimize();
                let _ = w.show();
                let _ = w.set_focus();
            }
        }))
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_opener::init())
        .manage(state.clone())
        .setup(move |app| {
            let h = app.handle().clone();
            let s = state.clone();
            std::thread::spawn(move || watcher(h, s));
            let h2 = app.handle().clone();
            let s2 = app.state::<Arc<AppState>>().inner().clone();
            std::thread::spawn(move || summarizer::run(h2, s2));
            std::thread::spawn(fx::run);
            if server::load_enabled() {
                let s3 = app.state::<Arc<AppState>>().inner().clone();
                match server::start(app.handle().clone(), s3.clone()) {
                    Ok(srv) => *s3.remote.lock().unwrap() = Some(srv),
                    Err(e) => eprintln!("remote server: {e}"),
                }
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_overview,
            get_detail,
            search,
            get_usage,
            open_in_vscode,
            get_summary_enabled,
            get_remote_status,
            set_remote_enabled,
            set_summary_enabled
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
