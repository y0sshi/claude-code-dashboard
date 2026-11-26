import { invoke } from "@tauri-apps/api/core";
import { call, isApp, onChange, onProgress } from "./api";
import qrcode from "qrcode-generator";
import { isPermissionGranted, requestPermission, sendNotification } from "@tauri-apps/plugin-notification";
import { openPath } from "@tauri-apps/plugin-opener";
import { Chart, registerables, type ChartDataset, type Plugin } from "chart.js";
import "chartjs-adapter-date-fns";
import { ja } from "date-fns/locale";

Chart.register(...registerables);

// ---------- types (Rust 側と対応) ----------
type State = "working" | "stalled" | "waiting" | "closed";
interface Session {
  sessionId: string;
  projectDir: string;
  cwd: string;
  gitBranch: string;
  title: string;
  firstTs: number;
  lastTs: number;
  userPromptCount: number;
  firstPrompt: string;
  lastPrompt: string;
  model?: string;
  skills: string[];
  cost: number;
  live: boolean;
  state: State;
  stateSince: number;
  waitKind?: WaitKind;
  waitDetail?: string;
  contextTokens: number;
  contextLimit: number;
  contextLimitKnown: boolean;
  summary?: string;
}
type WaitKind = "permission" | "question" | "plan" | "input" | "reply";
interface Overview { sessions: Session[]; progressTotal: number; progressDone: number; ready: boolean }
interface TimelineEntry { ts: number; role: "user" | "assistant" | "compact"; text: string }
interface Detail { timeline: TimelineEntry[]; filesTouched: string[]; skills: string[] }
interface Hit { sessionId: string; title: string; ts: number; role: string; snippet: string }
type Pt = [number, number];
interface Win { start: number; end: number; cost: number; cum: Pt[]; pct: Pt[]; hitAt?: number; byProject: [string, number][] }
interface Cal { limit?: number; source: string; fromHits: number[]; fromSamples: number[] }
interface Sample { ts: number; fivePct?: number; fiveReset?: number; weekPct?: number; weekReset?: number }
interface ProjectInfo { key: string; label: string; cost: number; slot?: number }
interface UsageView {
  now: number; weeks: Win[]; blocks: Win[]; weekCal: Cal; fiveCal: Cal; latest?: Sample; totalSamples: number; projects: ProjectInfo[]; fx?: Fx | null;
}

// ---------- helpers ----------
const $ = <T extends HTMLElement = HTMLElement>(sel: string) => document.querySelector(sel) as T;
const esc = (s: string) =>
  s.replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]!);
const css = (name: string) => getComputedStyle(document.documentElement).getPropertyValue(name).trim();
const pad = (n: number) => String(n).padStart(2, "0");
const hm = (ts: number) => { const d = new Date(ts); return `${pad(d.getHours())}:${pad(d.getMinutes())}`; };
const mdhm = (ts: number) => { const d = new Date(ts); return `${d.getMonth() + 1}/${d.getDate()} ${hm(ts)}`; };
const WD = ["日", "月", "火", "水", "木", "金", "土"];
const mdwhm = (ts: number) => { const d = new Date(ts); return `${d.getMonth() + 1}/${d.getDate()}(${WD[d.getDay()]}) ${hm(ts)}`; };
function ago(ts: number) {
  if (!ts) return "-";
  const s = Math.max(0, (Date.now() - ts) / 1000);
  if (s < 60) return `${Math.floor(s)}秒`;
  if (s < 3600) return `${Math.floor(s / 60)}分`;
  if (s < 86400) return `${Math.floor(s / 3600)}時間${Math.floor((s % 3600) / 60)}分`;
  return `${Math.floor(s / 86400)}日`;
}
const until = (ts: number) => {
  const m = Math.max(0, Math.round((ts - Date.now()) / 60000));
  return m >= 1440 ? `${Math.floor(m / 1440)}日${Math.floor((m % 1440) / 60)}時間` : `${Math.floor(m / 60)}時間${m % 60}分`;
};
const baseName = (p: string) => p.split(/[\\/]/).filter(Boolean).pop() ?? p;
function lsGet(k: string) { try { return localStorage.getItem(k); } catch { return null; } }
function lsSet(k: string, v: string) { try { localStorage.setItem(k, v); } catch { /* noop */ } }
const STATE_LABEL: Record<State, string> = { working: "作業中", stalled: "止まっている", waiting: "返事待ち", closed: "終了" };
const STATE_ICON: Record<State, string> = { working: "▶", stalled: "⚠", waiting: "●", closed: "■" };
const WAIT_LABEL: Record<WaitKind, string> = { permission: "許可待ち", question: "質問待ち", plan: "プラン承認待ち", input: "入力待ち", reply: "返事待ち" };
/** 状態の表示名（返事待ちは理由別に） */
const stateLabel = (s: Session) => (s.state === "waiting" && s.waitKind ? WAIT_LABEL[s.waitKind] : STATE_LABEL[s.state]);
// ---------- 円換算（レートは PC 側で 1 日 1 回取得） ----------
interface Fx { rate: number; date: string; source: string; fetchedAt: number }
let fx: Fx | null = null;
const yen = (usd: number) => (fx ? `${Math.round(usd * fx.rate).toLocaleString("ja-JP")}円` : "");
/** "$4.02（641円）"。レート未取得ならドルだけ */
const money = (usd: number, digits = 2) => `$${usd.toFixed(digits)}${fx ? `（${yen(usd)}）` : ""}`;
const fxNote = () => (fx ? `。1ドル = ${fx.rate.toFixed(2)}円（${fx.date} 時点）` : "");
const fmtTok =(n: number) => (n >= 1e6 ? `${(n / 1e6).toFixed(n % 1e6 ? 1 : 0)}M` : `${Math.round(n / 1e3)}k`);
const ctxPct = (s: Session) => (s.contextTokens > 0 && s.contextLimit > 0 ? (s.contextTokens / s.contextLimit) * 100 : null);

