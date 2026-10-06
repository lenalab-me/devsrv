# devsrv

コーディングエージェントが Bash から呼ぶ、開発用プロセスの係の Rust 製 CLI。裏で動かすサーバーを起動して準備ができるまで待ち、見て、止める。`sleep` のループ、`pkill -f`、`lsof` での持ち主探しの代わりに使う。

```sh
cargo install --locked --path .
devsrv start web --port auto -- python3 -m http.server {port}   # → http://127.0.0.1:52144/
devsrv ls
devsrv logs web -n 20
devsrv wait http://127.0.0.1:52144/ --timeout 10
devsrv who 8765                                                  # 持ち主を見るだけで、止めない
devsrv stop web
```

`{port}` は devsrv が空いているポートに置き換え、同じ値を `PORT` 環境変数にも入れる。`start` は準備ができたら住所を 1 行だけ出して返り、起動中に落ちたら終了コードとログの末尾を出して失敗する。同じ名前は git の top level（またはディレクトリ）ごとに 1 つで、動いているあいだの 2 回目の `start` は失敗する。使い方の全体は `devsrv --help`（先頭 40 行に書き方・使い方・出力・終了コード）。

止めるのは devsrv が起動したプロセスグループだけ。名前ごとに見張り役のプロセス（`devsrv __supervise`）が 1 つ残り、子を回収しないまま終了を見るので、PID が使い回されても他人には届かない。見張り役が殺されたときだけ、記録の PID と OS の起動時刻を照らし、送るたびに確かめ直してから止める。状態は `~/.local/state/devsrv/`（`DEVSRV_STATE_DIR` で変えられる）。

- [設計](docs/design.md): 見張り役、状態とロック、シグナルを送れる型、ポートと準備、arena での決め方
- [進め方](docs/plan.md): 完了の条件 12 個と段の並び
- [検証のスキル](.agents/skills/verify-devsrv/SKILL.md): 実際に動かして証拠を残す手順と、機能ごとの地図
- 決定の記録: `.audit/devsrv.tsv`

macOS が主。Linux でも動くように書いてあり（`/proc` を読む）、Linux のテストは GitHub Actions の手動の workflow（`.github/workflows/test.yml`）で回す。
