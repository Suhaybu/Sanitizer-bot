//! Handles in-memory caching of server configs. (This file is primarily written by an LLM)

use std::num::NonZeroUsize;
use std::sync::{Mutex, MutexGuard, PoisonError};

use dashmap::{DashMap, Entry};
use lru::LruCache;

use crate::db::ServerConfig;

/// Returns the ConfigCache.
pub fn load() -> &'static ConfigCache {
    crate::CONFIG_CACHE
        .get()
        .expect("CONFIG_CACHE not initialized")
}

#[derive(Debug)]
pub struct ConfigCache {
    cache: DashMap<u64, ServerConfig>,
    lru: Mutex<LruCache<u64, ()>>,
}

impl ConfigCache {
    pub fn new() -> Self {
        let cache_capacity =
            NonZeroUsize::new(1000).expect("Capacity must be > 0, please check source code.");
        Self {
            cache: DashMap::new(),
            lru: Mutex::new(LruCache::new(cache_capacity)),
        }
    }

    /// Gets config from cache, else retrieves it from db and adds to cache.
    pub async fn get_or_fetch(&self, guild_id: u64) -> anyhow::Result<ServerConfig> {
        if let Some(config) = self.cache.get(&guild_id) {
            self.touch(guild_id, false);
            tracing::debug!("Found Server Config in cache");
            return Ok(*config);
        }

        tracing::debug!("Could not find guild in cache, retrieving from database.");
        let config = ServerConfig::get_or_default(guild_id).await?;
        self.upsert(guild_id, config, /* overwrite */ false);

        Ok(config)
    }

    // Update the server config in the database and cache.
    pub async fn update_config(&self, guild_id: u64, config: ServerConfig) -> anyhow::Result<()> {
        // Updates the database with change.
        config.save().await?;
        // Updates the cache
        self.upsert(guild_id, config, /* overwrite */ true);

        Ok(())
    }

    /// Inserts or updates `guild_id` in the cache, atomically via DashMap's
    /// per-shard entry API (so concurrent callers can't double-insert or
    /// race the eviction count). If the key is already present:
    ///   - `overwrite = true`  -> replaces the stored value (explicit updates)
    ///   - `overwrite = false` -> leaves the existing value, just promotes it
    ///
    /// New keys are always inserted and pushed into the LRU, evicting the
    /// least-recently-used entry if that puts the cache over capacity.
    fn upsert(&self, guild_id: u64, config: ServerConfig, overwrite: bool) {
        let is_new = match self.cache.entry(guild_id) {
            Entry::Occupied(mut entry) => {
                if overwrite {
                    entry.insert(config);
                }
                false
            }
            Entry::Vacant(entry) => {
                entry.insert(config);
                true
            }
        };

        self.touch(guild_id, is_new);
    }

    /// Updates the LRU queue for `guild_id`. If `is_new`, this may evict the
    /// least-recently-used entry from the DashMap to stay within capacity;
    /// otherwise it just promotes the existing entry.
    fn touch(&self, guild_id: u64, is_new: bool) {
        match self.lru.lock() {
            Ok(mut lru) => {
                if is_new {
                    if let Some((evicted_id, _)) = lru.push(guild_id, ()) {
                        self.cache.remove(&evicted_id);
                        tracing::debug!("Evicted guild_id {} from config cache", evicted_id);
                    }
                } else {
                    lru.promote(&guild_id);
                }
            }
            Err(e) => self.handle_poison(e, guild_id),
        }
    }

    /// Recovers from a poisoned LRU mutex, clearing both structures together
    /// so they can't end up desynced.
    fn handle_poison(&self, e: PoisonError<MutexGuard<'_, LruCache<u64, ()>>>, guild_id: u64) {
        tracing::error!("LRU lock poisoned, attempting recovery...");

        let mut lru = e.into_inner();
        lru.clear();
        self.cache.clear();
        lru.push(guild_id, ());

        tracing::warn!("Cache and LRU cleared and reset after poison recovery.");
    }
}
