//! スマホ閲覧用の内蔵 HTTP サーバー（閲覧専用）。
//! 127.0.0.1 だけで待ち受け、外からは `tailscale serve` 経由（tailnet 内・HTTPS）でのみ届く。

use crate::{dash_dir, detail_of, overview_of, search_of, usage_of, AppState};
use serde::Serialize;
use std::io::Cursor;
use std::sync::Arc;
use tiny_http::{Header, Request, Response, Server};

pub const PORT: u16 = 7391;

fn settings_path() -> std::path::PathBuf {
    dash_dir().join("settings.json")
}

pub fn load_enabled() -> bool {
    std::fs::read_to_string(settings_path())
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .and_then(|v| v["remote"].as_bool())
        .unwrap_or(false)
}

pub fn save_enabled(on: bool) {
    let mut v = std::fs::read_to_string(settings_path())
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    v["remote"] = serde_json::Value::Bool(on);
    let _ = std::fs::write(settings_path(), v.to_string());
}

pub fn start(app: tauri::AppHandle, state: Arc<AppState>) -> Result<Arc<Server>, String> {
    let server = Arc::new(Server::http(("127.0.0.1", PORT)).map_err(|e| e.to_string())?);
    let s2 = server.clone();
    std::thread::spawn(move || {
        // unblock() されると incoming_requests が終わる
        for req in s2.incoming_requests() {
            let app = app.clone();
            let state = state.clone();
            std::thread::spawn(move || handle(req, &app, &state));
        }
    });
    Ok(server)
}

fn header(k: &str, v: &str) -> Header {
    Header::from_bytes(k.as_bytes(), v.as_bytes()).unwrap()
}

fn respond(req: Request, status: u16, ctype: &str, body: Vec<u8>, cache: bool) {
    let mut r = Response::new(status.into(), vec![], Cursor::new(body.clone()), Some(body.len()), None)
        .with_header(header("Content-Type", ctype))
        .with_header(header("X-Content-Type-Options", "nosniff"))
        .with_header(header("Referrer-Policy", "no-referrer"))
        .with_header(header("X-Frame-Options", "DENY"));
    if !cache {
        r = r.with_header(header("Cache-Control", "no-store"));
    }
    let _ = req.respond(r);
}

fn json<T: Serialize>(req: Request, v: &T) {
    respond(req, 200, "application/json; charset=utf-8", serde_json::to_vec(v).unwrap_or_default(), false);
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => {
                let hex = |c: u8| (c as char).to_digit(16);
                match (hex(b[i + 1]), hex(b[i + 2])) {
                    (Some(h), Some(l)) => {
                        out.push((h * 16 + l) as u8);
                        i += 2;
                    }
                    _ => out.push(b'%'),
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

fn query_param(q: &str, key: &str) -> String {
    q.split('&')
        .filter_map(|kv| kv.split_once('='))
        .find(|(k, _)| *k == key)
        .map(|(_, v)| percent_decode(v))
        .unwrap_or_default()
}

/// DNS リバインディング対策: ローカルと tailnet のホスト名だけ受け付ける
fn host_ok(req: &Request) -> bool {
    let host = req
        .headers()
        .iter()
        .find(|h| h.field.equiv("Host"))
        .map(|h| h.value.as_str().to_ascii_lowercase())
        .unwrap_or_default();
    let name = host.rsplit_once(':').map(|(h, _)| h.to_string()).unwrap_or(host.clone());
    name == "127.0.0.1" || name == "localhost" || name.ends_with(".ts.net")
}

fn handle(req: Request, app: &tauri::AppHandle, state: &AppState) {
    if !host_ok(&req) {
        return respond(req, 403, "text/plain", b"forbidden".to_vec(), false);
    }
    if req.method() != &tiny_http::Method::Get {
        return respond(req, 405, "text/plain", b"method not allowed".to_vec(), false);
    }
    let url = req.url().to_string();
    let (path, q) = url.split_once('?').unwrap_or((url.as_str(), ""));
    match path {
        "/api/overview" => json(req, &overview_of(state)),
        "/api/usage" => json(req, &usage_of(state)),
        "/api/detail" => json(req, &detail_of(state, &query_param(q, "id"))),
        "/api/search" => json(req, &search_of(state, &query_param(q, "q"))),
        _ => serve_asset(req, app, path),
    }
}

fn serve_asset(req: Request, app: &tauri::AppHandle, path: &str) {
    let p = path.trim_start_matches('/');
    let p = if p.is_empty() { "index.html" } else { p };
    if p.contains("..") {
        return respond(req, 400, "text/plain", b"bad path".to_vec(), false);
    }
    match app.asset_resolver().get(p.to_string()) {
        Some(a) => {
            let html = p.ends_with(".html");
            respond(req, 200, &a.mime_type, a.bytes, !html)
        }
        None => respond(req, 404, "text/plain", b"not found".to_vec(), false),
    }
}

/// `tailscale serve status` から、このサーバーを公開している URL を探す
pub fn tailscale_url() -> Option<String> {
    let mut cmd = std::process::Command::new("tailscale");
    cmd.args(["serve", "status"]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    let out = cmd.output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut current: Option<String> = None;
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with("https://") || t.starts_with("http://") {
            current = t.split_whitespace().next().map(|s| s.to_string());
        } else if t.contains(&format!("127.0.0.1:{}", PORT)) || t.contains(&format!("localhost:{}", PORT)) {
            return current;
        }
    }
    None
}
