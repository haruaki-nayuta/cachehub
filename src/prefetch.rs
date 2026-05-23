// アクティブリポジトリの issue list を起点にした連想プリフェッチ。
//
// `ch issue list` が成功すると、CLI 側 (exec::maybe_kick_issue_prefetch) が
// daemon に PrefetchIssues を投げる。daemon はこのモジュールの run() を
// ワーカースレッドで回し、
//   1. `gh issue list --json number` で issue 番号一覧を取り直し
//   2. 各 issue について `gh issue view <N>` の cache が cold なら埋める
// ことで、後続の `ch issue view <N>` を hit させる。
//
// 設計上の割り切り:
//   - opt-in（config の prefetch=true）。GitHub API のレート制限を尊重して既定 off。
//   - 1 回の list につき最大 PREFETCH_LIMIT 件まで。
//   - 既に fresh な issue_view は叩き直さない（TTL 内に list を連打しても安い）。
//   - gh が非ゼロで終わった view はキャッシュしない（rate limit の 403 を焼き付けない）。
//   - cwd はユーザ視点のものを使う。gh のリポジトリ解決と cache key の両方で、
//     後続の `ch issue view` と同じ条件を再現するために必須。

use anyhow::{Context, Result};
use serde::Deserialize;
use std::process::{Command, Stdio};

use crate::exec::{build_entry, epoch_secs};
use crate::key;
use crate::limiter;
use crate::router::{self, Action};
use crate::store::Store;

/// 1 回の issue list で先読みする issue view の上限。
/// レート制限と裏更新の重さのバランスでこの辺り。
const PREFETCH_LIMIT: usize = 20;

#[derive(Deserialize)]
struct IssueNumber {
    number: u64,
}

/// daemon のワーカースレッドから呼ばれる本体。
pub fn run(list_argv: &[String], cwd: &str) -> Result<()> {
    let numbers = fetch_issue_numbers(list_argv, cwd)?;
    if numbers.is_empty() {
        return Ok(());
    }

    let store = Store::open_default()?;
    let repo_tok = repo_tokens(list_argv);
    let limiter = limiter::global();

    for number in numbers.into_iter().take(PREFETCH_LIMIT) {
        // GlobalLimiter のトークンを取れなければ今回の list 起点プリフェッチは打ち切る。
        // - paused（headroom 低）→ 残り全部スキップが正
        // - バケット枯れ → 1 件先頭で弾かれて以降全部弾かれるはず。break で揃える
        // - 未初期化（テスト等）→ 通す
        if let Some(l) = limiter.as_deref() {
            if !l.try_acquire() {
                break;
            }
        }
        let view_argv = build_view_argv(number, &repo_tok);
        if let Err(e) = prefetch_one(&store, &view_argv, cwd) {
            // 1 件の失敗で全体を止めない（レート制限などは次回の list に任せる）
            eprintln!("chd: prefetch view #{number} 失敗: {e:#}");
        }
    }
    Ok(())
}

/// `gh issue list --json number` を回して issue 番号一覧を得る。
fn fetch_issue_numbers(list_argv: &[String], cwd: &str) -> Result<Vec<u64>> {
    let query = build_numbers_query(list_argv);
    let output = Command::new("gh")
        .args(&query)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .context("gh issue list (prefetch) を起動できなかった")?;

    // list 自体が失敗したら諦める（stale も無いので素直に空で返す）
    if output.status.code() != Some(0) {
        return Ok(Vec::new());
    }
    parse_numbers(&output.stdout)
}

/// `gh issue list --json number` の出力（JSON 配列）から番号だけ取り出す。
fn parse_numbers(json: &[u8]) -> Result<Vec<u64>> {
    let rows: Vec<IssueNumber> = serde_json::from_slice(json)
        .context("gh issue list --json number の出力を parse できなかった")?;
    Ok(rows.into_iter().map(|r| r.number).collect())
}

/// issue view 1 件を、cache が cold（未保存 or stale）なときだけ埋める。
fn prefetch_one(store: &Store, view_argv: &[String], cwd: &str) -> Result<()> {
    // kind / TTL は router の whitelist を単一の真実として引く
    let Action::Read { kind, ttl } = router::classify(view_argv) else {
        return Ok(());
    };

    let cache_key = key::cache_key_with_cwd(cwd, view_argv);
    let now = epoch_secs();

    // 既に fresh ならスキップ
    if let Some(entry) = store.get(&cache_key)? {
        if now.saturating_sub(entry.fetched_at) < entry.ttl_secs {
            return Ok(());
        }
    }

    let output = Command::new("gh")
        .args(view_argv)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .context("gh issue view (prefetch) を起動できなかった")?;

    if output.status.code() == Some(0) {
        let entry = build_entry(view_argv, kind, output.stdout, now, ttl);
        store.put(&cache_key, &entry)?;
    }
    Ok(())
}

