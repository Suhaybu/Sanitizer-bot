//! Core handler for all types of events

use std::sync::Arc;

use anyhow::Context;
use twilight_gateway::Event;
use twilight_http::Client;
use twilight_model::application::interaction::application_command::CommandData;
use twilight_model::application::interaction::message_component::MessageComponentInteractionData;
use twilight_model::application::interaction::{Interaction, InteractionData};
use twilight_model::channel::Message;
use twilight_model::channel::message::{Component, MessageFlags, MessageType};
use twilight_model::gateway::GatewayReaction;
use twilight_model::gateway::payload::incoming::MessageDelete;
use twilight_model::guild::Permissions;
use twilight_model::http::interaction::{InteractionResponse, InteractionResponseType};
use twilight_model::id::Id;
use twilight_util::builder::InteractionResponseDataBuilder;
use twilight_util::builder::message::{ContainerBuilder, TextDisplayBuilder};

use crate::db::{MessageAuthor, ResponseMap, ServerConfig};
use crate::discord::commands;
use crate::discord::models::{DeletePermission, SanitizerMode, SettingsMenuType};
use crate::utils::cache;
use crate::{sanitize, utils};

/// Handles all types of incomming events from Discord.
pub async fn handle_event(event: Event, client: Arc<Client>) {
    match event {
        Event::InteractionCreate(ctx) => {
            if let Err(e) = handle_interaction(ctx.0, &client).await {
                tracing::error!(?e, "Failed to handle Event::InteractionCreate");
            }
        }
        Event::MessageCreate(ctx) => {
            // Early exit if message is by the bot itself.
            if crate::BOT_USER_ID
                .get()
                .is_some_and(|&bot_id| ctx.0.author.id == bot_id)
                // Or if the message content has no URL & is not a reply
                // Note:
                //      Messages without URL are expected in SanitizerMode::ManualMention,
                //      such messages can mention the bot in a reply where the message
                //      referenced (replied to) has a valid URL.
                || (!utils::contains_url(&ctx.0.content) && !(ctx.0.kind == MessageType::Reply))
                // Special case: if message contains `—x`, ignore the message completely.
                || ctx.0.content.contains("—x")
            {
                return;
            }

            if let Err(e) = handle_on_message(ctx.0, &client).await {
                tracing::error!(?e, "Failed to handle Event::MessageCreate")
            }
        }
        Event::ReactionAdd(ctx) => {
            // Early exit if reaction is by bot or not in a guild.
            if crate::BOT_USER_ID
                .get()
                .is_some_and(|&bot_id| bot_id == ctx.0.user_id)
                || ctx.0.guild_id.is_none()
            {
                return;
            }

            if let Err(e) = handle_reaction_add(ctx.0, &client).await {
                tracing::error!(?e, "Failed to handle Event::ReactionAdd");
            }
        }
        Event::MessageDelete(ctx) => {
            if let Err(e) = handle_message_delete(ctx, &client).await {
                tracing::error!(?e, "Failed to handle Event::MessageDelete")
            }
        }
        _ => (),
    }
}

/// Handles twilight_gateway::Event::MessageDelete events.
async fn handle_message_delete(ctx: MessageDelete, client: &Client) -> anyhow::Result<()> {
    let matched_bot_response = ResponseMap::find_match(ctx.id).await?;

    let Some((response_map, side)) = matched_bot_response else {
        tracing::debug!(message_id = %ctx.id, "No match found for message");
        return Ok(());
    };

    tracing::debug!(message_id = %ctx.id, "Response match found for message");

    match side {
        // Handles case where the original user message is deleted.
        MessageAuthor::User => {
            if let Err(e) = client
                .delete_message(
                    Id::new(response_map.channel_id),
                    Id::new(response_map.bot_message_id),
                )
                .await
            {
                tracing::debug!(error = ?e, "Bot message already gone (likely race)");
            }
        }
        // Handles case where bot's response is deleted using
        // Discord interface, and not using the custom Delete button.
        MessageAuthor::Bot => {
            if let Ok(response) = client
                .message(
                    Id::new(response_map.channel_id),
                    Id::new(response_map.user_message_id),
                )
                .await
            {
                let user_msg = response
                    .model()
                    .await
                    .context("Failed to deserialize user message payload from Discord")?;

                if let Err(e) = utils::unsupress_embeds(&user_msg, client).await {
                    tracing::debug!(error = ?e, "Failed to unsuppress original message embed");
                }
            } else {
                tracing::debug!(
                    user_message_id = response_map.user_message_id,
                    "User message already deleted or inaccessible, skipping unsupressed"
                );
            }
        }
    }

    ResponseMap::delete_entry(ctx.id.get())
        .await
        .with_context(|| format!("Failed to delete response map entry for message {}", ctx.id))?;

    Ok(())
}