// ---------- 折りたたみセクション（開閉状態を保存） ----------
for (const id of ["sec-sessions", "sec-charts"]) {
  const d = $<HTMLDetailsElement>(`#${id}`);
  const saved = lsGet(`open:${id}`);
  if (saved != null) d.open = saved === "1";
  d.addEventListener("toggle", () => {
    lsSet(`open:${id}`, d.open ? "1" : "0");
    if (id === "sec-charts" && d.open) refreshUsage();
  });
}
const chartsOpen = () => $<HTMLDetailsElement>("#sec-charts").open;

// ---------- セクションの並べ替え（⋮⋮ をドラッグ。マウス・タッチ両対応、並び順は端末ごとに保存） ----------
const secWrap = $("#sections");
const SEC_ORDER_KEY = "sectionOrder";
const sectionEls = () => [...secWrap.querySelectorAll<HTMLElement>(":scope > .section")];
function saveSectionOrder() { lsSet(SEC_ORDER_KEY, JSON.stringify(sectionEls().map((x) => x.dataset.sec))); }
(function restoreSectionOrder() {
  let saved: string[] = [];
  try { saved = JSON.parse(lsGet(SEC_ORDER_KEY) ?? "[]"); } catch { /* noop */ }
  const els = sectionEls();
  const rank = (el: HTMLElement) => { const i = saved.indexOf(el.dataset.sec ?? ""); return i >= 0 ? i : 1000 + els.indexOf(el); };
  els.sort((a, b) => rank(a) - rank(b)).forEach((el) => secWrap.appendChild(el));
})();

secWrap.querySelectorAll<HTMLElement>(".drag-handle").forEach((handle) => {
  const sec = handle.closest<HTMLElement>(".section")!;
  // 見出しの開閉に反応させない
  handle.addEventListener("click", (e) => { e.preventDefault(); e.stopPropagation(); });
  handle.addEventListener("keydown", (e) => {
    if (e.key === "Enter" || e.key === " ") { e.preventDefault(); return; }
    if (e.key !== "ArrowUp" && e.key !== "ArrowDown") return;
    e.preventDefault();
    const sib = (e.key === "ArrowUp" ? sec.previousElementSibling : sec.nextElementSibling) as HTMLElement | null;
    if (!sib) return;
    if (e.key === "ArrowUp") secWrap.insertBefore(sec, sib); else secWrap.insertBefore(sib, sec);
    saveSectionOrder();
    handle.focus();
  });
  handle.addEventListener("pointerdown", (e) => {
    if (e.button !== 0) return;
    e.preventDefault();
    const rect = sec.getBoundingClientRect();
    const offY = e.clientY - rect.top;
    const ph = document.createElement("div");
    ph.className = "drop-placeholder";
    ph.style.height = `${Math.min(rect.height, 160)}px`;
    secWrap.insertBefore(ph, sec);
    Object.assign(sec.style, { position: "fixed", left: `${rect.left}px`, top: `${rect.top}px`, width: `${rect.width}px`, maxHeight: "160px", overflow: "hidden" });
    sec.classList.add("dragging");
    document.body.classList.add("sorting");
    handle.setPointerCapture(e.pointerId);
    const move = (ev: PointerEvent) => {
      sec.style.top = `${ev.clientY - offY}px`;
      const before = sectionEls().filter((x) => x !== sec).find((o) => { const r = o.getBoundingClientRect(); return ev.clientY < r.top + r.height / 2; });
      if (before) { if (ph.nextElementSibling !== before) secWrap.insertBefore(ph, before); } else if (secWrap.lastElementChild !== ph) secWrap.appendChild(ph);
      // 画面の端では自動スクロール
      if (ev.clientY < 50) window.scrollBy(0, -14); else if (ev.clientY > window.innerHeight - 50) window.scrollBy(0, 14);
    };
    const end = () => {
      handle.removeEventListener("pointermove", move);
      handle.removeEventListener("pointerup", end);
      handle.removeEventListener("pointercancel", end);
      secWrap.insertBefore(sec, ph);
      ph.remove();
      sec.removeAttribute("style");
      sec.classList.remove("dragging");
      document.body.classList.remove("sorting");
      saveSectionOrder();
    };
    handle.addEventListener("pointermove", move);
    handle.addEventListener("pointerup", end);
    handle.addEventListener("pointercancel", end);
  });
});

// ---------- プロジェクト色（上位5件 + その他。枠の割り当ては PC 側で保存し、アプリとスマホで共通） ----------
const TOP_N = 5;
let projectSlots: Record<string, number> = {};
let projectLabels: Record<string, string> = {};
function assignProjectSlots(projects: ProjectInfo[]) {
  projectSlots = Object.fromEntries(projects.filter((p) => p.slot).map((p) => [p.key, p.slot!]));
  projectLabels = Object.fromEntries(projects.map((p) => [p.key, p.label]));
}
const projectColor = (key: string) => (projectSlots[key] ? css(`--cat-${projectSlots[key]}`) : css("--cat-other"));
const topProjects = () => Object.entries(projectSlots).sort((a, b) => a[1] - b[1]).map(([k]) => k);

// ---------- sessions ----------
let sessions: Session[] = [];
let filter = lsGet("filter") ?? "live";

document.querySelectorAll<HTMLButtonElement>("#session-filter button").forEach((b) => {
  b.classList.toggle("active", b.dataset.f === filter);
  b.addEventListener("click", () => {
    filter = b.dataset.f!;
    lsSet("filter", filter);
    document.querySelectorAll("#session-filter button").forEach((x) => x.classList.toggle("active", x === b));
    renderCards();
  });
});

const STATE_ORDER: Record<State, number> = { waiting: 0, stalled: 1, working: 2, closed: 3 };

function visibleSessions() {
  const now = Date.now();
  const startOfDay = new Date(); startOfDay.setHours(0, 0, 0, 0);
  return sessions
    .filter((s) => {
      if (filter === "live") return s.live;
      if (filter === "today") return s.live || s.lastTs >= startOfDay.getTime();
      if (filter === "week") return s.live || s.lastTs >= now - 7 * 864e5;
      return true;
    })
    .sort((a, b) => STATE_ORDER[a.state] - STATE_ORDER[b.state] || b.lastTs - a.lastTs);
}

