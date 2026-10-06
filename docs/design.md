# devsrv の設計

2026-10-07。architect の arena で 3 モデル（Fable 5.1、GPT-6.1-Sol、Grok 4.7）が出した案を、別のモデル（GPT-6.1-Sol）が採点し、1 つにまとめた。もとの案は `/tmp/arena-devsrv/candidate-{a,b,c}/`（リポジトリの外）。決定の記録は `.audit/devsrv.tsv`。

## 問題

エージェントが Bash から、裏で動かすプロセスを起動し、待ち、見て、止める。形が自明でないのは次の 4 点。

- `start` は返るが、プロセスは残る。返ったあとの終了コードを知るのは親だけ。
- 止める操作は取り返しがつかない。PID は使い回される。
- 同じ名前の `start` が同時に来る。`devsrv start` 自身が、ツールの時間切れで途中で殺されることもある。
- 書ける場所と見える範囲が環境で違う（Codex のサンドボックス）。

## 使い方（呼ぶ側から見た形）

```
$ devsrv start web --port auto -- python3 -m http.server {port}
http://127.0.0.1:52144/                       ← stdout はこの 1 行だけ
devsrv: web ready in 0.3s (pid 4242); logs: devsrv logs web; stop: devsrv stop web   ← stderr
[exit 0]

$ devsrv start web --port auto -- python3 -m http.server {port}
devsrv: web is already started: http://127.0.0.1:52144/ (pid 4242, up 2m10s)
  hint: keep using it, or restart it: devsrv stop web, then start again
[exit 1]

$ devsrv start api -- sh -c 'echo "config.toml: no such file" >&2; exit 3'
devsrv: api exited with code 3 after 0.1s, before it was ready
  log (last 1 of 1 lines): …/api/log
  | config.toml: no such file
  hint: fix the command and start again; nothing is left running
[exit 1]

$ devsrv stop web
web: stopped (TERM, 0.1s)
[exit 0]
$ devsrv stop web
web: already stopped (stopped 4s ago)
[exit 0]

$ devsrv who 8765
8765: pid 911 python3 tools/log-analysis/dashboard.py (not started by devsrv; devsrv will not stop it)
[exit 0]

$ devsrv wait port 29999 --timeout 1
devsrv: port 29999 did not accept a connection within 1.0s
  hint: devsrv ls shows what devsrv started; devsrv who 29999 shows who holds the port
[exit 1]
```

`{port}` は `--` の後ろの引数の中で devsrv が置き換える。同じ値を `PORT` 環境変数にも入れる。`vite --port $PORT` と書くと、エージェントのシェルが先に展開してしまうため。

## 形

### 見張り役が名前の持ち主

`devsrv start` は、自分自身を `devsrv __supervise` として起動する（`setsid` で新しいセッションへ切り離す）。見張り役がコマンドを起動し、準備を確かめ、子の終了を見届け、止める。エージェントが打つ `devsrv` は、見張り役が書いた記録を読むだけで、止めるときは依頼を置くだけ。

```
devsrv start（エージェントのシェル）       devsrv __supervise（切り離し）        CMD
  名前のロックを取る ───────────────▶ ロックの記述子を受け継ぐ
  ポートを決める（bind(0)）              spawn（自分のグループ。出力は log へ）─▶ 起動
  Spec を stdin で渡す                   record: Up{ready: None}
  record.json を 20ms ごとに読む         準備を確かめる
       ◀──────────────────────────── record: Up{ready: Some}
  URL を出して exit 0                    100ms ごとに: 子は終わったか / stop はあるか
                                         グループを片付ける → record: Down → 子を回収 → 終了（ロックが外れる）
```

見張り役に集めるのは 3 つ。終了コード（親だけが読める）、起動の手順の全体（`devsrv start` が途中で殺されても、子は「準備できた」か「止めた」のどちらかに必ず着く）、止めること。コマンドの出力は、コマンドがログのファイルへ直接書く。見張り役が殺されても、コマンドとログは続く。

コマンドが終わったら、同じグループに残ったものも見張り役が止める。名前が生きている間は、コマンドが生きている間と同じ。

### 状態

```
<root>/<プロジェクト名>-<パスの FNV-1a 64 の 16 進>/
    root                 プロジェクトの絶対パス。中身が違えば終了 3（ハッシュの衝突を混ぜない）
    <name>/lock          見張り役が生きている間ずっと flock(LOCK_EX) を持つ
    <name>/record.json   ロックを持つ者だけが書く。一時ファイルと rename で置き換える
    <name>/log           コマンドの stdout と stderr。start のたびに空にする
    <name>/stop          止める依頼。中身は実行の ID（子の PID と起動時刻）
    <name>/supervisor.err  見張り役自身の stderr
```

`<root>` は `DEVSRV_STATE_DIR`、なければ `$XDG_STATE_HOME/devsrv`、なければ `~/.local/state/devsrv`。書けなければ子を起動する前に終了 3 で、`export DEVSRV_STATE_DIR=…` を案内する（`/tmp` に黙って移すと、次の `ls` や `stop` が記録を見失う）。プロジェクトは、作業ディレクトリから上へたどって最初に `.git`（ディレクトリでもファイルでも）がある場所。git の外なら作業ディレクトリ。`git` は呼ばない。

