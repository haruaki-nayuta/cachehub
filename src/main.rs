// `ch` のエントリポイント。
//
// 設計原則:
//   - Read whitelist は キャッシュ + TTL で返す
//   - Write (close/edit/merge など) は gh を stdio 完全透過で起動して invalidate
//   - 未知のサブコマンドは安全側に倒して passthrough
//   - `CH_BYPASS=1` を立てたら全部素通し
//   - `ch cache <stats|purge>` だけは ch 自身が処理する内部サブコマンド

mod cache_cli;
mod exec;
mod invalidate;
mod key;
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

    // 脱出弁: CH_BYPASS=1 のときは何も触らず gh に丸投げ
    if env::var("CH_BYPASS").ok().as_deref() == Some("1") {
        return exec::passthrough(&args);
    }

    // 内部サブコマンド `ch cache ...` だけは横取りする
    if args.first().map(|s| s.as_str()) == Some("cache") {
        return cache_cli::handle(&args[1..]);
    }

    // 引数なし → gh のヘルプ
    if args.is_empty() {
        return exec::passthrough(&args);
    }

    // 通常経路: ルーターで Read / Write / Passthrough を分類
    let store = store::Store::open_default()?;
    match router::classify(&args) {
        router::Action::Read { kind, ttl } => exec::handle_read(&store, &args, kind, ttl),
        router::Action::Write => exec::handle_write(&store, &args),
        router::Action::Passthrough => exec::passthrough(&args),
    }
}
