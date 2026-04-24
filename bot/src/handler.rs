use std::{borrow::Cow, collections::HashSet};
use std::{ops::ControlFlow, sync::Arc};

use eh2telegraph::{
    collector::{e_hentai::EHCollector, exhentai::EXCollector, nhentai::NHCollector},
    searcher::{
        f_hash::FHashConvertor,
        saucenao::{SaucenaoOutput, SaucenaoParsed, SaucenaoSearcher},
        ImageSearcher,
    },
    storage::KVStorage,
    sync::{Synchronizer, SyncResult},
};
use reqwest::Url;
use teloxide::{
    adaptors::DefaultParseMode,
    prelude::*,
    utils::{
        command::BotCommands,
        markdown::{code_inline},
    },
};

use tracing::{info, trace};
use tokio::time::{timeout, Duration};
use crate::{ok_or_break, util::PrettyChat, util::esc};

const MIN_SIMILARITY: u8 = 70;
const MIN_SIMILARITY_PRIVATE: u8 = 50;


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
        description = "Sync a gallery(e-hentai/exhentai/nhentai are supported now). 同步一个画廊(目前支持 EH/EX/NH)"
    )]
    Sync(String),
}

#[derive(BotCommands, Clone)]
#[command(rename_rule = "lowercase", description = "Command for admins")]
pub enum AdminCommand {
    #[command(description = "Delete cache with given key.")]
    Delete(String),
}

pub struct Handler<C> {
    pub synchronizer: Synchronizer<C>,
    pub searcher: SaucenaoSearcher,
    pub convertor: FHashConvertor,
    pub admins: HashSet<i64>,

    single_flight: singleflight_async::SingleFlight<String>,
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
        }
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

                return self
                    .spawn_sync_reply(bot, msg, url, "cmd handler")
                    .await;
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
                            .send_message(msg.chat.id, esc(&format!("Failed to delete key {key}: {e}")))
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
        let maybe_link = {
            let entries = msg
                .entities()
                .map(|es| {
                    es.iter().filter_map(|e| {
                        if let teloxide::types::MessageEntityKind::TextLink { url } = &e.kind {
                            Synchronizer::match_url_from_text(url.as_ref()).map(ToOwned::to_owned)
                        } else {
                            None
                        }
                    })
                })
                .into_iter()
                .flatten();
            msg.text()
                .and_then(|content| {
                    Synchronizer::match_url_from_text(content).map(ToOwned::to_owned)
                })
                .into_iter()
                .chain(entries)
                .next()
        };

        if let Some(url) = maybe_link {
            return self
                .spawn_sync_reply(bot, msg, url, "text handler")
                .await;
        }

        // fallback to the next branch
        ControlFlow::Continue(())
    }

    pub async fn respond_caption(
        self: Arc<Self>,
        bot: DefaultParseMode<Bot>,
        msg: Message,
    ) -> ControlFlow<()> {
        let caption_entities = msg.caption_entities();
        let mut final_url = None;
        for entry in caption_entities.map(|x| x.iter()).into_iter().flatten() {
            let url = match &entry.kind {
                teloxide::types::MessageEntityKind::Url => {
                    let Some(raw) = msg.caption() else {
                        return ControlFlow::Continue(());
                    };
                    let encoded: Vec<_> = raw
                        .encode_utf16()
                        .skip(entry.offset)
                        .take(entry.length)
                        .collect();
                    let content = ok_or_break!(String::from_utf16(&encoded));
                    Cow::from(content)
                }
                teloxide::types::MessageEntityKind::TextLink { url } => Cow::from(url.as_ref()),
                _ => {
                    continue;
                }
            };
            let url = if let Some(c) = Synchronizer::match_url_from_url(&url) {
                c
            } else {
                continue;
            };
            final_url = Some(url.to_string());
            break;
        }

        match final_url {
            Some(url) => self
                .spawn_sync_reply(bot, msg, url, "caption handler")
                .await,
            None => ControlFlow::Continue(()),
        }
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

        let f = ok_or_break!(bot.get_file(&first_photo.file.id).await);
        let mut buf: Vec<u8> = Vec::with_capacity(f.size as usize);
        ok_or_break!(teloxide::net::Download::download_file(&bot, &f.path, &mut buf).await);
        let search_result: SaucenaoOutput = ok_or_break!(self.searcher.search(buf).await);

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
                    url_sim = Some((
                        ok_or_break!(self.convertor.convert_to_gallery(&f_hash).await),
                        element.similarity,
                    ));
                    break;
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

        self.spawn_sync_reply(bot, msg, url, "photo handler").await
    }

    pub async fn respond_default(
        self: Arc<Self>,
        bot: DefaultParseMode<Bot>,
        msg: Message,
    ) -> ControlFlow<()> {
        if msg.chat.is_private() {
            ok_or_break!(
                bot.send_message(msg.chat.id, esc("Unrecognized message."))
                    .reply_to_message_id(msg.id)
                    .await
            );
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
                                    .map(|t| format!("#{}", esc(t.replace(' ', "_"))))
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
            _ => Err(anyhow::anyhow!("no matching collector")),
        }
    }
}
