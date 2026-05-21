// `ch` (CLI) と `chd` (デーモン) を結ぶ Unix domain socket の IPC。
//
// プロトコル: 1 メッセージ = 1 行の JSON Lines。fire-and-forget。
// ch 側は書いたら即終了する。受信側のレスポンスは無い。
//
// 重い処理は全部 chd 側で完結させるのでメッセージは極小（spec §7.3）。

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

/// chd に送るメッセージ。tag=kind で区別する典型的な enum 形。
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "kind")]
pub enum Message {
    /// SWR の裏更新依頼。chd が gh を叩いて cache を上書きする。
    Refresh {
        argv: Vec<String>,
        cache_kind: String,
        ttl_secs: u64,
        cache_key: String,
    },
    /// async_passthrough モードでの fire-and-forget な gh 実行依頼。
    /// chd が gh を実行し、失敗時のみ exec_errors に記録する。
    /// Write 系で成功した場合は cache invalidate もデーモン側で走らせる。
    AsyncExec { argv: Vec<String> },
    /// issue list を起点にした連想プリフェッチ依頼。
    /// chd が issue 番号を取り直し、各 `gh issue view` を裏で温める。
    /// `cwd` はユーザが `ch` を叩いた作業ディレクトリ。gh のリポジトリ解決と
    /// cache key の両方で「ユーザ視点の cwd」を再現するために必要。
    PrefetchIssues { list_argv: Vec<String>, cwd: String },
    /// 生存確認。daemon は何もしないで読み捨てる。
    Ping,
    /// `ch daemon stop` から送られる。daemon は素直に exit する。
    Stop,
}

/// socket の置き場所。`$CH_SOCK_PATH` で上書き可能（テストで便利）。
pub fn socket_path() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("CH_SOCK_PATH") {
        return Ok(PathBuf::from(p));
    }
    let home = std::env::var("HOME").context("HOME が未設定")?;
    Ok(PathBuf::from(home).join(".cache/ch/sock"))
}

/// 1 メッセージ送って即 close。connect に失敗したら Err を返す（fallback 判断に使う）。
pub fn send(msg: &Message) -> Result<()> {
    let path = socket_path()?;
    let mut stream = UnixStream::connect(&path)
        .with_context(|| format!("daemon に繋がらない: {}", path.display()))?;
    // 書き込みが詰まると ch のレイテンシを潰すので短めの timeout。
    // AsyncExec で長い argv (--body "<長文>" 等) を流すケースのため Refresh より少し長めに取る。
    stream.set_write_timeout(Some(Duration::from_millis(500)))?;
    let mut line = serde_json::to_vec(msg)?;
    line.push(b'\n');
    stream.write_all(&line)?;
    // shutdown は drop に任せる
    Ok(())
}

/// 失敗を握り潰して bool で返す薄いラッパ（fire-and-forget 用途）。
pub fn try_send(msg: &Message) -> bool {
    send(msg).is_ok()
}

/// daemon が生きているか確認する。Ping を送って成功すれば生きている。
pub fn is_alive() -> bool {
    try_send(&Message::Ping)
}
