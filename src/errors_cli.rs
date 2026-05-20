// 内部サブコマンド `ch errors ...`。
//
//   - `ch errors`              直近の async exec 失敗 20 件を一覧
//   - `ch errors list [N]`     N 件まで一覧（既定 20）
//   - `ch errors show <id>`    1 件の詳細（stdout/stderr 全文）
//   - `ch errors clear`        全削除

use anyhow::Result;
use std::io::Write;

use crate::store::Store;

pub fn handle(args: &[String]) -> Result<i32> {
    match args.first().map(String::as_str) {
        None => list(20),
        Some("list") => {
            let limit = args
                .get(1)
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(20);
            list(limit)
        }
        Some("show") => match args.get(1).and_then(|s| s.parse::<i64>().ok()) {
            Some(id) => show(id),
            None => {
                eprintln!("使い方: ch errors show <id>");
                Ok(2)
            }
        },
        Some("clear") => clear(),
        Some(other) => {
            eprintln!("ch: 未知のサブコマンド: errors {other}");
            eprintln!("使い方: ch errors [list [N] | show <id> | clear]");
            Ok(2)
        }
    }
}

fn list(limit: usize) -> Result<i32> {
    let store = Store::open_default()?;
    let rows = store.list_exec_errors(limit)?;
    if rows.is_empty() {
        println!("失敗ログはありません");
        return Ok(0);
    }
    println!("失敗ログ (新しい順, 最大 {limit} 件):");
    println!("  {:>5}  {:>10}  {:>4}  コマンド", "id", "failed_at", "exit");
    for e in rows {
        let argv = parse_argv(&e.argv_json);
        let cmd = format!("gh {}", argv.join(" "));
        println!(
            "  {:>5}  {:>10}  {:>4}  {}",
            e.id,
            e.failed_at,
            e.exit_code,
            truncate(&cmd, 80)
        );
        let preview = first_line_of(&e.stderr);
        if !preview.is_empty() {
            println!("         stderr: {}", truncate(&preview, 80));
        }
    }
    Ok(0)
}

fn show(id: i64) -> Result<i32> {
    let store = Store::open_default()?;
    let Some(e) = store.get_exec_error(id)? else {
        eprintln!("id={id} のエラーは見つかりません");
        return Ok(1);
    };
    let argv = parse_argv(&e.argv_json);
    println!("id        : {}", e.id);
    println!("failed_at : {} (epoch sec)", e.failed_at);
    println!("exit_code : {}", e.exit_code);
    println!("argv      : gh {}", argv.join(" "));
    println!("--- stdout ---");
    let mut out = std::io::stdout().lock();
    out.write_all(&e.stdout).ok();
    if !e.stdout.ends_with(b"\n") {
        out.write_all(b"\n").ok();
    }
    println!("--- stderr ---");
    out.write_all(&e.stderr).ok();
    if !e.stderr.ends_with(b"\n") {
        out.write_all(b"\n").ok();
    }
    Ok(0)
}

fn clear() -> Result<i32> {
    let store = Store::open_default()?;
    let n = store.clear_exec_errors()?;
    println!("失敗ログ {n} 件を削除しました");
    Ok(0)
}

fn parse_argv(json: &str) -> Vec<String> {
    serde_json::from_str(json).unwrap_or_else(|_| vec![json.to_string()])
}

fn first_line_of(bytes: &[u8]) -> String {
    let s = String::from_utf8_lossy(bytes);
    s.lines().next().unwrap_or("").to_string()
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}
