# 引き継ぎ: devsrv（エージェント向けの、開発用プロセス係の CLI）

新しいセッションは、この文書から始める。2026-10-07 に rpatch のセッション（`~/project/prod/rpatch`）で、普段のセッションのログを調べて決めた。同じ日に、別のセッションが下の「進め方の案」の 1〜4 と 5 の半分を済ませた（次の節）。

## 今どこまでできたか（2026-10-07）

- **動くもの.** `start` / `ls` / `logs` / `stop` / `who` / `wait` の 6 つ。`cargo install --locked --path .` で `~/.cargo/bin/devsrv`（0.1.0）に入れた。設計は `docs/design.md`、完了の条件と段は `docs/plan.md`、決定の記録は `.audit/devsrv.tsv`（どれもコミット済み）。
- **形.** 名前ごとに見張り役（`devsrv __supervise`）を 1 つ残し、子を回収しないまま（`WNOWAIT`）終了を見る。止めるのは見張り役で、`stop` は依頼のファイルを置くだけ。見張り役が殺されたときだけ、記録の PID と OS の起動時刻を照らし、送るたびに確かめ直して止める。3 モデルの arena で決めた（土台は Fable 5.1 の案。採点役の GPT-6.1-Sol は別の案を推したが、理由を読んで退けた。`docs/design.md`「まとめの判断」）。
- **確かめ.** `cargo test` に、本物のプロセスを使う結合テスト 29 件と単体テスト 14 件。完了の条件 12 個（`c01`〜`c12`）と、レビューで再現された不具合の回帰テスト。10 回続けて回して全部通る。直す前のコミットで落ちることも確かめた。わざと壊したとき（起動時刻の照合を外す、TERM を送らない）に落ちることも確かめた。
- **レビュー.** interrogate を 3 モデル（GPT-6.1-Sol、Grok 4.7、Fable 5.1）で 1 回かけ、critical と warning をほぼ全部直した。見送ったものは `.audit/devsrv.tsv` の review の行。
- **検証のスキル.** `.agents/skills/verify-devsrv/`（doctor・drive・cleanup と、機能ごとの地図）。1 回通した証拠は `target/verify-devsrv/`（git の外）。
- **案内.** `~/.claude/CLAUDE.md`、`~/.codex/AGENTS.md`、`~/.cursor/rules/global.mdc` の「CLI tools」に 1 行（Background processes: …）を **2026-10-06T23:57Z（日本時間 10/7 08:57）** に足した。比べるときは `python3 tools/baseline.py --since 2026-10-06T23:57Z`（末尾の `Z` を落とすと日本時間として読まれ、9 時間ずれる）。足す前の基準は `~/project/prod/devsrv-notes/baseline-2026-10-07.txt`（14 日、Bash 7,916 回、sleep 703、待ちループ 189、lsof 99、pkill 66、kill 76）。`sleep_minutes` はコマンドに書かれた秒数の合計で、実際に待った時間ではない。
- **規模.** Rust で約 4,100 行（本体 3,300、うち単体テストを含む。結合テスト 850）。見込みの 1,500〜2,500 行を超えた。超えた分の多くは、レビューで足した「見えないときは見えないと言う」扱いと、その回帰テスト。

## 次にやること

1. GitHub の private リポジトリ（`lenalab-me/devsrv` を想定）を作って push する。まだ作っていない（外に出す操作なので、ユーザーに確かめてから）。
2. 対象は macOS だけ（2026-10-07、ユーザーと決めた。このマシンでしか使わない）。`src/os/linux.rs` は残してあるが、一度もビルドしていない。Linux が要るようになったら、CI の matrix に ubuntu を戻して確かめる。
3. 数日たったら `tools/baseline.py --since 2026-10-06T23:57Z` で前後を比べる。devsrv が使われているか（`devsrv` の行）、sleep・lsof・pkill が減ったか。
4. 指示なしの試用とベンチ（rpatch の `tools/bench/` の形）。まだ作っていない。
5. 使われるようになったら、edit-guard のフックに「`pkill -f` や単独の `sleep` に devsrv を案内する」規則を足すのを考える（止めすぎない）。
6. Codex のサンドボックスの中で動くか（状態ディレクトリ、libproc、lsof、ループバックの bind）。書けないときは終了 3 で `DEVSRV_STATE_DIR` を案内する作りにしてある。

## 何を作るか

Claude Code（と、T3 から delegate される Codex）のエージェントが、Bash から呼ぶ CLI。開発中に裏で動かすサーバーやプロセスを起動し、待ち、見て、止める。rpatch がファイルの編集でしたことを、プロセスの扱いでする。確かめられる形にし、失敗したら原因と直し方をはっきり返す。