function renderCards() {
  const list = visibleSessions();
  const counts: Record<State, number> = { waiting: 0, stalled: 0, working: 0, closed: 0 };
  sessions.filter((s) => s.live).forEach((s) => counts[s.state]++);
  const waitBy: Partial<Record<WaitKind, number>> = {};
  sessions.filter((s) => s.live && s.state === "waiting" && s.waitKind && s.waitKind !== "reply").forEach((s) => (waitBy[s.waitKind!] = (waitBy[s.waitKind!] ?? 0) + 1));
  const waitNote = Object.entries(waitBy).map(([k, n]) => `${WAIT_LABEL[k as WaitKind].replace("待ち", "")} ${n}`).join("・");
  $("#state-counts").innerHTML = (["waiting", "stalled", "working"] as State[])
    .map((st) => `<span class="c" style="color:var(--st-${st})">${STATE_ICON[st]} ${STATE_LABEL[st]} ${counts[st]}${st === "waiting" && waitNote ? `<small>（${waitNote}）</small>` : ""}</span>`)
    .join("");
  const el = $("#cards");
  if (!list.length) {
    el.innerHTML = `<div class="empty">${filter === "live" ? "起動中のセッションはありません" : "セッションがありません"}</div>`;
    return;
  }
  el.innerHTML = list.map(cardHtml).join("");
}

function ctxRow(s: Session) {
  const pct = ctxPct(s);
  if (pct == null) return "";
  const lv = pct >= 80 ? "hi" : pct >= 60 ? "mid" : "";
  const tip = `直近の応答時点 ${s.contextTokens.toLocaleString()} トークン / コンテキスト窓 ${s.contextLimit.toLocaleString()}${s.contextLimitKnown ? "" : "（モデルからの推定。statusLine が動くと実際の値になります）"}`;
  return `<div class="row"><span class="k">コンテキスト</span><span class="v ctx" title="${tip}"><span class="ctx-meter"><i class="${lv}" style="width:${Math.min(100, pct)}%"></i></span><b class="${lv}">${Math.round(pct)}%</b> <span class="muted">${fmtTok(s.contextTokens)} / ${fmtTok(s.contextLimit)}${s.contextLimitKnown ? "" : "?"}</span></span></div>`;
}

function cardHtml(s: Session) {
  const late = s.state === "waiting" && Date.now() - s.stateSince > 30 * 60 * 1000;
  const content = s.summary || s.lastPrompt || s.firstPrompt || "";
  const skills = s.skills.slice(-2).reverse().join(", ");
  const dot = projectSlots[s.projectDir] ? `<span class="proj-dot" style="background:${projectColor(s.projectDir)}" title="使用量グラフの色"></span>` : "";
  return `
  <article class="card" data-state="${s.state}" data-id="${esc(s.sessionId)}">
    <div class="card-top">
      <div class="card-title" title="${esc(s.title)}">${esc(s.title)}</div>
      <span class="badge ${s.state === "working" ? "pulse" : ""}" ${s.waitKind ? `data-wait="${s.waitKind}"` : ""}>${s.state === "working" ? "" : STATE_ICON[s.state] + " "}${stateLabel(s)}</span>
    </div>
    ${content ? `<div class="summary">${s.summary ? "" : "<span style='color:var(--text-3)'>最新の依頼: </span>"}${esc(content.slice(0, 160))}</div>` : ""}
    <div class="row"><span class="k">場所</span><span class="v" title="${esc(s.cwd)}">${dot}${esc(baseName(s.cwd) || s.projectDir)}${s.gitBranch ? ` <span style="color:var(--text-3)">(${esc(s.gitBranch)})</span>` : ""}</span></div>
    ${s.state === "waiting" && s.waitDetail ? `<div class="row wait"><span class="k">${s.waitKind === "permission" ? "許可" : "内容"}</span><span class="v" title="${esc(s.waitDetail)}">${esc(s.waitDetail)}</span></div>` : ""}
    ${ctxRow(s)}
    ${skills ? `<div class="row"><span class="k">スキル</span><span class="v">${esc(skills)}</span></div>` : ""}
    <div class="foot">
      <span class="since ${late ? "late" : ""}" title="${mdhm(s.stateSince)}">${s.live ? `${stateLabel(s)} ${ago(s.stateSince)}` : `最終 ${mdhm(s.lastTs)}`}</span>
      <span>依頼 ${s.userPromptCount}</span>
      <span title="API換算（推定）${fxNote()}">${money(s.cost)}</span>
      <span class="actions tauri-only">
        <button class="btn" data-act="code" title="VS Codeで開く">VS Code</button>
        <button class="btn" data-act="folder" title="フォルダを開く">📁</button>
        <button class="btn" data-act="resume" title="claude --resume コマンドをコピー">resume</button>
      </span>
    </div>
  </article>`;
}

$("#cards").addEventListener("click", async (e) => {
  const t = e.target as HTMLElement;
  const card = t.closest<HTMLElement>(".card");
  if (!card) return;
  const s = sessions.find((x) => x.sessionId === card.dataset.id);
  if (!s) return;
  const act = t.closest<HTMLElement>("[data-act]")?.dataset.act;
  if (act === "code") return void invoke("open_in_vscode", { path: s.cwd }).catch(alert);
  if (act === "folder") return void openPath(s.cwd).catch(alert);
  if (act === "resume") {
    await navigator.clipboard.writeText(`cd "${s.cwd}"; claude --resume ${s.sessionId}`);
    (t as HTMLButtonElement).textContent = "コピー済";
    setTimeout(() => ((t as HTMLButtonElement).textContent = "resume"), 1200);
    return;
  }
  openDetail(s.sessionId, false);
});

// ---------- drawer (詳細 / 検索結果) ----------
const drawer = $("#drawer");
function showDrawer() { drawer.classList.remove("hidden"); drawer.setAttribute("aria-hidden", "false"); }
function closeDrawer() { drawer.classList.add("hidden"); drawer.setAttribute("aria-hidden", "true"); }
$("#drawer-close").addEventListener("click", closeDrawer);
$("#drawer-back").addEventListener("click", () => runSearch($<HTMLInputElement>("#search-input").value));
document.addEventListener("keydown", (e) => { if (e.key === "Escape") closeDrawer(); });
if (!isApp) document.body.classList.add("web");

