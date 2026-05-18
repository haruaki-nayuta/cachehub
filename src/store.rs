// SQLite ベースのキャッシュストア。
//
// WAL モードで開いて短い書き込みでも壊れにくくする。
// v0.1 では 1 テーブルで済ませる（kind/repo/fetched_at にインデックス）。

use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use std::path::{Path, PathBuf};

pub struct Entry {
    pub argv_json: String,
    pub kind: String,
    pub repo: Option<String>,
    pub body: Vec<u8>,
    pub fetched_at: u64,
    pub ttl_secs: u64,
}

pub struct Store {
    conn: Connection,
}

pub struct Stats {
    pub total: i64,
    pub hit_sum: i64,
    pub size_bytes: i64,
    pub by_kind: Vec<KindRow>,
}

pub struct KindRow {
    pub kind: String,
    pub count: i64,
    pub hits: i64,
}

impl Store {
    /// `$CH_DB_PATH` か、未指定なら `~/.cache/ch/ch.db` を開く。
    pub fn open_default() -> Result<Self> {
        let path = default_path()?;
        Self::open(&path)
    }

    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let conn = Connection::open(path)
            .with_context(|| format!("キャッシュ DB を開けない: {}", path.display()))?;
        // WAL は並列 read を安全にする + 書き込み詰まりを軽減
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn })
    }

    /// キーで 1 件取り出す。存在しないなら None。
    pub fn get(&self, key: &str) -> Result<Option<Entry>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT argv_json, kind, repo, body, fetched_at, ttl_secs \
             FROM cache WHERE key = ?1",
        )?;
        let mut rows = stmt.query(params![key])?;
        if let Some(row) = rows.next()? {
            Ok(Some(Entry {
                argv_json: row.get(0)?,
                kind: row.get(1)?,
                repo: row.get(2)?,
                body: row.get(3)?,
                fetched_at: row.get::<_, i64>(4)? as u64,
                ttl_secs: row.get::<_, i64>(5)? as u64,
            }))
        } else {
            Ok(None)
        }
    }

    /// キャッシュへの上書き保存。hit_count は既存値を保つ。
    pub fn put(&self, key: &str, e: &Entry) -> Result<()> {
        let mut stmt = self.conn.prepare_cached(
            "INSERT INTO cache (key, argv_json, kind, repo, body, fetched_at, ttl_secs, hit_count) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0) \
             ON CONFLICT(key) DO UPDATE SET \
                argv_json  = excluded.argv_json, \
                kind       = excluded.kind, \
                repo       = excluded.repo, \
                body       = excluded.body, \
                fetched_at = excluded.fetched_at, \
                ttl_secs   = excluded.ttl_secs",
        )?;
        stmt.execute(params![
            key,
            e.argv_json,
            e.kind,
            e.repo,
            e.body,
            e.fetched_at as i64,
            e.ttl_secs as i64,
        ])?;
        Ok(())
    }

    /// ヒット数を 1 増やす（stats 用）。
    pub fn bump_hit(&self, key: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE cache SET hit_count = hit_count + 1 WHERE key = ?1",
            params![key],
        )?;
        Ok(())
    }

    /// kind 単位で drop。repo を渡せばその repo と NULL（repo 不明）だけ drop。
    /// repo=None なら kind 全体を消す（保守的 invalidate）。
    pub fn drop_by_kind(&self, kind: &str, repo: Option<&str>) -> Result<usize> {
        let affected = match repo {
            Some(r) => self.conn.execute(
                "DELETE FROM cache WHERE kind = ?1 AND (repo = ?2 OR repo IS NULL)",
                params![kind, r],
            )?,
            None => self
                .conn
                .execute("DELETE FROM cache WHERE kind = ?1", params![kind])?,
        };
        Ok(affected)
    }

    pub fn stats(&self) -> Result<Stats> {
        let total: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM cache", [], |r| r.get(0))?;
        let hit_sum: i64 =
            self.conn
                .query_row("SELECT COALESCE(SUM(hit_count), 0) FROM cache", [], |r| {
                    r.get(0)
                })?;
        let size_bytes: i64 = self.conn.query_row(
            "SELECT COALESCE(SUM(LENGTH(body)), 0) FROM cache",
            [],
            |r| r.get(0),
        )?;
        let mut stmt = self.conn.prepare(
            "SELECT kind, COUNT(*), COALESCE(SUM(hit_count), 0) \
             FROM cache GROUP BY kind ORDER BY kind",
        )?;
        let by_kind = stmt
            .query_map([], |r| {
                Ok(KindRow {
                    kind: r.get(0)?,
                    count: r.get(1)?,
                    hits: r.get(2)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        Ok(Stats {
            total,
            hit_sum,
            size_bytes,
            by_kind,
        })
    }

    /// pattern を指定すれば kind/repo に対する LIKE マッチで削除。
    /// 指定なしなら全削除。
    pub fn purge(&self, pattern: Option<&str>) -> Result<usize> {
        let n = match pattern {
            Some(p) => self.conn.execute(
                "DELETE FROM cache WHERE kind LIKE ?1 OR repo LIKE ?1",
                params![p],
            )?,
            None => self.conn.execute("DELETE FROM cache", [])?,
        };
        Ok(n)
    }

    /// アクティブリポジトリ LRU の更新（spec §6.B「自動LRU」）。
    /// `--repo owner/name` か、無ければ cwd パスを ID として記録する。
    pub fn mark_active(&self, id: &str, now: u64) -> Result<()> {
        self.conn.execute(
            "INSERT INTO repo_activity (repo, last_used) VALUES (?1, ?2) \
             ON CONFLICT(repo) DO UPDATE SET last_used = ?2",
            params![id, now as i64],
        )?;
        Ok(())
    }

    /// 直近 `within_secs` 秒以内に触られた repo を新しい順で返す（プリフェッチ対象）。
    pub fn active_repos(&self, within_secs: u64, now: u64) -> Result<Vec<(String, u64)>> {
        let threshold = now.saturating_sub(within_secs);
        let mut stmt = self.conn.prepare(
            "SELECT repo, last_used FROM repo_activity \
             WHERE last_used > ?1 ORDER BY last_used DESC",
        )?;
        let rows: Vec<(String, u64)> = stmt
            .query_map(params![threshold as i64], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS cache (
    key         TEXT PRIMARY KEY,
    argv_json   TEXT NOT NULL,
    kind        TEXT NOT NULL,
    repo        TEXT,
    body        BLOB NOT NULL,
    fetched_at  INTEGER NOT NULL,
    ttl_secs    INTEGER NOT NULL,
    hit_count   INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_cache_kind        ON cache(kind);
CREATE INDEX IF NOT EXISTS idx_cache_repo_kind   ON cache(repo, kind);
CREATE INDEX IF NOT EXISTS idx_cache_fetched_at  ON cache(fetched_at);

-- spec §6.B: アクティブリポジトリの LRU。`ch` 起動のたびに upsert される
CREATE TABLE IF NOT EXISTS repo_activity (
    repo      TEXT PRIMARY KEY,
    last_used INTEGER NOT NULL
);
"#;

fn default_path() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("CH_DB_PATH") {
        return Ok(PathBuf::from(p));
    }
    let home = std::env::var("HOME").context("HOME 環境変数が未設定")?;
    Ok(PathBuf::from(home).join(".cache/ch/ch.db"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_store() -> Store {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "ch-test-{}-{}.db",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        Store::open(&path).expect("open")
    }

    fn entry(kind: &str, repo: Option<&str>, body: &[u8]) -> Entry {
        Entry {
            argv_json: "[]".into(),
            kind: kind.into(),
            repo: repo.map(|s| s.into()),
            body: body.to_vec(),
            fetched_at: 0,
            ttl_secs: 60,
        }
    }

    #[test]
    fn put_then_get_roundtrip() {
        let s = make_store();
        s.put("k1", &entry("issue_view", Some("a/b"), b"hello"))
            .unwrap();
        let got = s.get("k1").unwrap().unwrap();
        assert_eq!(got.body, b"hello");
        assert_eq!(got.kind, "issue_view");
        assert_eq!(got.repo.as_deref(), Some("a/b"));
    }

    #[test]
    fn drop_by_kind_with_repo_only_targets_matching_and_null() {
        let s = make_store();
        s.put("k1", &entry("issue_view", Some("a/b"), b"x"))
            .unwrap();
        s.put("k2", &entry("issue_view", Some("c/d"), b"y"))
            .unwrap();
        s.put("k3", &entry("issue_view", None, b"z")).unwrap();

        let n = s.drop_by_kind("issue_view", Some("a/b")).unwrap();
        // a/b と NULL が消えて c/d は残る
        assert_eq!(n, 2);
        assert!(s.get("k1").unwrap().is_none());
        assert!(s.get("k2").unwrap().is_some());
        assert!(s.get("k3").unwrap().is_none());
    }

    #[test]
    fn drop_by_kind_without_repo_wipes_kind() {
        let s = make_store();
        s.put("k1", &entry("pr_view", Some("a/b"), b"x")).unwrap();
        s.put("k2", &entry("pr_view", Some("c/d"), b"y")).unwrap();
        s.put("k3", &entry("issue_view", None, b"z")).unwrap();

        let n = s.drop_by_kind("pr_view", None).unwrap();
        assert_eq!(n, 2);
        assert!(s.get("k3").unwrap().is_some());
    }
}
