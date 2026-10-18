use chrono::{DateTime, Utc};
use std::time::Instant;

#[derive(Clone)]
pub struct CachedStat {
    pub size: u64,
    pub mod_time: DateTime<Utc>,
    pub etag: String,
    /// When this entry was stored. Reads older than `stat_cache_ttl_secs` are treated
    /// as misses so out-of-band changes to the file are picked up.
    pub cached_at: Instant,
}
