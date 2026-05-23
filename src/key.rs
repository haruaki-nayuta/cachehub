// キャッシュキー生成。
//
// v0.1 では「同じ場所 (cwd) で同じ argv を叩いたら同じキー」とする雑な版。
// 認証ユーザの混入対策は v0.1.1 以降で `gh auth status` の login を混ぜる予定。

use std::env;
use std::process::{Command, Stdio};

pub fn cache_key(argv: &[String]) -> String {
    let cwd = env::current_dir()
        .ok()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    cache_key_with_cwd(&cwd, argv)
}

/// `cache_key` の cwd を明示指定する版。
/// daemon は自分の cwd と無関係に「ユーザが ch を叩いた cwd」でキーを組む必要がある
/// （連想プリフェッチで、後続の `ch issue view` と同じキーに当てるため）。
pub fn cache_key_with_cwd(cwd: &str, argv: &[String]) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(cwd.as_bytes());
    hasher.update(b"\x1f");
    for a in argv {
        hasher.update(a.as_bytes());
        hasher.update(b"\x1f");
    }
    hasher.finalize().to_hex().to_string()
}

/// argv に `--repo` 指定が無いとき、cwd の `git remote get-url origin` から
/// `owner/name` を解決して argv 末尾に `--repo owner/name` を足したものを返す。
///
/// 目的は cache.repo を埋めて Warmer 対象に乗せること（issue #20）。
/// 解決失敗（origin 無し / GitHub 以外 / git 未インストール）は argv をそのまま返し、
/// 従来挙動にフォールバックする。
pub fn augment_argv_with_repo(argv: &[String]) -> Vec<String> {
    augment_argv_with_resolver(argv, resolve_repo_from_cwd)
}

fn augment_argv_with_resolver<F>(argv: &[String], resolver: F) -> Vec<String>
where
    F: FnOnce() -> Option<String>,
{
    if detect_repo(argv).is_some() {
        return argv.to_vec();
    }
    let Some(repo) = resolver() else {
        return argv.to_vec();
    };
    let mut out = Vec::with_capacity(argv.len() + 2);
    out.extend_from_slice(argv);
    out.push("--repo".to_string());
    out.push(repo);
    out
}

