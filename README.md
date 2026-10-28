# pr-wake

GitHubのPRで起きたことを知らせる、macOS用の常駐ツール。

- `pr-wake review`: 自分宛てのレビュー依頼を通知し、選んだSkillのプロンプトを入力済みのCodexスレッドを開く
- `pr-wake watch`: 自分のPRへのレビューコメント・競合・必須チェックの失敗を、作業中のAIタスクワーカーへ伝える

2つは別々に常駐し、片方だけでも使える。

## 導入

Go 1.24以上でビルドする。
更新時も同じコマンドでビルドし、使っている機能の`install`をやり直す。

```bash
go build -o ~/.local/bin/pr-wake ./cmd/pr-wake
```

## review

### 準備

- `gh`でGitHubにログインしておく
- alerterを入れる: `brew install vjeantet/tap/alerter`
- 空のフォルダ`~/review-home`を作り、Codex Appのプロジェクトとして追加する。レビュー用のスレッドはここで開く

```bash
mkdir -p ~/.config/pr-wake
cp config.example.json ~/.config/pr-wake/review.json
# review.jsonを編集してから
pr-wake review install
```

初回の通知時に、macOSからalerterの通知の許可を求められる。

### 使い方

通知のメニューからSkillを選ぶと、PRのheadに固定した専用worktreeを作り、Codexスレッドを開く。
Enterを押すとレビューが始まる。
見送る・閉じる・放置した依頼は再表示しない。
普段のcheckoutは変更せず、レビューの投稿やApproveは自分で行う。

```bash
pr-wake review status     # 常駐の状態と最近の依頼
pr-wake review logs -f
pr-wake review stop
pr-wake review start      # 開始、または再起動
pr-wake review uninstall  # 設定・記録・worktreeは残す
pr-wake review check      # 設定・コマンド・GitHub APIを確認する
pr-wake review smoke --pr owner/repo#123  # 常駐させずに、指定したPRで通知から試す
```

worktreeは自動で削除しないので、不要になったら`git -C <checkout> worktree remove <path>`で消す。
置き場所は既定で`~/.local/share/pr-wake/worktrees/`。

### 設定

`~/.config/pr-wake/review.json`を変えたら、`pr-wake review start`で再起動する。

| 項目 | 内容 |
| --- | --- |
| `repositories` | `owner/repo`とローカルcheckoutのパス。登録したownerの依頼を通知し、登録したリポジトリでレビューを開始できる |
| `teams` | 通知するチーム宛ての依頼（`org/team-slug`）。空なら自分宛てだけ |
| `pollIntervalSeconds` | 確認間隔。既定60、最小30 |
| `notificationTimeoutSeconds` | 通知を閉じるまでの秒数。既定1800 |
| `maxNotifications` | 同時に出す通知の数。既定3、最大10。超えると古い通知から閉じる |
| `worktreeRoot` | worktreeの置き場所 |
| `commands` | `gh`・`git`などの絶対パス。省略時は自動で探す |
| `choices` | 通知メニューの選択肢。`id`・`label`・`skill`・`prompt`を書く |

`skill`には`~/.codex/skills/<skill>/SKILL.md`があるSkill名を書く。
`prompt`はGoのtext/templateで、次の値を使える。

- `{{.PR.URL}}` `{{.PR.Title}}` `{{.PR.Repo}}` `{{.PR.Number}}` `{{.PR.HeadSHA}}` `{{.PR.BaseRef}}`
- `{{.Requester}}`: 依頼者
- `{{.Skill}}` `{{.SkillPath}}`: Skill名とSKILL.mdのパス
- `{{.Worktree}}`: レビュー用worktreeのパス

## watch

PRの監視は[harnexus](https://github.com/riii111/harnexus)の`harnexus-task watch`が行い、pr-wakeはその常駐を管理する。

1. harnexusのcheckoutで`bun run install:task`を実行する
2. harnexusのREADMEに従って`~/.config/harnexus/pr-watch.json`を書く
3. `pr-wake watch install`を実行する
4. 送信に使うCodexのチャットで`harnexus-task watch --sender`を一度実行する。それまでは何も送られない

```bash
pr-wake watch status                   # 常駐の状態と監視中のPR
pr-wake watch logs -f
pr-wake watch stop
pr-wake watch start                    # 開始、または再起動
pr-wake watch uninstall                # 設定・状態・ログは残す
harnexus-task watch --dry-run          # 送らずに、送る内容を表示する
harnexus-task watch --link <PR URL>    # 任意のチャットで、そのPRの出来事を受け取る
```
