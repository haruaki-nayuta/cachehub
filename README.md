# cachehub (`ch`)

`gh` の Read 系コマンドだけをキャッシュして高速化する小さな CLI。

`ch ARGV...` は基本 `gh ARGV...` と同じように振る舞うが、

- `gh issue view` / `gh pr list` のような **Read 系** はローカル SQLite にキャッシュして即返す
- `gh issue close` / `gh pr merge` のような **Write 系** は素通しした上で関連キャッシュを無効化する
- それ以外（`gh api`, `gh gist`, ...）は **そのまま `gh` に丸投げ** する

LLM エージェントから `gh` を大量に叩く用途を主眼にしている。

## 特徴

- **Stale-While-Revalidate**: TTL 切れでも古い body を即返し、裏で `chd`（同一バイナリの常駐デーモン）が `gh` を再実行してキャッシュを上書きする
- **常駐デーモン `chd`**: `ch` 初回起動時に fire-and-forget で自動 spawn される。Unix domain socket (`~/.cache/ch/sock`) で JSON Lines IPC
- **書き込み後の自動 invalidate**: `gh issue close` などが成功したら `issue_view` / `issue_list` を drop
- **`async_passthrough` モード**: Write/Passthrough 系も即 0 を返して `chd` に投げる。失敗は `ch errors` で参照できる
- **エラーは焼き付けない**: `gh` が非ゼロで終わったレスポンスはキャッシュしない（rate limit の 403 を残さないため）
- **脱出弁**: `CH_BYPASS=1 ch ...` は完全素通し

## インストール

```sh
cargo install --path .
```

`~/.cargo/bin/ch` が生えるので、`gh` の代わりに `ch` を叩く。Claude Code などのエージェントには alias ではなく `ch` を直接呼ばせるのがおすすめ。

## 使い方

普段は `gh` と同じ。

```sh
ch issue view 123
ch pr list --state open
ch repo view cli/cli
```

### 内部サブコマンド

| コマンド | 用途 |
| --- | --- |
| `ch cache stats` | 総エントリ数 / 累計ヒット数 / kind 別の内訳 |
| `ch cache purge [pattern]` | `pattern` 指定なしで全削除、ありなら kind/repo に LIKE マッチ |
| `ch daemon status` | `chd` の生存確認 + 直近 72h のアクティブリポジトリ |
| `ch daemon start` / `stop` | デーモンの明示起動・停止（通常は auto-spawn でよい） |
| `ch errors` / `ch errors list [N]` | `async_passthrough` で失敗した `gh` 実行の一覧 |
| `ch errors show <id>` | stdout / stderr 全文 |
| `ch errors clear` | 失敗ログを全削除 |

## キャッシュされるコマンド

| 形 | kind | TTL |
| --- | --- | --- |
| `gh issue list ...` | `issue_list` | 30 sec |
| `gh issue view ...` | `issue_view` | 60 sec |
| `gh pr list ...` | `pr_list` | 30 sec |
| `gh pr view ...` | `pr_view` | 60 sec |
| `gh repo view ...` | `repo_view` | 3600 sec |

それ以外はキャッシュされない（whitelist 方式）。

## 設定

`~/.config/ch/config` に `KEY=VALUE` 形式で書く。`#` 以降はコメント。

```ini
# Write / Passthrough を fire-and-forget で chd に投げる
async_passthrough = true
```

| キー / 環境変数 | 既定 | 説明 |
| --- | --- | --- |
| `async_passthrough` / `CH_ASYNC_PASSTHROUGH` | `false` | true なら Write / Passthrough 系を `chd` に投げて即 0 を返す |
| `CH_BYPASS` | unset | `1` のときは全コマンドを `gh` に素通し（キャッシュも IPC も使わない） |
| `CH_DB_PATH` | `~/.cache/ch/ch.db` | SQLite ファイルの置き場所 |
| `CH_SOCK_PATH` | `~/.cache/ch/sock` | デーモン socket のパス |
| `CH_CONFIG_PATH` | `~/.config/ch/config` | 設定ファイルのパス |

## 仕組み

`ch` の dispatch ルール（[src/main.rs](src/main.rs) 参照）:

1. `--daemon` → `chd` モードで常駐
2. `--refresh ARGV...` → SWR 裏更新の subprocess
3. `CH_BYPASS=1` → 全部素通し
4. `ch cache ...` / `ch daemon ...` / `ch errors ...` → 内部サブコマンド
5. その他 → router で **Read / Write / Passthrough** に分類

Read hit 時はバイナリ本文をそのまま `stdout` に書き出すので、`gh` のフォーマット（`--json` を含め）はそのまま透過する。キャッシュキーは `cwd` と argv を BLAKE3 にかけたもの。

## 開発

```sh
cargo build
cargo test
```

ライセンス: 未定。