/// `git remote get-url origin` を 1 回呼んで `owner/name` を返す。
/// GitHub 以外のホストや解決失敗は None。
fn resolve_repo_from_cwd() -> Option<String> {
    let output = Command::new("git")
        .args(["remote", "get-url", "origin"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let url = std::str::from_utf8(&output.stdout).ok()?.trim();
    parse_github_remote(url)
}

/// git remote URL から `owner/name` を抜き出す。
/// SSH (`git@github.com:owner/name(.git)?`) と
/// HTTPS / HTTP / SSH-over-URL (`https://github.com/owner/name(.git)?`) に対応。
/// それ以外のホスト（GHES など）や `owner/name` の形に解釈できないものは None。
fn parse_github_remote(url: &str) -> Option<String> {
    let url = url.strip_suffix(".git").unwrap_or(url);
    const PREFIXES: &[&str] = &[
        "git@github.com:",
        "ssh://git@github.com/",
        "https://github.com/",
        "http://github.com/",
    ];
    for prefix in PREFIXES {
        if let Some(rest) = url.strip_prefix(prefix) {
            let path = rest.trim_end_matches('/');
            return parse_owner_name(path);
        }
    }
    None
}

fn parse_owner_name(s: &str) -> Option<String> {
    let (owner, name) = s.split_once('/')?;
    if owner.is_empty() || name.is_empty() || name.contains('/') {
        return None;
    }
    Some(format!("{owner}/{name}"))
}

/// `--repo owner/name` / `-R owner/name` / `--repo=owner/name` を拾う。
/// argv 内に明示されていなければ None。
pub fn detect_repo(argv: &[String]) -> Option<String> {
    let mut iter = argv.iter();
    while let Some(a) = iter.next() {
        if a == "--repo" || a == "-R" {
            return iter.next().cloned();
        }
        if let Some(rest) = a.strip_prefix("--repo=") {
            return Some(rest.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().copied().map(String::from).collect()
    }

    #[test]
    fn detect_repo_variants() {
        assert_eq!(
            detect_repo(&argv(&["issue", "view", "1", "--repo", "cli/cli"])),
            Some("cli/cli".into())
        );
        assert_eq!(
            detect_repo(&argv(&["issue", "view", "1", "-R", "cli/cli"])),
            Some("cli/cli".into())
        );
        assert_eq!(
            detect_repo(&argv(&["issue", "view", "1", "--repo=cli/cli"])),
            Some("cli/cli".into())
        );
        assert_eq!(detect_repo(&argv(&["issue", "view", "1"])), None);
    }

    #[test]
    fn same_argv_same_key() {
        let a = cache_key(&argv(&["issue", "view", "1"]));
        let b = cache_key(&argv(&["issue", "view", "1"]));
        assert_eq!(a, b);
    }

    #[test]
    fn different_argv_different_key() {
        let a = cache_key(&argv(&["issue", "view", "1"]));
        let b = cache_key(&argv(&["issue", "view", "2"]));
        assert_ne!(a, b);
    }

    #[test]
    fn cache_key_with_cwd_is_cwd_sensitive() {
        let av = argv(&["issue", "view", "1"]);
        // 同じ cwd + 同じ argv なら一致
        assert_eq!(
            cache_key_with_cwd("/tmp/x", &av),
            cache_key_with_cwd("/tmp/x", &av)
        );
        // cwd が違えばキーも違う（daemon が誤った cwd で当てないことの担保）
        assert_ne!(
            cache_key_with_cwd("/tmp/x", &av),
            cache_key_with_cwd("/tmp/y", &av)
        );
    }

    // argv の「区切り位置」が違えば別キーになること。セパレータが効いていないと
    // ["issue","view1"] と ["issue","view","1"] が衝突し別レスポンスを取り違える。
    // 参考: sccache はキャッシュキー衝突を厳密に検証する。
    #[test]
    fn argv_boundary_does_not_collide() {
        assert_ne!(
            cache_key(&argv(&["issue", "view", "1"])),
            cache_key(&argv(&["issue", "view1"])),
        );
        assert_ne!(
            cache_key(&argv(&["ab", "c"])),
            cache_key(&argv(&["a", "bc"])),
        );
        // 結合して 1 引数にしたものとも衝突しない
        assert_ne!(
            cache_key(&argv(&["issue", "list"])),
            cache_key(&argv(&["issuelist"])),
        );
    }

    // 空 argv（ch を引数なしで叩いた相当）でもパニックせず安定したキーを返すこと。
    #[test]
    fn empty_argv_key_is_stable() {
        let a = cache_key(&argv(&[]));
        let b = cache_key(&argv(&[]));
        assert_eq!(a, b);
        assert!(!a.is_empty());
    }

    // `--repo` が値を伴わず末尾に来てもパニックせず None を返すこと（境界値）。
    #[test]
    fn detect_repo_trailing_flag_without_value() {
        assert_eq!(detect_repo(&argv(&["issue", "view", "--repo"])), None);
        assert_eq!(detect_repo(&argv(&["issue", "view", "-R"])), None);
    }

    #[test]
    fn parse_github_remote_ssh_and_https() {
        // SSH 形式（.git あり / 無し）
        assert_eq!(
            parse_github_remote("git@github.com:cli/cli.git"),
            Some("cli/cli".into())
        );
        assert_eq!(
            parse_github_remote("git@github.com:cli/cli"),
            Some("cli/cli".into())
        );
        // HTTPS / HTTP
        assert_eq!(
            parse_github_remote("https://github.com/cli/cli.git"),
            Some("cli/cli".into())
        );
        assert_eq!(
            parse_github_remote("http://github.com/cli/cli"),
            Some("cli/cli".into())
        );
        // ssh:// 形式
        assert_eq!(
            parse_github_remote("ssh://git@github.com/cli/cli.git"),
            Some("cli/cli".into())
        );
        // 末尾スラッシュは許容
        assert_eq!(
            parse_github_remote("https://github.com/cli/cli/"),
            Some("cli/cli".into())
        );
    }

    #[test]
    fn parse_github_remote_rejects_non_github() {
        // GHES / GitLab / 不明形式は None（焼き付け事故を避ける）
        assert_eq!(parse_github_remote("git@gitlab.com:foo/bar.git"), None);
        assert_eq!(parse_github_remote("https://ghe.example.com/foo/bar"), None);
        assert_eq!(parse_github_remote("totally-not-a-url"), None);
        assert_eq!(parse_github_remote(""), None);
        // owner/name に解釈できない形（subgroup や空 owner）
        assert_eq!(parse_github_remote("https://github.com/foo"), None);
        assert_eq!(parse_github_remote("https://github.com/foo/bar/baz"), None);
        assert_eq!(parse_github_remote("https://github.com//bar"), None);
    }

    #[test]
    fn augment_argv_noop_when_repo_already_present() {
        // --repo / -R / --repo= のどれかが既にあれば resolver は呼ばない（panic で検出）
        let raw = argv(&["issue", "view", "1", "--repo", "cli/cli"]);
        let got = augment_argv_with_resolver(&raw, || panic!("resolver should not run"));
        assert_eq!(got, raw);

        let raw = argv(&["issue", "view", "1", "-R", "cli/cli"]);
        let got = augment_argv_with_resolver(&raw, || panic!("resolver should not run"));
        assert_eq!(got, raw);

        let raw = argv(&["issue", "view", "1", "--repo=cli/cli"]);
        let got = augment_argv_with_resolver(&raw, || panic!("resolver should not run"));
        assert_eq!(got, raw);
    }

    #[test]
    fn augment_argv_appends_repo_when_resolver_succeeds() {
        let raw = argv(&["issue", "view", "1"]);
        let got = augment_argv_with_resolver(&raw, || Some("cli/cli".into()));
        assert_eq!(got, argv(&["issue", "view", "1", "--repo", "cli/cli"]));
        // 元の argv は変更しない（borrow を返さない）
        assert_eq!(raw, argv(&["issue", "view", "1"]));
    }

    #[test]
    fn augment_argv_falls_back_when_resolver_returns_none() {
        // git remote が無い / GitHub 以外 → argv そのまま（従来挙動を保つ）
        let raw = argv(&["issue", "view", "1"]);
        let got = augment_argv_with_resolver(&raw, || None);
        assert_eq!(got, raw);
    }

    // augment 後の argv が detect_repo で取り出せること（往復不変）。
    // ここが崩れると `cache.repo` が NULL のままになり Warmer に乗らない（issue #20 の本筋）。
    #[test]
    fn augment_argv_result_is_detected_back() {
        let raw = argv(&["issue", "view", "1"]);
        let got = augment_argv_with_resolver(&raw, || Some("cli/cli".into()));
        assert_eq!(detect_repo(&got), Some("cli/cli".into()));
    }
}
