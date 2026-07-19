//! Built-in collectors and trait.

use once_cell::sync::Lazy;
use regex::Regex;
use std::future::Future;

use crate::stream::AsyncStream;

use self::{
    e_hentai::EHCollector, exhentai::EXCollector, nhentai::NHCollector, pixiv::PixivCollector,
};

pub mod utils;

pub mod e_hentai;
pub mod exhentai;
pub mod nhentai;
pub mod pixiv;

#[derive(Debug, Clone)]
pub struct ImageMeta {
    pub id: String,
    pub url: String,
    pub description: Option<String>,
}

pub type ImageData = bytes::Bytes;

#[derive(Debug, Clone)]
pub struct AlbumMeta {
    pub link: String,
    pub name: String,
    pub class: Option<String>,
    pub description: Option<String>,
    pub authors: Option<Vec<String>>,
    pub tags: Option<Vec<String>>,
}

/// Generic collector.
/// The `async fetch` returns the result of `AlbumMeta` and `ImageStream`.
/// By exposing `ImageStream`, we can fetch the images lazily. For low
/// memory VM, it will keep only a small amount in memory.
pub trait Collector {
    type FetchError;
    type StreamError;
    type ImageStream: AsyncStream<Item = Result<ImageMeta, Self::StreamError>>;

    fn name() -> &'static str;
    fn fetch(
        &self,
        path: String,
    ) -> impl Future<Output = Result<(AlbumMeta, Self::ImageStream), Self::FetchError>>;
}

const SUPPORTED_URL_PATTERN: &str = concat!(
    r"((?:https://exhentai\.org/g/\w+/[\w-]+)",
    r"|(?:https://e-hentai\.org/g/\w+/[\w-]+)",
    r"|(?:https://nhentai\.net/g/\d+)",
    r"|(?:https://nhentai\.to/g/\d+)",
    r"|(?:https://(?:www\.)?pixiv\.net/(?:[a-z]{2}/)?artworks/\d+)",
    r#"|(?:https://(?:www\.)?pixiv\.net/member_illust\.php\?[^\s#<>"]*?illust_id=\d+))"#,
);

pub(crate) static URL_FROM_TEXT_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(SUPPORTED_URL_PATTERN).expect("valid supported URL regex"));

pub(crate) static URL_FROM_URL_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(&format!(r"^{SUPPORTED_URL_PATTERN}"))
        .expect("valid anchored supported URL regex")
});

#[derive(Debug, Clone)]
pub struct Registry {
    eh: EHCollector,
    nh: NHCollector,
    ex: EXCollector,
    pixiv: PixivCollector,
}

pub trait Param<T> {
    fn get(&self) -> &T;
}

impl Param<EHCollector> for Registry {
    fn get(&self) -> &EHCollector {
        &self.eh
    }
}

impl Param<NHCollector> for Registry {
    fn get(&self) -> &NHCollector {
        &self.nh
    }
}

impl Param<EXCollector> for Registry {
    fn get(&self) -> &EXCollector {
        &self.ex
    }
}

impl Param<PixivCollector> for Registry {
    fn get(&self) -> &PixivCollector {
        &self.pixiv
    }
}

impl Registry {
    pub fn new_from_config() -> Self {
        Self {
            eh: EHCollector::new_from_config().expect("unable to build e-hentai collector"),
            nh: NHCollector::new_from_config().expect("unable to build nhentai collector"),
            ex: EXCollector::new_from_config().expect("unable to build exhentai collector"),
            pixiv: PixivCollector::new_from_config().expect("unable to build pixiv collector"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{URL_FROM_TEXT_RE, URL_FROM_URL_RE};

    fn first_match<'a>(regex: &regex::Regex, input: &'a str) -> Option<&'a str> {
        regex
            .captures(input)
            .and_then(|captures| captures.get(1))
            .map(|matched| matched.as_str())
    }

    #[test]
    fn recognizes_pixiv_artwork_urls() {
        for url in [
            "https://www.pixiv.net/artworks/12345678",
            "https://pixiv.net/artworks/12345678",
            "https://www.pixiv.net/en/artworks/12345678",
            "https://www.pixiv.net/member_illust.php?mode=medium&illust_id=12345678",
        ] {
            assert_eq!(first_match(&URL_FROM_URL_RE, url), Some(url));
        }
    }

    #[test]
    fn extracts_pixiv_url_from_text_without_trailing_punctuation() {
        let text = "Pixiv: https://www.pixiv.net/en/artworks/12345678).";
        assert_eq!(
            first_match(&URL_FROM_TEXT_RE, text),
            Some("https://www.pixiv.net/en/artworks/12345678")
        );
    }

    #[test]
    fn ignores_non_artwork_pixiv_urls() {
        assert!(first_match(&URL_FROM_TEXT_RE, "https://www.pixiv.net/users/1234").is_none());
    }
}
