//! Pixiv collector.
//!
//! Supported inputs include:
//! - `/artworks/12345678`
//! - `https://www.pixiv.net/artworks/12345678`
//! - `https://www.pixiv.net/en/artworks/12345678`
//! - legacy `member_illust.php?...&illust_id=12345678`
//!
//! Public artworks work without authentication. If `pixiv.php_sessid` is set,
//! Pixiv AJAX requests are made directly with that session first. The project's
//! private HTTP proxy is only used as a fallback, preventing a proxy-side 403
//! from breaking every Pixiv gallery. Restricted works remain accessible only
//! when the configured Pixiv account itself has permission to view them.

use std::{str::FromStr, time::Duration};

use again::RetryPolicy;
use ipnet::Ipv6Net;
use reqwest::header::{self, HeaderMap, HeaderName, HeaderValue};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::Value;
use tokio::time::timeout;

use crate::{
    config,
    http_client::{GhostClient, GhostClientBuilder, HttpRequestBuilder},
    http_proxy::ProxiedClient,
    stream::AsyncStream,
};

use super::{AlbumMeta, Collector, ImageMeta};

const CONFIG_KEY: &str = "pixiv";
const PIXIV_HOST: &str = "www.pixiv.net";
const PIXIV_REFERER: &str = "https://www.pixiv.net/";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const PIXIV_SESSION_PROXY_HEADER: &str = "x-pixiv-phpsessid";
const MAX_ERROR_BODY_CHARS: usize = 512;

lazy_static::lazy_static! {
    static ref RETRY_POLICY: RetryPolicy = RetryPolicy::fixed(Duration::from_millis(250))
        .with_max_retries(4)
        .with_jitter(true);
    static ref BR_RE: regex::Regex =
        regex::Regex::new(r"(?i)<br\s*/?>").expect("valid BR_RE");
    static ref HTML_TAG_RE: regex::Regex =
        regex::Regex::new(r"(?s)<[^>]+>").expect("valid HTML_TAG_RE");
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct PixivConfig {
    /// The value of Pixiv's PHPSESSID cookie, without `PHPSESSID=`.
    #[serde(default, alias = "phpsessid", alias = "PHPSESSID")]
    pub php_sessid: Option<String>,

    /// Try the configured private HTTP proxy after a direct Pixiv request fails.
    /// Defaults to true when omitted.
    #[serde(default)]
    pub proxy_fallback: Option<bool>,
}

impl PixivConfig {
    fn session_value(&self) -> anyhow::Result<Option<&str>> {
        let Some(raw) = self.php_sessid.as_deref() else {
            return Ok(None);
        };

        let raw = raw.trim();
        if raw.is_empty() {
            return Ok(None);
        }

        let value = raw.strip_prefix("PHPSESSID=").unwrap_or(raw).trim();
        if value.is_empty() {
            return Ok(None);
        }

        if value.contains(';') || value.contains('\r') || value.contains('\n') {
            return Err(anyhow::anyhow!(
                "pixiv.php_sessid must contain only the PHPSESSID value, not a full Cookie header"
            ));
        }

        Ok(Some(value))
    }

    fn proxy_fallback_enabled(&self) -> bool {
        self.proxy_fallback.unwrap_or(true)
    }

    fn build_header_sets(&self) -> anyhow::Result<(HeaderMap, Option<HeaderMap>)> {
        let session = self.session_value()?;
        let mut direct_headers = base_headers();

        let Some(session) = session else {
            return Ok((direct_headers, None));
        };

        let session_value = HeaderValue::from_str(session)
            .map_err(|error| anyhow::anyhow!("invalid pixiv PHPSESSID: {error}"))?;
        let cookie_value = HeaderValue::from_str(&format!("PHPSESSID={session}"))
            .map_err(|error| anyhow::anyhow!("invalid pixiv PHPSESSID: {error}"))?;

        direct_headers.insert(header::COOKIE, cookie_value.clone());

        let mut proxy_headers = base_headers();
        proxy_headers.insert(
            HeaderName::from_static(PIXIV_SESSION_PROXY_HEADER),
            session_value,
        );
        // Backwards compatibility with older workers. The updated worker reads
        // X-Pixiv-PHPSESSID and reconstructs a restricted Cookie header itself.
        proxy_headers.insert(header::COOKIE, cookie_value);

        Ok((direct_headers, Some(proxy_headers)))
    }
}

#[derive(Debug, Clone)]
enum PixivHttpClient {
    Direct(GhostClient),
    Proxy(ProxiedClient),
}

impl PixivHttpClient {
    fn get_builder(&self, url: &str) -> reqwest::RequestBuilder {
        match self {
            Self::Direct(client) => client.get_builder(url),
            Self::Proxy(client) => client.get_builder(url),
        }
    }

    fn label(&self) -> &'static str {
        match self {
            Self::Direct(_) => "direct connection",
            Self::Proxy(_) => "private proxy",
        }
    }
}

