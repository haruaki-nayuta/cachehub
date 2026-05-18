// chd: 常駐デーモン本体。
//
// 同一バイナリで `ch --daemon` として起動される（busybox 方式）。
// 役割（v0.2 時点）:
//   - Unix domain socket を listen し、JSON Lines の Message を受け取る
//   - Refresh メッセージ: gh を別スレッドで実行して cache を上書き（SWR の裏更新）
//   - Stop メッセージ: socket file を削除して exit
//   - Ping: 何もしない（liveness 用）
//
// v0.3 以降の Events ポーリング・連想プリフェッチを足す土台でもある。

use anyhow::{Context, Result};
use std::io::{BufRead, BufReader};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crate::exec;
use crate::ipc::{is_alive, socket_path, Message};

/// daemon 本体。`--daemon` で呼ばれる長寿命プロセス。
pub fn run() -> Result<()> {
    let sock = socket_path()?;
    if let Some(parent) = sock.parent() {
        std::fs::create_dir_all(parent).ok();
    }

    let listener = bind_with_recovery(&sock)?;
    chmod_0600(&sock).ok();

    eprintln!("chd: 起動 (socket={})", sock.display());

    let stop_flag = Arc::new(AtomicBool::new(false));

    // accept ループ。Stop が来たら non-blocking で抜ける
    for stream in listener.incoming() {
        if stop_flag.load(Ordering::SeqCst) {
            break;
        }
        match stream {
            Ok(s) => {
                let flag = stop_flag.clone();
                thread::spawn(move || {
                    if let Err(e) = handle_client(s, flag) {
                        eprintln!("chd: client error: {e:#}");
                    }
                });
            }
            Err(e) => {
                eprintln!("chd: accept error: {e}");
            }
        }
    }

    // 後片付け
    let _ = std::fs::remove_file(&sock);
    eprintln!("chd: 終了");
    Ok(())
}

/// bind に失敗したら：
///   - 既に生きている daemon があれば「自分は引き下がる」
///   - 古い socket file が残っているだけなら削除して再 bind
fn bind_with_recovery(sock: &PathBuf) -> Result<UnixListener> {
    match UnixListener::bind(sock) {
        Ok(l) => Ok(l),
        Err(_) => {
            if is_alive() {
                anyhow::bail!("既に chd が動いている (socket={})", sock.display());
            }
            // socket file が古いだけ → 削除して再挑戦
            let _ = std::fs::remove_file(sock);
            UnixListener::bind(sock)
                .with_context(|| format!("socket bind 失敗: {}", sock.display()))
        }
    }
}

fn chmod_0600(p: &PathBuf) -> Result<()> {
    let perms = std::fs::Permissions::from_mode(0o600);
    std::fs::set_permissions(p, perms)?;
    Ok(())
}

fn handle_client(stream: UnixStream, stop_flag: Arc<AtomicBool>) -> Result<()> {
    // 読み込みが詰まらないように short timeout（fire-and-forget 前提）
    stream.set_read_timeout(Some(Duration::from_secs(2))).ok();
    let reader = BufReader::new(stream);
    for line in reader.lines() {
        let line = line?;
        let msg: Message = match serde_json::from_str(&line) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("chd: parse error: {e} (line={line:?})");
                continue;
            }
        };
        process(msg, &stop_flag);
    }
    Ok(())
}

fn process(msg: Message, stop_flag: &Arc<AtomicBool>) {
    match msg {
        Message::Refresh {
            argv,
            cache_kind,
            ttl_secs,
            cache_key,
        } => {
            // ワーカースレッドに投げて IPC ハンドラはすぐ戻す
            thread::spawn(move || {
                if let Err(e) = exec::refresh_into_cache(&argv, &cache_kind, ttl_secs, &cache_key) {
                    eprintln!("chd: refresh 失敗: {e:#}");
                }
            });
        }
        Message::Ping => {
            // liveness 用。何もしない
        }
        Message::Stop => {
            eprintln!("chd: Stop を受信、終了します");
            stop_flag.store(true, Ordering::SeqCst);
            // accept でブロックされているメインを起こすために自分宛にダミー接続
            if let Ok(p) = socket_path() {
                let _ = UnixStream::connect(&p);
            }
        }
    }
}

/// `ch` 側から呼ぶ：socket が無ければ自分自身を `--daemon` モードで spawn する。
/// fire-and-forget。立ち上がりを待たないので「次回叩いたとき」にちゃんと使えていれば OK。
pub fn ensure_running() {
    if is_alive() {
        return;
    }
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(_) => return,
    };
    use std::os::unix::process::CommandExt;
    let _ = std::process::Command::new(&exe)
        .arg("--daemon")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .process_group(0) // 自分のプロセスグループに移し、親が死んでも生かす
        .spawn();
}
