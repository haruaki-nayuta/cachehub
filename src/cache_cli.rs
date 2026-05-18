// 内部サブコマンド `ch cache ...` の実装。
//
//   - `ch cache stats`            キャッシュの概況を出す
//   - `ch cache purge [pattern]`  pattern 指定なしで全削除、ありなら kind/repo に LIKE マッチ

use anyhow::Result;

use crate::store::Store;

pub fn handle(args: &[String]) -> Result<i32> {
    match args.first().map(|s| s.as_str()) {
        Some("stats") => stats(),
        Some("purge") => purge(args.get(1).map(|s| s.as_str())),
        Some(other) => {
            eprintln!("ch: 未知のサブコマンド: cache {other}");
            eprintln!("使い方: ch cache <stats|purge> [pattern]");
            Ok(2)
        }
        None => {
            eprintln!("使い方: ch cache <stats|purge> [pattern]");
            Ok(2)
        }
    }
}

fn stats() -> Result<i32> {
    let store = Store::open_default()?;
    let s = store.stats()?;
    println!("総エントリ数 : {}", s.total);
    println!("累計ヒット数 : {}", s.hit_sum);
    println!(
        "本文サイズ   : {} bytes ({:.1} KB)",
        s.size_bytes,
        s.size_bytes as f64 / 1024.0
    );
    if !s.by_kind.is_empty() {
        println!();
        println!("kind 別:");
        println!("  {:<14} {:>8}  {:>8}", "kind", "entries", "hits");
        for row in s.by_kind {
            println!("  {:<14} {:>8}  {:>8}", row.kind, row.count, row.hits);
        }
    }
    Ok(0)
}

fn purge(pattern: Option<&str>) -> Result<i32> {
    let store = Store::open_default()?;
    let n = store.purge(pattern)?;
    match pattern {
        Some(p) => println!("'{p}' に一致するエントリを {n} 件削除しました"),
        None => println!("全 {n} エントリを削除しました"),
    }
    Ok(0)
}
