/// e-hentai collector.
/// Host matching: e-hentai.org
use again::RetryPolicy;
use ipnet::Ipv6Net;
use regex::Regex;
use reqwest::header;
use std::time::Duration;
use tokio::time::timeout;

use crate::{
    http_client::{GhostClient, GhostClientBuilder},
    stream::AsyncStream,
    util::{get_bytes, get_string, match_first_group},
};

use super::{
    utils::paged::{PageFormatter, PageIndicator, Paged},
    AlbumMeta, Collector, ImageData, ImageMeta,
};

const HOST: &str = "e-hentai.org";
const COOKIE_NW: &str = "nw=1";

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const IMAGE_PAGE_TIMEOUT: Duration = Duration::from_secs(20);
const IMAGE_DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(40);

lazy_static::lazy_static! {
    static ref PAGE_RE: Regex =
        Regex::new(r#"<a\s+href="(https://e-hentai\.org/s/[^"]+)""#).unwrap();

    static ref IMG_RE: Regex =
        Regex::new(r#"<img\s+id="img"\s+src="([^"]+)""#).unwrap();

    static ref TITLE_RE: Regex =
        Regex::new(r#"<h1\s+id="gn">(.*?)</h1>"#).unwrap();

    static ref RETRY_POLICY: RetryPolicy = RetryPolicy::fixed(Duration::from_millis(200))
        .with_max_retries(5)
        .with_jitter(true);

    static ref RAW_CLIENT: reqwest::Client = reqwest::Client::builder()
    .timeout(REQUEST_TIMEOUT)
    .build()
    .expect("failed to build e-hentai raw client");
}

#[derive(Debug, Clone, Default)]
pub struct EHCollector {
    client: GhostClient,
    raw_client: reqwest::Client,
}

impl EHCollector {
    pub fn new(prefix: Option<Ipv6Net>) -> Self {
        Self {
            client: ghost_client_builder().build(prefix),
            raw_client: raw_client(),
        }
    }

    pub fn new_from_config() -> anyhow::Result<Self> {
        Ok(Self {
            client: ghost_client_builder().build_from_config()?,
            raw_client: raw_client(),
        })
    }
}

fn ghost_client_builder() -> GhostClientBuilder {
    let mut request_headers = header::HeaderMap::new();
    request_headers.insert(header::COOKIE, header::HeaderValue::from_static(COOKIE_NW));

    GhostClientBuilder::default()
        .with_default_headers(request_headers)
        .with_cf_resolve(&[HOST])
}

fn raw_client() -> reqwest::Client {
    RAW_CLIENT.clone()
}

fn parse_gallery_path(path: &str) -> anyhow::Result<(String, String)> {
    let mut parts = path.trim_matches('/').split('/');

    match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some("g"), Some(album_id), Some(album_token), None)
            if !album_id.is_empty() && !album_token.is_empty() =>
        {
            Ok((album_id.to_string(), album_token.to_string()))
        }
        _ => Err(anyhow::anyhow!(
            "invalid input path({path}), gallery url is expected, like https://e-hentai.org/g/2127986/da1deffea5"
        )),
    }
}

fn collect_image_page_links(gallery_pages: &[String]) -> Vec<String> {
    gallery_pages
        .iter()
        .flat_map(|page| {
            PAGE_RE
                .captures_iter(page)
                .filter_map(|cap| cap.get(1).map(|m| m.as_str().to_string()))
        })
        .collect()
}

impl Collector for EHCollector {
    type FetchError = anyhow::Error;
    type StreamError = anyhow::Error;
    type ImageStream = EHImageStream;

    #[inline]
    fn name() -> &'static str {
        "e-hentai"
    }

    async fn fetch(
        &self,
        path: String,
    ) -> Result<(AlbumMeta, Self::ImageStream), Self::FetchError> {
        let (album_id, album_token) = parse_gallery_path(&path)?;
        let original_url = format!("https://{HOST}/g/{album_id}/{album_token}");

        tracing::info!("[e-hentai] process {original_url}");

        // Clone client to force changing / refreshing outbound identity if GhostClient supports it.
        let client = self.client.clone();

        let mut paged = Paged::new(
            0,
            EHPageIndicator {
                base: original_url.clone(),
            },
        );

        tracing::info!("[e-hentai] sending gallery metadata request: {original_url}");

        let gallery_pages = timeout(REQUEST_TIMEOUT, paged.pages(&client))
            .await
            .map_err(|_| anyhow::anyhow!("e-hentai gallery page request timed out"))??;

        tracing::info!("[e-hentai] gallery pages received: {}", gallery_pages.len());

        let first_page = gallery_pages
            .first()
            .ok_or_else(|| anyhow::anyhow!("empty gallery page response"))?;

        let title = match_first_group(&TITLE_RE, first_page)
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| format!("e-hentai-{album_id}"));

        let image_page_links = collect_image_page_links(&gallery_pages);

        if image_page_links.is_empty() {
            return Err(anyhow::anyhow!(
                "invalid url, maybe resource has been deleted"
            ));
        }

        tracing::info!(
            "[e-hentai] image page links collected: {}",
            image_page_links.len()
        );

        Ok((
            AlbumMeta {
                link: original_url,
                name: title,
                class: None,
                description: None,
                authors: None,
                tags: None,
            },
            EHImageStream {
                client,
                raw_client: self.raw_client.clone(),
                image_page_links: image_page_links.into_iter(),
            },
        ))
    }
}

