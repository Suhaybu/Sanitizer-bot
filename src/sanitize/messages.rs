use twilight_http::Client;
use twilight_http::request::channel::reaction::RequestReactionType;
use twilight_model::channel::Message;
use twilight_model::channel::message::component::{ActionRow, Button, ButtonStyle};
use twilight_model::channel::message::{
    AllowedMentions, Component, EmojiReactionType, MessageFlags, MessageType,
};

use crate::db::{ResponseMap, ServerConfig};
use crate::discord::models::{DeletePermission, SanitizerMode};
use crate::sanitize::UrlProcessor;
use crate::utils;

/// Converts the URL in a message if there is a valid URL.
pub async fn process_message(
    message: &Message,
    client: &Client,
    server_config: Option<ServerConfig>,
) -> anyhow::Result<()> {
    let mut target_message = message;
    let mut all_links = utils::get_links(target_message);

    if all_links.is_empty() {
        let fallback = if let Some(config) = server_config.as_ref()
            && (config.sanitizer_mode == SanitizerMode::ManualMention
                || config.sanitizer_mode == SanitizerMode::ManualBoth)
            && message.kind == MessageType::Reply
        {
            message.referenced_message.as_deref()
        } else {
            None
        };

        // Updates the target message to ref_msg
        if let Some(ref_msg) = fallback {
            target_message = ref_msg;
            all_links = utils::get_links(target_message);
        }
    }

    // Exits early if no links are found
    if all_links.is_empty() {
        return Ok(());
    }

    let mut combined_outputs = Vec::new();
    let mut processed_urls = Vec::new();

    for link in &all_links {
        let Some(url) = UrlProcessor::try_new(link, false) else {
            continue;
        };

        let Some(original_url) = url.get_original_url() else {
            tracing::error!("Original URL was not found.");
            continue;
        };

        let Some(captures) = url.capture_url().await else {
            return Err(anyhow::anyhow!("Failed to process URL"));
        };

        let Some(output) = captures.format_output() else {
            return Err(anyhow::anyhow!("Failed to process URL"));
        };

        combined_outputs.push(output);
        processed_urls.push(original_url);
    }

    if combined_outputs.is_empty() {
        return Ok(());
    }

    let mut all_buttons = Vec::new();
    let total_successes = processed_urls.len();

    for (idx, original_url) in processed_urls.into_iter().enumerate() {
        let label = if idx != 0 || total_successes > 1 {
            format!("Open Link {}", idx + 1)
        } else {
            "Open Link".to_string()
        };

        all_buttons.push(Component::Button(Button {
            id: None,
            custom_id: None,
            disabled: false,
            emoji: Some(EmojiReactionType::Unicode {
                name: "🔗".to_string(),
            }),
            label: Some(label),
            style: ButtonStyle::Link,
            url: Some(original_url),
            sku_id: None,
        }));
    }

    // Adds a delete button if config allows it and it's in a guild.
    if server_config
        .as_ref()
        .is_some_and(|config| config.delete_permission != DeletePermission::Disabled)
    {
        let delete_emoji = EmojiReactionType::Unicode {
            name: "🗑️".to_string(),
        };
        all_buttons.push(Component::Button(Button {
            id: None,
            custom_id: Some("delete".to_owned()),
            disabled: false,
            emoji: Some(delete_emoji),
            label: Some("Delete".to_owned()),
            style: ButtonStyle::Danger,
            url: None,
            sku_id: None,
        }));
    }

    let final_content = combined_outputs.join("\n");

    let components: Vec<Component> = all_buttons
        .chunks(5)
        .map(|chunk| {
            Component::ActionRow(ActionRow {
                id: None,
                components: chunk.to_vec(),
            })
        })
        .collect();

    let bot_response = client
        .create_message(message.channel_id)
        .content(&final_content)
        .components(&components)
        .reply(message.id)
        .allowed_mentions(Some(&AllowedMentions {
            replied_user: false,
            ..Default::default()
        }))
        .await?
        .model()
        .await?;

    // Saves the response in the response map.
    let response_map = ResponseMap::new(message, bot_response.id);
    if let Err(e) = response_map.save().await {
        tracing::warn!("Failed to save response_map due to: {:?}", e);
    }

    // Early exits if message is not in a server
    let Some(server_config) = server_config else {
        return Ok(());
    };

    // Removes all Sanitized emoji reactions after responding.
    if server_config.sanitizer_mode == SanitizerMode::ManualEmote
        || server_config.sanitizer_mode == SanitizerMode::ManualBoth
    {
        tracing::debug!("Removing all Sanitized emoji reactions.");
        client
            .delete_all_reaction(
                message.channel_id,
                message.id,
                &RequestReactionType::Custom {
                    id: crate::EMOJI_ID.get().unwrap().to_owned(),
                    name: Some("Sanitized"),
                },
            )
            .await?;
    }

    if server_config.hide_original_embed {
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

        if ResponseMap::find_match(message.id).await?.is_some()
            && let Err(e) = client
                .update_message(message.channel_id, message.id)
                .flags(MessageFlags::SUPPRESS_EMBEDS)
                .await
        {
            tracing::debug!("Failed to suppress embed (likely already deleted): {:?}", e);
        }
    }

    Ok(())
}

/// Adds an emote to a valid message in the Sanitizer::ManualEmote/Both mode.
pub async fn add_emote(message: &Message, client: &Client) -> anyhow::Result<()> {
    // Exits early if URL is not valid
    if UrlProcessor::try_new(&message.content, false).is_none() {
        tracing::debug!("No valid URL found in message");
        return Ok(());
    };

    client
        .create_reaction(
            message.channel_id,
            message.id,
            &RequestReactionType::Custom {
                id: crate::EMOJI_ID.get().unwrap().to_owned(),
                name: Some("Sanitized"),
            },
        )
        .await?;

    Ok(())
}
