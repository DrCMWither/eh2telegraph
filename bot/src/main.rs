use eh2telegraph::{
    collector::Registry,
    config::{self},
    http_proxy::ProxiedClient,
    storage,
    sync::Synchronizer,
    telegraph::Telegraph,
};

use std::time::Duration;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

use clap::Parser;

use teloxide::{
    adaptors::DefaultParseMode,
    error_handlers::IgnoringErrorHandler,
    prelude::*,
    types::{AllowedUpdate, ChatPermissions, ParseMode, UpdateKind},
    update_listeners,
    net::client_from_env,
};

use tracing::level_filters::LevelFilter;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

use handler::{Command, Handler};

use crate::{
    handler::AdminCommand,
    util::{wrap_endpoint, PrettyChat},
};

mod handler;
mod util;
mod version;

#[derive(Debug, serde::Deserialize)]
pub struct AppConfig {
    pub base: BaseConfig,
    pub image_proxy: ImageProxyConfig,
}

#[derive(Debug, serde::Deserialize)]
pub struct BaseConfig {
    pub bot_token: String,
    pub telegraph: TelegraphConfig,
    #[serde(default)]
    pub admins: Vec<i64>,
    #[serde(default = "default_polling_timeout")]
    pub polling_timeout_secs: u64,
    #[serde(default = "default_restart_delay")]
    pub restart_delay_secs: u64,
}

fn default_polling_timeout() -> u64 { 10 }
fn default_restart_delay()   -> u64 { 5  }

#[derive(Debug, serde::Deserialize)]
pub struct TelegraphConfig {
    pub tokens: Vec<String>,
    pub author_name: Option<String>,
    pub author_url: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
pub struct ImageProxyConfig {
    pub base_url: String,
}

#[derive(Parser, Debug)]
#[clap(author, version=version::VERSION, about, long_about = "eh2telegraph sync bot")]
struct Args {
    #[clap(short, long, help = "Config file path")]
    config: Option<String>,
}

// Don't use dptree unless you want to #[cfg(debug_assertions)] a bunch of type alias
fn wrap2param<H, F, R>(
    handler: Arc<H>,
    f: F,
) -> impl Fn(DefaultParseMode<Bot>, Message) -> BoxFuture<'static, R> + Clone
where
    H: Send + Sync + 'static,
    F: Fn(Arc<H>, DefaultParseMode<Bot>, Message) -> BoxFuture<'static, R>
        + Clone
        + Send
        + Sync
        + 'static,
    R: Send + 'static,
{
    move |bot, msg| {
        let handler = Arc::clone(&handler);
        f(handler, bot, msg)
    }
}

fn wrap3param<H, F, R, C>(
    handler: Arc<H>,
    f: F,
) -> impl Fn(DefaultParseMode<Bot>, Message, C) -> BoxFuture<'static, R> + Clone
where
    H: Send + Sync + 'static,
    C: Send + 'static,
    F: Fn(Arc<H>, DefaultParseMode<Bot>, Message, C) -> BoxFuture<'static, R>
        + Clone
        + Send
        + Sync
        + 'static,
    R: Send + 'static,
{
    move |bot, msg, cmd| {
        let handler = Arc::clone(&handler);
        f(handler, bot, msg, cmd)
    }
}

#[cfg(unix)]
async fn wait_for_shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut sigterm = signal(SignalKind::terminate()).expect("Failed to bind SIGTERM");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = sigterm.recv() => {}
    }
}

