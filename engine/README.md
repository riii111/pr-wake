# pr-wake-engine

`pr-wake watch`の監視本体。
Bun 1.4.4で単一の実行ファイルにビルドし、`pr-wake watch`の常駐から起動される。

```bash
cd engine
bun ci
bun run typecheck
bun run test
bun run build   # dist/pr-wake-engine
```

設定は`~/.config/harnexus/pr-watch.json`、記録は`$XDG_STATE_HOME/taskctl/watch/`（既定`~/.local/state/taskctl/watch/`）を、`harnexus-task watch`の形式のまま使う。
ワーカーへの送信には、harnexusの公開call socketだけを使う。

```text
pr-wake-engine watch                      常駐して監視する
pr-wake-engine watch --dry-run [--pr PR]  送る内容を記録を変えずに表示する
pr-wake-engine watch --status             PRごとの監視状態と送信元を表示する
pr-wake-engine watch --sender             呼び出したチャットを送信元に登録する
pr-wake-engine watch --link PR            呼び出したチャットにPRを紐づける
pr-wake-engine watch --link PR --take-over  別のチャットが監視中のPRを引き継ぐ
pr-wake-engine watch --release PR         止まったPRの監視を再開する
```

成功時は標準出力にJSONを1行出して0で終わる。
失敗は1、使い方の誤りは2で終わり、理由を標準エラーに出す。
