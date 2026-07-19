use crate::{
    collector::{
        AlbumMeta, Collector, ImageMeta, Param, Registry, URL_FROM_TEXT_RE, URL_FROM_URL_RE,
    },
    http_client::rand_ua,
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
use futures::{future, stream, StreamExt};
use reqwest::header::USER_AGENT;
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashSet, VecDeque},
    convert::Infallible,
    time::Duration,
};

const ERR_THRESHOLD: usize = 10;
const DEFAULT_CONCURRENT: usize = 20;
const PREWARM_CONCURRENT: usize = 8;
const PREWARM_TIMEOUT_SECS: u64 = 30;
const MESSAGE_META_CACHE_SUFFIX: &str = "|message_meta_v1";

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

/// A gallery whose metadata and image URLs have already been fetched.
///
/// Batch mode stores metadata and image URLs in memory. Image bytes continue
/// to be served by the configured image proxy, which is prewarmed during
/// `stash` so final publication mostly uses hot cache entries.
#[derive(Debug, Clone)]
pub struct StashedGallery {
    pub source_url: String,
    pub meta: AlbumMeta,
    pub images: Vec<ImageMeta>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedMessageMeta {
    pub title: Option<String>,
    pub authors: Option<Vec<String>>,
    pub tags: Option<Vec<String>>,
}

impl CachedMessageMeta {
    fn from_sync_result(result: &SyncResult) -> Self {
        Self {
            title: result.title.clone(),
            authors: result.authors.clone(),
            tags: result.tags.clone(),
        }
    }