/// list の argv から view 1 件の argv を組み立てる。
/// repo 指定はユーザが list で使った綴りをそのまま引き継ぎ、後続 `ch issue view` と
/// cache key を一致させる。
fn build_view_argv(number: u64, repo_tokens: &[String]) -> Vec<String> {
    let mut v = vec!["issue".to_string(), "view".to_string(), number.to_string()];
    v.extend_from_slice(repo_tokens);
    v
}

/// list の argv からフォーマット系フラグ（--json / --jq / --template / --web）を除き、
/// `--json number` を付けた「番号取得専用」クエリを作る。
/// --state や --label などの絞り込みはユーザ指定をそのまま残す。
fn build_numbers_query(list_argv: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(list_argv.len() + 2);
    let mut i = 0;
    while i < list_argv.len() {
        let a = list_argv[i].as_str();
        // 値を 1 つ取るフォーマット系フラグ → フラグ + 値の 2 トークンを飛ばす
        if matches!(a, "--json" | "-q" | "--jq" | "-t" | "--template") {
            i += 2;
            continue;
        }
        // ブラウザを開くだけの --web はキャッシュに使えない
        if matches!(a, "--web" | "-w") {
            i += 1;
            continue;
        }
        // `--json=foo` のような = 連結形
        if a.starts_with("--json=")
            || a.starts_with("--jq=")
            || a.starts_with("-q=")
            || a.starts_with("--template=")
            || a.starts_with("-t=")
        {
            i += 1;
            continue;
        }
        out.push(list_argv[i].clone());
        i += 1;
    }
    out.push("--json".to_string());
    out.push("number".to_string());
    out
}

/// list の argv から repo 指定トークンをそのまま抜き出す（`["-R","o/n"]` 等）。
/// 後続 `ch issue view` と同じ綴りで cache key を当てるため、正規化はしない。
fn repo_tokens(list_argv: &[String]) -> Vec<String> {
    let mut iter = list_argv.iter();
    while let Some(a) = iter.next() {
        if a == "--repo" || a == "-R" {
            return match iter.next() {
                Some(v) => vec![a.clone(), v.clone()],
                None => Vec::new(),
            };
        }
        if a.starts_with("--repo=") {
            return vec![a.clone()];
        }
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().copied().map(String::from).collect()
    }

    #[test]
    fn numbers_query_appends_json_number() {
        assert_eq!(
            build_numbers_query(&argv(&["issue", "list"])),
            argv(&["issue", "list", "--json", "number"])
        );
    }

    #[test]
    fn numbers_query_keeps_filters() {
        assert_eq!(
            build_numbers_query(&argv(&[
                "issue", "list", "--state", "closed", "--label", "bug", "--limit", "5"
            ])),
            argv(&[
                "issue", "list", "--state", "closed", "--label", "bug", "--limit", "5", "--json",
                "number"
            ])
        );
    }

    #[test]
    fn numbers_query_strips_format_flags() {
        // --json <v> / --jq <v> は値ごと落として絞り込みだけ残す
        assert_eq!(
            build_numbers_query(&argv(&[
                "issue", "list", "--json", "title,number", "-q", ".[].title", "--state", "open"
            ])),
            argv(&["issue", "list", "--state", "open", "--json", "number"])
        );
        // = 連結形と --web も落とす
        assert_eq!(
            build_numbers_query(&argv(&["issue", "list", "--json=title", "--web"])),
            argv(&["issue", "list", "--json", "number"])
        );
    }

    #[test]
    fn repo_tokens_variants() {
        assert_eq!(
            repo_tokens(&argv(&["issue", "list", "--repo", "cli/cli"])),
            argv(&["--repo", "cli/cli"])
        );
        assert_eq!(
            repo_tokens(&argv(&["issue", "list", "-R", "cli/cli"])),
            argv(&["-R", "cli/cli"])
        );
        assert_eq!(
            repo_tokens(&argv(&["issue", "list", "--repo=cli/cli"])),
            argv(&["--repo=cli/cli"])
        );
        assert!(repo_tokens(&argv(&["issue", "list"])).is_empty());
    }

    #[test]
    fn view_argv_mirrors_repo_spelling() {
        assert_eq!(build_view_argv(7, &[]), argv(&["issue", "view", "7"]));
        assert_eq!(
            build_view_argv(7, &argv(&["-R", "cli/cli"])),
            argv(&["issue", "view", "7", "-R", "cli/cli"])
        );
        // 組み立てた view argv は router で issue_view に分類される
        assert!(matches!(
            router::classify(&build_view_argv(7, &[])),
            Action::Read {
                kind: "issue_view",
                ttl: 60
            }
        ));
    }

    #[test]
    fn parse_numbers_extracts_numbers() {
        assert_eq!(parse_numbers(b"[]").unwrap(), Vec::<u64>::new());
        assert_eq!(
            parse_numbers(br#"[{"number":1},{"number":42}]"#).unwrap(),
            vec![1, 42]
        );
    }

    #[test]
    fn parse_numbers_rejects_garbage() {
        assert!(parse_numbers(b"not json").is_err());
    }
}
