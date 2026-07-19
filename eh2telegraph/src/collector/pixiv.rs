use std::time::Duration;

use again::RetryPolicy;
use ipnet::Ipv6Net;
use reqwest::header::{self, HeaderMap, HeaderValue};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::Value;
use tokio::time::timeout;

use crate::{
    http_client::{GhostClient, GhostClientBuilder, HttpRequestBuilder},
    stream::AsyncStream,
};

use super::{AlbumMeta, Collector, ImageMeta};

const PIXIV_HOST: &str = "www.pixiv.net";
const PIXIV_REFERER: &str = "https://www.pixiv.net/";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

lazy_static::lazy_static! {
    static ref RETRY_POLICY: RetryPolicy = RetryPolicy::fixed(Duration::from_millis(250))
        .with_max_retries(4)
        .with_jitter(true);
    static ref BR_RE: regex::Regex =
        regex::Regex::new(r"(?i)<br\s*/?>").expect("valid BR_RE");
    static ref HTML_TAG_RE: regex::Regex =
        regex::Regex::new(r"(?s)<[^>]+>").expect("valid HTML_TAG_RE");
}

#[derive(Debug, Clone)]
pub struct PixivCollector {
    client: GhostClient,
}

impl PixivCollector {
    pub fn new(prefix: Option<Ipv6Net>) -> Self {
        Self {
            client: pixiv_client_builder().build(prefix),
        }
    }

    pub fn new_from_config() -> anyhow::Result<Self> {
        Ok(Self {
            client: pixiv_client_builder().build_from_config()?,
        })
    }

    async fn get_api<T>(&self, url: &str, referer: &str) -> anyhow::Result<T>
    where
        T: DeserializeOwned,
    {
        let response: PixivApiResponse = timeout(
            REQUEST_TIMEOUT,
            RETRY_POLICY.retry(|| async {
                self.client
                    .get_builder(url)
                    .header(header::REFERER, referer)
                    .send()
                    .await?
                    .error_for_status()?
                    .json::<PixivApiResponse>()
                    .await
            }),
        )
        .await
        .map_err(|_| anyhow::anyhow!("pixiv request timed out: {url}"))??;

        if response.error {
            let message = response.message.trim();
            return Err(anyhow::anyhow!(
                "pixiv API rejected the request{}",
                if message.is_empty() {
                    String::new()
                } else {
                    format!(": {message}")
                }
            ));
        }

        if response.body.is_null() {
            return Err(anyhow::anyhow!("pixiv API returned an empty body: {url}"));
        }

        serde_json::from_value(response.body)
            .map_err(|error| anyhow::anyhow!("unable to parse pixiv API response from {url}: {error}"))
    }
}

fn pixiv_client_builder() -> GhostClientBuilder {
    let mut headers = HeaderMap::new();
    headers.insert(header::ACCEPT, HeaderValue::from_static("application/json"));
    headers.insert(
        header::ACCEPT_LANGUAGE,
        HeaderValue::from_static("en-US,en;q=0.9,ja;q=0.8"),
    );
    headers.insert(header::REFERER, HeaderValue::from_static(PIXIV_REFERER));

    GhostClientBuilder::default().with_default_headers(headers)
}

impl Collector for PixivCollector {
    type FetchError = anyhow::Error;
    type StreamError = anyhow::Error;
    type ImageStream = PixivImageStream;

