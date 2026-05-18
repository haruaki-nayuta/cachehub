// Write 成功後に関連キャッシュを drop する。
//
// v0.1 の方針:
//   - argv のトップレベル名詞（issue / pr / repo）から、影響範囲の kind を決め打ちで drop
//   - --repo が argv にあればその repo + NULL repo に限定、無ければ kind 全体を drop
//     （NULL は「argv からは repo を読めなかった = だいたい cwd のリポ」のつもり）
//   - 保守的に過剰 drop でも良い。整合性を優先する

use anyhow::Result;

use crate::key;
use crate::store::Store;

pub fn run(store: &Store, argv: &[String]) -> Result<()> {
    let s: Vec<&str> = argv.iter().map(|x| x.as_str()).collect();
    let repo = key::detect_repo(argv);
    let repo_ref = repo.as_deref();

    match s.as_slice() {
        ["issue", ..] => {
            store.drop_by_kind("issue_view", repo_ref)?;
            store.drop_by_kind("issue_list", repo_ref)?;
        }
        ["pr", ..] => {
            store.drop_by_kind("pr_view", repo_ref)?;
            store.drop_by_kind("pr_list", repo_ref)?;
        }
        ["repo", ..] => {
            store.drop_by_kind("repo_view", repo_ref)?;
        }
        _ => {
            // 未知の Write が来たら何もしない（呼び出し側で is_write が true のときだけ通る想定）
        }
    }
    Ok(())
}
