# Claude Code Dashboard

並列で動かしている Claude Code セッションと、プランの使用量（5時間枠・週枠）を一覧するローカル専用のデスクトップアプリ（Tauri 2 + TypeScript）。

## 機能

- **セッション一覧**: 状態札（返事待ち / 止まっている / 作業中 / 終了）、タイトル、要約、場所・ブランチ、使ったスキル、API換算コスト（円換算つき）。返事待ちが30分を超えると時刻が赤くなる
- **返事待ちの理由**: 許可待ち（どのツールの何の実行か）・質問待ち（AskUserQuestion）・プラン承認待ち・返事待ち（ターン終了）を区別して表示
- **コンテキスト残量**: 各セッションの直近の応答時点でコンテキスト窓の何%を使っているか（60% で黄、80% で赤）
- **通知**: 起動中セッションが返事待ちになったら音とデスクトップ通知（何件でも音は1回）。5時間枠・週枠が 80% / 95% を超えたときも1回ずつ通知
- **詳細**: 依頼と返答のタイムライン、触ったファイル、スキル。VS Code / フォルダを開く、`claude --resume` をコピー
- **検索**: 閉じたセッションも含めて、依頼・返答・タイトル・要約を全文検索
- **Haiku 要約**: `claude -p --model haiku` で各セッションを1行に要約（Pro の使用量を消費するので、新しい依頼が入ったセッションだけ、1分に1件、同じセッションは10分に1回まで。画面右上で ON/OFF）
- **使用量グラフ**: 現在の5時間枠、今週の累計（先週比較と予測線つき）、5時間枠ごとの使用量、週ごとの累計。どのグラフにも 100% の上限線を表示。棒グラフはプロジェクト別の積み上げ（直近6週の上位5件＋その他、色はセッションカードの「場所」と対応）で、上限到達した枠には ▲ を付ける
- **5時間枠の上限見込み**: 直近1時間のペースで上限に届く時刻を上部に表示
- **1ページ構成**: 上部に使用量の数値を常に表示し、セッションとグラフはカード状のセクション。折りたたみ可能で、見出しの ⋮⋮ をドラッグ（マウス・タッチ）するか、フォーカスして ↑↓ で並べ替えられる（開閉状態と並び順は端末ごとに保存）。検索はヘッダーから行い、結果は右のパネルに出す

## スマホ・他の端末から見る（Tailscale）

1. アプリ右上の「リモート閲覧」を ON にする（`127.0.0.1:7391` で閲覧専用のサーバーが起動。設定は `~/.claude/dashboard/settings.json` に保存され、次回起動時も有効）
2. 一度だけ、tailnet 内に HTTPS で公開する設定をする:

   ```powershell
   tailscale serve --bg --https=8444 http://127.0.0.1:7391
   ```

3. スマホ（同じ tailnet に参加）から `https://<PC名>.<tailnet>.ts.net:8444` を開く。アプリ右上に表示される URL をクリックすると QR コードが出るので、スマホのカメラで読み取ればよい（URL のコピーも可）

- 待ち受けは `127.0.0.1` だけで、外へは `tailscale serve` 経由（tailnet 内のみ）でしか届かない
- 公開するのは閲覧用の GET API（一覧・詳細・検索・使用量）だけ。VS Code 起動・要約 ON/OFF などの操作はできない
- Host ヘッダーが `127.0.0.1` / `localhost` / `*.ts.net` 以外のリクエストは拒否する（DNS リバインディング対策）
- 公開をやめる: アプリで「リモート閲覧」を OFF、または `tailscale serve --https=8444 off`
- スマホ側は 5 秒ごとに再取得する（画面が非表示の間は止まる）。通知は届かない

## データ元

| 用途 | ファイル |
|---|---|
| 起動中セッションと状態 (busy/idle) | `~/.claude/sessions/*.json`（PID の生存も確認する） |
| 会話・トークン使用量・上限到達 (429) の記録 | `~/.claude/projects/<project>/<session>.jsonl`、`<session>/subagents/*.jsonl` |
| 実測の使用率 % | `~/.claude/dashboard/ratelimits.jsonl`（`scripts/statusline.mjs` が statusLine から記録する） |
| セッションごとのコンテキスト窓の大きさ | `~/.claude/dashboard/context.json`（`scripts/statusline.mjs` が記録。記録が無いセッションは Haiku 200k・それ以外 1M と仮定） |
| 返事待ちの理由 | `~/.claude/sessions/*.json` の `status: "waiting"` と `waitingFor` |
| ドル円レート（円換算） | `~/.claude/dashboard/fx.json`（欧州中央銀行の公表値を [frankfurter.dev](https://frankfurter.dev) から 6 時間ごとに確認して取得。送るのは通貨の種類だけ。取れないときは前回の値を使う） |
| 要約のキャッシュと ON/OFF | `~/.claude/dashboard/summaries.json` |

### 使用率の「実測」と「推定」

- **実測**: Claude Code の statusLine に渡される `rate_limits.five_hour / seven_day.used_percentage`。claude.ai チャットなど他の利用も含む公式の値。statusLine が動いた時点の値しか残らない
- **推定**: このPCの会話ログのトークン数を API 価格に換算したコスト ÷ 推定上限。推定上限は、過去に上限 (429) に達したときの枠内コストの中央値（statusLine の実測が2枠以上たまったら、実測からの逆算に切り替わる）
- 週枠の区切りは、記録されたリセット時刻から7日刻みで計算する（このアカウントでは木曜 13:00 JST 固定）

## セットアップ

```powershell
npm install
npm run app:dev      # 開発
npm run app:build    # インストーラ作成 (src-tauri/target/release/bundle/nsis)
```

statusLine の登録（`~/.claude/settings.json`）:

```json
"statusLine": { "type": "command", "command": "node \"E:/work/git/claude-code-dashboard.git/scripts/statusline.mjs\"", "padding": 0 }
```

Rust 1.89 で動かすため、MSRV に合う依存へ自動で固定している（`src-tauri/.cargo/config.toml`）。Rust を 1.90 以上に上げたら、この設定と `rust-version` を外して最新版に更新できる。

実データで集計を確かめる: `cd src-tauri; cargo run --release --example dump`

## ライセンス

[MIT License](LICENSE)。

同梱しているサードパーティ製ソフトウェアのライセンス表示は [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md) にまとめている。このファイルは `npm run notices` で自動生成され、`npm run build`（`app:build` も含む）のたびに更新される。インストーラには `LICENSE.txt` と一緒に同梱される。

「Claude」は Anthropic の商標です。このプロジェクトは Anthropic の公式製品ではありません。
