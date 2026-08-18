mod db;
mod discord;
mod sanitize;
mod utils;

use std::env;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use anyhow::Context;
use time::UtcOffset;
use time::macros::format_description;
use tracing::Level;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::time::OffsetTime;
use twilight_gateway::{
    CloseFrame, ConfigBuilder, Event, EventTypeFlags, Intents, Shard, StreamExt as _,
};
use twilight_http::Client;
use twilight_model::gateway::payload::outgoing::update_presence::UpdatePresencePayload;
use twilight_model::gateway::presence::{ActivityType, MinimalActivity, Status};
use twilight_model::id::Id;
use twilight_model::id::marker::{EmojiMarker, UserMarker};

use crate::discord::{commands, handle_event};
use crate::utils::cache::ConfigCache;

// Flag that can be checked by any part of the program.
static SHUTDOWN: AtomicBool = AtomicBool::new(false);
// Gets set during initialization of the bot.
static BOT_USER_ID: OnceLock<Id<UserMarker>> = OnceLock::new();
// Custom Emoji ID.
static EMOJI_ID: OnceLock<Id<EmojiMarker>> = OnceLock::new();
// Cache for server config.
static CONFIG_CACHE: OnceLock<ConfigCache> = OnceLock::new();

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    if let Err(err) = run().await {
        tracing::error!("Critical error: {:#}", err);
        tracing::error!("Error details: {:#?}", err);
        std::process::exit(1);
    }
    Ok(())
}

/// Runner logic that spins the bot up.
async fn run() -> anyhow::Result<()> {
    // Initializes tracing logger and db.
    prerun_init().await?;

    let token = env::var("DISCORD_TOKEN").context("DISCORD_TOKEN environment variable not set")?;

    let emoji_id = env::var("EMOJI_ID")
        .context("EMOJI_ID environment variable not set")?
        .parse::<u64>()
        .context("EMOJI_ID must be a valid u64")?;
    EMOJI_ID
        .set(Id::<EmojiMarker>::new(emoji_id))
        .expect("EMOJI_ID already initialized");

    let intents = Intents::GUILD_MESSAGES
        | Intents::DIRECT_MESSAGES
        | Intents::MESSAGE_CONTENT
        | Intents::GUILD_MESSAGE_REACTIONS;

    // Set the bot's Discord status.
    let activity = MinimalActivity {
        kind: ActivityType::Watching,
        name: String::from("for embeds"),
        url: None,
    };
    let bot_presence = UpdatePresencePayload {
        activities: vec![activity.into()],
        afk: false,
        since: None,
        status: Status::Online,
    };

    // Initialize Twilight HTTP client and gateway configuration.
    let client = Arc::new(Client::new(token.clone()));
    let config = ConfigBuilder::new(token.clone(), intents)
        .presence(bot_presence)
        .build();

    // Register global commands.
    commands::register_global_commands(&client).await?;

    // Fetch & store the bot's user id for future use in event handler.
    let bot = client.current_user().await?.model().await?;
    BOT_USER_ID
        .set(bot.id)
        .expect("BOT_USER_ID already initialized");
    tracing::info!("{} online with ID: {}", bot.name, bot.id);

    // Initialize cache
    CONFIG_CACHE
        .set(ConfigCache::new())
        .expect("CONFIG_CACHE already initialized");
    tracing::info!("Config cache initialized");

    // Start gateway shards.
    let shards =
        twilight_gateway::create_recommended(&client, config, |_id, builder| builder.build())
            .await?;
    let shard_len = shards.len();
    let mut senders = Vec::with_capacity(shard_len);
    let mut tasks = Vec::with_capacity(shard_len);

    for shard in shards {
        senders.push(shard.sender());
        tasks.push(tokio::spawn(shard_runner(shard, client.clone())));
    }

    // Handle exiting Ctrl+C gracefully.
    tokio::signal::ctrl_c().await?;
    println!(); // Forces tracing info output to be on a seperate line.
    tracing::info!("Shutting down bot gracefully.");

    SHUTDOWN.store(true, Ordering::Relaxed);
    for sender in senders {
        // Ignore error if shard's already shutdown.
        _ = sender.close(CloseFrame::NORMAL);
    }

    // Wait for all background tasks to finish.
    for join_handle in tasks {
        _ = join_handle.await;
    }

    Ok(())
}

/// Handles shards.
async fn shard_runner(mut shard: Shard, client: Arc<Client>) {
    // Runs until next_event returns None.
    while let Some(item) = shard.next_event(EventTypeFlags::all()).await {
        let event = match item {
            Ok(Event::GatewayClose(_)) if SHUTDOWN.load(Ordering::Relaxed) => break,
            Ok(event) => event,
            Err(error) => {
                tracing::warn!(?error, "error while receiving event");
                continue;
            }
        };

        // Process Discord events (see `discord::events.rs` for implementation).
        tracing::debug!(kind = ?event.kind(), shard = ?shard.id().number(), "received event");
        tokio::spawn(handle_event(event, client.clone()));
    }
}

/// Pre-run initialization sequence for the bot.
async fn prerun_init() -> anyhow::Result<()> {
    let _ = dotenvy::dotenv();

    // Initialize logging with tracing.
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new(format!("{},turso_sync_engine=warn", Level::INFO.as_str()))
    });

    let time_format = format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]");
    let timer = OffsetTime::new(UtcOffset::UTC, time_format);

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_timer(timer)
        .compact()
        .init();

    tracing::debug!("Logging initialized");

    db::init().await?;

    Ok(())
}