## なぜ（数字は `python3 tools/baseline.py --days 14`、2026-10-07、Bash 7,899 回）

| 手間 | 回数 | 中身 |
|---|---|---|
| `sleep` | 702 回、合計 92 分 | 待ち合わせ。Claude Code 自身が「単独の sleep」として止めたものが 10 回 |
| 待ち合わせのループ | 189 回 | `until …; do sleep 1; done` の形 |
| `sleep` のあとの確認 | `curl` 44、`cat` 24、`pgrep` 23 | サーバーが応答したか、ログが出たか、プロセスが生きているかを見る |
| 裏でサーバーを起動 | 58 回 | `node …server… &`、`python3 -m http.server &`、`vite` など |
| ポートの持ち主を探す（`lsof -i`） | 99 回 | 多いのは 8787・8790・8799・8788・8798（beat-jam の模擬サーバー）、8080、5173 |
| 名前で止める（`pkill`） | 65 回 | 自分が起動していないプロセスまで止めかねない危ない形 |
| PID で止める（`kill`） | 76 回 | |

`sleep` の直後で一番多いのは `orca`（125 回）。これは Orca の出力を待つもので、Orca 側の話なので対象の外。

2026-10-06 の試用でも、エージェントが確認用のサーバーを立てようとして、ポート 8765 がユーザーのダッシュボードとぶつかった。エージェントは「ユーザーのものだから触らない」と自分で避けたが、そういう判断が要ること自体が手間になっている。

## 仕様の案（たたき台。設計の段で見直す）

```
devsrv start NAME [--port auto|N] [--ready http[:PATH]|port|log:REGEX] [--timeout 30] -- CMD...
    裏で起動する。--port auto なら空いているポートを選び、PORT 環境変数で渡す。
    準備ができるまで待ち、URL（または PID）を 1 行で返す。
    起動中に落ちたら、終了コードとログの末尾を出して失敗する。
devsrv ls               自分（devsrv）が起動したものの一覧: 名前、PID、ポート、経過時間、作業ディレクトリ、生きているか
devsrv logs NAME [-n 50] [--follow-until REGEX --timeout N]
devsrv stop NAME|--all  自分が起動したプロセスグループだけを止める（TERM、だめなら KILL）。名前では探さない
devsrv who PORT         そのポートの持ち主。devsrv が起動したものなら名前、そうでなければ PID とコマンドを出す（止めない）
devsrv wait http://…|port N|--file PATH|--log NAME REGEX|--gone PID [--timeout N]
    sleep のループの代わり。条件が成り立つまで待ち、時間切れなら失敗する。待った秒数を返す
```

- **状態の置き場所**: `~/.local/state/devsrv/`。作業ディレクトリ（git の top level）ごとに分け、名前の衝突を防ぐ。PID、ポート、起動コマンド、ログのパス、起動した時刻を JSON で持つ。死んだものは `ls` のときに印を付け、`start` のときに片付ける。
- **安全**: 止めるのは、devsrv が起動したプロセスグループだけ。PID の使い回しに気をつける（起動時刻やコマンドも照らし合わせる）。他人のプロセスは `who` で見せるだけで、止めない。
- **出力**: エージェントが読む前提で、短く 1 行目に結論。`--json` も用意する。終了コードは rpatch にならって 0 成功、1 条件が成り立たない（時間切れ、落ちた）、2 入力の誤り、3 OS の失敗、あたりで決める。
- **作り**: rpatch と同じく Rust の 1 つのバイナリ（rust-version 1.88、edition 2024。手元は 1.98.1）。macOS が主で、Linux でも動くように書く。

## 範囲の外（今は）

- Orca や T3 の出力待ち。
- サービスの常駐管理（launchd・systemd の代わり）。セッションの間だけの開発用プロセスに絞る。
- Docker（`docker compose` はそれ自体に管理の仕組みがある）。必要が見えてから考える。
- MCP サーバー。rpatch で、MCP のツールは説明文だけを見て使い方を推測されやすく、CLI のほうがまず `--help` を読むと分かっている。

## rpatch から持ってくる教訓

