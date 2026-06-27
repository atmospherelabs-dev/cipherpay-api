use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::Instant;
use tokio::sync::RwLock;

static INSTANCE: OnceLock<ScannerMetrics> = OnceLock::new();

pub struct ScannerMetrics {
    pub blocks_scanned: AtomicU64,
    pub last_block_height: AtomicU64,
    pub chain_tip_height: AtomicU64,
    pub payments_detected: AtomicU64,
    pub mempool_txs_checked: AtomicU64,
    pub scan_errors: AtomicU64,
    pub last_block_scan_ms: AtomicU64,
    pub last_mempool_scan_ms: AtomicU64,
    started_at: RwLock<Option<Instant>>,
    last_error: RwLock<Option<(String, Instant)>>,
}

impl ScannerMetrics {
    fn new() -> Self {
        Self {
            blocks_scanned: AtomicU64::new(0),
            last_block_height: AtomicU64::new(0),
            chain_tip_height: AtomicU64::new(0),
            payments_detected: AtomicU64::new(0),
            mempool_txs_checked: AtomicU64::new(0),
            scan_errors: AtomicU64::new(0),
            last_block_scan_ms: AtomicU64::new(0),
            last_mempool_scan_ms: AtomicU64::new(0),
            started_at: RwLock::new(None),
            last_error: RwLock::new(None),
        }
    }

    pub async fn mark_started(&self) {
        *self.started_at.write().await = Some(Instant::now());
    }

    pub async fn uptime_secs(&self) -> u64 {
        self.started_at
            .read()
            .await
            .map(|s| s.elapsed().as_secs())
            .unwrap_or(0)
    }

    pub fn record_blocks_scanned(&self, count: u64) {
        self.blocks_scanned.fetch_add(count, Ordering::Relaxed);
    }

    pub fn set_last_block_height(&self, height: u64) {
        self.last_block_height.store(height, Ordering::Relaxed);
    }

    pub fn set_chain_tip(&self, height: u64) {
        self.chain_tip_height.store(height, Ordering::Relaxed);
    }

    pub fn record_payment_detected(&self) {
        self.payments_detected.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_mempool_txs(&self, count: u64) {
        self.mempool_txs_checked.fetch_add(count, Ordering::Relaxed);
    }

    pub fn record_scan_error(&self, msg: &str) {
        self.scan_errors.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut guard) = self.last_error.try_write() {
            *guard = Some((msg.to_string(), Instant::now()));
        }
    }

    pub async fn last_error(&self) -> Option<(String, u64)> {
        self.last_error.read().await.as_ref().map(|(msg, when)| {
            (msg.clone(), when.elapsed().as_secs())
        })
    }

    pub fn set_last_block_scan_ms(&self, ms: u64) {
        self.last_block_scan_ms.store(ms, Ordering::Relaxed);
    }

    pub fn set_last_mempool_scan_ms(&self, ms: u64) {
        self.last_mempool_scan_ms.store(ms, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> MetricsSnapshot {
        MetricsSnapshot {
            blocks_scanned: self.blocks_scanned.load(Ordering::Relaxed),
            last_block_height: self.last_block_height.load(Ordering::Relaxed),
            chain_tip_height: self.chain_tip_height.load(Ordering::Relaxed),
            payments_detected: self.payments_detected.load(Ordering::Relaxed),
            mempool_txs_checked: self.mempool_txs_checked.load(Ordering::Relaxed),
            scan_errors: self.scan_errors.load(Ordering::Relaxed),
            last_block_scan_ms: self.last_block_scan_ms.load(Ordering::Relaxed),
            last_mempool_scan_ms: self.last_mempool_scan_ms.load(Ordering::Relaxed),
        }
    }
}

#[derive(serde::Serialize)]
pub struct MetricsSnapshot {
    pub blocks_scanned: u64,
    pub last_block_height: u64,
    pub chain_tip_height: u64,
    pub payments_detected: u64,
    pub mempool_txs_checked: u64,
    pub scan_errors: u64,
    pub last_block_scan_ms: u64,
    pub last_mempool_scan_ms: u64,
}

impl MetricsSnapshot {
    pub fn blocks_behind(&self) -> u64 {
        self.chain_tip_height.saturating_sub(self.last_block_height)
    }

    pub fn status(&self) -> &'static str {
        let behind = self.blocks_behind();
        if self.last_block_height == 0 {
            "starting"
        } else if behind <= 2 {
            "healthy"
        } else if behind <= 20 {
            "catching_up"
        } else {
            "behind"
        }
    }
}

pub fn global() -> &'static ScannerMetrics {
    INSTANCE.get_or_init(ScannerMetrics::new)
}