#[cfg(not(unix))]
async fn wait_for_shutdown_signal() {
    tokio::signal::ctrl_c().await.expect("Failed to listen for ctrl_c");
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    let timer = tracing_subscriber::fmt::time::LocalTime::new(time::macros::format_description!(
        "[month]-[day] [hour]:[minute]:[second]"
    ));

    tracing_subscriber::registry()
        .with(fmt::layer().with_timer(timer))
        .with(
            EnvFilter::builder()
                .with_default_directive(LevelFilter::INFO.into())
                .from_env_lossy(),
        )
        .init();

    tracing::info!("initializing...");

    // ---- static init: only do once ----
    config::init(args.config);

    let base_config: BaseConfig = config::parse("base")?
        .ok_or_else(|| anyhow::anyhow!("base config missing or empty"))?;
    let image_proxy_config: ImageProxyConfig = config::parse("image_proxy")?
        .ok_or_else(|| anyhow::anyhow!("image_proxy config missing or empty"))?;

    let telegraph_config = base_config.telegraph;

    let telegraph =
        Telegraph::new(telegraph_config.tokens).with_proxy(ProxiedClient::new_from_config());

    let registry = Registry::new_from_config();

    #[cfg(debug_assertions)]
    let cache = storage::SimpleMemStorage::default();
    #[cfg(not(debug_assertions))]
    let cache = storage::cloudflare_kv::CFOrMemStorage::new_from_config()?;

    let mut synchronizer = Synchronizer::new(
        telegraph,
        registry,
        cache,
        image_proxy_config.base_url,
    );

    if telegraph_config.author_name.is_some() {
        synchronizer =
            synchronizer.with_author(telegraph_config.author_name, telegraph_config.author_url);
    }

    let admins = base_config.admins.into_iter().collect();
    let handler = Arc::new(Handler::new(synchronizer, admins));

    // ---- handler closures: only build once ----
    let command_handler = wrap3param(handler.clone(), |handler, bot, message, command| {
        Box::pin(async move {
            handler.respond_cmd(bot, message, command).await
        })
    });

    let admin_command_handler = wrap3param(handler.clone(), |handler, bot, message, command| {
        Box::pin(async move {
            handler.respond_admin_cmd(bot, message, command).await
        })
    });

    let text_handler = wrap2param(handler.clone(), |handler, bot, message| {
        Box::pin(async move {
            handler.respond_text(bot, message).await
        })
    });

    let caption_handler = wrap2param(handler.clone(), |handler, bot, message| {
        Box::pin(async move {
            handler.respond_caption(bot, message).await
        })
    });

    let photo_handler = wrap2param(handler.clone(), |handler, bot, message| {
        Box::pin(async move {
            handler.respond_photo(bot, message).await
        })
    });

    let default_handler = wrap2param(handler.clone(), |handler, bot, message| {
        Box::pin(async move {
            handler.respond_default(bot, message).await
        })
    });

    let permission_filter = |bot: DefaultParseMode<Bot>, message: Message| async move {
        let blocked = message
            .chat
            .permissions()
            .map(|p| !p.contains(ChatPermissions::SEND_MESSAGES))
            .unwrap_or_default();

        if blocked {
            tracing::info!(
                "[permission filter] leave chat {:?}",
                PrettyChat(&message.chat)
            );
            let _ = bot.leave_chat(message.chat.id).await;
            None
        } else {
            Some(message)
        }
    };

    let process_message_date = chrono::Utc::now()
    .checked_sub_signed(chrono::Duration::try_days(1).unwrap())
    .expect("illegal current date");


    let time_filter = move |message: Message| {
        let boundary = process_message_date;
        async move {
            if message.date > boundary {
                Some(message)
            } else {
                None
            }
        }
    };

    tracing::info!("initializing finished, entering supervisor loop");

    let app_shutdown = CancellationToken::new();

    {
        let app_shutdown = app_shutdown.clone();
        tokio::spawn(async move {
            wait_for_shutdown_signal().await;
            tracing::info!("system shutdown signal received");
            app_shutdown.cancel();
        });
    }

    let telegram_client = client_from_env();
    // ---- supervisor loop ----
    loop {
        tracing::info!("starting telegram bot...");

        let bot = Bot::with_client(base_config.bot_token.clone(), telegram_client.clone())
        .parse_mode(ParseMode::MarkdownV2);

        let admin_filter_handler = handler.clone();

        let admin_command_handler_ = admin_command_handler.clone();
        let command_handler_ = command_handler.clone();
        let text_handler_ = text_handler.clone();
        let caption_handler_ = caption_handler.clone();
        let photo_handler_ = photo_handler.clone();
        let default_handler_ = default_handler.clone();

        // startup probe: avoid panic-on-start when network is unstable
        loop {
            tokio::select! {
                _ = app_shutdown.cancelled() => {
                    tracing::info!("shutdown signal received during startup probe");
                    return Ok(());
                }
                res = bot.get_me().send() => {
                    match res {
                        Ok(me) => {
                            tracing::info!(
                                "telegram ready: @{}",
                                me.user.username.unwrap_or_else(|| "<unknown>".to_string())
                            );
                            break;
                        }
                        Err(e) => {
                            tracing::warn!("telegram getMe failed: {e}; retrying in 5s");
                            tokio::select! {
                                _ = app_shutdown.cancelled() => {
                                    tracing::info!("shutdown signal received during retry delay");
                                    return Ok(());
                                }
                                _ = sleep(Duration::from_secs(base_config.restart_delay_secs)) => {}
                            }
                        }
                    }
                }
            }
        }

        let mut bot_dispatcher = Dispatcher::builder(
            bot.clone(),
            dptree::entry()
                .chain(dptree::filter_map(move |update: Update| match update.kind {
                    UpdateKind::Message(x) | UpdateKind::EditedMessage(x) => Some(x),
                    _ => None,
                }))
                .chain(dptree::filter_map_async(time_filter))
                .chain(dptree::filter_map_async(permission_filter))
                .branch(
                    dptree::entry()
                        .chain(dptree::filter(move |message: Message| {
                            admin_filter_handler.admins.contains(&message.chat.id.0)
                        }))
                        .filter_command::<AdminCommand>()
                        .branch(wrap_endpoint(admin_command_handler_)),
                )
                .branch(
                    dptree::entry()
                        .filter_command::<Command>()
                        .branch(wrap_endpoint(command_handler_)),
                )
                .branch(
                    dptree::entry()
                        .chain(dptree::filter_map(move |message: Message| {
                            #[allow(clippy::manual_map)]
                            match message.text() {
                                Some(v) if !v.is_empty() => Some(message),
                                _ => None,
                            }
                        }))
                        .branch(wrap_endpoint(text_handler_)),
                )
                .branch(
                    dptree::entry()
                        .chain(dptree::filter_map(move |message: Message| {
                            #[allow(clippy::manual_map)]
                            match message.caption() {
                                Some(v) if !v.is_empty() => Some(message),
                                _ => None,
                            }
                        }))
                        .branch(wrap_endpoint(caption_handler_)),
                )
                .branch(
                    dptree::entry()
                        .chain(dptree::filter_map(move |message: Message| {
                            #[allow(clippy::manual_map)]
                            match message.photo() {
                                Some(v) if !v.is_empty() => Some(message),
                                _ => None,
                            }
                        }))
                        .branch(wrap_endpoint(photo_handler_)),
                )
                .branch(wrap_endpoint(default_handler_)),
        )
        .default_handler(Box::new(|_upd| {
            #[cfg(debug_assertions)]
            tracing::warn!("Unhandled update: {:?}", _upd);
            Box::pin(async {})
        }))
        .error_handler(std::sync::Arc::new(IgnoringErrorHandler))
        .build();

        let shutdown = bot_dispatcher.shutdown_token();

        let bot_listener = update_listeners::Polling::builder(bot.clone())
            .allowed_updates(vec![AllowedUpdate::Message, AllowedUpdate::EditedMessage])
            .timeout(Duration::from_secs(base_config.polling_timeout_secs))
            .build();

        tracing::info!("bot is running");

        let dispatch_fut = bot_dispatcher
            .dispatch_with_listener(
                bot_listener,
                LoggingErrorHandler::with_custom_text("An error from the update listener")
            );
        tokio::pin!(dispatch_fut);

        let should_exit = tokio::select! {
            _ = app_shutdown.cancelled() => {
                tracing::info!("shutdown signal received, asking dispatcher to stop");
                let _ = shutdown.shutdown();
                let _ = (&mut dispatch_fut).await;
                true
            }
            _ = &mut dispatch_fut => {
                false
            }
        };

        if should_exit {
            tracing::info!("supervisor exiting");
            break;
        }

        tracing::warn!("dispatcher stopped unexpectedly; restarting in 5s");
        tokio::select! {
            _ = app_shutdown.cancelled() => {
                tracing::info!("shutdown signal received during restart delay");
                break;
            }
            _ = sleep(Duration::from_secs(base_config.restart_delay_secs)) => {}
        }
    }
    Ok(())
}