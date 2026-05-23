// SQLite ベースのキャッシュストア。
//
// WAL モードで開いて短い書き込みでも壊れにくくする。
// v0.1 では 1 テーブルで済ませる（kind/repo/fetched_at にインデックス）。

use anyhow::{Context, Result};
use rusqlite::{params, Connection, Row};
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

/// write-through 用に「Write 後に refresh で呼び直したい cache 行」の最小情報。
/// argv_json は cache に保存していた gh の引数列を JSON で持ったもの。
pub struct RefreshTarget {
    pub cache_key: String,
    pub argv_json: String,
    pub kind: String,
    pub ttl_secs: u64,
}

/// Warmer が「TTL 切れだから再取得したい」と判定した cache 行の最小情報。
/// `list_refresh_targets` の RefreshTarget と形が似ているが、
/// 「active な repo に紐付き」「fetched_at + ttl_secs <= now」だけを抽出する点が異なる。
pub struct StaleEntry {
    pub cache_key: String,
    pub argv_json: String,
    pub kind: String,
    pub ttl_secs: u64,
}

/// `ratelimit_state` シングルロー テーブルの読み出し結果。chd プロセス内の
/// `GlobalLimiter` 状態 + 直近サンプリングした GitHub 側 remaining。
#[derive(Debug, Clone)]
pub struct RatelimitState {
    pub bucket_tokens: f64,
    pub bucket_capacity: u32,
    pub refill_per_sec: f64,
    pub paused: bool,
    pub remaining: Option<u32>,
    pub remaining_at: Option<u64>,
    pub consumed_total: u64,
    pub enqueued_total: u64,
    pub skipped_total: u64,
    pub updated_at: u64,
}

/// async_passthrough モードで daemon が gh を実行して失敗したときの記録。
/// stdout/stderr は大き過ぎると DB を圧迫するので保存時に 64KiB で頭打ちにする。
pub struct ExecError {
    pub id: i64,
    pub argv_json: String,
    pub exit_code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub failed_at: u64,
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

    /// write-through 用。`drop_by_kind` と同じ条件にマッチする行を返す（消さない）。
    /// 戻り値は「Write 成功直後に gh を呼び直して cache を埋め直したい entry」のリスト。
    pub fn list_refresh_targets(
        &self,
        kind: &str,
        repo: Option<&str>,
    ) -> Result<Vec<RefreshTarget>> {
        let mut stmt = self.conn.prepare(
            "SELECT key, argv_json, kind, ttl_secs FROM cache \
             WHERE kind = ?1 AND (?2 IS NULL OR repo = ?2 OR repo IS NULL)",
        )?;
        let rows = stmt
            .query_map(params![kind, repo], row_to_refresh_target)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// kind 単位で drop。repo を渡せばその repo と NULL（repo 不明）だけ drop。
    /// repo=None なら kind 全体を消す（保守的 invalidate）。
    pub fn drop_by_kind(&self, kind: &str, repo: Option<&str>) -> Result<usize> {
        let affected = self.conn.execute(
            "DELETE FROM cache WHERE kind = ?1 AND (?2 IS NULL OR repo = ?2 OR repo IS NULL)",
            params![kind, repo],
        )?;
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

    /// async exec が失敗した記録を 1 件挿入する。
    /// stdout/stderr は MAX_LOG_BYTES で頭打ち。
    pub fn log_exec_error(
        &self,
        argv_json: &str,
        exit_code: i32,
        stdout: &[u8],
        stderr: &[u8],
        failed_at: u64,
    ) -> Result<i64> {
        let so = cap_log(stdout);
        let se = cap_log(stderr);
        let mut stmt = self.conn.prepare_cached(
            "INSERT INTO exec_errors (argv_json, exit_code, stdout, stderr, failed_at) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )?;
        stmt.execute(params![argv_json, exit_code, so, se, failed_at as i64])?;
        Ok(self.conn.last_insert_rowid())
    }

    /// 新しい順で最大 `limit` 件取り出す。
    pub fn list_exec_errors(&self, limit: usize) -> Result<Vec<ExecError>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, argv_json, exit_code, stdout, stderr, failed_at \
             FROM exec_errors ORDER BY failed_at DESC, id DESC LIMIT ?1",
        )?;
        let rows = stmt
            .query_map(params![limit as i64], row_to_exec_error)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// 1 件詳細を引く。
    pub fn get_exec_error(&self, id: i64) -> Result<Option<ExecError>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, argv_json, exit_code, stdout, stderr, failed_at \
             FROM exec_errors WHERE id = ?1",
        )?;
        let mut rows = stmt.query(params![id])?;
        if let Some(row) = rows.next()? {
            Ok(Some(row_to_exec_error(row)?))
        } else {
            Ok(None)
        }
    }

