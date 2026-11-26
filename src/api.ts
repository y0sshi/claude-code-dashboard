// Tauri アプリ内では invoke/listen、スマホのブラウザ（内蔵サーバー経由）では fetch/ポーリングで同じデータを取る
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

export const isApp = "__TAURI_INTERNALS__" in window;

const WEB_ROUTES: Record<string, (a: Record<string, unknown>) => string> = {
  get_overview: () => "/api/overview",
  get_usage: () => "/api/usage",
  get_detail: (a) => `/api/detail?id=${encodeURIComponent(String(a.sessionId ?? ""))}`,
  search: (a) => `/api/search?q=${encodeURIComponent(String(a.query ?? ""))}`,
};

export async function call<T>(cmd: string, args: Record<string, unknown> = {}): Promise<T> {
  if (isApp) return invoke<T>(cmd, args);
  const route = WEB_ROUTES[cmd];
  if (!route) throw new Error(`${cmd} はスマホ表示では使えません`);
  const r = await fetch(route(args), { cache: "no-store" });
  if (!r.ok) throw new Error(`${r.status} ${r.statusText}`);
  return (await r.json()) as T;
}

/** アプリではバックエンドのイベント、ブラウザでは一定間隔で呼ぶ */
export function onChange(event: "changed" | "usage-changed", cb: () => void, webIntervalMs: number) {
  if (isApp) void listen(event, cb);
  else setInterval(() => { if (document.visibilityState === "visible") cb(); }, webIntervalMs);
}

export function onProgress(cb: (done: number, total: number) => void) {
  if (isApp) void listen<[number, number]>("progress", (e) => cb(e.payload[0], e.payload[1]));
}
