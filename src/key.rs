// キャッシュキー生成。
//
// v0.1 では「同じ場所 (cwd) で同じ argv を叩いたら同じキー」とする雑な版。
// 認証ユーザの混入対策は v0.1.1 以降で `gh auth status` の login を混ぜる予定。

use std::env;

pub fn cache_key(argv: &[String]) -> String {
    let cwd = env::current_dir()
        .ok()
        .map(|p| p.display().to_string())
        .unwrap_or_default();

    let mut hasher = blake3::Hasher::new();
    hasher.update(cwd.as_bytes());
    hasher.update(b"\x1f");
    for a in argv {
        hasher.update(a.as_bytes());
        hasher.update(b"\x1f");
    }
    hasher.finalize().to_hex().to_string()
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
}