/// Handles twilight_gateway::Event::ReactionAdd events.
async fn handle_reaction_add(reaction: GatewayReaction, client: &Client) -> anyhow::Result<()> {
    // Exits early if reaction is not in a guild. This exit should never happen.
    let Some(guild_id) = reaction.guild_id else {
        anyhow::bail!("ReactionAdd is not in a guild.")
    };

    let reaction_emoji_id = match &reaction.emoji {
        twilight_model::channel::message::EmojiReactionType::Custom { id, .. } => *id,
        _ => return Ok(()),
    };

    if reaction_emoji_id == *crate::EMOJI_ID.get().unwrap() {
        let server_config = ServerConfig::get_or_default(guild_id.get()).await?;
        let message = client
            .message(reaction.channel_id, reaction.message_id)
            .await?
            .model()
            .await?;

        sanitize::process_message(&message, client, Some(server_config)).await?;
    }

    Ok(())
}

/// Handles twilight_gateway::Event::MessageCreate events.
async fn handle_on_message(message: Message, client: &Client) -> anyhow::Result<()> {
    // Retrieves guild_id, else early exits.
    let Some(guild_id) = message.guild_id else {
        sanitize::process_message(&message, client, None).await?;
        return Ok(());
    };
    let server_config = cache::load().get_or_fetch(guild_id.get()).await?;

    match server_config.sanitizer_mode {
        SanitizerMode::Automatic => {
            sanitize::process_message(&message, client, Some(server_config)).await?;
        }
        SanitizerMode::ManualEmote => {
            sanitize::add_emote(&message, client).await?;
        }
        SanitizerMode::ManualMention => {
            // Early exit if not mentioned.
            if !utils::is_bot_mentioned(&message) && message.kind != MessageType::Reply {
                return Ok(());
            }
            sanitize::process_message(&message, client, Some(server_config)).await?;
        }
        SanitizerMode::ManualBoth => {
            sanitize::add_emote(&message, client).await?;
            if !utils::is_bot_mentioned(&message) && message.kind != MessageType::Reply {
                return Ok(());
            }
            sanitize::process_message(&message, client, Some(server_config)).await?;
        }
    }

    Ok(())
}

/// Handles twilight_gateway::Event::InteractionCreate events.
async fn handle_interaction(mut interaction: Interaction, client: &Client) -> anyhow::Result<()> {
    let Some(data) = interaction.data.take() else {
        tracing::debug!("Ignoring interaction with no data");
        return Ok(());
    };

    match data {
        // Handles command invocations
        InteractionData::ApplicationCommand(data) => {
            tracing::debug!("Recieved ApplicationCommand event with name: {}", data.name);
            handle_app_command(data.name.as_str(), &interaction, client, &data).await
        }
        // Handles component invocations
        InteractionData::MessageComponent(data) => {
            tracing::debug!(
                "Recieved MessageComponent event with custom_id: {}",
                data.custom_id
            );
            handle_component(&data, &interaction, client).await
        }
        _ => {
            tracing::debug!("Ignoring unknown interaction type");

            Ok(())
        }
    }
}

/// Matches command name from event data to call the respective command handler
async fn handle_app_command(
    command_name: &str,
    interaction: &Interaction,
    client: &Client,
    data: &CommandData,
) -> anyhow::Result<()> {
    match command_name {
        "help" => commands::HelpCommand::handle(interaction, client).await,
        "settings" => commands::SettingsCommand::handle(interaction, client).await,
        "Sanitize" | "sanitize" => {
            commands::SanitizeCommand::handle(interaction, client, data).await
        }
        unknown_name => anyhow::bail!("unknown command: {}", unknown_name),
    }
}

