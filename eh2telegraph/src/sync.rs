use crate::{
    collector::{
        AlbumMeta, Collector, ImageMeta, Param, Registry, URL_FROM_TEXT_RE,
        URL_FROM_URL_RE,
    },
    http_proxy::ProxiedClient,
    storage::{cloudflare_kv::CFStorage, KVStorage},
    stream::{AsyncStream, Buffered},
    telegraph::{
        types::{Node, NodeElement, NodeElementAttr, Page, PageCreate, Tag},
        RandomAccessToken, Telegraph, TelegraphError,
    },
    util::match_first_group,
    util::public_image_url,
};

const ERR_THRESHOLD: usize = 10;
const DEFAULT_CONCURRENT: usize = 20;

#[derive(thiserror::Error, Debug)]
pub enum UploadError<SE> {
    #[error("stream error {0}")]
    Stream(SE),
    #[error("telegraph error {0}")]
    Reqwest(#[from] TelegraphError),
}

#[derive(Debug, Clone)]
pub struct SyncResult {
    pub page_url: String,
    pub title: Option<String>,
    pub authors: Option<Vec<String>>,
    pub tags: Option<Vec<String>>,
}

pub struct Synchronizer<C = CFStorage> {
    pub image_proxy_base: String,
    tg: Telegraph<RandomAccessToken, ProxiedClient>,
    limit: Option<usize>,

    author_name: Option<String>,
    author_url: Option<String>,
    cache_ttl: Option<usize>,

