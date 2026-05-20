// gh の起動経路。Read / Write / Passthrough の 3 種類 + SWR 用の refresh worker。
//
//   - passthrough        : stdin/stdout/stderr 完全継承。エディタ起動も生きる
//   - handle_read        : fresh = 即返却。stale = 古い body を即返して裏で refresh を kick
//                          miss = 同期 gh + 保存。
//   - handle_write       : passthrough + 終了コード 0 のときだけ invalidate。
//                          drop した行は write-through で daemon に再取得を投げる
//   - refresh_into_cache : chd / detached subprocess から呼ばれる裏更新本体

use anyhow::{Context, Result};
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::Config;
use crate::daemon;
use crate::invalidate;
use crate::ipc::{self, Message};
use crate::key;
use crate::router;
use crate::store::{Entry, RefreshTarget, Store};

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

/// Read 経路（SWR 対応）。
///
///   fresh hit  → body を即返す
///   stale hit  → body を即返してから refresh を裏で kick（IPC → fallback で detached subprocess）
///   miss       → gh を同期実行して保存
pub fn handle_read(store: &Store, argv: &[String], kind: &'static str, ttl: u64) -> Result<i32> {
    let k = key::cache_key(argv);
    let now = epoch_secs();

    // アクティブリポジトリ LRU の更新（spec §6.B）
    mark_active(store, argv, now).ok();

    if let Some(entry) = store.get(&k)? {
        let age = now.saturating_sub(entry.fetched_at);
        if age < entry.ttl_secs {
            // fresh hit
            std::io::stdout().write_all(&entry.body)?;
            store.bump_hit(&k)?;
            return Ok(0);
        }

        // stale: 先に古いやつを返して、裏で更新を走らせる
        std::io::stdout().write_all(&entry.body)?;
        store.bump_hit(&k)?;
        kick_background_refresh(argv, kind, ttl, &k);
        return Ok(0);
    }

    // miss: 同期で gh を呼ぶ
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
        let entry = build_entry(argv, kind, output.stdout.clone(), now, ttl);
        store.put(&k, &entry)?;
    }

    std::io::stdout().write_all(&output.stdout)?;
    Ok(code)
}

/// Read 経路で gh の出力からキャッシュ用 Entry を組み立てる。
/// 同じ形を refresh worker でも使うので 1 か所に寄せておく。
fn build_entry(argv: &[String], kind: &str, body: Vec<u8>, fetched_at: u64, ttl_secs: u64) -> Entry {
    Entry {
        argv_json: serde_json::to_string(argv).unwrap_or_default(),
        kind: kind.to_string(),
        repo: key::detect_repo(argv),
        body,
        fetched_at,
        ttl_secs,
    }
}

/// Write 経路。gh を透過で実行し、成功時のみ invalidate。
/// config.async_passthrough が true なら daemon に投げて即 0 を返す（fire-and-forget）。
pub fn handle_write(store: &Store, argv: &[String], cfg: &Config) -> Result<i32> {
    // Write も「触ったリポ」なので LRU を更新
    let now = epoch_secs();
    mark_active(store, argv, now).ok();

    if cfg.async_passthrough && offload_to_daemon(argv) {
        return Ok(0);
    }

    let code = passthrough(argv)?;
    if code == 0 {
        // 1) 関連 cache を drop しつつ、write-through で再取得したい argv を集める
        let targets = invalidate::run(store, argv)?;
        // 2) 集めた argv を daemon に投げて非同期に gh で取り直す（fire-and-forget）
        for t in targets {
            kick_write_through_refresh(&t);
        }
    }
    Ok(code)
}

/// 通常 Passthrough 経路。config.async_passthrough が true なら daemon に投げて即 0 を返す。
pub fn handle_passthrough(argv: &[String], cfg: &Config) -> Result<i32> {
    if cfg.async_passthrough && offload_to_daemon(argv) {
        return Ok(0);
    }
    passthrough(argv)
}

/// daemon に AsyncExec を投げる。daemon が居なければ false を返してフォールバック判断に使う。
fn offload_to_daemon(argv: &[String]) -> bool {
    daemon::ensure_running();
    ipc::try_send(&Message::AsyncExec {
        argv: argv.to_vec(),
    })
}

