// gh の起動経路。Read / Write / Passthrough の 3 種類。
//
//   - passthrough: stdin/stdout/stderr 完全継承。エディタ起動も生きる
//   - handle_read: ヒットなら body をそのまま流す。miss/期限切れは gh exec → SQLite に保存
//   - handle_write: passthrough + 終了コード 0 のときだけ invalidate を走らせる

use anyhow::{Context, Result};
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::invalidate;
use crate::key;
use crate::store::{Entry, Store};

/// gh を stdio 完全透過で起動する。終了コードを返す。
pub fn passthrough(argv: &[String]) -> Result<i32> {
    let status = Command::new("gh")
        .args(argv)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("gh を起動できなかった（PATH に gh はある？）")?;
    Ok(status.code().unwrap_or(1))
}

/// Read 経路。
///
/// v0.1 では SWR は実装せず、期限切れも miss と同じ扱いにする（同期 fetch）。
/// SWR は v0.1.1 で追加予定。
pub fn handle_read(store: &Store, argv: &[String], kind: &'static str, ttl: u64) -> Result<i32> {
    let k = key::cache_key(argv);
    let now = epoch_secs();

    if let Some(entry) = store.get(&k)? {
        if now.saturating_sub(entry.fetched_at) < entry.ttl_secs {
            // fresh: 即返却
            std::io::stdout().write_all(&entry.body)?;
            store.bump_hit(&k)?;
            return Ok(0);
        }
    }

    // miss / 期限切れ: gh を同期 exec して stdout を捕まえる
    let output = Command::new("gh")
        .args(argv)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .output()
        .context("gh を起動できなかった（PATH に gh はある？）")?;

    let code = output.status.code().unwrap_or(1);

    // 4xx/5xx 相当はキャッシュしない（特に rate limit の 403 を焼き付けない）
    if code == 0 {
        let entry = Entry {
            argv_json: serde_json::to_string(argv).unwrap_or_default(),
            kind: kind.to_string(),
            repo: key::detect_repo(argv),
            body: output.stdout.clone(),
            fetched_at: now,
            ttl_secs: ttl,
        };
        store.put(&k, &entry)?;
    }

    std::io::stdout().write_all(&output.stdout)?;
    Ok(code)
}

/// Write 経路。gh を透過で実行し、成功時のみ invalidate。
pub fn handle_write(store: &Store, argv: &[String]) -> Result<i32> {
    let code = passthrough(argv)?;
    if code == 0 {
        invalidate::run(store, argv)?;
    }
    Ok(code)
}

fn epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