    registry: Registry,
    cache: C,
}

impl<CACHE> Synchronizer<CACHE>
where
    CACHE: KVStorage<String>,
{
    // cache ttl is 45 days
    const DEFAULT_CACHE_TTL: usize = 3600 * 24 * 45;

    pub fn new(
        tg: Telegraph<RandomAccessToken, ProxiedClient>,
        registry: Registry,
        cache: CACHE,
        image_proxy_base: String,
    ) -> Self {
        Self {
            tg,
            limit: None,
            author_name: None,
            author_url: None,
            cache_ttl: None,
            registry,
            cache,
            image_proxy_base,
        }
    }

    pub fn with_concurrent_limit(mut self, limit: usize) -> Self {
        self.limit = Some(limit);
        self
    }

    pub fn with_author<S: Into<String>>(mut self, name: Option<S>, url: Option<S>) -> Self {
        self.author_name = name.map(Into::into);
        self.author_url = url.map(Into::into);
        self
    }

    pub fn with_cache_ttl(mut self, ttl: Option<usize>) -> Self {
        self.cache_ttl = ttl;
        self
    }

    pub async fn delete_cache(&self, key: &str) -> anyhow::Result<()> {
        self.cache.delete(key).await
    }

    pub async fn sync<C: Collector>(&self, path: String) -> anyhow::Result<SyncResult>
    where
        Registry: Param<C>,
        C::FetchError: Into<anyhow::Error> + Send + 'static,
        C::StreamError:
            Into<anyhow::Error> + std::fmt::Debug + std::fmt::Display + Send + Sync + 'static,
        C::ImageStream: Send + 'static,
        <C::ImageStream as AsyncStream>::Future: Send + 'static,
    {
        let cache_key = format!("{}|{}", C::name(), path);

        if let Ok(Some(v)) = self.cache.get(&cache_key).await {
            tracing::info!("[cache] hit key {cache_key}");
            return Ok(SyncResult {
                page_url: v,
                title: None,
                authors: None,
                tags: None,
            });
        }

        tracing::info!("[cache] miss key {cache_key}");

        let collector: &C = self.registry.get();
        let (meta, stream) = collector.fetch(path).await.map_err(Into::into)?;

        let title = meta.name.clone();
        let authors = meta.authors.clone();
        let tags = meta.tags.clone();

        let page = self
            .sync_stream(meta, stream)
            .await
            .map_err(anyhow::Error::from)?;

        let _ = self
            .cache
            .set(
                cache_key,
                page.url.clone(),
                Some(self.cache_ttl.unwrap_or(Self::DEFAULT_CACHE_TTL)),
            )
            .await;

        Ok(SyncResult {
            page_url: page.url,
            title: Some(title),
            authors,
            tags,
        })
    }

    pub async fn sync_stream<S, SE>(
        &self,
        meta: AlbumMeta,
        stream: S,
    ) -> Result<Page, UploadError<SE>>
    where
        SE: Send + std::fmt::Debug + 'static,
        S: AsyncStream<Item = Result<ImageMeta, SE>>,
        S::Future: Send + 'static,
    {
        let buffered_stream = Buffered::new(stream, self.limit.unwrap_or(DEFAULT_CONCURRENT));
        let r = self.inner_sync_stream(meta, buffered_stream).await;
        match &r {
            Ok(p) => {
                tracing::info!("[sync] sync success with url {}", p.url);
            }
            Err(e) => {
                tracing::error!("[sync] sync fail! {e:?}");
            }
        }
        r
    }

    async fn inner_sync_stream<S, SE>(
        &self,
        meta: AlbumMeta,
        mut stream: S,
    ) -> Result<Page, UploadError<SE>>
    where
        S: AsyncStream<Item = Result<ImageMeta, SE>>,
    {
        let mut err_count = 0;
        let mut uploaded = Vec::new();

        while let Some(fut) = stream.next() {
            let image_meta = match fut.await {
                Err(e) => {
                    err_count += 1;
                    if err_count > ERR_THRESHOLD {
                        return Err(UploadError::Stream(e));
                    }
                    continue;
                }
                Ok(meta) => {
                    err_count = 0;
                    meta
                }
            };
            let src = public_image_url(&self.image_proxy_base, &image_meta.url);
            tracing::info!("proxy image src = {}", src);
            uploaded.push(UploadedImage {
                meta: image_meta,
                src,
            });
        }
        tracing::info!("uploaded total count after loop = {}", uploaded.len());
        const PAGE_SIZE_LIMIT: usize = 48 * 1024;
        let mut chunks = Vec::with_capacity(8);
        chunks.push(Vec::new());
        let mut last_chunk_size = 0;
        for item in uploaded.into_iter().map(Into::<Node>::into) {
            let item_size = item.estimate_size();
            if last_chunk_size + item_size > PAGE_SIZE_LIMIT {
                chunks.push(Vec::new());
                last_chunk_size = 0;
            }
            last_chunk_size += item_size;
            chunks.last_mut().unwrap().push(item);
        }

        let mut last_page: Option<Page> = None;
        let title = meta.name.replace('|', "");
        while let Some(last_chunk) = chunks.pop() {
            let mut content = last_chunk;
            write_footer(
                &mut content,
                meta.link.as_str(),
                last_page.as_ref().map(|p| p.url.as_str()),
            );
            let title = match chunks.len() {
                0 => title.clone(),
                n => format!("{}-Page{}", title, n + 1),
            };
            tracing::debug!("create page with content: {content:?}");
            let page = self
                .tg
                .create_page(&PageCreate {
                    title,
                    content,
                    author_name: self
                        .author_name
                        .clone()
                        .or_else(|| meta.authors.as_ref().map(|x| x.join(", "))),
                    author_url: self.author_url.clone(),
                })
                .await
                .map_err(UploadError::Reqwest)?;

            last_page = Some(page);
        }
        Ok(last_page.unwrap())
    }
}

fn write_footer(content: &mut Vec<Node>, original_link: &str, next_page: Option<&str>) {
    if let Some(page) = next_page {
        content.push(np!(na!(@page, nt!("Next Page"))));
    }
    content.push(np!(
        nt!("Generated by "),
        na!(@"https://github.com/DrCMWither/eh2telegraph", nt!("eh2telegraph"))
    ));
    content.push(np!(
        nt!("Original link: "),
        na!(@original_link, nt!(original_link))
    ));
}

impl Synchronizer {
    pub fn match_url_from_text(content: &str) -> Option<&str> {
        match_first_group(&URL_FROM_TEXT_RE, content)
    }

    pub fn match_url_from_url(content: &str) -> Option<&str> {
        match_first_group(&URL_FROM_URL_RE, content)
    }
}

struct UploadedImage {
    #[allow(unused)]
    meta: ImageMeta,
    src: String,
}

// Size: {"tag":"img","attrs":{"src":"https://telegra.ph..."}}
impl From<UploadedImage> for Node {
    fn from(i: UploadedImage) -> Self {
        if i.src.starts_with("http://") || i.src.starts_with("https://") {
            Node::new_image(i.src)
        } else {
            Node::new_image(format!("https://telegra.ph{}", i.src))
        }
    }
}
