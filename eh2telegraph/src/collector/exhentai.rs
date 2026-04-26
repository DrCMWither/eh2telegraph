use std::time::Duration;

use again::RetryPolicy;
use anyhow::{anyhow, Context};
use ipnet::Ipv6Net;
use regex::Regex;
use reqwest::header::{self, HeaderMap};
use serde::Deserialize;

use crate::{
    config,
    http_client::{GhostClient, GhostClientBuilder},
    stream::AsyncStream,
    util::{get_bytes, get_string, match_first_group},
};

use super::{
    utils::paged::{PageFormatter, PageIndicator, Paged},
    AlbumMeta, Collector, ImageData, ImageMeta,
};

lazy_static::lazy_static! {
    static ref PAGE_RE: Regex =
        Regex::new(r#"<a href="(https://exhentai\.org/s/\w+/[\w-]+)">"#)
            .expect("valid PAGE_RE");

    static ref IMG_RE: Regex =
        Regex::new(r#"<img id="img" src="([^"]+)""#)
            .expect("valid IMG_RE");

    static ref TITLE_RE: Regex =
        Regex::new(r#"<h1 id="gn">(.*?)</h1>"#)
            .expect("valid TITLE_RE");

    static ref RETRY_POLICY: RetryPolicy = RetryPolicy::fixed(Duration::from_millis(200))
        .with_max_retries(5)
        .with_jitter(true);

    static ref RAW_CLIENT: reqwest::Client = reqwest::Client::builder()
        .timeout(TIMEOUT)
        .build()
        .expect("failed to build exhentai raw client");
}

const CONFIG_KEY: &str = "exhentai";
const TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone)]
pub struct EXCollector {
    ghost_client: GhostClient,
    raw_client: reqwest::Client,
}

#[derive(Debug, Deserialize)]
pub struct ExConfig {
    pub ipb_pass_hash: String,
    pub ipb_member_id: String,
    pub igneous: String,
}

#[derive(Debug, Clone)]
struct GalleryPath {
    album_id: String,
    album_token: String,
}

impl GalleryPath {
    fn parse(path: &str) -> anyhow::Result<Self> {
        let parts = path
            .trim_matches('/')
            .split('/')
            .collect::<Vec<_>>();

        match parts.as_slice() {
            ["g", album_id, album_token, ..]
                if !album_id.is_empty() && !album_token.is_empty() =>
            {
                Ok(Self {
                    album_id: (*album_id).to_owned(),
                    album_token: (*album_token).to_owned(),
                })
            }
            _ => Err(anyhow!(
                "invalid input path({path}), gallery url is expected, like https://exhentai.org/g/2129939/01a6e086b9"
            )),
        }
    }

    fn gallery_url(&self) -> String {
        format!(
            "https://exhentai.org/g/{}/{}",
            self.album_id, self.album_token
        )
    }

    fn display_key(&self) -> String {
        format!("{}/{}", self.album_id, self.album_token)
    }
}

impl ExConfig {
    fn build_header(&self) -> anyhow::Result<HeaderMap> {
        let cookie_value = format!(
            "ipb_pass_hash={};ipb_member_id={};igneous={};nw=1",
            self.ipb_pass_hash, self.ipb_member_id, self.igneous
        );

        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            header::HeaderValue::from_str(&cookie_value)
                .context("invalid exhentai cookie header")?,
        );

        Ok(headers)
    }
}

fn raw_client() -> reqwest::Client {
    RAW_CLIENT.clone()
}

impl EXCollector {
    pub fn new(config: &ExConfig, prefix: Option<Ipv6Net>) -> anyhow::Result<Self> {
        Ok(Self {
            ghost_client: GhostClientBuilder::default()
                .with_default_headers(config.build_header()?)
                .with_cf_resolve(&["exhentai.org"])
                .build(prefix),
            raw_client: raw_client(),
        })
    }

    pub fn new_from_config() -> anyhow::Result<Self> {
        let config: ExConfig = config::parse(CONFIG_KEY)?
            .ok_or_else(|| anyhow!("exhentai config(key: exhentai) not found"))?;

        Ok(Self {
            ghost_client: GhostClientBuilder::default()
                .with_default_headers(config.build_header()?)
                .with_cf_resolve(&["exhentai.org"])
                .build_from_config()?,
            raw_client: raw_client(),
        })
    }