記録は 2 状態。生きているかは書かず、読むたびにロックから導く。

| 記録 | ロック | 見えるもの |
|---|---|---|
| `Up { ready: None }` | 取られている | starting |
| `Up { ready: Some }` | 取られている | ready |
| `Down` | どちらでも | dead（終了コード、原因） |
| `Up` | 空いている | orphan（見張り役が殺された。子がまだいるかは OS に聞く） |

ロックはカーネルが持ち主の死とともに外すので、見張り役がどう死んでも嘘にならない。同じ名前の `start` が 2 つ重なったら、`flock(LOCK_EX|LOCK_NB)` を通るのは 1 つだけ。前の `start` はロックを取ってから見張り役を起動し、見張り役は記述子を受け継ぐので、2 つの間に隙がない。

### シグナルを送れるのは `OwnedGroup` だけ

`libc::kill` はクレートの中で 1 か所にしかなく、相手は必ずプロセスグループ（pgid が 1 以下なら送らない）。`OwnedGroup` の作り方は 2 つ。

- `Child::group(&self) -> OwnedGroup<'_>`: 見張り役が起動して、まだ回収していない子のグループ。子の借用なので、回収（`Child::reap(self)`）のあとには作れない。子の終了は `waitid(WEXITED|WNOHANG|WNOWAIT)` で見るので、回収するまで PID は使い回されず、pgid が別のグループを指すことはない。
- `OwnedGroup::verify(&ProcRef)`: 見張り役がいないとき（orphan）だけ。記録の PID がいまもあり、ゾンビでなく、グループの先頭で、OS の起動時刻が記録と完全に同じときだけ作れる。送る前には毎回確かめ直す（先頭がまだ合うか、グループにまだメンバーがいて pgid が使い回されないか）。起動時刻を記録できなかった起動は照合できないので、送らずに終了 3。

起動時刻は macOS が `proc_pidinfo(PROC_PIDTBSDINFO)` の `pbi_start_tvsec/usec`、Linux が `/proc/<pid>/stat` の 22 番目と `boot_id`。OS が答えないとき（`Denied`）は、いないとも自分のものとも見なさず、送らない。

止め方は `OwnedGroup::terminate` の 1 つ: TERM、グループが空になるのを 5 秒まで待つ、KILL、2 秒まで待つ。空かどうかはプロセスの一覧で見る（ゾンビは数えない。macOS は `kill(-pgid, 0)` がゾンビだけのグループで EPERM を返すため使わない）。グループの外へ出た子孫（自分で setsid したもの）は追わない。

### ポートと準備

`--port auto` は `127.0.0.1:0` を bind して OS に選ばせ、閉じてから渡す。閉じてから子が bind するまでの隙は残るので、準備ができたと言う前に、そのポートで待ち受けているのが自分のグループのプロセスかを確かめる。他人なら、自分の子を止めて終了 1（他人には何もしない）。持ち主が見えないときは確かめを飛ばす。固定のポートは、起動の前に使われていないかを見て、使われていれば持ち主を出して終了 1。

`--ready` は `http[:PATH]`（最終の応答が 200〜499）、`port`（TCP がつながる）、`log:REGEX`（ログの行に合う）、`none`（0.5 秒たっても動いている）。既定はポートがあれば `http`、なければ `none`。127.0.0.1 のあと ::1 も試し、応答したアドレスで URL を作る。1 回の試しには、残り時間と 250ms の短いほうを期限に渡す。

`start`、`wait`、`logs --follow-until` は同じ待ちのループを使う。

### who

macOS は `lsof -nP -iTCP:PORT -sTCP:LISTEN -Fp`（2 秒の期限）、Linux は `/proc/net/tcp{,6}` と `/proc/<pid>/fd`。見つけた PID の pgid を、すべてのプロジェクトの生きている記録の子と照らし、devsrv の名前か、PID とコマンドかを出す。止めない。

### 出力と終了コード

- `start` の stdout は住所の 1 行だけ（`http://127.0.0.1:N/`、HTTP を確かめていなければ `127.0.0.1:N`、ポートがなければ `pid N`）。補足は stderr。
- 失敗は stderr の 1 行目に結論、続けてログの末尾と `hint:`。
- `--json` はどのサブコマンドでも、成功でも失敗でも stdout に JSON を 1 つ出す（`{"ok":…}`）。
- 終了コード: 0 できた、1 条件が成り立たない（落ちた、時間切れ、already started、知らない名前、ポートが使用中、止め残し、who でポートが空き）、2 書き方の誤り（コマンドが見つからないも含む）、3 devsrv が働けない（状態ディレクトリ、壊れた記録、OS が答えない）。

### モジュール