/// Matches the component id from event data to call the respective component handler
async fn handle_component(
    data: &MessageComponentInteractionData,
    interaction: &Interaction,
    client: &Client,
) -> anyhow::Result<()> {
    match data.custom_id.as_str() {
        "delete" => handle_delete_button(interaction, client).await,
        _ => {
            let menu_type = data
                .custom_id
                .parse::<SettingsMenuType>()
                .with_context(|| format!("Unknown component: {}", data.custom_id))?;
            commands::SettingsCommand::handle_component(interaction, menu_type, data, client).await
        }
    }
}

/// These handles delete button for both Interaction & OnMessage invocation of Sanitize.
/// Expected to be called ONLY in Guild Context, NOT in PrivateChannel Context.
async fn handle_delete_button(interaction: &Interaction, client: &Client) -> anyhow::Result<()> {
    let Some(ref bot_msg) = interaction.message else {
        anyhow::bail!("Delete button pressed but no message found")
    };

    // Have to get the message again via REST so that we can access the referenced message.
    let msg = client
        .message(bot_msg.channel_id, bot_msg.id)
        .await?
        .model()
        .await?;

    // Early exit should not happen. Delete button is only to be created in guild context.
    let Some(guild_id) = interaction.guild_id else {
        anyhow::bail!("Interaction missing guild_id")
    };
    let server_config = ServerConfig::get_or_default(guild_id.get()).await?;

    if server_config.delete_permission == DeletePermission::Disabled {
        tracing::debug!("Early exit: Delete button is disabled in server config");
        return Ok(());
    }

    // This is the user_id of who invoked the Delete button.
    let invoker_id = interaction
        .author_id()
        .ok_or_else(|| anyhow::anyhow!("User ID could not be found in interaction."))?;

    let invoker_has_perm = match server_config.delete_permission {
        DeletePermission::Everyone => true,
        DeletePermission::Disabled => false,
        DeletePermission::AuthorAndMods => {
            let author_id = msg.referenced_message.as_ref().map(|msg| msg.author.id);

            let is_author = author_id.is_some_and(|author_id| author_id == invoker_id);

            let has_manage_message = interaction
                .member
                .as_ref()
                .and_then(|m| m.permissions)
                .is_some_and(|perms| perms.contains(Permissions::MANAGE_MESSAGES));

            is_author || has_manage_message
        }
    };

    // Early exit and a message to the user if user has insufficient permissions.
    if !invoker_has_perm {
        tracing::debug!(
            "User {} does not have permission to delete this message.",
            invoker_id
        );

        let container = ContainerBuilder::new()
            .spoiler(false)
            .component(
                TextDisplayBuilder::new("You do not have permission to delete this message")
                    .build(),
            )
            .build();
        let data = InteractionResponseDataBuilder::new()
            .components([Component::Container(container)])
            .flags(MessageFlags::IS_COMPONENTS_V2 | MessageFlags::EPHEMERAL)
            .build();
        let response = InteractionResponse {
            kind: InteractionResponseType::ChannelMessageWithSource,
            data: Some(data),
        };

        client
            .interaction(interaction.application_id)
            .create_response(interaction.id, &interaction.token, &response)
            .await?;

        return Ok(());
    };

    // Attempts to Delete bot's response.
    match client.delete_message(bot_msg.channel_id, bot_msg.id).await {
        Ok(_) => tracing::debug!(
            "Deleted bot message {} in channel {}",
            bot_msg.id,
            bot_msg.channel_id
        ),
        Err(e) => {
            tracing::debug!("Bot message already gone (likely race): {:?}", e);
            return Ok(());
        }
    }

    if let Some(referenced_message) = msg.referenced_message.as_ref() {
        // Removes the now-stale response_map entry so a later deletion of the
        // original message doesn't try to re-delete this already-deleted bot message.
        if let Err(e) = ResponseMap::delete_entry(referenced_message.id.get()).await {
            tracing::warn!(error = ?e, "Failed to delete response_map entry");
        }

        // Handles unsupressing original embed.
        if server_config.hide_original_embed
            && let Err(e) = utils::unsupress_embeds(referenced_message, client).await
        {
            tracing::warn!(error = ?e, "Failed to unsuppress original message embed");
        }
    }

    Ok(())
}