    pub fn get_client(&self) -> reqwest::Client {
        self.raw_client.clone()
    }
}

impl Collector for EXCollector {
    type FetchError = anyhow::Error;
    type StreamError = anyhow::Error;
    type ImageStream = EXImageStream;

    #[inline]
    fn name() -> &'static str {
        "exhentai"
    }

    async fn fetch(
        &self,
        path: String,
    ) -> Result<(AlbumMeta, Self::ImageStream), Self::FetchError> {
        let gallery = GalleryPath::parse(&path)?;
        let url = gallery.gallery_url();

        tracing::info!("[exhentai] process {url}");

        let mut paged = Paged::new(0, EXPageIndicator { base: url.clone() });
        let gallery_pages = paged
            .pages(&self.ghost_client)
            .await
            .with_context(|| format!("[exhentai] failed to load gallery pages for {url}"))?;

        tracing::info!("[exhentai] pages loaded for {}", gallery.display_key());

        let first_page = gallery_pages
            .first()
            .ok_or_else(|| anyhow!("[exhentai] paged returned no pages for {url}"))?;

        let title = match_first_group(&TITLE_RE, first_page)
            .map(str::to_owned)
            .unwrap_or_else(|| format!("exhentai-{}", gallery.album_id));

        let image_page_links = gallery_pages
            .iter()
            .flat_map(|page| {
                PAGE_RE
                    .captures_iter(page)
                    .filter_map(|c| c.get(1).map(|m| m.as_str().to_owned()))
            })
            .collect::<Vec<_>>();

        if image_page_links.is_empty() {
            return Err(anyhow!(
                "no image page links found; resource may be deleted, unavailable, or blocked"
            ));
        }

        Ok((
            AlbumMeta {
                link: url,
                name: title,
                class: None,
                description: None,
                authors: None,
                tags: None,
            },
            EXImageStream {
                raw_client: self.raw_client.clone(),
                ghost_client: self.ghost_client.clone(),
                image_page_links: image_page_links.into_iter(),
            },
        ))
    }
}

#[derive(Debug)]
pub struct EXImageStream {
    raw_client: reqwest::Client,
    ghost_client: GhostClient,
    image_page_links: std::vec::IntoIter<String>,
}

impl EXImageStream {
    async fn load_image(
        ghost_client: GhostClient,
        raw_client: reqwest::Client,
        link: String,
    ) -> anyhow::Result<(ImageMeta, ImageData)> {
        let content = RETRY_POLICY
            .retry(|| async { get_string(&ghost_client, &link).await })
            .await
            .with_context(|| format!("[exhentai] failed to load image page {link}"))?;

        let img_url = match_first_group(&IMG_RE, &content)
            .ok_or_else(|| anyhow!("[exhentai] unable to find image url in page {link}"))?
            .to_owned();

        let image_data = RETRY_POLICY
            .retry(|| async { get_bytes(&raw_client, &img_url).await })
            .await
            .with_context(|| format!("[exhentai] failed to download image {img_url}"))?;

        tracing::trace!(
            "download exhentai image with size {}, page link: {link}",
            image_data.len()
        );

        Ok((
            ImageMeta {
                id: link,
                url: img_url,
                description: None,
            },
            image_data,
        ))
    }
}

impl AsyncStream for EXImageStream {
    type Item = anyhow::Result<(ImageMeta, ImageData)>;

    type Future = crate::stream::BoxFuture<Self::Item>;

    fn next(&mut self) -> Option<Self::Future> {
        let link = self.image_page_links.next()?;
        let ghost_client = self.ghost_client.clone();
        let raw_client = self.raw_client.clone();

        Some(Box::pin(async move { Self::load_image(ghost_client, raw_client, link).await }))
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.image_page_links.size_hint()
    }
}

struct EXPageIndicator {
    base: String,
}

impl PageFormatter for EXPageIndicator {
    fn format_n(&self, n: usize) -> String {
        format!("{}/?p={}", self.base, n)
    }
}

impl PageIndicator for EXPageIndicator {
    fn is_last_page(&self, content: &str, next_page: usize) -> bool {
        let next = format!(
            "<a href=\"{}/?p={}\" onclick=\"return false\">",
            self.base, next_page
        );
        !content.contains(&next)
    }
}