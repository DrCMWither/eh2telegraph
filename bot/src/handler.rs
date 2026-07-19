use std::{
    collections::{HashMap, HashSet, VecDeque},
    fmt,
    ops::ControlFlow,
    sync::Arc,
};

use eh2telegraph::{
    collector::{
        e_hentai::EHCollector, exhentai::EXCollector, nhentai::NHCollector,
        pixiv::PixivCollector,
    },
    searcher::{
        f_hash::FHashConvertor,
        saucenao::{SaucenaoOutput, SaucenaoParsed, SaucenaoSearcher},
        ImageSearcher,
    },
    storage::KVStorage,
    sync::{StashedGallery, SyncResult, Synchronizer},
};
use reqwest::Url;
use teloxide::{
    adaptors::DefaultParseMode,
    prelude::*,
    utils::{command::BotCommands, markdown::code_inline},
};

use crate::{util::esc, util::PrettyChat};
use tokio::{
    sync::{Mutex, Notify},
    time::{timeout, Duration},
};
use tracing::{info, trace};

const MIN_SIMILARITY: u8 = 70;
const MIN_SIMILARITY_PRIVATE: u8 = 50;
const MAX_BATCH_LINKS: usize = 100;
const MAX_BATCH_IMAGES: usize = 5000;

fn push_unique_url(urls: &mut Vec<String>, url: String) {
    if !urls.iter().any(|existing| existing == &url) {
        urls.push(url);
    }
}

fn collect_urls_from_text(content: &str) -> Vec<String> {
    let mut urls = Vec::new();
    let mut remaining = content;

    while let Some(url) = Synchronizer::match_url_from_text(remaining) {
        push_unique_url(&mut urls, url.to_owned());

        let Some(offset) = remaining.find(url) else {
            break;
        };
        let next = offset + url.len();
        if next >= remaining.len() {
            break;
        }
        remaining = &remaining[next..];
    }

    urls
}

#[derive(BotCommands, Clone)]
#[command(
    rename_rule = "lowercase",
    description = "\
    This is a gallery synchronization robot that is convenient for users to view pictures directly in Telegram.\n\
    这是一个方便用户直接在 Telegram 里看图的画廊同步机器人。\n\
    Bot supports sync with command, text url, or image(private chat search thrashold is lower).\n\
    机器人支持通过 命令、直接发送链接、图片(私聊搜索相似度阈值会更低) 的形式同步。\n\n\
    These commands are supported:\n\
    支持指令:"
)]
pub enum Command {
    #[command(description = "Display this help. 显示这条帮助信息。")]
    Help,
    #[command(description = "Show bot verison. 显示机器人版本。")]
    Version,
    #[command(description = "Show your account id. 显示你的账号 ID。")]
    Id,
    #[command(
        description = "Sync a gallery(e-hentai/exhentai/nhentai/pixiv are supported now). 同步一个画廊(目前支持 EH/EX/NH/Pixiv)"
    )]
    Sync(String),
    #[command(
        description = "Start a batch stash session. 开始批量暂存。"
    )]
    Batch,
    #[command(
        rename = "batch_end",
        description = "Finish the current batch and publish it. 结束批量暂存并发布。"
    )]
    BatchEnd,
}

#[derive(BotCommands, Clone)]
#[command(rename_rule = "lowercase", description = "Command for admins")]
pub enum AdminCommand {
    #[command(description = "Delete cache with given key.")]
    Delete(String),
}


#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct BatchKey {
    chat_id: i64,
    user_id: u64,
}

impl BatchKey {
    fn from_message(msg: &Message) -> Self {
        Self {
            chat_id: msg.chat.id.0,
            user_id: msg.from().map(|user| user.id.0).unwrap_or_default(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BatchSource {
    EHentai,
    ExHentai,
    NHentai,
    Pixiv,
}

impl BatchSource {
    fn from_url(url: &str) -> anyhow::Result<Self> {
        let parsed = Url::parse(url).map_err(|_| anyhow::anyhow!("Invalid url"))?;
        match parsed.host_str().unwrap_or_default() {
            "e-hentai.org" => Ok(Self::EHentai),
            "exhentai.org" => Ok(Self::ExHentai),
            "nhentai.net" | "nhentai.to" => Ok(Self::NHentai),
            "pixiv.net" | "www.pixiv.net" => Ok(Self::Pixiv),
            _ => Err(anyhow::anyhow!("no matching collector")),
        }
    }
}

impl fmt::Display for BatchSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::EHentai => "e-hentai",
            Self::ExHentai => "exhentai",
            Self::NHentai => "nhentai",
            Self::Pixiv => "pixiv",
        })
    }
}

