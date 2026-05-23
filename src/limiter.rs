// グローバルレートリミッタ (spec §10, §9 [ratelimit])。
//
// chd プロセス内に 1 つだけ存在し、Warmer と prefetch の両方が同じバケットを共有する。
// トークンバケット: 容量 = `ratelimit_per_min`、補充 = 容量 / 60 秒。
//
// 使い方:
//   - 起動時に `init_global(per_min)` で 1 度だけ初期化（daemon::run）。
//   - 裏で勝手に走る経路（Warmer / prefetch）の入口で `global().try_acquire()` を呼ぶ。
//     非ブロッキング。false なら今回はスキップする。
//   - headroom sampler が `gh api rate_limit` の結果から `set_paused(true|false)` を切り替える。
//
// CLI 側の Read miss/SWR refresh / AsyncExec はこのリミッタを通さない。
// 「ユーザー操作は通す。プリフェッチだけ全停止」（issue #7）の方針に沿うため。

use std::sync::{Mutex, OnceLock};
use std::sync::Arc;
use std::time::Instant;

pub struct GlobalLimiter {
    inner: Mutex<Inner>,
}

struct Inner {
    capacity: f64,
    refill_per_sec: f64,
    tokens: f64,
    last_refill: Instant,
    paused: bool,
    consumed_total: u64,
    enqueued_total: u64,
    skipped_total: u64,
}

#[derive(Debug, Clone)]
pub struct LimiterSnapshot {
    pub capacity: u32,
    pub refill_per_sec: f64,
    pub tokens: f64,
    pub paused: bool,
    pub consumed_total: u64,
    pub enqueued_total: u64,
    pub skipped_total: u64,
}

impl GlobalLimiter {
    pub fn new(per_min: u32) -> Self {
        let cap = per_min as f64;
        Self {
            inner: Mutex::new(Inner {
                capacity: cap,
                refill_per_sec: cap / 60.0,
                tokens: cap,
                last_refill: Instant::now(),
                paused: false,
                consumed_total: 0,
                enqueued_total: 0,
                skipped_total: 0,
            }),
        }
    }

    /// 1 トークン取れたら true。`paused` または空のとき false。
    /// enqueued / consumed / skipped カウンタは常に更新する。
    pub fn try_acquire(&self) -> bool {
        let mut inner = self.inner.lock().unwrap();
        inner.enqueued_total += 1;
        if inner.paused {
            inner.skipped_total += 1;
            return false;
        }
        refill(&mut inner, Instant::now());
        if inner.tokens >= 1.0 {
            inner.tokens -= 1.0;
            inner.consumed_total += 1;
            true
        } else {
            inner.skipped_total += 1;
            false
        }
    }

    pub fn set_paused(&self, paused: bool) {
        self.inner.lock().unwrap().paused = paused;
    }

    /// 現在のスナップショット。SQLite ミラー書き出し用。
    pub fn snapshot(&self) -> LimiterSnapshot {
        let mut inner = self.inner.lock().unwrap();
        // 経過時間を反映してから読みたい（acquire と独立に時間は進むので）
        refill(&mut inner, Instant::now());
        LimiterSnapshot {
            capacity: inner.capacity as u32,
            refill_per_sec: inner.refill_per_sec,
            tokens: inner.tokens,
            paused: inner.paused,
            consumed_total: inner.consumed_total,
            enqueued_total: inner.enqueued_total,
            skipped_total: inner.skipped_total,
        }
    }
}

fn refill(inner: &mut Inner, now: Instant) {
    let elapsed = now.saturating_duration_since(inner.last_refill).as_secs_f64();
    if elapsed > 0.0 {
        inner.tokens = (inner.tokens + elapsed * inner.refill_per_sec).min(inner.capacity);
        inner.last_refill = now;
    }
}

// --- グローバルシングルトン ---
//
// daemon::run() の冒頭で `init_global(per_min)` を呼ぶ。
// それ以降、warmer や prefetch は `global()` で同じインスタンスを参照する。

static GLOBAL_LIMITER: OnceLock<Arc<GlobalLimiter>> = OnceLock::new();

/// daemon 起動時に 1 度だけ呼ぶ。複数回呼んでも 2 回目以降は無視（既存を返す）。
pub fn init_global(per_min: u32) -> Arc<GlobalLimiter> {
    GLOBAL_LIMITER
        .get_or_init(|| Arc::new(GlobalLimiter::new(per_min)))
        .clone()
}

/// 取得。未初期化なら None。
/// CLI 経路から誤って呼ばれても破綻しないよう Option を返す。
pub fn global() -> Option<Arc<GlobalLimiter>> {
    GLOBAL_LIMITER.get().cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn full_bucket_lets_capacity_through_immediately() {
        let l = GlobalLimiter::new(5);
        for _ in 0..5 {
            assert!(l.try_acquire());
        }
        assert!(!l.try_acquire());
        let snap = l.snapshot();
        assert_eq!(snap.consumed_total, 5);
        assert_eq!(snap.enqueued_total, 6);
        assert_eq!(snap.skipped_total, 1);
    }

    #[test]
    fn refill_restores_tokens_over_time() {
        let l = GlobalLimiter::new(60); // 1 token/sec
        for _ in 0..60 {
            assert!(l.try_acquire());
        }
        assert!(!l.try_acquire());

        // 2 秒分巻き戻して時間が進んだ風にする
        {
            let mut inner = l.inner.lock().unwrap();
            inner.last_refill = Instant::now() - Duration::from_secs(2);
        }
        assert!(l.try_acquire());
        assert!(l.try_acquire());
        assert!(!l.try_acquire());
    }

    #[test]
    fn paused_blocks_all_acquires() {
        let l = GlobalLimiter::new(120);
        l.set_paused(true);
        assert!(!l.try_acquire());
        assert!(!l.try_acquire());
        let snap = l.snapshot();
        assert_eq!(snap.consumed_total, 0);
        assert_eq!(snap.skipped_total, 2);
        assert!(snap.paused);

        l.set_paused(false);
        assert!(l.try_acquire());
    }

    #[test]
    fn zero_capacity_blocks_everything() {
        let l = GlobalLimiter::new(0);
        assert!(!l.try_acquire());
        let snap = l.snapshot();
        assert_eq!(snap.capacity, 0);
        assert_eq!(snap.consumed_total, 0);
    }
}
