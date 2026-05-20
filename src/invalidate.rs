// Write 成功後に関連キャッシュを drop して、write-through 用の再取得対象を返す。
//
// v0.2 の方針:
//   - argv のトップレベル名詞（issue / pr / repo）から、影響範囲の kind を決め打ち
//   - --repo が argv にあればその repo + NULL repo に限定、無ければ kind 全体を drop
//     （NULL は「argv からは repo を読めなかった = だいたい cwd のリポ」のつもり）
//   - drop する前に対象 entry の argv_json を控えておき、呼び出し側が gh を呼び直して
//     キャッシュを埋め直せるようにする（=「次の Read で gh を 1 回節約」）
//   - drop の前に取るのは、refresh が失敗したときに stale が残らないようにするため
//     （refresh 成功 = 上書き、refresh 失敗 = miss にフォールバック、で整合性側に倒す）

use anyhow::Result;

use crate::key;
use crate::store::{RefreshTarget, Store};

/// Write 系 argv に対して、(a) 影響範囲の kind を drop し、(b) drop した行の
/// 再取得情報を返す。
///
/// 未知の Write が来た場合は何もせず空 Vec を返す（呼び出し側が is_write=true の
/// ときだけここに来る前提なので通常は走らない）。
pub fn run(store: &Store, argv: &[String]) -> Result<Vec<RefreshTarget>> {
    let s: Vec<&str> = argv.iter().map(String::as_str).collect();
    let repo = key::detect_repo(argv);
    let repo_ref = repo.as_deref();

    let kinds: &[&str] = match s.as_slice() {
        ["issue", ..] => &["issue_view", "issue_list"],
        ["pr", ..] => &["pr_view", "pr_list"],
        ["repo", ..] => &["repo_view"],
        _ => &[],
    };

    // まず再取得対象を控える
    let mut targets = Vec::new();
    for kind in kinds {
        targets.extend(store.list_refresh_targets(kind, repo_ref)?);
    }

    // それから drop（drop 順は kind だけなので順序は意味を持たない）
    for kind in kinds {
        store.drop_by_kind(kind, repo_ref)?;
    }

    Ok(targets)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Entry;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().copied().map(String::from).collect()
    }

    fn open_tmp_store() -> Store {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "ch-invalidate-test-{}-{}.db",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        Store::open(&path).expect("open")
    }

    fn entry(kind: &str, repo: Option<&str>, argv_json: &str) -> Entry {
        Entry {
            argv_json: argv_json.into(),
            kind: kind.into(),
            repo: repo.map(Into::into),
            body: b"x".to_vec(),
            fetched_at: 0,
            ttl_secs: 60,
        }
    }

    #[test]
    fn issue_write_drops_and_returns_view_and_list_targets() {
        let s = open_tmp_store();
        s.put("kv", &entry("issue_view", Some("a/b"), "[\"issue\",\"view\",\"1\"]"))
            .unwrap();
        s.put(
            "kl",
            &entry("issue_list", Some("a/b"), "[\"issue\",\"list\"]"),
        )
        .unwrap();
        // 別 repo は触らない
        s.put(
            "kx",
            &entry("issue_view", Some("c/d"), "[\"issue\",\"view\",\"9\"]"),
        )
        .unwrap();

        let targets = run(&s, &argv(&["issue", "close", "1", "--repo", "a/b"])).unwrap();

        assert_eq!(targets.len(), 2, "view + list で 2 件返るはず");
        assert!(targets.iter().any(|t| t.kind == "issue_view"));
        assert!(targets.iter().any(|t| t.kind == "issue_list"));

        // drop 済み
        assert!(s.get("kv").unwrap().is_none());
        assert!(s.get("kl").unwrap().is_none());
        // 別 repo は残ってる
        assert!(s.get("kx").unwrap().is_some());
    }

    #[test]
    fn pr_write_targets_pr_kinds() {
        let s = open_tmp_store();
        s.put(
            "p1",
            &entry("pr_view", Some("a/b"), "[\"pr\",\"view\",\"42\"]"),
        )
        .unwrap();
        s.put("p2", &entry("issue_view", Some("a/b"), "[\"issue\",\"view\",\"1\"]"))
            .unwrap();

        let targets = run(&s, &argv(&["pr", "merge", "42", "--repo", "a/b"])).unwrap();

        // pr_view は対象、issue_view は対象外
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].kind, "pr_view");
        // pr は dropped、issue は残る
        assert!(s.get("p1").unwrap().is_none());
        assert!(s.get("p2").unwrap().is_some());
    }

    #[test]
    fn unknown_write_returns_empty_and_changes_nothing() {
        let s = open_tmp_store();
        s.put("k", &entry("issue_view", None, "[]")).unwrap();
        let targets = run(&s, &argv(&["gist", "create"])).unwrap();
        assert!(targets.is_empty());
        assert!(s.get("k").unwrap().is_some());
    }
}