async function openDetail(id: string, fromSearch: boolean) {
  const s = sessions.find((x) => x.sessionId === id);
  const d = await call<Detail | null>("get_detail", { sessionId: id });
  if (!s || !d) return;
  $("#drawer-back").classList.toggle("hidden", !fromSearch);
  $("#drawer-title").textContent = s.title;
  $("#drawer-meta").innerHTML = [
    `<span>${stateLabel(s)}${s.waitDetail ? `: ${esc(s.waitDetail)}` : ""}</span>`,
    `<span title="${esc(s.cwd)}">${esc(s.cwd)}</span>`,
    s.gitBranch ? `<span>branch: ${esc(s.gitBranch)}</span>` : "",
    `<span>${mdhm(s.firstTs)} 〜 ${mdhm(s.lastTs)}</span>`,
    s.model ? `<span>${esc(s.model)}</span>` : "",
    `<span>${money(s.cost)}（API換算）</span>`,
    ctxPct(s) != null ? `<span>コンテキスト ${Math.round(ctxPct(s)!)}%（${fmtTok(s.contextTokens)} / ${fmtTok(s.contextLimit)}${s.contextLimitKnown ? "" : " 推定"}）</span>` : "",
    `<span style="user-select:all">${esc(s.sessionId)}</span>`,
  ].join("");
  const body = $("#drawer-body");
  body.innerHTML = `
    ${s.summary ? `<section><h3>要約</h3><div>${esc(s.summary)}</div></section>` : ""}
    ${d.skills.length ? `<section><h3>使ったスキル</h3><div class="chips">${d.skills.slice().reverse().map((x) => `<span class="chip">${esc(x)}</span>`).join("")}</div></section>` : ""}
    ${d.filesTouched.length ? `<section><h3>触ったファイル (${d.filesTouched.length})</h3><div class="chips">${d.filesTouched.slice().reverse().map((x) => `<span class="chip" title="${esc(x)}">${esc(x.replace(s.cwd, "").replace(/^[\\/]/, ""))}</span>`).join("")}</div></section>` : ""}
    <section><h3>依頼と返答 (${d.timeline.filter((m) => m.role !== "compact").length})</h3><div class="timeline">
      ${d.timeline.map((m) => m.role === "compact"
        ? `<div class="compact">${esc(m.text)} · ${mdhm(m.ts)}</div>`
        : `<div class="msg ${m.role}"><span class="when">${m.role === "user" ? "あなた" : "Claude"} · ${mdhm(m.ts)}</span>${esc(m.text)}</div>`).join("")}
    </div></section>`;
  showDrawer();
  body.scrollTop = body.scrollHeight;
}

// ---------- search (ヘッダー → 結果はドロワー) ----------
let searchTimer = 0;
$("#search-input").addEventListener("input", (e) => {
  clearTimeout(searchTimer);
  const q = (e.target as HTMLInputElement).value;
  searchTimer = window.setTimeout(() => runSearch(q), 250);
});
async function runSearch(q: string) {
  if (!q.trim()) { closeDrawer(); return; }
  const hits = await call<Hit[]>("search", { query: q });
  const re = new RegExp(q.replace(/[.*+?^${}()|[\]\\]/g, "\\$&"), "gi");
  const roleLabel: Record<string, string> = { user: "依頼", assistant: "返答", title: "タイトル", summary: "要約" };
  $("#drawer-back").classList.add("hidden");
  $("#drawer-title").textContent = `「${q}」の検索結果`;
  $("#drawer-meta").textContent = `${hits.length}${hits.length >= 300 ? "+" : ""} 件`;
  $("#drawer-body").innerHTML = hits.length
    ? `<div class="search-results">${hits.map((h) => `<div class="hit" data-id="${esc(h.sessionId)}">
        <div class="h"><b>${esc(h.title)}</b><span>${roleLabel[h.role] ?? h.role}</span><span>${mdhm(h.ts)}</span></div>
        <div class="s">${esc(h.snippet).replace(re, (m) => `<mark>${m}</mark>`)}</div></div>`).join("")}</div>`
    : `<div class="empty">見つかりませんでした</div>`;
  $("#drawer-body").scrollTop = 0;
  showDrawer();
}
$("#drawer-body").addEventListener("click", (e) => {
  const h = (e.target as HTMLElement).closest<HTMLElement>(".hit");
  if (h) openDetail(h.dataset.id!, true);
});

// ---------- notifications ----------
const notifyToggle = $<HTMLInputElement>("#notify-toggle");
notifyToggle.checked = lsGet("notify") !== "0";
notifyToggle.addEventListener("change", async () => {
  lsSet("notify", notifyToggle.checked ? "1" : "0");
  if (notifyToggle.checked && !(await isPermissionGranted())) await requestPermission();
});
let prevStates = new Map<string, string>();
let audioCtx: AudioContext | null = null;
function beep() {
  try {
    audioCtx ??= new AudioContext();
    const t = audioCtx.currentTime;
    [880, 1320].forEach((f, i) => {
      const o = audioCtx!.createOscillator(), g = audioCtx!.createGain();
      o.frequency.value = f; o.type = "sine";
      g.gain.setValueAtTime(0.0001, t + i * 0.16);
      g.gain.exponentialRampToValueAtTime(0.15, t + i * 0.16 + 0.02);
      g.gain.exponentialRampToValueAtTime(0.0001, t + i * 0.16 + 0.15);
      o.connect(g).connect(audioCtx!.destination);
      o.start(t + i * 0.16); o.stop(t + i * 0.16 + 0.16);
    });
  } catch { /* noop */ }
}
async function desktopNotify(title: string, body: string) {
  if (!isApp) return false;
  try {
    if (!(await isPermissionGranted()) && (await requestPermission()) !== "granted") return false;
    sendNotification({ title, body });
    return true;
  } catch (e) {
    console.error(e);
    return false;
  }
}
$("#notify-test").addEventListener("click", async () => {
  beep();
  const ok = await desktopNotify("Claude Code Dashboard", "通知のテストです");
  if (!ok) alert("デスクトップ通知を送れませんでした（Windows の通知設定を確認してください）");
});
// WebView は一度操作されるまで音を出せないので、最初のクリックで AudioContext を用意しておく
document.addEventListener("pointerdown", () => { try { audioCtx ??= new AudioContext(); void audioCtx.resume(); } catch { /* noop */ } }, { once: true });

