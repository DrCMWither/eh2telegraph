use std::{sync::Arc, time::Duration};

use reqwest::{header, StatusCode};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::Value;

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

#[derive(Debug)]
struct KVClient {
    endpoint: String,
    client: reqwest::Client,
}

impl KVClient {
    fn new(endpoint: impl Into<String>, token: impl Into<String>) -> anyhow::Result<Self> {
        let mut endpoint = endpoint.into();
        if !endpoint.ends_with('/') {
            endpoint.push('/');
        }

        let mut headers = header::HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            header::HeaderValue::from_str(&token.into())?,
        );

        let client = reqwest::Client::builder()
            .default_headers(headers)
            .timeout(TIMEOUT)
            .tcp_nodelay(true)
            .build()?;

        Ok(Self { endpoint, client })
    }

    fn url(&self, key: &str) -> String {
        format!("{}{}", self.endpoint, key)
    }

    async fn get<T: DeserializeOwned>(&self, key: &str) -> anyhow::Result<Option<T>> {
        let resp = self.client.get(self.url(key)).send().await?;

        if resp.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }

        let resp = resp.error_for_status()?;
        let value: Value = resp.json().await?;

        if is_not_found_payload(&value) {
            return Ok(None);
        }

        let payload = unwrap_proxy_payload(value)?;
        Ok(Some(serde_json::from_value(payload)?))
    }

    async fn put<T: Serialize + ?Sized>(
        &self,
        key: &str,
        value: &T,
        ttl: Option<usize>,
    ) -> anyhow::Result<()> {
        let mut req = self.client.put(self.url(key)).json(value);

        if let Some(ttl) = ttl {
            req = req.header("ttl", ttl.to_string());
        }

        let resp = req.send().await?;
        let status = resp.status();
        let text = resp.text().await?;

        if !status.is_success() {
            anyhow::bail!("KV put failed: status={status}, body={text}");
        }

        Ok(())
    }

    async fn delete(&self, key: &str) -> anyhow::Result<()> {
        let resp = self.client.delete(self.url(key)).send().await?;
        let status = resp.status();
        let text = resp.text().await?;

        if !status.is_success() && status != StatusCode::NOT_FOUND {
            anyhow::bail!("KV delete failed: status={status}, body={text}");
        }

        Ok(())
    }
}

fn is_not_found_payload(value: &Value) -> bool {
    let s = value.to_string().to_lowercase();
    s.contains("notfound") || s.contains("not found") || s.contains("not_found")
}

fn unwrap_proxy_payload(value: Value) -> anyhow::Result<Value> {
    if let Some(success) = value.get("success").and_then(Value::as_bool) {
        if !success {
            anyhow::bail!("KV proxy returned failure: {value}");
        }
    }

    if let Some(ok) = value.get("ok").and_then(Value::as_bool) {
        if !ok {
            anyhow::bail!("KV proxy returned failure: {value}");
        }
    }

    for key in ["result", "data", "value"] {
        if let Some(inner) = value.get(key) {
            return Ok(inner.clone());
        }
    }

    Ok(value)
}

#[derive(Clone, Debug)]
pub struct CFStorage {
    client: Arc<KVClient>,
}

impl CFStorage {
    pub fn new<T, E>(
        endpoint: E,
        token: T,
        _cache_size: usize,
        _expire: Duration,
    ) -> anyhow::Result<Self>
    where
        T: Into<String>,
        E: Into<String>,
    {
        Ok(Self {
            client: Arc::new(KVClient::new(endpoint, token)?),
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
    }
}

impl<T> KVStorage<T> for CFStorage
where
    T: DeserializeOwned + Serialize + Send + Sync,
{
    async fn get(&self, key: &str) -> anyhow::Result<Option<T>> {
        self.client.get(key).await
    }

    async fn set(&self, key: String, value: T, expire_ttl: Option<usize>) -> anyhow::Result<()> {
        self.client.put(&key, &value, expire_ttl).await
    }

    async fn delete(&self, key: &str) -> anyhow::Result<()> {
        self.client.delete(key).await
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