#[derive(Debug, Clone)]
pub struct PixivCollector {
    primary: PixivHttpClient,
    fallback: Option<PixivHttpClient>,
    authenticated: bool,
}

impl PixivCollector {
    /// Build an unauthenticated collector with an optional IPv6 prefix.
    pub fn new(prefix: Option<Ipv6Net>) -> Self {
        Self {
            primary: PixivHttpClient::Direct(
                GhostClientBuilder::default()
                    .with_default_headers(base_headers())
                    .build(prefix),
            ),
            fallback: None,
            authenticated: false,
        }
    }

    /// Build from an explicit Pixiv configuration.
    ///
    /// A configured session is used on a direct Pixiv client. The global proxy
    /// is retained only as a fallback so a Worker policy/upstream 403 does not
    /// make public and authenticated galleries fail together.
    pub fn new_with_config(config: &PixivConfig, prefix: Option<Ipv6Net>) -> anyhow::Result<Self> {
        let (direct_headers, proxy_headers) = config.build_header_sets()?;
        let authenticated = proxy_headers.is_some();

        let primary = PixivHttpClient::Direct(
            GhostClientBuilder::default()
                .with_default_headers(direct_headers)
                .build(prefix),
        );
        let fallback = if config.proxy_fallback_enabled() {
            proxy_headers.map(|headers| {
                PixivHttpClient::Proxy(
                    ProxiedClient::new_from_config().with_default_headers(headers),
                )
            })
        } else {
            None
        };

        Ok(Self {
            primary,
            fallback,
            authenticated,
        })
    }

    pub fn new_from_config() -> anyhow::Result<Self> {
        let pixiv_config: PixivConfig = config::parse(CONFIG_KEY)?.unwrap_or_default();
        let (direct_headers, proxy_headers) = pixiv_config.build_header_sets()?;
        let authenticated = proxy_headers.is_some();

        let primary = PixivHttpClient::Direct(
            GhostClientBuilder::default()
                .with_default_headers(direct_headers)
                .build_from_config()?,
        );
        let fallback = if pixiv_config.proxy_fallback_enabled() {
            proxy_headers.map(|headers| {
                PixivHttpClient::Proxy(
                    ProxiedClient::new_from_config().with_default_headers(headers),
                )
            })
        } else {
            None
        };

        Ok(Self {
            primary,
            fallback,
            authenticated,
        })
    }

    async fn get_api<T>(&self, url: &str, referer: &str) -> anyhow::Result<T>
    where
        T: DeserializeOwned,
    {
        match self.get_api_from(&self.primary, url, referer).await {
            Ok(value) => Ok(value),
            Err(primary_error) => {
                let Some(fallback) = self.fallback.as_ref() else {
                    return Err(primary_error);
                };

                tracing::warn!(
                    "[pixiv] {} failed for {}: {}; trying {}",
                    self.primary.label(),
                    url,
                    primary_error,
                    fallback.label()
                );

                self.get_api_from(fallback, url, referer)
                    .await
                    .map_err(|fallback_error| {
                        anyhow::anyhow!(
                            "pixiv {} failed: {}; {} fallback failed: {}",
                            self.primary.label(),
                            primary_error,
                            fallback.label(),
                            fallback_error
                        )
                    })
            }
        }
    }