async function detectTransitions(next: Session[], initial: boolean) {
  const changed: Session[] = [];
  for (const s of next) {
    const prev = prevStates.get(s.sessionId);
    const key = s.state === "waiting" ? `waiting:${s.waitKind ?? ""}` : s.state;
    // 返事待ちになったとき、または待ちの理由が変わったとき（質問への回答後に許可待ちになった など）
    if (!initial && s.live && s.state === "waiting" && prev && prev !== key) changed.push(s);
  }
  prevStates = new Map(next.map((s) => [s.sessionId, s.state === "waiting" ? `waiting:${s.waitKind ?? ""}` : s.state]));
  if (!changed.length || !notifyToggle.checked || !isApp) return;
  beep(); // 何件でも音は1回
  await desktopNotify(
    changed.length === 1 ? `${stateLabel(changed[0])}: ${changed[0].title}` : `返事待ち ${changed.length}件`,
    (changed.length === 1
      ? changed[0].waitDetail ?? ""
      : changed.map((s) => `[${stateLabel(s)}] ${s.title}`).join("\n")
    ).slice(0, 200),
  );
}

// ---------- summary toggle ----------
const sumToggle = $<HTMLInputElement>("#summary-toggle");
if (isApp) call<boolean>("get_summary_enabled").then((v) => (sumToggle.checked = v));
sumToggle.addEventListener("change", () => invoke("set_summary_enabled", { enabled: sumToggle.checked }));