#[derive(Debug)]
struct BatchQueueItem {
    url: String,
}

#[derive(Debug)]
struct BatchFailure {
    url: String,
    error: String,
}

#[derive(Debug, Default)]
struct BatchSession {
    source: Option<BatchSource>,
    queue: VecDeque<BatchQueueItem>,
    submitted_links: Vec<String>,
    galleries: Vec<StashedGallery>,
    failures: Vec<BatchFailure>,
    image_count: usize,
    processing: bool,
    closing: bool,
}

#[derive(Debug, Default)]
struct BatchState {
    session: Mutex<BatchSession>,
    changed: Notify,
}

pub struct Handler<C> {
    pub synchronizer: Synchronizer<C>,
    pub searcher: SaucenaoSearcher,
    pub convertor: FHashConvertor,
    pub admins: HashSet<i64>,

    single_flight: singleflight_async::SingleFlight<String>,
    batch_sessions: Mutex<HashMap<BatchKey, Arc<BatchState>>>,
}

impl<C> Handler<C>
where
    C: KVStorage<String> + Send + Sync + 'static,
{
    async fn spawn_sync_reply(
        self: Arc<Self>,
        bot: DefaultParseMode<Bot>,
        msg: Message,
        url: String,
        source: &'static str,
    ) -> ControlFlow<()> {
        info!(
            "[{source}] receive sync request from {:?} for {url}",
            PrettyChat(&msg.chat)
        );

        let sent: Message = match timeout(Duration::from_secs(20), async {
            bot.send_message(msg.chat.id, esc(&format!("Syncing url {url}")))
                .reply_to_message_id(msg.id)
                .await
        })
        .await
        {
            Ok(Ok(sent)) => sent,
            Ok(Err(e)) => {
                tracing::error!("[{source}] failed to send syncing message: {e}");
                return ControlFlow::Break(());
            }
            Err(_) => {
                tracing::error!("[{source}] timeout while sending syncing message for {url}");
                return ControlFlow::Break(());
            }
        };

        tokio::spawn(async move {
            tracing::info!("[{source}] spawned sync task for {url}");

            let text = self.sync_response(&url).await;

            tracing::info!("[{source}] sync_response returned for {url}, editing message");

            match timeout(Duration::from_secs(20), async {
                bot.edit_message_text(sent.chat.id, sent.id, text).await
            })
            .await
            {
                Ok(Ok(_)) => {
                    tracing::info!("[{source}] edited sync message for {url}");
                }
                Ok(Err(e)) => {
                    tracing::error!("[{source}] failed to edit sync message for {url}: {e}");
                }
                Err(_) => {
                    tracing::error!("[{source}] timeout while editing sync message for {url}");
                }
            }
        });

        ControlFlow::Break(())
    }

    pub fn new(synchronizer: Synchronizer<C>, admins: HashSet<i64>) -> Self {
        Self {
            synchronizer,
            searcher: SaucenaoSearcher::new_from_config(),
            convertor: FHashConvertor::new_from_config(),
            admins,

            single_flight: Default::default(),
            batch_sessions: Mutex::new(HashMap::new()),
        }
    }

    async fn start_batch(&self, bot: &DefaultParseMode<Bot>, msg: &Message) {
        let key = BatchKey::from_message(msg);
        let mut sessions = self.batch_sessions.lock().await;

        if let Some(state) = sessions.get(&key).cloned() {
            drop(sessions);
            let session = state.session.lock().await;
            let source = session
                .source
                .map(|source| source.to_string())
                .unwrap_or_else(|| "not selected".to_owned());
            let text = format!(
                "Batch is already active.\nSource: {source}\nQueued: {}\nStashed galleries: {}\nCached images: {}\nUse /batch_end to publish.",
                session.queue.len(),
                session.galleries.len(),
                session.image_count,
            );
            let _ = bot
                .send_message(msg.chat.id, esc(&text))
                .reply_to_message_id(msg.id)
                .await;
            return;
        }

        sessions.insert(key, Arc::new(BatchState::default()));
        drop(sessions);

        let _ = bot
            .send_message(
                msg.chat.id,
                esc(
                    "Batch stash started.\nSend supported gallery links. The first accepted link locks the source for this batch. Images are cached while links are processed. Use /batch_end to publish.",
                ),
            )
            .reply_to_message_id(msg.id)
            .await;
    }

    async fn enqueue_batch_urls(
        self: &Arc<Self>,
        bot: &DefaultParseMode<Bot>,
        msg: &Message,
        urls: Vec<String>,
    ) -> bool {
        let key = BatchKey::from_message(msg);
        let state = {
            let sessions = self.batch_sessions.lock().await;
            sessions.get(&key).cloned()
        };
        let Some(state) = state else {
            return false;
        };

        let mut accepted = 0usize;
        let mut rejected = Vec::new();
        let mut should_spawn = false;
        let (source, queued, stashed, image_count, closing) = {
            let mut session = state.session.lock().await;
            if session.closing {
                (
                    session.source,
                    session.queue.len(),
                    session.galleries.len(),
                    session.image_count,
                    true,
                )
            } else {
                for url in urls {
                    if session.submitted_links.len() >= MAX_BATCH_LINKS {
                        rejected.push(format!(
                            "{url}: batch link limit {MAX_BATCH_LINKS} reached"
                        ));
                        continue;
                    }

                    let source = match BatchSource::from_url(&url) {
                        Ok(source) => source,
                        Err(error) => {
                            rejected.push(format!("{url}: {error}"));
                            continue;
                        }
                    };

                    if let Some(locked) = session.source {
                        if locked != source {
                            rejected.push(format!(
                                "{url}: source {source} does not match locked source {locked}"
                            ));
                            continue;
                        }
                    } else {
                        session.source = Some(source);
                    }

                    session.submitted_links.push(url.clone());
                    session.queue.push_back(BatchQueueItem { url });
                    accepted += 1;
                }

                if !session.processing && !session.queue.is_empty() {
                    session.processing = true;
                    should_spawn = true;
                }

                (
                    session.source,
                    session.queue.len(),
                    session.galleries.len(),
                    session.image_count,
                    false,
                )
            }
        };

        if should_spawn {
            let handler = Arc::clone(self);
            let worker_state = Arc::clone(&state);
            tokio::spawn(async move {
                handler.drain_batch(key, worker_state).await;
            });
        }

        let response = if closing {
            "Batch is already closing; no more links are accepted.".to_owned()
        } else {
            let source = source
                .map(|source| source.to_string())
                .unwrap_or_else(|| "not selected".to_owned());
            let mut response = format!(
                "Batch queue updated.\nAccepted: {accepted}\nRejected: {}\nSource: {source}\nWaiting: {queued}\nStashed galleries: {stashed}\nCached images: {image_count}",
                rejected.len(),
            );
            if !rejected.is_empty() {
                response.push_str("\nRejected details:");
                for detail in rejected.iter().take(3) {
                    response.push_str("\n- ");
                    response.push_str(detail);
                }
                if rejected.len() > 3 {
                    response.push_str(&format!("\n- and {} more", rejected.len() - 3));
                }
            }
            response
        };

        let _ = bot
            .send_message(msg.chat.id, esc(&response))
            .reply_to_message_id(msg.id)
            .await;
        true
    }

    async fn drain_batch(self: Arc<Self>, _key: BatchKey, state: Arc<BatchState>) {
        loop {
            let next = {
                let mut session = state.session.lock().await;
                match session.queue.pop_front() {
                    Some(item) => Some(item),
                    None => {
                        session.processing = false;
                        state.changed.notify_waiters();
                        None
                    }
                }
            };

            let Some(item) = next else {
                return;
            };

            tracing::info!("[batch] stashing {}", item.url);
            let result = self.route_stash(&item.url).await;

            let mut session = state.session.lock().await;
            match result {
                Ok(gallery) => {
                    let gallery_images = gallery.images.len();
                    if session.image_count.saturating_add(gallery_images) > MAX_BATCH_IMAGES {
                        session.failures.push(BatchFailure {
                            url: item.url,
                            error: format!(
                                "batch image limit {MAX_BATCH_IMAGES} would be exceeded"
                            ),
                        });
                    } else {
                        session.image_count += gallery_images;
                        session.galleries.push(gallery);
                    }
                }
                Err(error) => {
                    session.failures.push(BatchFailure {
                        url: item.url,
                        error: error.to_string(),
                    });
                }
            }
            drop(session);
            state.changed.notify_waiters();
        }
    }

    async fn finish_batch(
        self: Arc<Self>,
        bot: DefaultParseMode<Bot>,
        msg: Message,
    ) -> ControlFlow<()> {
        let key = BatchKey::from_message(&msg);
        let state = {
            let sessions = self.batch_sessions.lock().await;
            sessions.get(&key).cloned()
        };

        let Some(state) = state else {
            let _ = bot
                .send_message(msg.chat.id, esc("No active batch. Use /batch first."))
                .reply_to_message_id(msg.id)
                .await;
            return ControlFlow::Break(());
        };

        {
            let mut session = state.session.lock().await;
            if session.closing {
                let _ = bot
                    .send_message(msg.chat.id, esc("This batch is already being finalized."))
                    .reply_to_message_id(msg.id)
                    .await;
                return ControlFlow::Break(());
            }
            session.closing = true;
        }

        let sent = match bot
            .send_message(
                msg.chat.id,
                esc("Finishing batch. Waiting for the stash queue to drain..."),
            )
            .reply_to_message_id(msg.id)
            .await
        {
            Ok(sent) => sent,
            Err(error) => {
                tracing::error!("[batch] failed to send finalizing message: {error}");
                let mut session = state.session.lock().await;
                session.closing = false;
                return ControlFlow::Break(());
            }
        };

        tokio::spawn(async move {
            loop {
                let changed = state.changed.notified();
                let finished = {
                    let session = state.session.lock().await;
                    !session.processing && session.queue.is_empty()
                };
                if finished {
                    break;
                }
                changed.await;
            }

            let (galleries, submitted_links, failures, image_count, source) = {
                let mut session = state.session.lock().await;
                (
                    std::mem::take(&mut session.galleries),
                    std::mem::take(&mut session.submitted_links),
                    std::mem::take(&mut session.failures),
                    session.image_count,
                    session.source,
                )
            };

            let gallery_count = galleries.len();
            let source_name = source
                .map(|source| source.to_string())
                .unwrap_or_else(|| "not selected".to_owned());

            let text = if galleries.is_empty() {
                let mut text = format!(
                    "Batch finished without a publishable gallery.\nSource: {source_name}\nFailed: {}",
                    failures.len(),
                );
                for failure in failures.iter().take(5) {
                    text.push_str("\n- ");
                    text.push_str(&failure.url);
                    text.push_str(": ");
                    text.push_str(&failure.error);
                }
                text
            } else {
                match self
                    .synchronizer
                    .sync_stashed_batch(galleries, submitted_links)
                    .await
                {
                    Ok(result) => {
                        let title_line = result
                            .title
                            .map(|title| format!("Title: {title}\n"))
                            .unwrap_or_default();
                        let authors_line = result
                            .authors
                            .filter(|authors| !authors.is_empty())
                            .map(|authors| format!("Authors: {}\n", authors.join(", ")))
                            .unwrap_or_default();
                        let tags_line = result
                            .tags
                            .filter(|tags| !tags.is_empty())
                            .map(|tags| {
                                format!(
                                    "Tags: {}\n",
                                    tags.into_iter()
                                        .take(12)
                                        .map(|tag| format!("#{}", tag.replace(' ', "_")))
                                        .collect::<Vec<_>>()
                                        .join(" ")
                                )
                            })
                            .unwrap_or_default();

                        format!(
                            "Batch sync finished\nSource: {source_name}\nGalleries: {gallery_count}\nCached images: {image_count}\nFailed galleries: {}\n{title_line}{authors_line}{tags_line}URL: {}",
                            failures.len(),
                            result.page_url,
                        )
                    }
                    Err(error) => format!(
                        "Batch publish failed.\nSource: {source_name}\nGalleries stashed: {gallery_count}\nCached images: {image_count}\nError: {error}"
                    ),
                }
            };

            match timeout(Duration::from_secs(20), async {
                bot.edit_message_text(sent.chat.id, sent.id, esc(&text)).await
            })
            .await
            {
                Ok(Ok(_)) => {}
                Ok(Err(error)) => {
                    tracing::error!("[batch] failed to edit final batch message: {error}");
                }
                Err(_) => {
                    tracing::error!("[batch] timeout while editing final batch message");
                }
            }

            let mut sessions = self.batch_sessions.lock().await;
            sessions.remove(&key);
        });

        ControlFlow::Break(())
    }

    /// Executed when a command comes in and parsed successfully.
    pub async fn respond_cmd(
        self: Arc<Self>,
        bot: DefaultParseMode<Bot>,
        msg: Message,
        command: Command,
    ) -> ControlFlow<()> {
        match command {
            Command::Help => {
                let _ = bot
                    .send_message(msg.chat.id, esc(&Command::descriptions().to_string()))
                    .reply_to_message_id(msg.id)
                    .await;
            }
            Command::Version => {
                let _ = bot
                    .send_message(msg.chat.id, esc(crate::version::VERSION))
                    .reply_to_message_id(msg.id)
                    .await;
            }
            Command::Id => {
                let _ = bot
                    .send_message(
                        msg.chat.id,
                        format!(
                            "Current chat id is {} \\(in private chat this is your account id\\)",
                            code_inline(&msg.chat.id.to_string())
                        ),
                    )
                    .reply_to_message_id(msg.id)
                    .await;
            }
            Command::Sync(url) => {
                if url.is_empty() {
                    let _ = bot
                        .send_message(msg.chat.id, esc("Usage: /sync url"))
                        .reply_to_message_id(msg.id)
                        .await;
                    return ControlFlow::Break(());
                }

                let batch_urls = collect_urls_from_text(&url);
                let batch_urls = if batch_urls.is_empty() {
                    vec![url.clone()]
                } else {
                    batch_urls
                };
                if self.enqueue_batch_urls(&bot, &msg, batch_urls).await {
                    return ControlFlow::Break(());
                }

                return self.spawn_sync_reply(bot, msg, url, "cmd handler").await;
            }
            Command::Batch => {
                self.start_batch(&bot, &msg).await;
            }
            Command::BatchEnd => {
                return self.finish_batch(bot, msg).await;
            }
        };

        ControlFlow::Break(())
    }

    pub async fn respond_admin_cmd(
        self: Arc<Self>,
        bot: DefaultParseMode<Bot>,
        msg: Message,
        command: AdminCommand,
    ) -> ControlFlow<()> {
        match command {
            AdminCommand::Delete(key) => {
                match self.synchronizer.delete_cache(&key).await {
                    Ok(_) => {
                        let _ = bot
                            .send_message(msg.chat.id, esc(&format!("Key {key} deleted.")))
                            .reply_to_message_id(msg.id)
                            .await;
                    }
                    Err(e) => {
                        let _ = bot
                            .send_message(
                                msg.chat.id,
                                esc(&format!("Failed to delete key {key}: {e}")),
                            )
                            .reply_to_message_id(msg.id)
                            .await;
                    }
                }
                ControlFlow::Break(())
            }
        }
    }

    pub async fn respond_text(
        self: Arc<Self>,
        bot: DefaultParseMode<Bot>,
        msg: Message,
    ) -> ControlFlow<()> {
        let mut urls = msg
            .text()
            .map(collect_urls_from_text)
            .unwrap_or_default();

        for entity in msg.entities().map(|entities| entities.iter()).into_iter().flatten() {
            if let teloxide::types::MessageEntityKind::TextLink { url } = &entity.kind {
                if let Some(url) = Synchronizer::match_url_from_url(url.as_ref()) {
                    push_unique_url(&mut urls, url.to_owned());
                }
            }
        }

        if urls.is_empty() {
            return ControlFlow::Continue(());
        }

        if self.enqueue_batch_urls(&bot, &msg, urls.clone()).await {
            return ControlFlow::Break(());
        }

        self.spawn_sync_reply(bot, msg, urls.remove(0), "text handler")
            .await
    }

    pub async fn respond_caption(
        self: Arc<Self>,
        bot: DefaultParseMode<Bot>,
        msg: Message,
    ) -> ControlFlow<()> {
        let mut urls = msg
            .caption()
            .map(collect_urls_from_text)
            .unwrap_or_default();

        for entity in msg
            .caption_entities()
            .map(|entities| entities.iter())
            .into_iter()
            .flatten()
        {
            if let teloxide::types::MessageEntityKind::TextLink { url } = &entity.kind {
                if let Some(url) = Synchronizer::match_url_from_url(url.as_ref()) {
                    push_unique_url(&mut urls, url.to_owned());
                }
            }
        }

        if urls.is_empty() {
            return ControlFlow::Continue(());
        }

        if self.enqueue_batch_urls(&bot, &msg, urls.clone()).await {
            return ControlFlow::Break(());
        }

        self.spawn_sync_reply(bot, msg, urls.remove(0), "caption handler")
            .await
    }

    pub async fn respond_photo(
        self: Arc<Self>,
        bot: DefaultParseMode<Bot>,
        msg: Message,
    ) -> ControlFlow<()> {
        let first_photo = match msg.photo().and_then(|x| x.last()) {
            Some(p) => p,
            None => {
                return ControlFlow::Continue(());
            }
        };

        let f = match bot.get_file(&first_photo.file.id).await {
            Ok(f) => f,
            Err(e) => {
                tracing::error!(
                    "[photo handler] failed to get Telegram file {}: {}",
                    first_photo.file.id,
                    e
                );
                let _ = bot
                    .send_message(
                        msg.chat.id,
                        esc("Failed to get the image file from Telegram."),
                    )
                    .reply_to_message_id(msg.id)
                    .await;
                return ControlFlow::Break(());
            }
        };

        let mut buf: Vec<u8> = Vec::with_capacity(f.size as usize);
        if let Err(e) = teloxide::net::Download::download_file(&bot, &f.path, &mut buf).await {
            tracing::error!(
                "[photo handler] failed to download Telegram file {}: {}",
                f.path,
                e
            );

            let _ = bot
                .send_message(
                    msg.chat.id,
                    esc("Failed to download the image file from Telegram."),
                )
                .reply_to_message_id(msg.id)
                .await;
            return ControlFlow::Break(());
        }

        let search_result: SaucenaoOutput = match self.searcher.search(buf).await {
            Ok(result) => result,
            Err(e) => {
                tracing::error!("[photo handler] SauceNAO search failed: {}", e);
                let _ = bot
                    .send_message(
                        msg.chat.id,
                        esc("Image search failed. Please try again later."),
                    )
                    .reply_to_message_id(msg.id)
                    .await;
                return ControlFlow::Break(());
            }
        };

        let mut url_sim = None;
        let threshold = if msg.chat.is_private() {
            MIN_SIMILARITY_PRIVATE
        } else {
            MIN_SIMILARITY
        };
        for element in search_result
            .data
            .into_iter()
            .filter(|x| x.similarity >= threshold)
        {
            match element.parsed {
                SaucenaoParsed::EHentai(f_hash) => {
                    match self.convertor.convert_to_gallery(&f_hash).await {
                        Ok(url) => {
                            url_sim = Some((url, element.similarity));
                            break;
                        }
                        Err(e) => {
                            tracing::error!(
                                "[photo handler] failed to convert EHentai file hash to gallery URL: {}",
                                e
                            );
                            continue;
                        }
                    }
                }
                SaucenaoParsed::NHentai(nid) => {
                    url_sim = Some((format!("https://nhentai.net/g/{nid}/"), element.similarity));
                    break;
                }
                _ => continue,
            }
        }

        let (url, sim) = match url_sim {
            Some(u) => u,
            None => {
                trace!("[photo handler] image not found");
                return ControlFlow::Continue(());
            }
        };
        info!(
            "[photo handler] image matched {:?} for {url} with similarity {sim}",
            PrettyChat(&msg.chat)
        );

        if self
            .enqueue_batch_urls(&bot, &msg, vec![url.clone()])
            .await
        {
            return ControlFlow::Break(());
        }

        self.spawn_sync_reply(bot, msg, url, "photo handler").await
    }

    pub async fn respond_default(
        self: Arc<Self>,
        bot: DefaultParseMode<Bot>,
        msg: Message,
    ) -> ControlFlow<()> {
        if msg.chat.is_private() {
            if let Err(e) = bot
                .send_message(msg.chat.id, esc("Unrecognized message."))
                .reply_to_message_id(msg.id)
                .await
            {
                tracing::error!(
                    "[default handler] failed to send unrecognized-message reply: {}",
                    e
                );
            }
        }
        #[cfg(debug_assertions)]
        tracing::warn!("{:?}", msg);
        ControlFlow::Break(())
    }

    async fn sync_response(&self, url: &str) -> String {
        self.single_flight
            .work(url, || async {
                match self.route_sync(url).await {
                    Ok(result) => {
                        let title_line = result
                            .title
                            .map(|t| format!("Title: {}\n", esc(t)))
                            .unwrap_or_default();

                        let authors_line = result
                            .authors
                            .filter(|xs| !xs.is_empty())
                            .map(|xs| {
                                let authors = xs
                                    .into_iter()
                                    .map(|a| esc(a))
                                    .collect::<Vec<_>>()
                                    .join(", ");
                                format!("Authors: {}\n", authors)
                            })
                            .unwrap_or_default();

                        let tags_line = result
                            .tags
                            .filter(|xs| !xs.is_empty())
                            .map(|xs| {
                                let shown = xs
                                    .iter()
                                    .take(12)
                                    .map(|t| format!("\\#{}", esc(t.replace(' ', "_"))))
                                    .collect::<Vec<_>>();
                                format!("Tags: {}\n", shown.join(" "))
                            })
                            .unwrap_or_default();

                        format!(
                            "Sync to telegraph finished\n{}{}{}URL: {}",
                            title_line,
                            authors_line,
                            tags_line,
                            esc(result.page_url)
                        )
                    }
                    Err(e) => {
                        format!("Sync to telegraph failed: {}", esc(e.to_string()))
                    }
                }
            })
            .await
    }


    async fn route_stash(&self, url: &str) -> anyhow::Result<StashedGallery> {
        let parsed = Url::parse(url).map_err(|_| anyhow::anyhow!("Invalid url"))?;
        let host = parsed.host_str().unwrap_or_default();
        let path = parsed.path().to_owned();
        let source_url = url.to_owned();

        match host {
            "e-hentai.org" => {
                info!("[registry] stash e-hentai for path {}", path);
                self.synchronizer
                    .stash::<EHCollector>(path, source_url)
                    .await
            }
            "nhentai.to" | "nhentai.net" => {
                info!("[registry] stash nhentai for path {}", path);
                self.synchronizer
                    .stash::<NHCollector>(path, source_url)
                    .await
            }
            "exhentai.org" => {
                info!("[registry] stash exhentai for path {}", path);
                self.synchronizer
                    .stash::<EXCollector>(path, source_url)
                    .await
            }
            "pixiv.net" | "www.pixiv.net" => {
                let pixiv_path = match parsed.query() {
                    Some(query) => format!("{path}?{query}"),
                    None => path,
                };
                info!("[registry] stash pixiv for path {}", pixiv_path);
                self.synchronizer
                    .stash::<PixivCollector>(pixiv_path, source_url)
                    .await
            }
            _ => Err(anyhow::anyhow!("no matching collector")),
        }
    }

    async fn route_sync(&self, url: &str) -> anyhow::Result<SyncResult> {
        let u = Url::parse(url).map_err(|_| anyhow::anyhow!("Invalid url"))?;
        let host = u.host_str().unwrap_or_default();
        let path = u.path().to_string();

        // TODO: use macro to generate them
        // Lilia's crit: Keep routing explicit. normalize hosts, instead of introducing a macro.

        #[allow(clippy::single_match)]
        match host {
            "e-hentai.org" => {
                info!("[registry] sync e-hentai for path {}", path);
                self.synchronizer
                    .sync::<EHCollector>(path)
                    .await
                    .map_err(anyhow::Error::from)
            }
            "nhentai.to" | "nhentai.net" => {
                info!("[registry] sync nhentai for path {}", path);
                self.synchronizer
                    .sync::<NHCollector>(path)
                    .await
                    .map_err(anyhow::Error::from)
            }
            "exhentai.org" => {
                info!("[registry] sync exhentai for path {}", path);
                self.synchronizer
                    .sync::<EXCollector>(path)
                    .await
                    .map_err(anyhow::Error::from)
            }
            "pixiv.net" | "www.pixiv.net" => {
                let pixiv_path = match u.query() {
                    Some(query) => format!("{path}?{query}"),
                    None => path,
                };
                info!("[registry] sync pixiv for path {}", pixiv_path);
                self.synchronizer
                    .sync::<PixivCollector>(pixiv_path)
                    .await
                    .map_err(anyhow::Error::from)
            }
            _ => Err(anyhow::anyhow!("no matching collector")),
        }
    }
}