    async fn get_api_from<T>(
        &self,
        client: &PixivHttpClient,
        url: &str,
        referer: &str,
    ) -> anyhow::Result<T>
    where
        T: DeserializeOwned,
    {
        let (status, proxy_stage, body) = timeout(
            REQUEST_TIMEOUT,
            RETRY_POLICY.retry(|| async {
                let response = client
                    .get_builder(url)
                    .header(header::REFERER, referer)
                    .send()
                    .await?;
                let status = response.status();
                let proxy_stage = response
                    .headers()
                    .get("x-proxy-stage")
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_owned);
                let body = response.bytes().await?;

                Ok::<_, reqwest::Error>((status, proxy_stage, body))
            }),
        )
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "pixiv request through {} timed out: {url}",
                client.label()
            )
        })??;

        if !status.is_success() {
            let stage = proxy_stage
                .as_deref()
                .map(|stage| format!(", stage={stage}"))
                .unwrap_or_default();
            return Err(anyhow::anyhow!(
                "HTTP {status} through {}{stage}: {}",
                client.label(),
                response_body_snippet(&body)
            ));
        }

        let response: PixivApiResponse = serde_json::from_slice(&body).map_err(|error| {
            anyhow::anyhow!(
                "unable to parse Pixiv API response through {} from {url}: {error}; body: {}",
                client.label(),
                response_body_snippet(&body)
            )
        })?;

        if response.error {
            let message = response.message.trim();
            let auth_hint = if self.authenticated {
                "the configured Pixiv session may be expired, or its account may not have permission to view this work"
            } else {
                "set pixiv.php_sessid to access works that require a Pixiv login"
            };

            return Err(anyhow::anyhow!(
                "Pixiv API rejected the request through {}{}; {auth_hint}",
                client.label(),
                if message.is_empty() {
                    String::new()
                } else {
                    format!(": {message}")
                }
            ));
        }

        if response.body.is_null() {
            return Err(anyhow::anyhow!(
                "Pixiv API returned an empty body through {}: {url}",
                client.label()
            ));
        }

        serde_json::from_value(response.body).map_err(|error| {
            anyhow::anyhow!(
                "unable to decode Pixiv API body through {} from {url}: {error}",
                client.label()
            )
        })
    }
}

fn base_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(header::ACCEPT, HeaderValue::from_static("application/json"));
    headers.insert(
        header::ACCEPT_LANGUAGE,
        HeaderValue::from_static("en-US,en;q=0.9,ja;q=0.8"),
    );
    headers.insert(header::REFERER, HeaderValue::from_static(PIXIV_REFERER));
    headers.insert(
        header::ORIGIN,
        HeaderValue::from_static("https://www.pixiv.net"),
    );
    headers.insert(
        HeaderName::from_static("x-requested-with"),
        HeaderValue::from_static("XMLHttpRequest"),
    );
    headers
}

fn response_body_snippet(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body);
    let text = text.trim();
    if text.is_empty() {
        return "empty response body".to_owned();
    }

    let mut chars = text.chars();
    let snippet = chars.by_ref().take(MAX_ERROR_BODY_CHARS).collect::<String>();
    if chars.next().is_some() {
        format!("{snippet}…")
    } else {
        snippet
    }
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

        tracing::info!(
            "[pixiv] process {original_url} (authenticated={})",
            self.authenticated
        );

        let metadata: PixivIllust = self.get_api(&metadata_url, &original_url).await?;
        let pages: Vec<PixivPage> = self.get_api(&pages_url, &original_url).await?;

        if pages.is_empty() {
            let reason = if self.authenticated {
                "the artwork is unavailable or the configured account lacks access"
            } else {
                "the artwork may require login; configure pixiv.php_sessid"
            };
            return Err(anyhow::anyhow!(
                "pixiv returned no image pages for artwork {illust_id}; {reason}"
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

    if host != "i.pximg.net" {
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
    fn normalize_session_value() {
        let plain = PixivConfig {
            php_sessid: Some("12345_deadbeef".to_owned()),
        };
        assert_eq!(plain.session_value().unwrap(), Some("12345_deadbeef"));

        let prefixed = PixivConfig {
            php_sessid: Some("PHPSESSID=12345_deadbeef".to_owned()),
        };
        assert_eq!(
            prefixed.session_value().unwrap(),
            Some("12345_deadbeef")
        );

        let full_cookie = PixivConfig {
            php_sessid: Some("PHPSESSID=123; other=value".to_owned()),
        };
        assert!(full_cookie.session_value().is_err());
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