    /// 失敗ログを全削除。
    pub fn clear_exec_errors(&self) -> Result<usize> {
        let n = self.conn.execute("DELETE FROM exec_errors", [])?;
        Ok(n)
    }

    /// Warmer 対象。TTL 切れかつ active な repo に紐付くキャッシュエントリを古い順に最大 `limit` 件返す。
    /// `repo IS NULL` のエントリは active 判定不可 + chd の cwd で gh を回せないため除外する。
    pub fn stale_entries(
        &self,
        now: u64,
        active_within_secs: u64,
        limit: usize,
    ) -> Result<Vec<StaleEntry>> {
        let active_threshold = now.saturating_sub(active_within_secs);
        let mut stmt = self.conn.prepare(
            "SELECT c.key, c.argv_json, c.kind, c.ttl_secs \
             FROM cache c \
             INNER JOIN repo_activity r ON c.repo = r.repo \
             WHERE c.repo IS NOT NULL \
               AND c.fetched_at + c.ttl_secs <= ?1 \
               AND r.last_used > ?2 \
             ORDER BY (c.fetched_at + c.ttl_secs) ASC \
             LIMIT ?3",
        )?;
        let rows = stmt
            .query_map(
                params![now as i64, active_threshold as i64, limit as i64],
                |r| {
                    Ok(StaleEntry {
                        cache_key: r.get(0)?,
                        argv_json: r.get(1)?,
                        kind: r.get(2)?,
                        ttl_secs: r.get::<_, i64>(3)? as u64,
                    })
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// `ratelimit_state` シングルロー upsert。`remaining` / `remaining_at` は
    /// Some のときだけ書き換え、None なら既存値を保つ（Warmer ティックと headroom sampler の
    /// 書き込みタイミングがズレてもサンプリング値を上書きで消さないようにするため）。
    #[allow(clippy::too_many_arguments)]
    pub fn upsert_ratelimit_state(
        &self,
        bucket_tokens: f64,
        bucket_capacity: u32,
        refill_per_sec: f64,
        paused: bool,
        remaining: Option<u32>,
        remaining_at: Option<u64>,
        consumed_total: u64,
        enqueued_total: u64,
        skipped_total: u64,
        updated_at: u64,
    ) -> Result<()> {
        let mut stmt = self.conn.prepare_cached(
            "INSERT INTO ratelimit_state \
                (id, bucket_tokens, bucket_capacity, refill_per_sec, paused, \
                 remaining, remaining_at, consumed_total, enqueued_total, skipped_total, updated_at) \
             VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10) \
             ON CONFLICT(id) DO UPDATE SET \
                bucket_tokens   = excluded.bucket_tokens, \
                bucket_capacity = excluded.bucket_capacity, \
                refill_per_sec  = excluded.refill_per_sec, \
                paused          = excluded.paused, \
                remaining       = COALESCE(excluded.remaining, ratelimit_state.remaining), \
                remaining_at    = COALESCE(excluded.remaining_at, ratelimit_state.remaining_at), \
                consumed_total  = excluded.consumed_total, \
                enqueued_total  = excluded.enqueued_total, \
                skipped_total   = excluded.skipped_total, \
                updated_at      = excluded.updated_at",
        )?;
        stmt.execute(params![
            bucket_tokens,
            bucket_capacity as i64,
            refill_per_sec,
            paused as i64,
            remaining.map(|x| x as i64),
            remaining_at.map(|x| x as i64),
            consumed_total as i64,
            enqueued_total as i64,
            skipped_total as i64,
            updated_at as i64,
        ])?;
        Ok(())
    }

    pub fn get_ratelimit_state(&self) -> Result<Option<RatelimitState>> {
        let mut stmt = self.conn.prepare(
            "SELECT bucket_tokens, bucket_capacity, refill_per_sec, paused, \
                    remaining, remaining_at, consumed_total, enqueued_total, skipped_total, updated_at \
             FROM ratelimit_state WHERE id = 1",
        )?;
        let mut rows = stmt.query([])?;
        if let Some(row) = rows.next()? {
            Ok(Some(RatelimitState {
                bucket_tokens: row.get(0)?,
                bucket_capacity: row.get::<_, i64>(1)? as u32,
                refill_per_sec: row.get(2)?,
                paused: row.get::<_, i64>(3)? != 0,
                remaining: row.get::<_, Option<i64>>(4)?.map(|x| x as u32),
                remaining_at: row.get::<_, Option<i64>>(5)?.map(|x| x as u64),
                consumed_total: row.get::<_, i64>(6)? as u64,
                enqueued_total: row.get::<_, i64>(7)? as u64,
                skipped_total: row.get::<_, i64>(8)? as u64,
                updated_at: row.get::<_, i64>(9)? as u64,
            }))
        } else {
            Ok(None)
        }
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

/// `list_refresh_targets` の 2 アームで共有する row マッパー。
/// SELECT 列順は `(key, argv_json, kind, ttl_secs)` 固定。
fn row_to_refresh_target(row: &Row) -> rusqlite::Result<RefreshTarget> {
    Ok(RefreshTarget {
        cache_key: row.get(0)?,
        argv_json: row.get(1)?,
        kind: row.get(2)?,
        ttl_secs: row.get::<_, i64>(3)? as u64,
    })
}

/// `list_exec_errors` と `get_exec_error` で共有する row マッパー。
/// SELECT 列順は `(id, argv_json, exit_code, stdout, stderr, failed_at)` 固定。
fn row_to_exec_error(row: &Row) -> rusqlite::Result<ExecError> {
    Ok(ExecError {
        id: row.get(0)?,
        argv_json: row.get(1)?,
        exit_code: row.get(2)?,
        stdout: row.get(3)?,
        stderr: row.get(4)?,
        failed_at: row.get::<_, i64>(5)? as u64,
    })
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

-- async_passthrough モードで daemon が回した gh の失敗ログ。
-- ch errors で参照する。
CREATE TABLE IF NOT EXISTS exec_errors (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    argv_json  TEXT    NOT NULL,
    exit_code  INTEGER NOT NULL,
    stdout     BLOB    NOT NULL,
    stderr     BLOB    NOT NULL,
    failed_at  INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_exec_errors_failed_at ON exec_errors(failed_at);

-- spec §10 / §9 [ratelimit]: GlobalLimiter のスナップショット + headroom 状態。
-- chd プロセス内のメモリが真の在処で、ここは CLI 側 (`ch daemon status`) から読むためのミラー。
-- 常に id=1 の 1 行のみ。
CREATE TABLE IF NOT EXISTS ratelimit_state (
    id              INTEGER PRIMARY KEY CHECK (id = 1),
    bucket_tokens   REAL    NOT NULL,
    bucket_capacity INTEGER NOT NULL,
    refill_per_sec  REAL    NOT NULL,
    paused          INTEGER NOT NULL DEFAULT 0,
    remaining       INTEGER,
    remaining_at    INTEGER,
    consumed_total  INTEGER NOT NULL DEFAULT 0,
    enqueued_total  INTEGER NOT NULL DEFAULT 0,
    skipped_total   INTEGER NOT NULL DEFAULT 0,
    updated_at      INTEGER NOT NULL
);
"#;

/// exec_errors に保存する stdout/stderr の上限。
/// gh のエラー出力は通常数十〜数百バイトだが、`gh api` で巨大レスポンスを叩いた場合の保険。
const MAX_LOG_BYTES: usize = 64 * 1024;

fn cap_log(b: &[u8]) -> Vec<u8> {
    if b.len() <= MAX_LOG_BYTES {
        b.to_vec()
    } else {
        let mut v = b[..MAX_LOG_BYTES].to_vec();
        v.extend_from_slice(b"\n... (truncated by ch)");
        v
    }
}

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
            repo: repo.map(Into::into),
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
    fn exec_error_log_list_clear_roundtrip() {
        let s = make_store();
        let id1 = s
            .log_exec_error("[\"issue\",\"close\",\"1\"]", 1, b"out1", b"err1", 100)
            .unwrap();
        let id2 = s
            .log_exec_error("[\"pr\",\"merge\",\"2\"]", 128, b"", b"err2", 200)
            .unwrap();
        assert!(id1 < id2);

        let rows = s.list_exec_errors(10).unwrap();
        assert_eq!(rows.len(), 2);
        // failed_at 降順なので id2 が先
        assert_eq!(rows[0].id, id2);
        assert_eq!(rows[0].exit_code, 128);
        assert_eq!(rows[0].stderr, b"err2");

        let one = s.get_exec_error(id1).unwrap().unwrap();
        assert_eq!(one.stdout, b"out1");

        let n = s.clear_exec_errors().unwrap();
        assert_eq!(n, 2);
        assert!(s.list_exec_errors(10).unwrap().is_empty());
    }

    #[test]
    fn exec_error_truncates_large_payload() {
        let s = make_store();
        let big = vec![b'x'; 70 * 1024];
        let id = s.log_exec_error("[]", 1, b"", &big, 0).unwrap();
        let got = s.get_exec_error(id).unwrap().unwrap();
        // 64KiB + truncation marker
        assert!(got.stderr.len() <= 64 * 1024 + 64);
        assert!(got.stderr.ends_with(b"(truncated by ch)"));
    }

    #[test]
    fn list_refresh_targets_mirrors_drop_filter() {
        let s = make_store();
        // ttl/argv_json まで読み戻せるか見たいので Entry を組み立てて入れる
        let mk = |kind: &str, repo: Option<&str>, argv_json: &str, ttl: u64| Entry {
            argv_json: argv_json.into(),
            kind: kind.into(),
            repo: repo.map(Into::into),
            body: b"-".to_vec(),
            fetched_at: 0,
            ttl_secs: ttl,
        };
        s.put("k1", &mk("issue_view", Some("a/b"), "[\"issue\",\"view\",\"1\"]", 60))
            .unwrap();
        s.put("k2", &mk("issue_view", Some("c/d"), "[\"issue\",\"view\",\"2\"]", 60))
            .unwrap();
        s.put("k3", &mk("issue_view", None, "[\"issue\",\"view\",\"3\"]", 60))
            .unwrap();

        // repo を絞ったときは a/b と NULL を拾い、c/d は拾わない
        let mut got = s.list_refresh_targets("issue_view", Some("a/b")).unwrap();
        got.sort_by(|x, y| x.cache_key.cmp(&y.cache_key));
        assert_eq!(got.len(), 2);
        assert!(got.iter().any(|t| t.cache_key == "k1"));
        assert!(got.iter().any(|t| t.cache_key == "k3"));
        assert!(got.iter().all(|t| t.kind == "issue_view" && t.ttl_secs == 60));

        // repo=None なら kind 全部
        let all = s.list_refresh_targets("issue_view", None).unwrap();
        assert_eq!(all.len(), 3);
    }

    // stats() の集計と、put 上書きで hit_count が保たれること。
    // SWR 裏更新は cache を put し直すため、hit_count がリセットされると
    // `ch cache stats` のヒット数が嘘になる。
    // 参考: moka はヒット/ミス統計の正確性を検証する。
    #[test]
    fn stats_aggregates_and_overwrite_preserves_hit_count() {
        let s = make_store();
        assert_eq!(s.stats().unwrap().total, 0, "空 DB は total=0");
        assert_eq!(s.stats().unwrap().hit_sum, 0, "空 DB は hit_sum=0 (COALESCE)");

        s.put("k1", &entry("issue_view", Some("a/b"), b"hello"))
            .unwrap();
        s.put("k2", &entry("pr_view", Some("a/b"), b"xy")).unwrap();
        s.bump_hit("k1").unwrap();
        s.bump_hit("k1").unwrap();
        s.bump_hit("k2").unwrap();

        // 裏更新を模して k1 を上書き
        s.put("k1", &entry("issue_view", Some("a/b"), b"hello2"))
            .unwrap();

        let st = s.stats().unwrap();
        assert_eq!(st.total, 2);
        assert_eq!(st.hit_sum, 3, "put 上書き後も hit_count は保持される");
        assert_eq!(st.size_bytes, "hello2".len() as i64 + 2);
        // by_kind は kind 昇順
        assert_eq!(st.by_kind.len(), 2);
        assert_eq!(st.by_kind[0].kind, "issue_view");
        assert_eq!(st.by_kind[0].hits, 2);
        assert_eq!(st.by_kind[1].kind, "pr_view");
    }

    // purge: pattern なしで全削除、ありなら kind/repo に LIKE マッチ。
    #[test]
    fn purge_all_and_by_pattern() {
        let s = make_store();
        s.put("k1", &entry("issue_view", Some("a/b"), b"x")).unwrap();
        s.put("k2", &entry("pr_view", Some("a/b"), b"y")).unwrap();
        s.put("k3", &entry("repo_view", Some("c/d"), b"z")).unwrap();

        // kind LIKE 'issue%' → k1 のみ
        let n = s.purge(Some("issue%")).unwrap();
        assert_eq!(n, 1);
        assert!(s.get("k1").unwrap().is_none());
        assert!(s.get("k2").unwrap().is_some());

        // repo LIKE 'c/%' → k3 がヒット（pattern は repo にも当たる）
        let n = s.purge(Some("c/%")).unwrap();
        assert_eq!(n, 1);
        assert!(s.get("k3").unwrap().is_none());

        // pattern なし → 残り全部
        s.put("k4", &entry("pr_view", None, b"w")).unwrap();
        let n = s.purge(None).unwrap();
        assert_eq!(n, 2, "k2 と k4");
        assert_eq!(s.stats().unwrap().total, 0);
    }

    // mark_active は repo ごとに last_used を upsert し、
    // active_repos は within_secs の閾値より新しいものだけを新しい順で返す。
    #[test]
    fn mark_active_upserts_and_active_repos_filters_by_threshold() {
        let s = make_store();
        s.mark_active("a/b", 100).unwrap();
        s.mark_active("a/b", 200).unwrap(); // 同 repo は upsert で last_used 更新
        s.mark_active("old/repo", 100).unwrap();

        // now=210, within=50 → threshold=160。a/b(200) は残り old/repo(100) は落ちる
        let active = s.active_repos(50, 210).unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0], ("a/b".to_string(), 200));

        // within を広げれば両方拾い、last_used 降順で並ぶ
        let all = s.active_repos(1000, 210).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].0, "a/b");
        assert_eq!(all[1].0, "old/repo");
    }

    #[test]
    fn stale_entries_returns_only_ttl_expired_and_active() {
        let s = make_store();
        let mk = |kind: &str, repo: Option<&str>, fetched_at: u64, ttl: u64| Entry {
            argv_json: "[]".into(),
            kind: kind.into(),
            repo: repo.map(Into::into),
            body: b"x".to_vec(),
            fetched_at,
            ttl_secs: ttl,
        };

        // now=1000、active 窓 200s → threshold=800。
        s.mark_active("a/b", 900).unwrap(); // active
        s.mark_active("c/d", 900).unwrap(); // active
        s.mark_active("x/y", 100).unwrap(); // inactive (古い)

        s.put("k1", &mk("issue_view", Some("a/b"), 800, 60)).unwrap(); // 期限 860 → stale ✓
        s.put("k2", &mk("issue_view", Some("c/d"), 990, 60)).unwrap(); // 期限 1050 → fresh ✗
        s.put("k3", &mk("issue_view", Some("x/y"), 800, 60)).unwrap(); // stale だが inactive ✗
        s.put("k4", &mk("issue_view", None, 800, 60)).unwrap();        // repo NULL ✗

        let stale = s.stale_entries(1000, 200, 10).unwrap();
        let keys: Vec<&str> = stale.iter().map(|e| e.cache_key.as_str()).collect();
        assert_eq!(keys, vec!["k1"]);
        assert_eq!(stale[0].kind, "issue_view");
        assert_eq!(stale[0].ttl_secs, 60);
    }

    #[test]
    fn stale_entries_orders_by_expiry_and_respects_limit() {
        let s = make_store();
        s.mark_active("a/b", 900).unwrap();

        let mk = |fetched_at: u64, ttl: u64| Entry {
            argv_json: "[]".into(),
            kind: "issue_view".into(),
            repo: Some("a/b".into()),
            body: b"x".to_vec(),
            fetched_at,
            ttl_secs: ttl,
        };
        s.put("late", &mk(800, 100)).unwrap(); // expiry 900
        s.put("early", &mk(500, 100)).unwrap(); // expiry 600
        s.put("middle", &mk(700, 100)).unwrap(); // expiry 800

        let stale = s.stale_entries(1000, 200, 2).unwrap();
        assert_eq!(stale.len(), 2);
        assert_eq!(stale[0].cache_key, "early");
        assert_eq!(stale[1].cache_key, "middle");
    }

    #[test]
    fn ratelimit_state_upsert_preserves_remaining_when_none() {
        let s = make_store();
        s.upsert_ratelimit_state(120.0, 120, 2.0, false, Some(4000), Some(100), 1, 2, 0, 100)
            .unwrap();
        let got = s.get_ratelimit_state().unwrap().unwrap();
        assert_eq!(got.remaining, Some(4000));
        assert_eq!(got.remaining_at, Some(100));
        assert!(!got.paused);

        // 二回目: remaining=None → 既存の 4000 / 100 を保つ
        s.upsert_ratelimit_state(118.0, 120, 2.0, true, None, None, 5, 8, 3, 200)
            .unwrap();
        let got = s.get_ratelimit_state().unwrap().unwrap();
        assert_eq!(got.remaining, Some(4000));
        assert_eq!(got.remaining_at, Some(100));
        assert!(got.paused);
        assert_eq!(got.bucket_tokens, 118.0);
        assert_eq!(got.consumed_total, 5);
        assert_eq!(got.enqueued_total, 8);
        assert_eq!(got.skipped_total, 3);
        assert_eq!(got.updated_at, 200);
    }

    #[test]
    fn ratelimit_state_upsert_overwrites_remaining_when_some() {
        let s = make_store();
        s.upsert_ratelimit_state(120.0, 120, 2.0, false, Some(4000), Some(100), 0, 0, 0, 100)
            .unwrap();
        s.upsert_ratelimit_state(120.0, 120, 2.0, false, Some(300), Some(200), 0, 0, 0, 200)
            .unwrap();
        let got = s.get_ratelimit_state().unwrap().unwrap();
        assert_eq!(got.remaining, Some(300));
        assert_eq!(got.remaining_at, Some(200));
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