    #[inline]
    fn name() -> &'static str {
        "pixiv"
    }

    async fn fetch(
        &self,
        path: String,
    ) -> Result<(AlbumMeta, Self::ImageStream), Self::FetchError> {
        let illust_id = parse_illust_id(&path)?;
        let original_url = format!("https://{PIXIV_HOST}/artworks/{illust_id}");
        let metadata_url = format!("https://{PIXIV_HOST}/ajax/illust/{illust_id}");
        let pages_url = format!("https://{PIXIV_HOST}/ajax/illust/{illust_id}/pages");

        tracing::info!("[pixiv] process {original_url}");

        let metadata: PixivIllust = self.get_api(&metadata_url, &original_url).await?;
        let pages: Vec<PixivPage> = self.get_api(&pages_url, &original_url).await?;

        if pages.is_empty() {
            return Err(anyhow::anyhow!(
                "pixiv returned no image pages for artwork {illust_id}; it may be deleted, private, restricted, or unavailable"
            ));
        }

        if metadata.page_count != 0 && metadata.page_count != pages.len() {
            tracing::warn!(
                "[pixiv] metadata page count differs from pages response for {}: metadata={}, pages={}",
                illust_id,
                metadata.page_count,
                pages.len()
            );
        }

        let images = pages
            .into_iter()
            .enumerate()
            .map(|(index, page)| page.into_image_meta(&illust_id, index))
            .collect::<anyhow::Result<Vec<_>>>()?;

        let name = first_non_empty(&[&metadata.illust_title, &metadata.title])
            .map(str::to_owned)
            .unwrap_or_else(|| format!("pixiv-{illust_id}"));

        let description = first_non_empty(&[
            &metadata.illust_comment,
            &metadata.description,
        ])
        .and_then(clean_html_text);

        let authors = non_empty(&metadata.user_name).map(|name| vec![name.to_owned()]);
        let tags = metadata.tags.and_then(PixivTags::into_names);

        Ok((
            AlbumMeta {
                link: original_url,
                name,
                class: Some(metadata.illust_type.class_name().to_owned()),
                description,
                authors,
                tags,
            },
            PixivImageStream {
                images: images.into_iter(),
            },
        ))
    }
}

#[derive(Debug)]
pub struct PixivImageStream {
    images: std::vec::IntoIter<ImageMeta>,
}

impl AsyncStream for PixivImageStream {
    type Item = anyhow::Result<ImageMeta>;
    type Future = crate::stream::BoxFuture<anyhow::Result<ImageMeta>>;

    fn next(&mut self) -> Option<Self::Future> {
        let image = self.images.next()?;
        Some(Box::pin(async move { Ok(image) }))
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.images.size_hint()
    }
}

#[derive(Debug, Deserialize)]
struct PixivApiResponse {
    #[serde(default)]
    error: bool,
    #[serde(default)]
    message: String,
    #[serde(default)]
    body: Value,
}

#[derive(Debug, Deserialize)]
struct PixivIllust {
    #[serde(rename = "illustTitle", default)]
    illust_title: String,
    #[serde(default)]
    title: String,
    #[serde(rename = "illustComment", default)]
    illust_comment: String,
    #[serde(default)]
    description: String,
    #[serde(rename = "userName", default)]
    user_name: String,
    #[serde(rename = "illustType", default)]
    illust_type: PixivIllustType,
    #[serde(rename = "pageCount", default)]
    page_count: usize,
    #[serde(default)]
    tags: Option<PixivTags>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(transparent)]
struct PixivIllustType(u8);

impl PixivIllustType {
    fn class_name(self) -> &'static str {
        match self.0 {
            0 => "illustration",
            1 => "manga",
            2 => "ugoira",
            _ => "pixiv",
        }
    }
}

#[derive(Debug, Deserialize)]
struct PixivTags {
    #[serde(default)]
    tags: Vec<PixivTag>,
}

impl PixivTags {
    fn into_names(self) -> Option<Vec<String>> {
        let mut names = Vec::with_capacity(self.tags.len());

        for tag in self.tags {
            let tag = tag.tag.trim();
            if tag.is_empty() || names.iter().any(|existing| existing == tag) {
                continue;
            }
            names.push(tag.to_owned());
        }

        (!names.is_empty()).then_some(names)
    }
}

#[derive(Debug, Deserialize)]
struct PixivTag {
    #[serde(default)]
    tag: String,
}

#[derive(Debug, Deserialize)]
struct PixivPage {
    urls: PixivPageUrls,
}

impl PixivPage {
    fn into_image_meta(self, illust_id: &str, index: usize) -> anyhow::Result<ImageMeta> {
        let url = self
            .urls
            .original
            .or(self.urls.regular)
            .or(self.urls.small)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "pixiv page {} of artwork {} has no usable image URL",
                    index + 1,
                    illust_id
                )
            })?;

        let url = validate_image_url(url)?;

        Ok(ImageMeta {
            id: format!("{illust_id}-p{index}"),
            url,
            description: None,
        })
    }
}

#[derive(Debug, Deserialize)]
struct PixivPageUrls {
    #[serde(default)]
    original: Option<String>,
    #[serde(default)]
    regular: Option<String>,
    #[serde(default)]
    small: Option<String>,
}

