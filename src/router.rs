// argv を Read / Write / Passthrough に分類する。
//
// v0.1 の方針:
//   - Read whitelist だけ明示する。kind と TTL を返す
//   - 既知の Write verb は Write に倒す（gh を透過実行して invalidate）
//   - それ以外は安全側で Passthrough（未知のサブコマンドは ch を素通り）

pub enum Action {
    /// キャッシュ参照対象。kind は invalidate / stats と紐づくラベル
    Read { kind: &'static str, ttl: u64 },
    /// gh を stdio 透過で実行し、成功時に関連キーを drop する
    Write,
    /// 何もせず gh に丸投げ
    Passthrough,
}

pub fn classify(argv: &[String]) -> Action {
    let s: Vec<&str> = argv.iter().map(String::as_str).collect();

    // Read 系: Some(kind) を返す関数で一発判定
    if let Some((kind, ttl)) = match_read(&s) {
        return Action::Read { kind, ttl };
    }

    // Write 系: 既知の破壊的 verb のとき
    if is_write(&s) {
        return Action::Write;
    }

    // それ以外: 未知サブコマンドや読み取りでも whitelist 外のものは素通し
    Action::Passthrough
}

fn match_read(s: &[&str]) -> Option<(&'static str, u64)> {
    match s {
        ["issue", "list", ..] => Some(("issue_list", 30)),
        ["issue", "view", ..] => Some(("issue_view", 60)),
        ["pr", "list", ..] => Some(("pr_list", 30)),
        ["pr", "view", ..] => Some(("pr_view", 60)),
        ["repo", "view", ..] => Some(("repo_view", 3600)),
        _ => None,
    }
}

fn is_write(s: &[&str]) -> bool {
    match s {
        ["issue", v, ..] => matches!(
            *v,
            "close"
                | "reopen"
                | "edit"
                | "comment"
                | "delete"
                | "create"
                | "pin"
                | "unpin"
                | "lock"
                | "unlock"
                | "transfer"
                | "develop"
        ),
        ["pr", v, ..] => matches!(
            *v,
            "close"
                | "reopen"
                | "edit"
                | "merge"
                | "create"
                | "comment"
                | "review"
                | "ready"
                | "lock"
                | "unlock"
                | "update-branch"
        ),
        ["repo", v, ..] => matches!(
            *v,
            "edit" | "create" | "delete" | "rename" | "archive" | "unarchive" | "fork"
        ),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().copied().map(String::from).collect()
    }

    #[test]
    fn read_whitelist_hits() {
        assert!(matches!(
            classify(&argv(&["issue", "view", "123"])),
            Action::Read {
                kind: "issue_view",
                ttl: 60
            }
        ));
        assert!(matches!(
            classify(&argv(&["pr", "list", "--state", "open"])),
            Action::Read {
                kind: "pr_list",
                ttl: 30
            }
        ));
        assert!(matches!(
            classify(&argv(&["repo", "view", "cli/cli"])),
            Action::Read {
                kind: "repo_view",
                ttl: 3600
            }
        ));
    }

    #[test]
    fn write_verbs_detected() {
        assert!(matches!(
            classify(&argv(&["issue", "close", "1"])),
            Action::Write
        ));
        assert!(matches!(
            classify(&argv(&["pr", "merge", "42"])),
            Action::Write
        ));
        assert!(matches!(classify(&argv(&["pr", "create"])), Action::Write));
        assert!(matches!(classify(&argv(&["repo", "edit"])), Action::Write));
    }

    #[test]
    fn unknown_subcommands_passthrough() {
        // gist は whitelist にない → 安全側に倒して素通し
        assert!(matches!(
            classify(&argv(&["gist", "create"])),
            Action::Passthrough
        ));
        assert!(matches!(
            classify(&argv(&["api", "/user"])),
            Action::Passthrough
        ));
        // run はまだ未対応（v0.2 で追加予定）
        assert!(matches!(
            classify(&argv(&["run", "list"])),
            Action::Passthrough
        ));
    }
}