// ---------- リモート閲覧（内蔵サーバー + tailscale serve） ----------
interface RemoteStatus { enabled: boolean; running: boolean; localUrl: string; tailscaleUrl?: string; error?: string }
const remoteToggle = $<HTMLInputElement>("#remote-toggle");
const remoteUrl = $<HTMLAnchorElement>("#remote-url");
function showRemote(st: RemoteStatus) {
  remoteToggle.checked = st.enabled;
  const url = st.tailscaleUrl ?? (st.running ? st.localUrl : "");
  remoteUrl.classList.toggle("hidden", !st.running && !st.error);
  remoteUrl.textContent = st.error ? `起動失敗: ${st.error}` : st.tailscaleUrl ? url.replace(/^https?:\/\//, "") : "tailscale serve 未設定";
  remoteUrl.dataset.url = url;
  remoteUrl.title = st.tailscaleUrl ? `${url}（クリックでQRコード）` : `ローカルでは ${st.localUrl} で待受中。スマホから見るには tailscale serve の設定が必要です`;
}
if (isApp) {
  call<RemoteStatus>("get_remote_status").then(showRemote);
  remoteToggle.addEventListener("change", async () => showRemote(await invoke<RemoteStatus>("set_remote_enabled", { enabled: remoteToggle.checked })));
  const pop = $("#remote-pop");
  const closePop = () => pop.classList.add("hidden");
  remoteUrl.addEventListener("click", (e) => {
    e.preventDefault();
    e.stopPropagation();
    if (!pop.classList.contains("hidden")) return closePop();
    const url = remoteUrl.dataset.url ?? "";
    const ready = url.startsWith("https://");
    if (ready) {
      const qr = qrcode(0, "M");
      qr.addData(url);
      qr.make();
      $("#qr").innerHTML = qr.createSvgTag({ cellSize: 5, margin: 3, scalable: true });
    }
    $("#qr").classList.toggle("hidden", !ready);
    $("#pop-copy").classList.toggle("hidden", !ready);
    $("#pop-url").textContent = ready ? url : "";
    $("#pop-note").innerHTML = ready
      ? "スマホのカメラで読み取ってください（Tailscale に接続中の端末だけ開けます）"
      : "スマホから開くには、PC で一度だけ次を実行してください:<br><code>tailscale serve --bg --https=8444 http://127.0.0.1:7391</code>";
    pop.classList.remove("hidden");
  });
  $("#pop-copy").addEventListener("click", async () => {
    await navigator.clipboard.writeText(remoteUrl.dataset.url ?? "");
    $("#pop-copy").textContent = "コピーしました";
    setTimeout(() => ($("#pop-copy").textContent = "URLをコピー"), 1200);
  });
  document.addEventListener("click", (e) => { if (!pop.contains(e.target as Node)) closePop(); });
  document.addEventListener("keydown", (e) => { if (e.key === "Escape") closePop(); });
}

// ---------- overview refresh ----------
let firstLoad = true;
let indexReady = false;
async function refreshOverview() {
  const ov = await call<Overview>("get_overview");
  sessions = ov.sessions;
  showProgress(ov.progressDone, ov.progressTotal, ov.ready);
  if (ov.ready && !indexReady) {
    indexReady = true;
    refreshUsage();
  }
  if (ov.ready) {
    await detectTransitions(sessions, firstLoad);
    firstLoad = false;
  }
  renderCards();
}
function showProgress(done: number, total: number, ready: boolean) {
  const p = $("#progress");
  p.classList.toggle("hidden", ready);
  if (ready) return;
  const pct = total ? (done / total) * 100 : 0;
  (p.querySelector(".bar") as HTMLElement).style.width = `${pct}%`;
  (p.querySelector(".label") as HTMLElement).textContent =
    `会話ログを読み込み中… ${(done / 1e6).toFixed(0)} / ${(total / 1e6).toFixed(0)} MB`;
}

let ovTimer = 0;
const scheduleOverview = () => { clearTimeout(ovTimer); ovTimer = window.setTimeout(refreshOverview, 150); };
let usageTimer = 0;
const scheduleUsage = () => { clearTimeout(usageTimer); usageTimer = window.setTimeout(refreshUsage, 1500); };
onChange("changed", () => { scheduleOverview(); scheduleUsage(); }, 5_000);
onChange("usage-changed", scheduleUsage, 30_000);
onProgress((d, t) => showProgress(d, t, false));
setInterval(() => renderCards(), 30_000); // 経過時間表示の更新
setInterval(refreshOverview, 15_000); // 「止まっている」判定は時間経過で変わるため
setInterval(refreshUsage, 60_000);

// ---------- usage ----------
const charts: Record<string, Chart> = {};

/** 窓内のある時刻での値（階段補間） */
function valueAt(pts: Pt[], t: number) {
  let v = 0;
  for (const [ts, y] of pts) { if (ts > t) break; v = y; }
  return v;
}
/** 現在のペースで枠末まで伸ばした予測線と 100% 到達時刻 */
function project(pts: Pt[], start: number, end: number, now: number, lookback: number) {
  if (!pts.length || now >= end) return null;
  const cur = valueAt(pts, now);
  const from = Math.max(start, now - lookback);
  const past = from === start ? 0 : valueAt(pts, from);
  const rate = (cur - past) / Math.max(1, now - from);
  const endVal = cur + rate * (end - now);
  const eta = rate > 0 && cur < 100 ? now + (100 - cur) / rate : null;
  return { cur, endVal, eta: eta && eta <= end ? eta : null, line: [[now, cur], [end, endVal]] as Pt[] };
}

/** 5時間枠: 直近1時間のペースで 100% に届くまでの時間 */
function fiveEtaText(p: ReturnType<typeof project>, cur: number | null) {
  if (cur != null && cur >= 100) return `<b class="warn">上限に到達しています</b>`;
  if (!p) return "";
  if (p.eta) return `<b class="warn">このペースだと ${hm(p.eta)} に上限（あと約${until(p.eta)}）</b>`;
  return `このペースなら枠内に収まる（枠の終わりに約${Math.round(p.endVal)}%）`;
}

/** 5時間枠・週枠が 80% / 95% を超えたら1回だけ通知（枠ごとに記録） */
const LIMIT_STEPS = [95, 80];
function limitAlert(kind: string, winStart: number, pct: number | null, resetAt: number, estimated: boolean) {
  if (pct == null || !isApp || !notifyToggle.checked) return;
  const step = LIMIT_STEPS.find((x) => pct >= x);
  if (!step) return;
  const key = `limitAlert:${kind}:${winStart}`;
  if (Number(lsGet(key) ?? 0) >= step) return;
  lsSet(key, String(step));
  beep();
  void desktopNotify(
    `${kind}が ${step}% を超えました`,
    `現在 ${Math.round(pct)}%${estimated ? "（推定）" : ""}。リセット ${mdwhm(resetAt)}（あと${until(resetAt)}）`,
  );
}

async function refreshUsage() {
  if (!indexReady) return; // 読み込み途中は週の区切りなどが確定しないので待つ
  const u = await call<UsageView>("get_usage");
  assignProjectSlots(u.projects);
  fx = u.fx ?? null;
  renderCards(); // カードの色ドットを更新
  const now = u.now;
  const wLimit = u.weekCal.limit, fLimit = u.fiveCal.limit;
  const toPct = (pts: Pt[], lim?: number): Pt[] => (lim ? pts.map(([t, c]) => [t, (c / lim) * 100]) : []);

  const week = u.weeks[0];
  const block = u.blocks.find((b) => b.start <= now && now < b.end);
  const l = u.latest;
  const weekActual = week?.pct ?? [];
  const fiveActual = block?.pct ?? [];
  const weekEst = toPct(week?.cum ?? [], wLimit);
  const fiveEst = toPct(block?.cum ?? [], fLimit);
  const weekBase = weekActual.length ? weekActual : weekEst;
  const fiveBase = fiveActual.length ? fiveActual : fiveEst;
  const wProj = week ? project(weekBase, week.start, week.end, now, 24 * 3600e3) : null;
  const fProj = block ? project(fiveBase, block.start, block.end, now, 3600e3) : null;

  // ---- tiles（常に表示） ----
  const tile = (label: string, pct: number | null, sub: string, kind: string) => `
    <div class="tile"><div class="label">${label}</div>
      <div class="value">${pct == null ? "—" : `${Math.round(pct)}%`}<small>${kind}</small></div>
      <div class="meter"><div class="${(pct ?? 0) >= 100 ? "over" : ""}" style="width:${Math.min(100, pct ?? 0)}%"></div></div>
      <div class="sub">${sub}</div></div>`;
  const last = (p: Pt[]) => (p.length ? p[p.length - 1][1] : null);
  const fiveNow = last(fiveActual) ?? last(fiveEst);
  const weekNow = last(weekActual) ?? last(weekEst);
  $("#usage-tiles").innerHTML = [
    tile("5時間枠", fiveNow, block ? `リセット ${hm(block.end)}（あと${until(block.end)}）<br>${fiveEtaText(fProj, fiveNow)}` : "現在の枠なし", fiveActual.length ? "実測" : "推定"),
    tile("週枠", weekNow, week ? `リセット ${mdwhm(week.end)}（あと${until(week.end)}）` : "", weekActual.length ? "実測" : "推定"),
    `<div class="tile"><div class="label">週枠 100% 到達見込み</div>
      <div class="value" style="font-size:20px">${wProj?.eta ? mdwhm(wProj.eta) : wProj ? "枠内では到達しない" : "—"}</div>
      <div class="sub">${wProj ? `直近24hのペースで週末時点 ${Math.round(wProj.endVal)}%` : ""}</div></div>`,
    `<div class="tile"><div class="label">今週の API 換算（ローカル分）</div>
      <div class="value">$${(week?.cost ?? 0).toFixed(1)}${fx ? `<small>${yen(week?.cost ?? 0)}</small>` : ""}</div>
      <div class="sub">推定上限 ${wLimit ? `${money(wLimit, 0)}/週・${fLimit ? money(fLimit, 1) : "?"}/5h` : "未推定"}</div>
      ${fx ? `<div class="sub fx" title="${esc(fx.source)}。${mdhm(fx.fetchedAt)} に取得">1ドル = ${fx.rate.toFixed(2)}円（${fx.date.slice(5).replace("-", "/")} 時点）</div>` : ""}</div>`,
  ].join("");

  if (block) limitAlert("5時間枠", block.start, fiveNow, block.end, !fiveActual.length);
  if (week) limitAlert("週枠", week.start, weekNow, week.end, !weekActual.length);

  if (!chartsOpen()) return; // 閉じている間はグラフを描かない
  $("#cap-five").textContent = block ? `${hm(block.start)}〜${hm(block.end)}` : "";
  $("#cap-week").textContent = week ? `${mdwhm(week.start)}〜${mdwhm(week.end)}` : "";

  const C = { ink: css("--ink"), est: css("--ink-est"), prev: css("--series-prev"), crit: css("--critical"), grid: css("--grid"), text2: css("--text-2"), text3: css("--text-3") };
  Chart.defaults.color = C.text2;
  Chart.defaults.font.family = css("--font");
  Chart.defaults.borderColor = C.grid;

  const limitLine = (x0: number, x1: number): ChartDataset<"line", Pt[]> => ({
    label: "上限 100%", data: [[x0, 100], [x1, 100]], borderColor: C.crit, borderWidth: 1.5, borderDash: [6, 4], pointRadius: 0,
  });
  const nowLine = (x: number, top: number): ChartDataset<"line", Pt[]> => ({
    label: "現在", data: [[x, 0], [x, top]], borderColor: C.text3, borderWidth: 1, borderDash: [2, 3], pointRadius: 0,
  });
  const yMax = (...arrs: Pt[][]) => Math.max(110, ...arrs.flat().map((p) => p[1] + 5));

  // ---- 現在の5時間枠 ----
  if (block) {
    const ds: ChartDataset<"line", Pt[]>[] = [];
    if (fiveActual.length) ds.push({ label: "実測 %", data: [[block.start, 0], ...fiveActual], borderColor: C.ink, backgroundColor: C.ink, borderWidth: 2, stepped: true, pointRadius: 0 });
    if (fiveEst.length) ds.push({ label: "推定 %（ローカルログ）", data: fiveEst, borderColor: C.est, backgroundColor: C.est, borderWidth: 2, stepped: true, pointRadius: 0 });
    if (fProj) ds.push({ label: "予測", data: fProj.line, borderColor: fiveActual.length ? C.ink : C.est, borderWidth: 1.5, borderDash: [4, 4], pointRadius: 0 });
    const top = yMax(fiveActual, fiveEst, fProj?.line ?? []);
    ds.push(limitLine(block.start, block.end), nowLine(now, top));
    drawLine("five", ds, block.start, block.end, top);
  }

  // ---- 今週 ----
  if (week) {
    const prev = u.weeks[1];
    const prevEst: Pt[] = prev ? toPct(prev.cum, wLimit).map(([t, v]) => [t + 7 * 864e5, v]) : [];
    const prevAct: Pt[] = prev ? prev.pct.map(([t, v]) => [t + 7 * 864e5, v]) : [];
    const ds: ChartDataset<"line", Pt[]>[] = [];
    if (weekActual.length) ds.push({ label: "実測 %", data: [[week.start, 0], ...weekActual], borderColor: C.ink, backgroundColor: C.ink, borderWidth: 2, stepped: true, pointRadius: 0 });
    if (weekEst.length) ds.push({ label: "推定 %（ローカルログ）", data: weekEst, borderColor: C.est, backgroundColor: C.est, borderWidth: 2, pointRadius: 0 });
    if (prevAct.length || prevEst.length) ds.push({ label: "先週", data: prevAct.length ? prevAct : prevEst, borderColor: C.prev, backgroundColor: C.prev, borderWidth: 2, borderDash: [2, 3], pointRadius: 0 });
    if (wProj) ds.push({ label: "予測", data: wProj.line, borderColor: weekActual.length ? C.ink : C.est, borderWidth: 1.5, borderDash: [4, 4], pointRadius: 0 });
    const top = yMax(weekActual, weekEst, prevAct, prevEst, wProj?.line ?? []);
    ds.push(limitLine(week.start, week.end), nowLine(now, top));
    drawLine("week", ds, week.start, week.end, top);
  }

  // ---- 棒グラフ（プロジェクト別の積み上げ） ----
  const blocks = u.blocks.filter((b) => b.end > now - 7 * 864e5);
  drawStacked("blocks", blocks, fLimit, (b) => mdhm(b.start), (b) => `${mdhm(b.start)}〜${hm(b.end)}`);
  drawStacked("weeks", u.weeks.slice().reverse(), wLimit, (w) => `${mdhm(w.start).split(" ")[0]}〜`, (w) => `${mdwhm(w.start)}〜${mdwhm(w.end)}`);

  // ---- notes ----
  const fmtCal = (c: Cal, unit: string) =>
    c.limit ? `${money(c.limit, 1)}/${unit}（根拠: ${c.source === "429" ? `過去に上限到達した ${c.fromHits.length} 回の中央値` : `statusLine 実測 ${c.fromSamples.length} 件から逆算`}）` : "未推定（上限到達記録も実測もまだありません）";
  $("#usage-notes").innerHTML = `<ul>
    <li><b>実測 %</b>（黒線）: statusLine が受け取る rate_limits（記録 ${u.totalSamples} 件${l ? `、最終 ${mdhm(l.ts)}` : ""}）。claude.ai など他の利用も含んだ公式の値です。</li>
    <li><b>推定 %</b>（灰線・棒グラフ）: このPCの会話ログ（サブエージェント含む）の API 換算コスト ÷ 推定上限。他の端末・claude.ai チャットの利用は含みません。</li>
    <li>棒グラフの色はプロジェクト別（直近6週の使用量 上位${TOP_N}件、それ以外は「その他」）。セッションカードの「場所」の色と対応します。</li>
    <li>推定上限 — 週: ${fmtCal(u.weekCal, "週")} / 5時間: ${fmtCal(u.fiveCal, "5h")}</li>
    <li>▲ は上限 (429) に達した枠です（週 ${u.weekCal.fromHits.length} 回、5時間 ${u.fiveCal.fromHits.length} 回）。追加クレジットでの利用は上限到達後も続くため、100% を超えることがあります。</li>
    <li>週枠の区切りは記録されたリセット時刻から 7 日刻みで計算しています。</li>
  </ul>`;
}

function drawLine(key: string, datasets: ChartDataset<"line", Pt[]>[], min: number, max: number, yTop: number) {
  charts[key]?.destroy();
  charts[key] = new Chart($<HTMLCanvasElement>(`#chart-${key}`), {
    type: "line",
    // parsing:false では {x,y} 形式が必要
    data: { datasets: datasets.map((d) => ({ ...d, data: d.data.map(([x, y]) => ({ x, y })) })) as ChartDataset<"line">[] },
    options: {
      animation: false, maintainAspectRatio: false, parsing: false as const, normalized: true,
      interaction: { mode: "nearest", axis: "x", intersect: false },
      scales: {
        x: {
          type: "time", min, max, adapters: { date: { locale: ja } },
          time: max - min > 864e5 ? { unit: "day", displayFormats: { day: "M/d(EEE)" } } : { unit: "hour", displayFormats: { hour: "HH:mm" } },
          ticks: { maxRotation: 0 }, grid: { display: false },
        },
        y: { min: 0, max: Math.ceil(yTop / 10) * 10, ticks: { callback: (v) => `${v}%` } },
      },
      plugins: {
        legend: { position: "bottom", labels: { boxWidth: 14, boxHeight: 2, filter: (i) => i.text !== "現在" } },
        tooltip: {
          filter: (i) => i.dataset.label !== "現在" && i.dataset.label !== "上限 100%",
          callbacks: {
            title: (items) => (items[0] ? mdwhm(items[0].parsed.x as number) : ""),
            label: (i) => `${i.dataset.label}: ${(i.parsed.y as number).toFixed(1)}%`,
          },
        },
      },
    },
  });
}

/** プロジェクト別の積み上げ棒 + 上限線 + 上限到達マーカー(▲) */
function drawStacked(key: string, wins: Win[], limit: number | undefined, label: (w: Win) => string, tip: (w: Win) => string) {
  charts[key]?.destroy();
  const isPct = !!limit;
  const conv = (c: number) => (isPct ? (c / limit!) * 100 : c);
  const top = topProjects();
  const series = [...top, "__other__"];
  const valueFor = (w: Win, k: string) =>
    conv(k === "__other__"
      ? w.byProject.filter(([p]) => !top.includes(p)).reduce((a, [, c]) => a + c, 0)
      : w.byProject.find(([p]) => p === k)?.[1] ?? 0);
  const totals = wins.map((w) => conv(w.cost));
  const crit = css("--critical");
  const surface = css("--surface");
  const hitMarker: Plugin<"bar"> = {
    id: "hitMarker",
    afterDatasetsDraw(chart) {
      const { ctx, scales } = chart;
      ctx.save();
      ctx.fillStyle = crit;
      ctx.font = `600 11px ${css("--font")}`;
      ctx.textAlign = "center";
      wins.forEach((w, i) => {
        if (!w.hitAt) return;
        const x = scales.x.getPixelForValue(i);
        const y = scales.y.getPixelForValue(totals[i]);
        ctx.fillText("▲上限", x, y - 6);
      });
      ctx.restore();
    },
  };
  charts[key] = new Chart($<HTMLCanvasElement>(`#chart-${key}`), {
    type: "bar",
    data: {
      labels: wins.map(label),
      datasets: [
        ...series.map((k) => ({
          type: "bar" as const,
          label: k === "__other__" ? "その他" : projectLabels[k] ?? k,
          data: wins.map((w) => valueFor(w, k)),
          backgroundColor: k === "__other__" ? css("--cat-other") : projectColor(k),
          borderColor: surface, borderWidth: { top: 2, bottom: 0, left: 0, right: 0 },
          borderRadius: 3, borderSkipped: "bottom" as const, maxBarThickness: 28, stack: "p",
        })),
        ...(isPct ? [{ type: "line" as const, label: "上限 100%", data: wins.map(() => 100), borderColor: crit, borderWidth: 1.5, borderDash: [6, 4], pointRadius: 0, stack: "limit" }] : []),
      ],
    },
    options: {
      animation: false, maintainAspectRatio: false,
      interaction: { mode: "index", intersect: false },
      scales: {
        x: { stacked: true, grid: { display: false }, ticks: { maxRotation: 0, autoSkip: true } },
        y: { stacked: true, min: 0, suggestedMax: isPct ? 115 : undefined, ticks: { callback: (v) => (isPct ? `${v}%` : `$${v}`) } },
      },
      plugins: {
        legend: { position: "bottom", labels: { boxWidth: 12 } },
        tooltip: {
          filter: (i) => i.dataset.label !== "上限 100%" && (i.parsed.y as number) > 0,
          itemSort: (a, b) => (b.parsed.y as number) - (a.parsed.y as number),
          callbacks: {
            title: (items) => {
              const w = items[0] ? wins[items[0].dataIndex] : null;
              if (!w) return "";
              const act = w.pct.length ? ` · 実測 ${w.pct[w.pct.length - 1][1].toFixed(0)}%` : "";
              return `${tip(w)}\n合計 ${isPct ? `${conv(w.cost).toFixed(1)}%（推定） · ` : ""}${money(w.cost)}${act}${w.hitAt ? `\n上限到達 ${mdwhm(w.hitAt)}` : ""}`;
            },
            label: (i) => {
              const v = i.parsed.y as number;
              // % 表示でも元のドル（と円）を添える
              return ` ${i.dataset.label}: ${isPct ? `${v.toFixed(1)}% · ${money((v / 100) * limit!)}` : money(v)}`;
            },
          },
        },
      },
    },
    plugins: [hitMarker],
  });
}

// ---------- start ----------
refreshOverview();
refreshUsage();
