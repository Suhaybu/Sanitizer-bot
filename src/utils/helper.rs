use twilight_http::Client;
use twilight_model::channel::Message;
use twilight_model::channel::message::MessageFlags;

use crate::BOT_USER_ID;

/// Unsupresses a message's embed.
pub async fn unsupress_embeds(message: &Message, client: &Client) -> anyhow::Result<()> {
    let current_flags = message.flags.unwrap_or(MessageFlags::empty());
    let new_flags = current_flags - MessageFlags::SUPPRESS_EMBEDS;

    client
        .update_message(message.channel_id, message.id)
        .flags(new_flags)
        .await?;

    Ok(())
}

/// Checks if the bot is mentioned in a message.
pub fn is_bot_mentioned(message: &Message) -> bool {
    let bot_user_id = BOT_USER_ID.get().expect("BOT_USER_ID not initialized");

    message
        .mentions
        .iter()
        .any(|mention| &mention.id == bot_user_id)
}

/// A simple & fast pre-check to see if a url is present.
pub fn contains_url(input: &str) -> bool {
    let input = input.to_lowercase();
    input.contains("instagram.com")
        || input.contains("reddit.com")
        || input.contains("tiktok.com")
        // || input.contains("twitch.tv")
        || input.contains("twitter.com")
        || input.contains("x.com")
}

/// Iterates over the input to return a list of links.
pub fn get_links(msg: &Message) -> Vec<&str> {
    msg.content
        .split_whitespace()
        .filter(|word| contains_url(word))
        .fold(Vec::new(), |mut unique, word| {
            if !unique.contains(&word) {
                unique.push(word);
            }
            unique
        })
}
