//! `claude -p --model haiku` で各セッションを1行要約する。
//! Pro プランの使用量を消費するので、新しい依頼が入ったセッションだけ・1分に1件・同じセッションは10分に1回まで。

use crate::{dash_dir, now_ms, AppState};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;
use tauri::Emitter;

#[derive(Serialize, Deserialize, Clone)]
pub struct Entry {
    pub key: String,
    pub text: String,
    pub ts: i64,
}

#[derive(Serialize, Deserialize)]
pub struct Store {
    pub enabled: bool,
    pub map: HashMap<String, Entry>,
}

impl Store {
    fn path() -> std::path::PathBuf {
        dash_dir().join("summaries.json")
    }
    pub fn load() -> Self {
        std::fs::read_to_string(Self::path())
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or(Store { enabled: true, map: HashMap::new() })
    }
    pub fn save(&self) {
        if let Ok(t) = serde_json::to_string(self) {
            let _ = std::fs::write(Self::path(), t);
        }
    }
    pub fn get(&self, sid: &str) -> Option<String> {
        self.map.get(sid).map(|e| e.text.clone())
    }
}

const MIN_INTERVAL: i64 = 10 * 60 * 1000;

pub fn run(app: tauri::AppHandle, state: Arc<AppState>) {
    let workdir = dash_dir().join(crate::indexer::SUMMARIZER_MARK);
    let _ = std::fs::create_dir_all(&workdir);
    loop {
        std::thread::sleep(Duration::from_secs(30));
        if !state.progress.lock().unwrap().ready || !state.summaries.lock().unwrap().enabled {
            continue;
        }
        let now = now_ms();
        let candidate = {
            let idx = state.index.lock().unwrap();
            let store = state.summaries.lock().unwrap();
            idx.sessions
                .values()
                .filter(|s| s.user_prompt_count > 0 && s.last_ts > now - 3 * 24 * 3600 * 1000 && !s.last_user_uuid.is_empty())
                .filter(|s| match store.map.get(&s.session_id) {
                    None => true,
                    Some(e) => e.key != s.last_user_uuid && now - e.ts > MIN_INTERVAL,
                })
                .max_by_key(|s| s.last_ts)
                .map(|s| (s.session_id.clone(), s.last_user_uuid.clone(), s.user_prompts.clone()))
        };
        let Some((sid, key, prompts)) = candidate else { continue };
        // 最初の依頼 + 直近11件
        let mut picked: Vec<String> = vec![];
        if let Some(f) = prompts.first() {
            picked.push(f.clone());
        }
        let tail = prompts.len().saturating_sub(11).max(1);
        picked.extend(prompts[tail.min(prompts.len())..].iter().cloned());
        let body: String = picked.iter().enumerate().map(|(i, p)| format!("[{}] {}\n", i + 1, p)).collect();
        let prompt = format!(
            "以下は Claude Code の1セッションでユーザーが出した依頼の抜粋です（[1]が最初、以降は直近）。\n\
             このセッションが今何の作業をしているかを、日本語40字以内の1文で要約してください。\n\
             前置き・引用符・句点以外の装飾は不要で、要約文だけを出力してください。\n\n{}",
            body
        );
        let text = call_claude(&workdir, &prompt);
        let mut store = state.summaries.lock().unwrap();
        match text {
            Some(t) => {
                store.map.insert(sid, Entry { key, text: t, ts: now_ms() });
            }
            None => {
                // 失敗しても同じセッションを連打しない
                let prev = store.map.get(&sid).map(|e| e.text.clone()).unwrap_or_default();
                store.map.insert(sid, Entry { key: String::new(), text: prev, ts: now_ms() });
            }
        }
        store.save();
        drop(store);
        let _ = app.emit("changed", ());
        std::thread::sleep(Duration::from_secs(30));
    }
}

fn call_claude(workdir: &std::path::Path, prompt: &str) -> Option<String> {
    #[cfg(windows)]
    let mut cmd = {
        use std::os::windows::process::CommandExt;
        let mut c = Command::new("cmd");
        // claude は .cmd ラッパーなので cmd 経由。--tools "" (ツール無し) を崩さないよう raw で渡す
        c.raw_arg("/C claude -p --model haiku --no-session-persistence --tools \"\"");
        c.creation_flags(0x0800_0000);
        c
    };
    #[cfg(not(windows))]
    let mut cmd = {
        let mut c = Command::new("claude");
        c.args(["-p", "--model", "haiku", "--no-session-persistence", "--tools", ""]);
        c
    };
    cmd.current_dir(workdir).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
    let mut child = cmd.spawn().ok()?;
    {
        let mut stdin = child.stdin.take()?;
        stdin.write_all(prompt.as_bytes()).ok()?;
    }
    let started = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if started.elapsed() > Duration::from_secs(120) => {
                let _ = child.kill();
                return None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(500)),
            Err(_) => return None,
        }
    }
    let out = child.wait_with_output().ok()?;
    if !out.status.success() {
        return None;
    }
    let t = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if t.is_empty() {
        None
    } else {
        Some(t.lines().next().unwrap_or("").chars().take(80).collect())
    }
}