- **`--help` の先頭 40 行に、書き方・主な使い方・出力・終了コードを収める。** エージェントは `--help | head -40` で読む。
- **失敗は大きな音で、直し方つきで。** どこで何が足りないかを 1 行目で言い、次の 1 回で直せるヒントを付ける。ログの末尾も見せる。
- **書式はゆるく、動作は厳しく。** 意味がひとつにしか取れない書き方の揺れは受け付ける。止めると、エージェントは確かめのない別の手（ここでは `pkill -f` や `sleep`）に逃げる。
- **作る前に基準を取り、入れたあとにログで測る。** `tools/baseline.py` に `--since` があるので、入れた時刻から後だけを数えられる。
- **案内は CLAUDE.md に短く。** rpatch は `~/.claude/CLAUDE.md` の「CLI tools」に 2 行で案内しているだけで、指示しなくても自分から使うようになった。devsrv の 1 行を足すときは、`~/.codex/AGENTS.md` と `~/.cursor/rules/global.mdc` にも同じ内容を写す（rpatch の HANDOFF「入れ方と、普段の使われ方」を参照）。足した時刻は、比較のためにこの文書に残す。
- **逃げ道はフックでふさげる。** rpatch では、PreToolUse のフック（`~/project/prod/rpatch/plugin/hooks/edit_guard.py`、説明は `docs/edit-guard.md`）で確かめずに書く経路を止めた。devsrv が使われるようになったら、`pkill -f` や単独の `sleep` に devsrv を案内する規則を、同じフックに足すのが次の候補になる（止めすぎない。分からないものは通す）。
- **検証のスキルとベンチ。** rpatch には `.agents/skills/verify-rpatch/`（実際に動かして証拠を残す）と `tools/bench/run.sh`（指示なしの試用を 11 課題で回す）がある。devsrv でも、実装したら同じ形で用意する。

## 進め方の案

p3 のスキルを使う。`figure-it-out` で全体を組み立て、型と構成は `architect` で 2 案以上を比べて決め、仕上げに `interrogate`（3 モデルの敵対的なレビュー）をかける。モデルの割り当ては `~/.agents/p3-models.md`。

1. 枠組み: 完了の条件を、確かめられる形で書く（例: 模擬サーバーを `start --port auto --ready http` で起動して URL が返る、`stop` で子プロセスまで消える、他人のプロセスは止めない、`wait` が時間切れで失敗する）。
2. 設計: 状態の持ち方、プロセスグループ、PID の使い回し対策、空きポートの選び方の競合（選んでから bind するまでの間）、準備の判定の種類。
3. 実装: `start` / `stop` / `ls` / `logs` から始め、`wait` と `who` を足す。結合テストは、本物のプロセス（小さなサーバー）を立てて行う。
4. 検証のスキル（`/create-verification-skill`）、`--help` の先頭 40 行の確認、`cargo install` で入れる。
5. CLAUDE.md（と AGENTS.md・Cursor）に 1 行足し、その時刻を残す。試用とベンチを回し、`tools/baseline.py --since <時刻>` で前後を比べる。

## 決めたこと（2026-10-07、ユーザーと）

- 名前は `devsrv`。
- GitHub の private リポジトリに置く（rpatch と同じく `lenalab-me` の下を想定。作るときに確かめる）。
- 動いているものと同じ名前で `start` を呼んだら、`already started` で失敗にする。失敗のメッセージには、動いているものの URL（または PID）と経過時間を出し、`devsrv stop NAME` で止めてから起動し直せることを添える。エージェントが、そのまま使い続けるか、起動し直すかを次の 1 回で選べるように。前のものが落ちて記録だけ残っているときは、片付けて起動する（失敗にしない）。終了コードは設計の段で決める（「条件が成り立たない」の 1 か、「入力の誤り」の 2 か）。

## 環境と決まり

- 返答は日本語（`~/.claude/CLAUDE.md`）。コード内のコメントやコミットメッセージは、そのリポジトリの書き方に合わせる（rpatch は英語のコミットメッセージ、日本語の文書）。
- ファイルの編集は rpatch（1 か所でも）。コミットは、編集の結果を見てから別の呼び出しで。
- 手元で使われているポート: 8765 はユーザーのダッシュボード（rpatch の `tools/log-analysis/dashboard.py`）。8787〜8799 は beat-jam の模擬サーバー。
- セッションのログ（`~/.claude/projects/`）には認証情報が入ることがある。中身を出すときはリポジトリの外に置き、終わったら消す。`tools/baseline.py` は件数しか出さない。

## 参考

- rpatch: `~/project/prod/rpatch`。`HANDOFF.md`（進め方と測り方の例）、`docs/design.md`、`tools/log-analysis/`（session.py、metrics.py、dashboard.py）、`tools/bench/`、`.agents/skills/verify-rpatch/`、`plugin/`、`docs/edit-guard.md`、`.audit/edit-guard.tsv`（figure-it-out の決定ログの例）。
