use std::collections::HashMap;
use std::path::PathBuf;
use quick_cache::sync::Cache;
use crate::config::Config;
use crate::cache::CachedStat;

pub struct AppState {
    pub config: Config,
    /// Bounded stat cache: holds at most `config.cache_size` entries, evicting
    /// automatically (S3-FIFO), so a bucket with more keys than the bound can't
    /// grow the cache without limit.
    pub cache: Cache<String, CachedStat>,
    pub storage_map: HashMap<String, PathBuf>,
}
