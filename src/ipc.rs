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

#[cfg(test)]
mod tests {
    use super::*;

    // ch と chd は別バイナリ起動なので Message の wire 形式が両者で一致している必要がある。
    // daemon 側は serde_json::from_str で 1 行ずつ受けるため、round-trip が壊れると
    // 全メッセージが parse error で黙って捨てられる。
    #[test]
    fn message_json_roundtrip_all_variants() {
        let cases = vec![
            Message::Refresh {
                argv: vec!["issue".into(), "view".into(), "1".into()],
                cache_kind: "issue_view".into(),
                ttl_secs: 60,
                cache_key: "abc123".into(),
            },
            Message::AsyncExec {
                argv: vec!["issue".into(), "close".into(), "1".into()],
            },
            Message::Ping,
            Message::Stop,
        ];
        for msg in cases {
            let line = serde_json::to_string(&msg).expect("serialize");
            // tag=kind なので必ず "kind" フィールドを持つ
            assert!(line.contains("\"kind\""), "tag が欠落: {line}");
            // 改行を含まない（JSON Lines プロトコルの前提）
            assert!(!line.contains('\n'), "1 メッセージ 1 行のはず: {line}");
            // deserialize → 再 serialize で安定（往復で形が変わらない）
            let back: Message = serde_json::from_str(&line).expect("deserialize");
            assert_eq!(serde_json::to_string(&back).unwrap(), line);
        }
    }

    // Refresh は tag 名 "kind" と中身の "cache_kind" が別物として共存できること。
    #[test]
    fn refresh_tag_and_cache_kind_coexist() {
        let line = serde_json::to_string(&Message::Refresh {
            argv: vec![],
            cache_kind: "pr_list".into(),
            ttl_secs: 30,
            cache_key: "k".into(),
        })
        .unwrap();
        let back: Message = serde_json::from_str(&line).unwrap();
        match back {
            Message::Refresh {
                cache_kind, ttl_secs, ..
            } => {
                assert_eq!(cache_kind, "pr_list");
                assert_eq!(ttl_secs, 30);
            }
            _ => panic!("Refresh として復元されるべき"),
        }
    }

    // 壊れた行は from_str が Err を返す（daemon が握り潰せる前提）。
    #[test]
    fn malformed_line_is_rejected() {
        assert!(serde_json::from_str::<Message>("{not json}").is_err());
        assert!(serde_json::from_str::<Message>("{\"kind\":\"Unknown\"}").is_err());
    }
}
