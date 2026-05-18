// 内部サブコマンド `ch daemon ...`。
//
//   - `ch daemon status` : 生存確認 + アクティブリポ一覧
//   - `ch daemon stop`   : IPC で Stop を送る（fire-and-forget）
//   - `ch daemon start`  : 明示的に立ち上げたいとき用（通常は ch 起動時に auto-spawn）

use anyhow::Result;

use crate::daemon;
use crate::exec;
use crate::ipc::{self, Message};
use crate::store::Store;

pub fn handle(args: &[String]) -> Result<i32> {
    match args.first().map(|s| s.as_str()) {
        Some("status") => status(),
        Some("stop") => stop(),
        Some("start") => start(),
        Some(other) => {
            eprintln!("ch: 未知のサブコマンド: daemon {other}");
            eprintln!("使い方: ch daemon <status|start|stop>");
            Ok(2)
        }
        None => {
            eprintln!("使い方: ch daemon <status|start|stop>");
            Ok(2)
        }
    }
}

fn status() -> Result<i32> {
    let sock = ipc::socket_path()?;
    let alive = ipc::is_alive();
    println!("socket : {}", sock.display());
    println!("状態   : {}", if alive { "起動中" } else { "停止中" });

    let store = Store::open_default()?;
    // §6.B のしきい値：直近 72h
    let active = store.active_repos(72 * 3600, exec::epoch_secs())?;
    println!("アクティブリポジトリ ({} 件, 直近72h):", active.len());
    for (repo, last_used) in active.iter().take(20) {
        let age_secs = exec::epoch_secs().saturating_sub(*last_used);
        println!("  {:<60}  ({}s 前)", repo, age_secs);
    }
    if active.len() > 20 {
        println!("  ... 他 {} 件", active.len() - 20);
    }
    Ok(0)
}

fn stop() -> Result<i32> {
    if !ipc::is_alive() {
        println!("chd は動いていません");
        return Ok(0);
    }
    if ipc::try_send(&Message::Stop) {
        println!("Stop を送信しました");
        Ok(0)
    } else {
        eprintln!("Stop の送信に失敗しました");
        Ok(1)
    }
}

fn start() -> Result<i32> {
    if ipc::is_alive() {
        println!("既に起動しています");
        return Ok(0);
    }
    daemon::ensure_running();
    // 起動完了を最大 500ms 待つ
    for _ in 0..25 {
        if ipc::is_alive() {
            println!("起動しました");
            return Ok(0);
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    eprintln!("起動を確認できませんでした（log を確認してください）");
    Ok(1)
}
