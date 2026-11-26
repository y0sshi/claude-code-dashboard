//! ドル円レート（円換算の表示用）。
//! frankfurter.dev（欧州中央銀行の公表値。キー不要）から 1 日 1 回ほど取得し、
//! ~/.claude/dashboard/fx.json に保存する。取れないときは前回の値を使う。
//! 送るのは通貨の種類だけ。HTTP クライアントの依存を増やさないよう Windows 標準の curl.exe を使う。

use serde::{Deserialize, Serialize};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

const URL: &str = "https://api.frankfurter.dev/v1/latest?base=USD&symbols=JPY";
/// これより古ければ取り直す（公表は営業日 1 回）
const MAX_AGE_MS: i64 = 6 * 3600 * 1000;

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Fx {
    /// 1 USD あたりの円
    pub rate: f64,
    /// レートの基準日（YYYY-MM-DD）
    pub date: String,
    pub source: String,
    /// 取得した時刻（epoch ms）
    pub fetched_at: i64,
}

fn cell() -> &'static Mutex<Option<Fx>> {
    static C: OnceLock<Mutex<Option<Fx>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(load()))
}

fn path() -> std::path::PathBuf {
    crate::dash_dir().join("fx.json")
}

fn load() -> Option<Fx> {
    serde_json::from_str(&std::fs::read_to_string(path()).ok()?).ok()
}

pub fn current() -> Option<Fx> {
    cell().lock().unwrap().clone()
}

fn fetch() -> Result<Fx, String> {
    let mut cmd = std::process::Command::new("curl.exe");
    cmd.args(["-s", "-L", "--max-time", "15", URL]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    let out = cmd.output().map_err(|e| e.to_string())?;
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).map_err(|e| e.to_string())?;
    let rate = v["rates"]["JPY"].as_f64().filter(|r| *r > 0.0).ok_or("JPY レートがありません")?;
    Ok(Fx {
        rate,
        date: v["date"].as_str().unwrap_or("").to_string(),
        source: "欧州中央銀行（frankfurter.dev）".into(),
        fetched_at: crate::now_ms(),
    })
}

/// 起動時と 30 分ごとに鮮度を確かめ、古ければ取り直す
pub fn run() {
    loop {
        let stale = current().map(|f| crate::now_ms() - f.fetched_at > MAX_AGE_MS).unwrap_or(true);
        if stale {
            match fetch() {
                Ok(f) => {
                    let _ = std::fs::write(path(), serde_json::to_string(&f).unwrap_or_default());
                    *cell().lock().unwrap() = Some(f);
                }
                Err(e) => eprintln!("fx: {e}"),
            }
        }
        std::thread::sleep(Duration::from_secs(30 * 60));
    }
}