    fn into_sync_result(self, page_url: String) -> SyncResult {
        SyncResult {
            page_url,
            title: self.title,
            authors: self.authors,
            tags: self.tags,
        }
    }
}

fn message_meta_cache_key(cache_key: &str) -> String {
    format!("{cache_key}{MESSAGE_META_CACHE_SUFFIX}")
}

pub struct Synchronizer<C = CFStorage> {
    pub image_proxy_base: String,
    tg: Telegraph<RandomAccessToken, ProxiedClient>,
    limit: Option<usize>,
    author_name: Option<String>,
    author_url: Option<String>,
    cache_ttl: Option<usize>,
    registry: Registry,
    prewarm_client: reqwest::Client,
    cache: C,
}

impl<CACHE> Synchronizer<CACHE>
where
    CACHE: KVStorage<String>,
{
    // Cache TTL is 45 days.
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
            prewarm_client: reqwest::Client::builder()
                .timeout(Duration::from_secs(PREWARM_TIMEOUT_SECS))
                .build()
                .expect("unable to build prewarm reqwest client"),
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

    fn effective_cache_ttl(&self) -> usize {
        self.cache_ttl.unwrap_or(Self::DEFAULT_CACHE_TTL)
    }

    async fn write_page_url_cache(&self, cache_key: &str, page_url: &str) {
        if let Err(e) = self
            .cache
            .set(
                cache_key.to_owned(),
                page_url.to_owned(),
                Some(self.effective_cache_ttl()),
            )
            .await
        {
            tracing::warn!("[cache] failed to write page url cache key {cache_key}: {e}");
        }
    }

    async fn write_message_meta_cache(&self, meta_cache_key: &str, result: &SyncResult) {
        let payload = match serde_json::to_string(&CachedMessageMeta::from_sync_result(result)) {
            Ok(payload) => payload,
            Err(e) => {
                tracing::warn!(
                    "[cache] failed to serialize message metadata for key {meta_cache_key}: {e}"
                );
                return;
            }
        };

        if let Err(e) = self
            .cache
            .set(
                meta_cache_key.to_owned(),
                payload,
                Some(self.effective_cache_ttl()),
            )
            .await
        {
            tracing::warn!(
                "[cache] failed to write message metadata cache key {meta_cache_key}: {e}"
            );
        }
    }

    async fn read_message_meta_cache(&self, meta_cache_key: &str) -> Option<CachedMessageMeta> {
        let payload = match self.cache.get(meta_cache_key).await {
            Ok(Some(payload)) => payload,
            Ok(None) => return None,
            Err(e) => {
                tracing::warn!(
                    "[cache] failed to read message metadata cache key {meta_cache_key}: {e}"
                );
                return None;
            }
        };

        match serde_json::from_str::<CachedMessageMeta>(&payload) {
            Ok(meta) => Some(meta),
            Err(e) => {
                tracing::warn!(
                    "[cache] failed to parse message metadata cache key {meta_cache_key}: {e}"
                );
                None
            }
        }
    }

    async fn prewarm_page(&self, page_url: &str, image_urls: Vec<String>) {
        tracing::info!(
            "[prewarm] fetching Telegraph page {} and {} image(s)",
            page_url,
            image_urls.len()
        );

        if let Err(e) = prewarm_fetch(self.prewarm_client.clone(), page_url.to_owned()).await {
            tracing::warn!("[prewarm] failed to fetch Telegraph page {}: {}", page_url, e);
        }

        let total = image_urls.len();
        if total == 0 {
            return;
        }

        let client = self.prewarm_client.clone();
        let mut tasks = stream::iter(image_urls.into_iter().map(move |url| {
            let client = client.clone();
            async move {
                let display_url = url.clone();
                prewarm_fetch(client, url)
                    .await
                    .map_err(|e| (display_url, e))
            }
        }))
        .buffer_unordered(PREWARM_CONCURRENT);

        let mut ok = 0usize;
        let mut failed = 0usize;
        while let Some(result) = tasks.next().await {
            match result {
                Ok(()) => ok += 1,
                Err((url, e)) => {
                    failed += 1;
                    tracing::warn!("[prewarm] failed to fetch image {}: {}", url, e);
                }
            }
        }

        if failed == 0 {
            tracing::info!(
                "[prewarm] finished Telegraph page {}; warmed {}/{} image(s)",
                page_url,
                ok,
                total
            );
        } else {
            tracing::warn!(
                "[prewarm] finished Telegraph page {}; warmed {}/{} image(s), failed {}",
                page_url,
                ok,
                total,
                failed
            );
        }
    }

    async fn prewarm_stashed_images(&self, image_urls: Vec<String>) {
        let total = image_urls.len();
        if total == 0 {
            return;
        }

        tracing::info!("[batch] prewarming {} stashed image(s)", total);
        let client = self.prewarm_client.clone();
        let mut tasks = stream::iter(image_urls.into_iter().map(move |url| {
            let client = client.clone();
            async move {
                let display_url = url.clone();
                prewarm_fetch(client, url)
                    .await
                    .map_err(|error| (display_url, error))
            }
        }))
        .buffer_unordered(PREWARM_CONCURRENT);

        let mut warmed = 0usize;
        let mut failed = 0usize;
        while let Some(result) = tasks.next().await {
            match result {
                Ok(()) => warmed += 1,
                Err((url, error)) => {
                    failed += 1;
                    tracing::warn!("[batch] failed to prewarm image {}: {}", url, error);
                }
            }
        }

        tracing::info!(
            "[batch] image prewarm finished; warmed {}/{}, failed {}",
            warmed,
            total,
            failed
        );
    }

    pub async fn delete_cache(&self, key: &str) -> anyhow::Result<()> {
        self.cache.delete(key).await?;
        let meta_key = message_meta_cache_key(key);
        if let Err(e) = self.cache.delete(&meta_key).await {
            tracing::warn!("[cache] failed to delete message metadata cache key {meta_key}: {e}");
        }
        Ok(())
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
        let meta_cache_key = message_meta_cache_key(&cache_key);

        if let Ok(Some(page_url)) = self.cache.get(&cache_key).await {
            tracing::info!("[cache] hit key {cache_key}");
            if let Some(cached_meta) = self.read_message_meta_cache(&meta_cache_key).await {
                return Ok(cached_meta.into_sync_result(page_url));
            }

            tracing::info!(
                "[cache] hit URL-only cache key {cache_key}; refreshing message metadata only"
            );
            let collector: &C = self.registry.get();
            match collector.fetch(path.clone()).await {
                Ok((meta, _stream)) => {
                    let result = SyncResult {
                        page_url,
                        title: Some(meta.name),
                        authors: meta.authors,
                        tags: meta.tags,
                    };
                    self.write_message_meta_cache(&meta_cache_key, &result).await;
                    return Ok(result);
                }
                Err(e) => {
                    let err: anyhow::Error = e.into();
                    tracing::warn!(
                        "[cache] failed to refresh message metadata for key {cache_key}: {err}"
                    );
                    return Ok(SyncResult {
                        page_url,
                        title: None,
                        authors: None,
                        tags: None,
                    });
                }
            }
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
        let result = SyncResult {
            page_url: page.url,
            title: Some(title),
            authors,
            tags,
        };
        self.write_page_url_cache(&cache_key, &result.page_url).await;
        self.write_message_meta_cache(&meta_cache_key, &result).await;
        Ok(result)
    }

    /// Fetch a gallery into a FIFO batch stash and prewarm its proxied images.
    pub async fn stash<C: Collector>(
        &self,
        path: String,
        source_url: String,
    ) -> anyhow::Result<StashedGallery>
    where
        Registry: Param<C>,
        C::FetchError: Into<anyhow::Error> + Send + 'static,
        C::StreamError:
            Into<anyhow::Error> + std::fmt::Debug + std::fmt::Display + Send + Sync + 'static,
        C::ImageStream: Send + 'static,
        <C::ImageStream as AsyncStream>::Future: Send + 'static,
    {
        let collector: &C = self.registry.get();
        let (meta, mut image_stream) = collector.fetch(path).await.map_err(Into::into)?;
        let mut images = Vec::with_capacity(image_stream.size_hint().0);
        let mut consecutive_errors = 0usize;

        while let Some(future) = image_stream.next() {
            match future.await {
                Ok(image) => {
                    consecutive_errors = 0;
                    images.push(image);
                }
                Err(error) => {
                    consecutive_errors += 1;
                    if consecutive_errors > ERR_THRESHOLD {
                        return Err(error.into());
                    }
                    tracing::warn!(
                        "[batch] collector {} skipped an image: {}",
                        C::name(),
                        error
                    );
                }
            }
        }

        if images.is_empty() {
            return Err(anyhow::anyhow!("gallery contains no usable images"));
        }

        let proxied_urls = images
            .iter()
            .map(|image| public_image_url(&self.image_proxy_base, &image.url))
            .collect();
        self.prewarm_stashed_images(proxied_urls).await;

        Ok(StashedGallery {
            source_url,
            meta,
            images,
        })
    }

    /// Publish already-stashed galleries as one Telegraph article.
    ///
    /// Gallery and image order remain FIFO. Authors and tags use a stable
    /// union. `source_links` includes every accepted link, including failed
    /// galleries, so the article footer records the complete batch input.
    pub async fn sync_stashed_batch(
        &self,
        galleries: Vec<StashedGallery>,
        source_links: Vec<String>,
    ) -> anyhow::Result<SyncResult> {
        if galleries.is_empty() {
            return Err(anyhow::anyhow!("batch contains no stashed galleries"));
        }

        let fallback_links = galleries
            .iter()
            .map(|gallery| gallery.source_url.clone())
            .collect::<Vec<_>>();
        let gallery_count = galleries.len();
        let first_title = galleries[0].meta.name.clone();
        let title = if gallery_count == 1 {
            first_title
        } else {
            format!("{} (+{} galleries)", first_title, gallery_count - 1)
        };

        let mut authors = Vec::new();
        let mut author_set = HashSet::new();
        let mut tags = Vec::new();
        let mut tag_set = HashSet::new();
        let mut images = Vec::new();

        for gallery in galleries {
            if let Some(gallery_authors) = gallery.meta.authors {
                for author in gallery_authors {
                    if author_set.insert(author.clone()) {
                        authors.push(author);
                    }
                }
            }
            if let Some(gallery_tags) = gallery.meta.tags {
                for tag in gallery_tags {
                    if tag_set.insert(tag.clone()) {
                        tags.push(tag);
                    }
                }
            }
            images.extend(gallery.images);
        }

        if images.is_empty() {
            return Err(anyhow::anyhow!("batch contains no usable images"));
        }

        let links = if source_links.is_empty() {
            fallback_links
        } else {
            source_links
        };
        let meta = AlbumMeta {
            link: links[0].clone(),
            name: title.clone(),
            class: Some("batch".to_owned()),
            description: Some(format!("FIFO batch of {gallery_count} galleries")),
            authors: (!authors.is_empty()).then_some(authors.clone()),
            tags: (!tags.is_empty()).then_some(tags.clone()),
        };

        let image_stream = ReadyImageStream::new(images);
        let page = self
            .sync_stream_with_links(meta, image_stream, links)
            .await
            .map_err(|error| match error {
                UploadError::Reqwest(error) => anyhow::Error::from(error),
                UploadError::Stream(never) => match never {},
            })?;

        Ok(SyncResult {
            page_url: page.url,
            title: Some(title),
            authors: (!authors.is_empty()).then_some(authors),
            tags: (!tags.is_empty()).then_some(tags),
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
        let original_links = vec![meta.link.clone()];
        self.sync_stream_with_links(meta, stream, original_links).await
    }

    async fn sync_stream_with_links<S, SE>(
        &self,
        meta: AlbumMeta,
        stream: S,
        original_links: Vec<String>,
    ) -> Result<Page, UploadError<SE>>
    where
        SE: Send + std::fmt::Debug + 'static,
        S: AsyncStream<Item = Result<ImageMeta, SE>>,
        S::Future: Send + 'static,
    {
        let buffered_stream = Buffered::new(stream, self.limit.unwrap_or(DEFAULT_CONCURRENT));
        let result = self
            .inner_sync_stream(meta, buffered_stream, original_links)
            .await;
        match &result {
            Ok(page) => tracing::info!("[sync] sync success with url {}", page.url),
            Err(error) => tracing::error!("[sync] sync fail! {error:?}"),
        }
        result
    }

    async fn inner_sync_stream<S, SE>(
        &self,
        meta: AlbumMeta,
        mut stream: S,
        original_links: Vec<String>,
    ) -> Result<Page, UploadError<SE>>
    where
        S: AsyncStream<Item = Result<ImageMeta, SE>>,
    {
        let mut err_count = 0;
        let mut uploaded = Vec::new();
        while let Some(future) = stream.next() {
            let image_meta = match future.await {
                Err(error) => {
                    err_count += 1;
                    if err_count > ERR_THRESHOLD {
                        return Err(UploadError::Stream(error));
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
                &original_links,
                last_page.as_ref().map(|page| page.url.as_str()),
            );
            let title = match chunks.len() {
                0 => title.clone(),
                n => format!("{}-Page{}", title, n + 1),
            };
            tracing::debug!("create page with content: {content:?}");
            let image_urls = collect_image_urls(&content);
            let page = self
                .tg
                .create_page(&PageCreate {
                    title,
                    content,
                    author_name: self
                        .author_name
                        .clone()
                        .or_else(|| meta.authors.as_ref().map(|authors| authors.join(", "))),
                    author_url: self.author_url.clone(),
                })
                .await
                .map_err(UploadError::Reqwest)?;
            self.prewarm_page(&page.url, image_urls).await;
            last_page = Some(page);
        }

        Ok(last_page.unwrap())
    }
}

async fn prewarm_fetch(client: reqwest::Client, url: String) -> reqwest::Result<()> {
    let response = client
        .get(&url)
        .header(USER_AGENT, rand_ua())
        .send()
        .await?
        .error_for_status()?;
    drain_response(response).await
}

async fn drain_response(mut response: reqwest::Response) -> reqwest::Result<()> {
    while response.chunk().await?.is_some() {}
    Ok(())
}

fn collect_image_urls(content: &[Node]) -> Vec<String> {
    let mut urls = Vec::new();
    for node in content {
        collect_image_urls_from_node(node, &mut urls);
    }
    urls
}

fn collect_image_urls_from_node(node: &Node, urls: &mut Vec<String>) {
    if let Node::NodeElement(element) = node {
        if let Some(attrs) = &element.attrs {
            if let Some(src) = &attrs.src {
                urls.push(if src.starts_with("http://") || src.starts_with("https://") {
                    src.clone()
                } else {
                    format!("https://telegra.ph{}", src)
                });
            }
        }
        if let Some(children) = &element.children {
            for child in children {
                collect_image_urls_from_node(child, urls);
            }
        }
    }
}

fn write_footer(content: &mut Vec<Node>, original_links: &[String], next_page: Option<&str>) {
    if let Some(page) = next_page {
        content.push(np!(na!(@page, nt!("Next Page"))));
    }
    content.push(np!(
        nt!("Generated by "),
        na!(
            @"https://github.com/DrCMWither/eh2telegraph",
            nt!("eh2telegraph")
        )
    ));

    if original_links.len() == 1 {
        let original_link = original_links[0].as_str();
        content.push(np!(
            nt!("Original link: "),
            na!(@original_link, nt!(original_link))
        ));
    } else if next_page.is_none() {
        // Pages are created from the tail backwards, so this is the physical end of the multi-page Telegraph article.
        content.push(np!(nt!("Original links:")));
        for (index, original_link) in original_links.iter().enumerate() {
            let href = original_link.as_str();
            let label = format!("{}. {}", index + 1, original_link);
            content.push(np!(na!(@href, nt!(label.as_str()))));
        }
    }
}

impl Synchronizer {
    pub fn match_url_from_text(content: &str) -> Option<&str> {
        match_first_group(&URL_FROM_TEXT_RE, content)
    }

    pub fn match_url_from_url(content: &str) -> Option<&str> {
        match_first_group(&URL_FROM_URL_RE, content)
    }
}

struct ReadyImageStream {
    images: VecDeque<ImageMeta>,
}

impl ReadyImageStream {
    fn new(images: Vec<ImageMeta>) -> Self {
        Self {
            images: images.into_iter().collect(),
        }
    }
}

impl AsyncStream for ReadyImageStream {
    type Item = Result<ImageMeta, Infallible>;
    type Future = future::Ready<Self::Item>;

    fn next(&mut self) -> Option<Self::Future> {
        self.images
            .pop_front()
            .map(|image| future::ready(Ok(image)))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let len = self.images.len();
        (len, Some(len))
    }
}

struct UploadedImage {
    #[allow(unused)]
    meta: ImageMeta,
    src: String,
}

// Size: {"tag":"img","attrs":{"src":"https://telegra.ph..."}}
impl From<UploadedImage> for Node {
    fn from(image: UploadedImage) -> Self {
        if image.src.starts_with("http://") || image.src.starts_with("https://") {
            Node::new_image(image.src)
        } else {
            Node::new_image(format!("https://telegra.ph{}", image.src))
        }
    }
}