/// daemon 側の AsyncExec ワーカー本体。
///
/// gh を実行し、
///   - exit_code != 0 なら stdout/stderr ごと exec_errors に積む
///   - exit_code == 0 で argv が Write 系なら cache を invalidate する
pub fn run_async_exec(argv: &[String]) -> Result<()> {
    let output = Command::new("gh")
        .args(argv)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .context("gh を起動できなかった (async exec)")?;

    let code = output.status.code().unwrap_or(1);
    let store = Store::open_default()?;

    if code != 0 {
        let argv_json = serde_json::to_string(argv).unwrap_or_default();
        store.log_exec_error(
            &argv_json,
            code,
            &output.stdout,
            &output.stderr,
            epoch_secs(),
        )?;
        return Ok(());
    }

    // 成功時: Write 系なら cache を吹き飛ばす（Passthrough は何が変わったか分からないので触らない）
    if matches!(router::classify(argv), router::Action::Write) {
        let targets = invalidate::run(&store, argv)?;
        // daemon 内なので IPC を経由せず、refresh は別スレッドで直接走らせる
        for t in targets {
            spawn_local_refresh(t);
        }
    }
    Ok(())
}

/// SWR の裏更新本体。
///
/// chd のワーカースレッドから直接呼ばれる経路と、
/// `ch --refresh ...` のフォールバック subprocess から呼ばれる経路、両方の出口。
/// gh を同期実行し、終了コード 0 なら cache を上書きする。
pub fn refresh_into_cache(
    argv: &[String],
    kind: &str,
    ttl_secs: u64,
    cache_key: &str,
) -> Result<()> {
    let output = Command::new("gh")
        .args(argv)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .context("gh を起動できなかった（refresh worker）")?;

    if output.status.code() != Some(0) {
        // 非ゼロ終了はキャッシュしない。stale を残したほうがマシ
        return Ok(());
    }

    let store = Store::open_default()?;
    let entry = build_entry(argv, kind, output.stdout, epoch_secs(), ttl_secs);
    store.put(cache_key, &entry)?;
    Ok(())
}

/// 裏更新を走らせる：
///   1) chd に IPC で投げる（fire-and-forget）
///   2) IPC が失敗したら自分のコピーを `--refresh` で detached spawn
///   3) ついでに daemon を auto-spawn して次回以降に備える
fn kick_background_refresh(argv: &[String], kind: &str, ttl: u64, cache_key: &str) {
    dispatch_refresh(argv, kind, ttl, cache_key);
}

/// Write 成功後の write-through 再取得を daemon にお願いする。
/// drop された entry の argv をそのまま使うので、Read whitelist にヒットしたものだけが
/// 届いている前提（drop 元が cache テーブル＝Read 経路で put された行なので保証される）。
fn kick_write_through_refresh(target: &RefreshTarget) {
    let argv: Vec<String> = match serde_json::from_str(&target.argv_json) {
        Ok(v) => v,
        Err(_) => return, // 壊れた argv_json は捨てる。stale が残るより mass 化したくないので無視
    };
    dispatch_refresh(&argv, &target.kind, target.ttl_secs, &target.cache_key);
}

/// Refresh メッセージを daemon に投げる共通経路。
/// IPC 成功で即終了、失敗時は daemon を立ち上げつつ自分の copy を detached subprocess
/// として `--refresh` で代行起動する。
fn dispatch_refresh(argv: &[String], kind: &str, ttl: u64, cache_key: &str) {
    let msg = Message::Refresh {
        argv: argv.to_vec(),
        cache_kind: kind.to_string(),
        ttl_secs: ttl,
        cache_key: cache_key.to_string(),
    };
    if ipc::try_send(&msg) {
        return;
    }
    daemon::ensure_running();
    spawn_refresh_subprocess(argv).ok();
}

/// daemon 内部用の write-through 再取得。IPC を経由せず別スレッドで直接 gh を回す。
fn spawn_local_refresh(target: RefreshTarget) {
    let argv: Vec<String> = match serde_json::from_str(&target.argv_json) {
        Ok(v) => v,
        Err(_) => return,
    };
    std::thread::spawn(move || {
        if let Err(e) =
            refresh_into_cache(&argv, &target.kind, target.ttl_secs, &target.cache_key)
        {
            eprintln!("chd: write-through refresh 失敗: {e:#}");
        }
    });
}

/// `ch --refresh ARGV...` を detached で起動する。親 ch はすぐ終わってよい。
fn spawn_refresh_subprocess(argv: &[String]) -> Result<()> {
    let exe = std::env::current_exe().context("current_exe を取得できない")?;
    Command::new(&exe)
        .arg("--refresh")
        .args(argv)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .context("refresh subprocess の spawn 失敗")?;
    Ok(())
}

/// `--repo` があればその値、無ければ cwd の絶対パスを「触ったリポ」として記録する。
fn mark_active(store: &Store, argv: &[String], now: u64) -> Result<()> {
    let id = key::detect_repo(argv).or_else(|| {
        std::env::current_dir()
            .ok()
            .map(|p| p.display().to_string())
    });
    if let Some(id) = id {
        store.mark_active(&id, now)?;
    }
    Ok(())
}

pub fn epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