#[derive(Debug)]
pub struct EHImageStream {
    client: GhostClient,
    raw_client: reqwest::Client,
    image_page_links: std::vec::IntoIter<String>,
}

impl EHImageStream {
    async fn load_image(
        client: GhostClient,
        raw_client: reqwest::Client,
        image_page_link: String,
    ) -> anyhow::Result<(ImageMeta, ImageData)> {
        let content = timeout(
            IMAGE_PAGE_TIMEOUT,
            RETRY_POLICY.retry(|| async { get_string(&client, &image_page_link).await }),
        )
        .await
        .map_err(|_| anyhow::anyhow!("e-hentai image page request timed out: {image_page_link}"))??;

        let img_url = match_first_group(&IMG_RE, &content)
            .ok_or_else(|| anyhow::anyhow!("unable to find image in page: {image_page_link}"))?
            .to_string();

        let image_data = timeout(
            IMAGE_DOWNLOAD_TIMEOUT,
            RETRY_POLICY.retry(|| async { get_bytes(&raw_client, &img_url).await }),
        )
        .await
        .map_err(|_| anyhow::anyhow!("e-hentai image download timed out: {img_url}"))??;

        tracing::trace!(
            "download e-hentai image with size {}, page: {image_page_link}",
            image_data.len()
        );

        Ok((
            ImageMeta {
                id: image_page_link,
                url: img_url,
                description: None,
            },
            image_data,
        ))
    }
}

impl AsyncStream for EHImageStream {
    type Item = anyhow::Result<(ImageMeta, ImageData)>;
    type Future = crate::stream::BoxFuture<Self::Item>;

    fn next(&mut self) -> Option<Self::Future> {
        let image_page_link = self.image_page_links.next()?;
        let client = self.client.clone();
        let raw_client = self.raw_client.clone();

        Some(Box::pin(async move {
            match EHImageStream::load_image(client, raw_client, image_page_link.clone()).await {
                Ok(r) => Ok(r),
                Err(e) => {
                    tracing::error!("e-hentai image failed: {image_page_link}: {e}");
                    Err(e)
                }
            }
        }))
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.image_page_links.size_hint()
    }
}

struct EHPageIndicator {
    base: String,
}

impl PageFormatter for EHPageIndicator {
    fn format_n(&self, n: usize) -> String {
        format!("{}/?p={}", self.base, n)
    }
}

impl PageIndicator for EHPageIndicator {
    fn is_last_page(&self, content: &str, next_page: usize) -> bool {
        let disabled_next = format!(
            r#"<a href="{}/?p={}" onclick="return false">"#,
            self.base, next_page
        );

        !content.contains(&disabled_next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[ignore]
    #[tokio::test]
    async fn demo() {
        let collector = EHCollector {
            raw_client: raw_client(),
            client: Default::default(),
        };

        let (album, mut image_stream) = collector
            .fetch("/g/2122174/fd2525031e".to_string())
            .await
            .unwrap();

        println!("album: {album:?}");

        let maybe_first_image = image_stream.next().unwrap().await;
        if let Ok((meta, data)) = maybe_first_image {
            println!("first image meta: {meta:?}");
            println!("first image data length: {}", data.len());
        }
    }

    #[test]
    fn parse_gallery_path_accepts_valid_path() {
        let (id, token) = parse_gallery_path("/g/2122174/fd2525031e").unwrap();
        assert_eq!(id, "2122174");
        assert_eq!(token, "fd2525031e");
    }

    #[test]
    fn parse_gallery_path_rejects_invalid_path() {
        assert!(parse_gallery_path("/gallery/2122174/fd2525031e").is_err());
        assert!(parse_gallery_path("/g/2122174").is_err());
        assert!(parse_gallery_path("/g/2122174/fd2525031e/extra").is_err());
    }

    #[test]
    fn regex_match() {
        let h = r#"<div class="gdtm" style="height:170px"><div style="margin:1px auto 0; width:100px; height:140px; background:transparent url(https://ehgt.org/m/002122/2122174-00.jpg) -600px 0 no-repeat"><a href="https://e-hentai.org/s/bd2b37d829/2122174-7"><img alt="007" title="Page 7: 2.png" src="https://ehgt.org/g/blank.gif" style="width:100px; height:139px; margin:-1px 0 0 -1px" /></a></div></div><div class="gdtm" style="height:170px"><div style="margin:1px auto 0; width:100px; height:100px; background:transparent url(https://ehgt.org/m/002122/2122174-00.jpg) -700px 0 no-repeat"><a href="https://e-hentai.org/s/4ca72f757d/2122174-8"><img alt="008" title="Page 8: 3.png" src="https://ehgt.org/g/blank.gif" style="width:100px; height:99px; margin:-1px 0 0 -1px" />"#;

        let links = collect_image_page_links(&[h.to_string()]);

        assert_eq!(links.len(), 2);
        assert_eq!(links[0], "https://e-hentai.org/s/bd2b37d829/2122174-7");
        assert_eq!(links[1], "https://e-hentai.org/s/4ca72f757d/2122174-8");
    }
}