// `~/.config/ch/config` から設定を読む。シンプルな KEY=VALUE 行のみ。
//
// 探索順 (後勝ち):
//   1. ファイル: `$CH_CONFIG_PATH` か `~/.config/ch/config`
//   2. 環境変数: `CH_ASYNC_PASSTHROUGH` / `CH_PREFETCH`
//
// 余計な依存を増やしたくないので TOML/INI パーサは使わず、自前の薄い行パーサで済ませる。

use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, Default)]
pub struct Config {
    /// true なら Write / Passthrough 系の gh 呼び出しを daemon に投げて即 0 を返す。
    /// LLM から fire-and-forget で使うときに便利。失敗は exec_errors に記録される。
    pub async_passthrough: bool,
    /// true なら `ch issue list` のあと、列挙された各 issue view を daemon が
    /// 裏で先読みする（連想プリフェッチ）。詳細は prefetch.rs。
    pub prefetch: bool,
}

impl Config {
    pub fn load() -> Self {
        let mut cfg = Config::default();

        if let Some(path) = config_path() {
            if let Ok(content) = fs::read_to_string(&path) {
                apply_kv(&mut cfg, &content);
            }
        }

        if let Ok(v) = std::env::var("CH_ASYNC_PASSTHROUGH") {
            cfg.async_passthrough = parse_bool(&v);
        }
        if let Ok(v) = std::env::var("CH_PREFETCH") {
            cfg.prefetch = parse_bool(&v);
        }

        cfg
    }
}

fn apply_kv(cfg: &mut Config, content: &str) {
    for raw in content.lines() {
        // `#` 以降はコメント扱い
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        match k.trim() {
            "async_passthrough" => cfg.async_passthrough = parse_bool(v.trim()),
            "prefetch" => cfg.prefetch = parse_bool(v.trim()),
            _ => {}
        }
    }
}

fn config_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("CH_CONFIG_PATH") {
        return Some(PathBuf::from(p));
    }
    let home = std::env::var("HOME").ok()?;
    Some(PathBuf::from(home).join(".config/ch/config"))
}

fn parse_bool(s: &str) -> bool {
    matches!(
        s.to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_kv_basics() {
        let mut cfg = Config::default();
        apply_kv(&mut cfg, "async_passthrough=true");
        assert!(cfg.async_passthrough);

        let mut cfg = Config::default();
        apply_kv(&mut cfg, "  async_passthrough = on  # コメント");
        assert!(cfg.async_passthrough);

        let mut cfg = Config::default();
        apply_kv(&mut cfg, "# 全行コメント\nasync_passthrough = 0\n");
        assert!(!cfg.async_passthrough);
    }

    #[test]
    fn prefetch_key_parsed() {
        let mut cfg = Config::default();
        apply_kv(&mut cfg, "prefetch = true");
        assert!(cfg.prefetch);
        assert!(!cfg.async_passthrough);

        // 2 キー併用しても互いに干渉しない
        let mut cfg = Config::default();
        apply_kv(&mut cfg, "prefetch=on\nasync_passthrough=yes");
        assert!(cfg.prefetch);
        assert!(cfg.async_passthrough);
    }

    #[test]
    fn unknown_keys_are_ignored() {
        let mut cfg = Config::default();
        apply_kv(&mut cfg, "future_setting=yes\nasync_passthrough=true");
        assert!(cfg.async_passthrough);
    }
}
