use serde::Deserialize;

#[derive(Debug, Deserialize, Clone)]
pub struct BucketConfig {
    pub name: String,
    pub storage: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct AuthConfig {
    pub access_key: String,
    pub secret_key: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    pub port: u16,
    pub endpoint: String,
    pub verbose: bool,
    #[serde(default = "default_cache_size")]
    pub cache_size: usize,
    /// Fsync each uploaded object before acknowledging the PUT (default true). Set to
    /// false to trade crash durability for PUT latency, e.g. when the proxy fronts a
    /// cache or replica rather than the source of truth.
    #[serde(default = "default_fsync")]
    pub fsync: bool,
    pub auth: Option<AuthConfig>,
    pub buckets: Vec<BucketConfig>,
}

fn default_cache_size() -> usize {
    10000
}

fn default_fsync() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fsync_defaults_to_true_for_existing_configs() {
        // A config written before the `fsync` option existed must keep durable PUTs.
        let yaml = "port: 8080\nendpoint: \"0.0.0.0\"\nverbose: false\nbuckets: []\n";
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert!(config.fsync);
        assert_eq!(config.cache_size, 10000);
    }
}