fn validate_image_url(url: String) -> anyhow::Result<String> {
    let parsed = reqwest::Url::parse(&url)
        .map_err(|error| anyhow::anyhow!("invalid pixiv image URL {url:?}: {error}"))?;

    if parsed.scheme() != "https" || parsed.username() != "" || parsed.password().is_some() {
        return Err(anyhow::anyhow!("unsafe pixiv image URL: {url}"));
    }

    let host = parsed
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("pixiv image URL has no host: {url}"))?;

    if host != "i.pximg.net" && !host.ends_with(".pximg.net") {
        return Err(anyhow::anyhow!(
            "unexpected pixiv image host {host:?} in URL {url}"
        ));
    }

    Ok(parsed.to_string())
}

fn parse_illust_id(input: &str) -> anyhow::Result<String> {
    let input = input.trim();

    if let Some(query) = input.split_once('?').map(|(_, query)| query) {
        let query = query.split('#').next().unwrap_or(query);
        for pair in query.split('&') {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            if key == "illust_id" && is_valid_illust_id(value) {
                return Ok(value.to_owned());
            }
        }
    }

    let path = input
        .split(['?', '#'])
        .next()
        .unwrap_or(input)
        .trim_end_matches('/');
    let segments = path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();

    if segments.len() == 1 && is_valid_illust_id(segments[0]) {
        return Ok(segments[0].to_owned());
    }

    for (index, segment) in segments.iter().enumerate() {
        if *segment != "artworks" {
            continue;
        }

        if let Some(id) = segments.get(index + 1) {
            if index + 2 == segments.len() && is_valid_illust_id(id) {
                return Ok((*id).to_owned());
            }
        }
    }

    Err(anyhow::anyhow!(
        "invalid input path({input}); a Pixiv artwork URL is expected, like https://www.pixiv.net/artworks/12345678"
    ))
}

fn is_valid_illust_id(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
}

fn first_non_empty<'a>(values: &[&'a str]) -> Option<&'a str> {
    values.iter().copied().find_map(non_empty)
}

fn non_empty(value: &str) -> Option<&str> {
    let value = value.trim();
    (!value.is_empty()).then_some(value)
}

fn clean_html_text(value: &str) -> Option<String> {
    let with_line_breaks = BR_RE.replace_all(value, "\n");
    let without_tags = HTML_TAG_RE.replace_all(&with_line_breaks, "");
    let decoded = without_tags
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&#x27;", "'");
    let decoded = decoded.trim();

    (!decoded.is_empty()).then(|| decoded.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_supported_paths() {
        for input in [
            "12345678",
            "/artworks/12345678",
            "https://www.pixiv.net/artworks/12345678",
            "https://www.pixiv.net/en/artworks/12345678/",
            "https://www.pixiv.net/member_illust.php?mode=medium&illust_id=12345678",
        ] {
            assert_eq!(parse_illust_id(input).unwrap(), "12345678");
        }
    }

    #[test]
    fn reject_invalid_paths() {
        for input in [
            "",
            "/artworks/",
            "/artworks/not-a-number",
            "/artworks/123/extra",
            "https://example.com/12345678",
        ] {
            assert!(parse_illust_id(input).is_err(), "accepted {input:?}");
        }
    }

    #[test]
    fn deserialize_api_response() {
        let raw = r#"{
            "error": false,
            "message": "",
            "body": {
                "illustTitle": "Example",
                "title": "Example",
                "illustComment": "hello<br>world",
                "description": "hello<br>world",
                "userName": "artist",
                "illustType": 0,
                "pageCount": 2,
                "tags": {"tags": [{"tag": "tag-a"}, {"tag": "tag-b"}]}
            }
        }"#;

        let response: PixivApiResponse = serde_json::from_str(raw).unwrap();
        let body: PixivIllust = serde_json::from_value(response.body).unwrap();
        assert_eq!(body.illust_title, "Example");
        assert_eq!(body.page_count, 2);
        assert_eq!(
            body.tags.unwrap().into_names().unwrap(),
            vec!["tag-a".to_owned(), "tag-b".to_owned()]
        );
    }

    #[test]
    fn clean_description_html() {
        assert_eq!(
            clean_html_text("hello<br />world &amp; pixiv").as_deref(),
            Some("hello\nworld & pixiv")
        );
    }
}
