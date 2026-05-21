// `ch` のエントリポイント。
//
// dispatch ルール（上から順に判定）:
//   1. `--daemon`           → chd モードで常駐
//   2. `--refresh ARGV...`  → SWR 裏更新の subprocess
//   3. `CH_BYPASS=1`        → 全部素通し
//   4. `ch cache ...`       → 内部サブコマンド（stats / purge）
//   5. `ch daemon ...`      → 内部サブコマンド（status / stop）
//   6. `ch errors ...`      → async exec の失敗ログ参照
//   7. 引数なし             → gh のヘルプ
//   8. その他               → router で Read / Write / Passthrough に分類

mod cache_cli;
mod config;
mod daemon;
mod daemon_cli;
mod errors_cli;
mod exec;
mod invalidate;
mod ipc;
mod key;
mod prefetch;
mod router;
mod store;

use anyhow::Result;
use std::env;
use std::process::ExitCode;

fn main() -> ExitCode {
    match run() {
        Ok(code) => ExitCode::from(code.clamp(0, 255) as u8),
        Err(e) => {
            eprintln!("ch: {e:#}");
            ExitCode::from(1)
        }
    }
}

fn run() -> Result<i32> {
    let args: Vec<String> = env::args().skip(1).collect();

    // 1-2. 同一バイナリの内部モード（CH_BYPASS の前に処理する：daemon 自身は素通り対象外）
    match args.first().map(String::as_str) {
        Some("--daemon") => {
            daemon::run()?;
            return Ok(0);
        }
        Some("--refresh") => return run_refresh(&args[1..]),
        _ => {}
    }

    // 3. 脱出弁
    if env::var("CH_BYPASS").ok().as_deref() == Some("1") {
        return exec::passthrough(&args);
    }

    // 4-6. 内部サブコマンド
    match args.first().map(String::as_str) {
        Some("cache") => return cache_cli::handle(&args[1..]),
        Some("daemon") => return daemon_cli::handle(&args[1..]),
        Some("errors") => return errors_cli::handle(&args[1..]),
        _ => {}
    }

    // 7. 引数なし
    if args.is_empty() {
        return exec::passthrough(&args);
    }

    // 8. 通常経路
    let cfg = config::Config::load();
    let store = store::Store::open_default()?;

    // daemon が居なければ立ち上げる（fire-and-forget、待たない）
    daemon::ensure_running();

    match router::classify(&args) {
        router::Action::Read { kind, ttl } => exec::handle_read(&store, &args, kind, ttl, &cfg),
        router::Action::Write => exec::handle_write(&store, &args, &cfg),
        router::Action::Passthrough => exec::handle_passthrough(&args, &cfg),
    }
}

/// `ch --refresh ARGV...` のエントリ。
/// 渡された argv を router で再分類し、Read 系だったときだけ cache を上書きする。
fn run_refresh(gh_argv: &[String]) -> Result<i32> {
    let router::Action::Read { kind, ttl } = router::classify(gh_argv) else {
        // Read じゃないものを refresh する意味は無い
        return Ok(0);
    };
    let cache_key = key::cache_key(gh_argv);
    exec::refresh_into_cache(gh_argv, kind, ttl, &cache_key)?;
    Ok(0)
}
