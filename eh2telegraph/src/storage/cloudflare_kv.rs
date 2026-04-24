use std::{sync::Arc, time::Duration};

use cloudflare_kv_proxy::{Client, ClientError, NotFoundMapping};
use serde::{de::DeserializeOwned, Deserialize, Serialize};

use crate::config;

use super::{KVStorage, SimpleMemStorage};

const TIMEOUT: Duration = Duration::from_secs(3);
const CONFIG_KEY: &str = "worker_kv";
const DEFAULT_EXPIRE_SEC: u64 = 60 * 60 * 24 * 60; // 60 days
const DEFAULT_CACHE_SIZE: usize = 10240;

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StorageMode {
    Cloudflare,
    Memory,
    Auto,
}

impl Default for StorageMode {
    fn default() -> Self {
        StorageMode::Cloudflare
    }
}

#[derive(Debug, Deserialize)]
pub struct CFConfig {
    pub endpoint: Option<String>,
    pub token: Option<String>,
    pub cache_size: Option<usize>,
    pub expire_sec: Option<u64>,

    #[serde(default)]
    pub mode: StorageMode,
}

#[derive(Clone, Debug)]
pub struct CFStorage {
    client: Arc<Client>,
}

impl CFStorage {
    pub fn new<T, E>(
        endpoint: E,
        token: T,
        cache_size: usize,
        expire: Duration,
    ) -> Result<Self, ClientError>
    where
        T: Into<String>,
        E: Into<String>,
    {
        let client = Client::new(endpoint, token, TIMEOUT, cache_size, expire)?;
        Ok(Self {
            client: Arc::new(client),
        })
    }

    pub fn new_from_config(config: CFConfig) -> anyhow::Result<Self> {
        let endpoint = config
            .endpoint
            .ok_or_else(|| anyhow::anyhow!("worker_kv.endpoint is required for cloudflare mode"))?;

        let token = config
            .token
            .ok_or_else(|| anyhow::anyhow!("worker_kv.token is required for cloudflare mode"))?;

        let cache_size = config.cache_size.unwrap_or(DEFAULT_CACHE_SIZE);
        let expire_sec = config.expire_sec.unwrap_or(DEFAULT_EXPIRE_SEC);

        Self::new(endpoint, token, cache_size, Duration::from_secs(expire_sec))
            .map_err(Into::into)
    }
}

impl<T> KVStorage<T> for CFStorage
where
    T: DeserializeOwned + Serialize + Send + Sync,
{
    async fn get(&self, key: &str) -> anyhow::Result<Option<T>> {
        self.client
            .get(key)
            .await
            .map_not_found_to_option()
            .map_err(Into::into)
    }

    async fn set(&self, key: String, value: T, expire_ttl: Option<usize>) -> anyhow::Result<()> {
        if let Some(ttl) = expire_ttl {
            anyhow::bail!(
                "CFStorage does not support per-key expire_ttl with current client; requested ttl={ttl}s for key={key}"
            );
        }

        self.client
            .put(&key, &value)
            .await
            .map_err(Into::into)
    }

    async fn delete(&self, key: &str) -> anyhow::Result<()> {
        self.client
            .delete(key)
            .await
            .map_err(Into::into)
    }
}

#[derive(Clone, Debug)]
pub enum CFOrMemStorage<T> {
    Mem(SimpleMemStorage<T>),
    CF(CFStorage),
}

impl<T> CFOrMemStorage<T> {
    pub fn new_from_config() -> anyhow::Result<Self> {
        let config: Option<CFConfig> = config::parse(CONFIG_KEY)?;

        match config {
            Some(config) => Self::from_config(config),
            None => anyhow::bail!(
                "cloudflare worker config(key: worker_kv) not found; \
                 set worker_kv.mode = memory explicitly if you want in-memory cache"
            ),
        }
    }

    fn from_config(config: CFConfig) -> anyhow::Result<Self> {
        match config.mode {
            StorageMode::Cloudflare => {
                let storage = CFStorage::new_from_config(config)?;
                Ok(Self::CF(storage))
            }

            StorageMode::Memory => {
                tracing::warn!("using in-memory cache by explicit configuration");
                Ok(Self::Mem(SimpleMemStorage::<T>::default()))
            }

            StorageMode::Auto => match CFStorage::new_from_config(config) {
                Ok(storage) => Ok(Self::CF(storage)),
                Err(e) => {
                    tracing::warn!(
                        "cloudflare cache unavailable, falling back to memory because worker_kv.mode=auto: {e:?}"
                    );
                    Ok(Self::Mem(SimpleMemStorage::<T>::default()))
                }
            },
        }
    }
}

impl<T> KVStorage<T> for CFOrMemStorage<T>
where
    T: Clone + Send + Sync,
    CFStorage: KVStorage<T>,
{
    async fn get(&self, key: &str) -> anyhow::Result<Option<T>> {
        match self {
            Self::Mem(inner) => inner.get(key).await,
            Self::CF(inner) => inner.get(key).await,
        }
    }

    async fn set(&self, key: String, value: T, expire_ttl: Option<usize>) -> anyhow::Result<()> {
        match self {
            Self::Mem(inner) => inner.set(key, value, expire_ttl).await,
            Self::CF(inner) => inner.set(key, value, expire_ttl).await,
        }
    }

    async fn delete(&self, key: &str) -> anyhow::Result<()> {
        match self {
            Self::Mem(inner) => inner.delete(key).await,
            Self::CF(inner) => inner.delete(key).await,
        }
    }
}