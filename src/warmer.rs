// chd 内の Warmer / headroom sampler。
//
//   - Warmer: 定期的に `cache` 表の TTL 切れエントリを GlobalLimiter 越しに refresh する。
//     対象は `repo_activity` の active 窓 (72h) に居る repo に紐付くものだけ。spec §6.B。
//     prefetch.rs と役割は分けている: prefetch はイベント駆動 (issue list 起点)、
//     Warmer は時間駆動 (TTL 切れ全般)。
//
//   - HeadroomSampler: 60s おきに `gh api rate_limit` を叩いて `resources.core.remaining`
//     を読む。`ratelimit_headroom` を切ったら Limiter を pause、戻れば resume。
//     `/rate_limit` 自体は GitHub 側で rate limit にカウントされない。
//
// どちらも `stop_flag` を細かく刻んで確認し、chd 終了時に素直に抜ける。

use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::exec;
use crate::limiter::{GlobalLimiter, LimiterSnapshot};
use crate::store::{StaleEntry, Store};

/// active 判定の窓（spec §6.B）。
const ACTIVE_WITHIN_SECS: u64 = 72 * 3600;

/// HeadroomSampler の間隔。`/rate_limit` は無料だが gh 起動コストはあるので分単位で十分。
const SAMPLER_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Clone)]
pub struct WarmerConfig {
    pub interval: Duration,
    pub batch_limit: usize,
}

pub fn spawn_warmer(
    limiter: Arc<GlobalLimiter>,
    cfg: WarmerConfig,
    stop_flag: Arc<AtomicBool>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        // 起動直後に 1 度スナップショットを書いて status から見えるようにする
        if let Ok(store) = Store::open_default() {
            persist_snapshot(&store, &limiter, None, None);
        }
        loop {
            if stop_flag.load(Ordering::SeqCst) {
                return;
            }
            if let Err(e) = tick(&limiter, cfg.batch_limit) {
                eprintln!("chd: warmer error: {e:#}");
            }
            sleep_until_or_stop(cfg.interval, &stop_flag);
        }
    })
}

pub fn spawn_headroom_sampler(
    limiter: Arc<GlobalLimiter>,
    headroom_threshold: u32,
    stop_flag: Arc<AtomicBool>,
) -> JoinHandle<()> {
    thread::spawn(move || loop {
        if stop_flag.load(Ordering::SeqCst) {
            return;
        }
        sample_once(&limiter, headroom_threshold);
        sleep_until_or_stop(SAMPLER_INTERVAL, &stop_flag);
    })
}

fn tick(limiter: &GlobalLimiter, batch_limit: usize) -> anyhow::Result<()> {
    let store = Store::open_default()?;
    let now = exec::epoch_secs();
    let stale = store.stale_entries(now, ACTIVE_WITHIN_SECS, batch_limit)?;
    for entry in stale {
        if !limiter.try_acquire() {
            // バケット枯れか paused → 今ティックでこれ以上回しても通らないので break
            break;
        }
        refresh_one(&entry);
    }
    persist_snapshot(&store, limiter, None, None);
    Ok(())
}

fn refresh_one(entry: &StaleEntry) {
    let argv: Vec<String> = match serde_json::from_str(&entry.argv_json) {
        Ok(v) => v,
        Err(e) => {
            eprintln!(
                "chd: warmer: argv_json をパースできない key={} : {e}",
                entry.cache_key
            );
            return;
        }
    };
    if let Err(e) =
        exec::refresh_into_cache(&argv, &entry.kind, entry.ttl_secs, &entry.cache_key)
    {
        eprintln!("chd: warmer: refresh 失敗 key={} : {e:#}", entry.cache_key);
    }
}

fn sample_once(limiter: &GlobalLimiter, threshold: u32) {
    let Some(remaining) = fetch_remaining() else {
        // 取れなかったときは状態を変えない（safe default）
        return;
    };
    let paused = remaining < threshold;
    limiter.set_paused(paused);

    if let Ok(store) = Store::open_default() {
        persist_snapshot(&store, limiter, Some(remaining), Some(exec::epoch_secs()));
    }
}

fn fetch_remaining() -> Option<u32> {
    let output = Command::new("gh")
        .args(["api", "rate_limit"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    v.get("resources")?
        .get("core")?
        .get("remaining")?
        .as_u64()
        .map(|x| x as u32)
}

fn persist_snapshot(
    store: &Store,
    limiter: &GlobalLimiter,
    remaining: Option<u32>,
    remaining_at: Option<u64>,
) {
    let snap: LimiterSnapshot = limiter.snapshot();
    let _ = store.upsert_ratelimit_state(
        snap.tokens,
        snap.capacity,
        snap.refill_per_sec,
        snap.paused,
        remaining,
        remaining_at,
        snap.consumed_total,
        snap.enqueued_total,
        snap.skipped_total,
        exec::epoch_secs(),
    );
}

fn sleep_until_or_stop(total: Duration, stop_flag: &AtomicBool) {
    let step = Duration::from_millis(200);
    let mut elapsed = Duration::ZERO;
    while elapsed < total {
        if stop_flag.load(Ordering::SeqCst) {
            return;
        }
        thread::sleep(step);
        elapsed += step;
    }
}
