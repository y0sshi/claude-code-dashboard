#!/usr/bin/env node
// 配布物に含まれるサードパーティのライセンス表示 (THIRD_PARTY_NOTICES.md) を生成する。
// - Rust: アプリ本体にリンクされる crate（通常依存のみ。ビルド用依存は除外）
// - npm: package.json の dependencies とその依存（画面側にバンドルされるもの）
// 同じライセンス文はまとめて1回だけ載せる。
import { execFileSync } from "node:child_process";
import crypto from "node:crypto";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const LICENSE_FILE = /^(licen[cs]e|copying|notice|unlicense)([-._].*)?$/i;

function licenseTexts(dir) {
  let names = [];
  try {
    names = fs.readdirSync(dir).filter((n) => LICENSE_FILE.test(n)).sort();
  } catch {
    return [];
  }
  return names
    .map((n) => {
      try {
        return { file: n, text: fs.readFileSync(path.join(dir, n), "utf8").replace(/\r\n/g, "\n").trim() };
      } catch {
        return null;
      }
    })
    .filter((x) => x && x.text);
}

/** ライセンス本文ファイルが無いパッケージ用: README や本体ソース先頭の著作権表示を拾う */
function copyrightLines(dir, extra = []) {
  const files = [];
  try {
    files.push(...fs.readdirSync(dir).filter((n) => /^readme/i.test(n)));
  } catch { /* noop */ }
  files.push(...extra);
  const found = new Set();
  for (const f of files) {
    try {
      const head = fs.readFileSync(path.join(dir, f), "utf8").split(/\r?\n/).slice(0, 60);
      for (const l of head) if (/copyright\s*(\(c\)|©|\d{4})/i.test(l)) found.add(l.replace(/^[\s/*#!-]+/, "").trim());
    } catch { /* noop */ }
  }
  return [...found];
}

const MIT_TEXT = `Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.`;

// ---------- Rust ----------
function rustPackages() {
  const host = execFileSync("rustc", ["-vV"], { encoding: "utf8" }).match(/host: (\S+)/)[1];
  const meta = JSON.parse(
    execFileSync("cargo", ["metadata", "--format-version", "1", "--filter-platform", host, "--manifest-path", path.join(root, "src-tauri", "Cargo.toml")], {
      encoding: "utf8",
      maxBuffer: 256 * 1024 * 1024,
    }),
  );
  const pkgs = new Map(meta.packages.map((p) => [p.id, p]));
  const nodes = new Map(meta.resolve.nodes.map((n) => [n.id, n]));
  const rootId = meta.resolve.root;
  const seen = new Set();
  const stack = [rootId];
  while (stack.length) {
    const id = stack.pop();
    if (seen.has(id)) continue;
    seen.add(id);
    for (const d of nodes.get(id)?.deps ?? []) {
      // 通常依存（kind: null）だけをたどる。build / dev 依存は配布物に含まれない
      if (d.dep_kinds.some((k) => k.kind === null)) stack.push(d.pkg);
    }
  }
  seen.delete(rootId);
  return [...seen].map((id) => {
    const p = pkgs.get(id);
    return {
      name: p.name,
      version: p.version,
      license: p.license ?? (p.license_file ? `see ${p.license_file}` : "unknown"),
      url: p.repository ?? "",
      texts: licenseTexts(path.dirname(p.manifest_path)),
      copyright: copyrightLines(path.dirname(p.manifest_path), ["src/lib.rs"]),
    };
  });
}

// ---------- npm ----------
function npmPackages() {
  const rootPkg = JSON.parse(fs.readFileSync(path.join(root, "package.json"), "utf8"));
  const out = new Map();
  const stack = Object.keys(rootPkg.dependencies ?? {});
  while (stack.length) {
    const name = stack.pop();
    if (out.has(name)) continue;
    const dir = path.join(root, "node_modules", name);
    let pj;
    try {
      pj = JSON.parse(fs.readFileSync(path.join(dir, "package.json"), "utf8"));
    } catch {
      continue;
    }
    const repo = typeof pj.repository === "string" ? pj.repository : pj.repository?.url ?? "";
    out.set(name, {
      name,
      version: pj.version,
      license: typeof pj.license === "string" ? pj.license : JSON.stringify(pj.license ?? pj.licenses ?? "unknown"),
      url: repo.replace(/^git\+/, ""),
      texts: licenseTexts(dir),
      copyright: copyrightLines(dir, [pj.main ?? "index.js"]),
    });
    stack.push(...Object.keys(pj.dependencies ?? {}));
  }
  return [...out.values()];
}

// ---------- 出力 ----------
function render(title, list) {
  list.sort((a, b) => a.name.localeCompare(b.name) || a.version.localeCompare(b.version));
  const lines = [`## ${title}（${list.length} 件）`, "", "| パッケージ | バージョン | ライセンス |", "|---|---|---|"];
  for (const p of list) lines.push(`| ${p.url ? `[${p.name}](${p.url})` : p.name} | ${p.version} | ${p.license} |`);
  lines.push("");
  // 同じ本文はまとめる
  const groups = new Map();
  for (const p of list) {
    for (const t of p.texts) {
      const h = crypto.createHash("sha256").update(t.text).digest("hex");
      if (!groups.has(h)) groups.set(h, { text: t.text, users: [] });
      groups.get(h).users.push(`${p.name} ${p.version}`);
    }
  }
  const missing = list.filter((p) => !p.texts.length);
  lines.push(`### ${title}: ライセンス本文`, "");
  for (const g of groups.values()) {
    lines.push(`<details><summary>${g.users.length} 件: ${g.users.slice(0, 6).join(", ")}${g.users.length > 6 ? " ほか" : ""}</summary>`, "");
    lines.push(`対象: ${g.users.join(", ")}`, "", "```text", g.text, "```", "", "</details>", "");
  }
  if (missing.length) {
    lines.push(`### ${title}: ライセンス本文ファイルが同梱されていないもの`, "");
    for (const p of missing) {
      const spdx = p.license.split(/\s+(?:OR|AND|WITH)\s+|[()\/]/).map((x) => x.trim()).filter(Boolean);
      lines.push(`**${p.name} ${p.version}** — ${p.license}`, "");
      if (p.copyright.length) lines.push(...p.copyright.map((c) => `- ${c}`), "");
      if (spdx.includes("MIT")) lines.push("```text", MIT_TEXT, "```", "");
      else lines.push(`ライセンス本文: ${spdx.map((id) => `https://spdx.org/licenses/${id}.html`).join(" / ")}`, "");
    }
  }
  return lines.join("\n");
}

const rust = rustPackages();
const npm = npmPackages();
const md = [
  "# Third-Party Notices",
  "",
  "Claude Code Dashboard（MIT License）は、以下のサードパーティ製ソフトウェアを含みます。",
  "各ソフトウェアはそれぞれのライセンスに従います。",
  "",
  "このファイルは `npm run notices`（`scripts/gen-notices.mjs`）で自動生成されます。手で編集しないでください。",
  "",
  render("Rust crates", rust),
  "",
  render("npm packages", npm),
  "",
].join("\n");
fs.writeFileSync(path.join(root, "THIRD_PARTY_NOTICES.md"), md);
console.log(`THIRD_PARTY_NOTICES.md: Rust ${rust.length} 件, npm ${npm.length} 件`);