| モジュール | 持っている知識 |
|---|---|
| `cli` | 引数の書き方の揺れ。文字列を `Command` にする。`help.txt` |
| `model` | 名前、プロジェクト、`Spec`、記録 |
| `store` | 状態ディレクトリ、ロック、誰が書けるか |
| `os` | macOS と Linux の違い。シグナルを送る唯一の場所 |
| `probe` | 条件と待ちのループ、ログの末尾 |
| `supervisor` | 1 つの名前の一生 |
| `commands` | エージェントのシェルで動く側（start、stop、ls、logs、who、wait） |
| `report` | 文面、JSON、終了コード |

## まとめの判断（arena）

土台は案 A（Fable 5.1）。採点役は案 B を推した（安全 5 点、A は 2 点）。理由を確かめ、次のとおり扱った。

- **A の型の穴**（`OwnedGroup` が回収前の子に縛られていない）は当たっている。`OwnedGroup<'a>` を子の借用にし、回収は子を消費する形にして塞いだ。
- **A の読み取りが破損を伝えない**も当たっている。`store` の読み取りは `Result` を返し、壊れた記録は終了 3。
- **見張り役がいないときに止めるか**は、B は止めない、A は起動時刻とグループの先頭を確かめてから止める。止めないと、エージェントは `kill` や `pkill` に逃げる。これは devsrv がなくしたい手そのもの。残る隙は「確かめてから送るまでのマイクロ秒の間に、子が終わり、回収され、同じ PID が新しいグループの先頭になる」場合だけなので、A を採った。
- **停止の依頼**は、B のソケットではなく A のファイルにした。ただし採点役の指摘どおり、古い依頼が次の起動を止めないように、ファイルに実行の ID（子の PID と起動時刻）を書き、見張り役は自分のものだけに従う。ソケットはパスの長さ（macOS で 104 バイト）とサンドボックスに当たるうえ、相手の確認と応答の手順が増える。
- B と C から移したもの: `--port auto` は bind(0)（A の「名前のハッシュで 20000〜29999」は、範囲の前提が設定で崩れる）。準備の成功に待ち受けの持ち主の確認（B）。HTTP の成功は 200〜499（採点役）。状態ディレクトリに書けなければ終了 3（B、C）。知らない名前の `stop` は 1（C）。コマンドが見つからないのは 2（C）。pgid が 1 以下には送らない（C）。
- 移さなかったもの: B の起動用 socketpair と制御ソケット、非同期の持ち主調べ、DNS のスレッド（数百行を足すわりに、A の形で同じ約束が守れる）。C の SIGUSR1 による依頼（見張り役の PID の使い回しが戻る）と、起動の間だけのロック。
- 3 案とも同じだったもの（強い一致として、そのまま採った）: 名前ごとの見張り役、子は自分のプロセスグループ、名前ごとの flock、起動時刻による同一性、依存は libc のみ、引数は手書き、already started は 1。

## 受け入れた代わりのもの

- 起動したコマンド 1 つにつき、見張り役のプロセスが 1 つ増える。代わりに終了コード、途中で殺されても壊れない起動、隙のない停止を得る。
- 停止が効くまで最大 100ms 遅れる。代わりに、スレッドもソケットも持たない 1 本のループにする。
- コマンドが終わると同じグループの残りも止める。`sh -c 'server &'` のように自分で裏に回す書き方は失敗になる。
- グループの外へ出た子孫は追わないし、報告もしない（見つけるにはプロセス表を親子でたどる必要があり、たどった PID が使い回されると他人を名指しする）。グループの中で KILL のあとも残ったものだけを PID で出す。
- 見張り役がいないときの停止には、確かめてから送るまでの隙がある（上記）。送るたびに確かめ直すので、隙は 1 回の送信ごとのマイクロ秒に限られる。
- グループの一覧が読めないときは「何も残っていない」と言わない。stop は終了 3、start の失敗には警告を付ける。
- 見張り役が spawn してから記録を書くまでの数ミリ秒に殺されると、記録のない子が残る。誰かが見張り役を名指しで殺さない限り起きない。
- ログは回さない。同じ名前の次の `start` で空にする。
- `--port auto` は起動し直すたびに番号が変わる。
- `https://` は持たない。

## ほかに考えた形

- **見張りなし**（setsid で切り離すだけ。止めるときは PID と起動時刻を照らす）。一番小さいが、返ったあとの終了コードは誰にも分からず、`start` が途中で殺されると時間切れのない半端なプロセスが残る。この案の停止の道は、本案の orphan の道として残っている。
- **全体で 1 つのデーモン**（pm2 や overmind の形）。デーモンを誰が起こし、いつ終わらせ、入れ替えたときにどうするかが呼ぶ側に漏れ、落ちると全部の見張りを失う。
- **見張り役が出力をパイプで受ける**。ログに時刻を付けられるが、見張り役が死ぬとコマンドが SIGPIPE で道連れになる。

## 未決と危険

- Codex のサンドボックスの中で、flock、libproc、lsof、ループバックの bind が通るか。通らないものは `Denied` として送らない側に倒す。E 段で確かめる。
- Linux は手元で動かせない。CI（手動の workflow）で確かめる。
