//! Sanitize Command: Core functionlity, takes a url from the user and attempts to fix the embed.

use anyhow::Context;
use twilight_http::Client;
use twilight_model::application::command::{Command, CommandType};
use twilight_model::application::interaction::application_command::{
    CommandData, CommandOptionValue,
};
use twilight_model::application::interaction::{Interaction, InteractionContextType};
use twilight_model::channel::message::component::{ActionRow, Button, ButtonStyle};
use twilight_model::channel::message::{Component, EmojiReactionType};
use twilight_model::http::interaction::{InteractionResponse, InteractionResponseType};
use twilight_model::oauth::ApplicationIntegrationType;
use twilight_util::builder::command::{BooleanBuilder, CommandBuilder, StringBuilder};

use crate::sanitize::UrlProcessor;

pub struct SanitizeCommand;

impl SanitizeCommand {
    /// Creates /sanitize command.
    pub fn create_command() -> Command {
        CommandBuilder::new(
            "sanitize",
            "Fix the embed of your link! 🫧",
            CommandType::ChatInput,
        )
        .contexts([
            InteractionContextType::Guild,
            InteractionContextType::BotDm,
            InteractionContextType::PrivateChannel,
        ])
        .integration_types([
            ApplicationIntegrationType::GuildInstall,
            ApplicationIntegrationType::UserInstall,
        ])
        .option(
            StringBuilder::new("link", "Your link goes here!")
                .max_length(100)
                .required(true),
        )
        .option(
            BooleanBuilder::new(
                "spoiler",
                "Would you like the output message to be in a spoiler?",
            )
            .required(false),
        )
        .build()
    }

    /// Creates Sanitize command for on right-click.
    pub fn create_command_message() -> Command {
        CommandBuilder::new("Sanitize", "", CommandType::Message)
            .contexts([
                InteractionContextType::Guild,
                InteractionContextType::BotDm,
                InteractionContextType::PrivateChannel,
            ])
            .integration_types([
                ApplicationIntegrationType::GuildInstall,
                ApplicationIntegrationType::UserInstall,
            ])
            .build()
    }

    /// Handles responding to command invocation.
    pub async fn handle(
        ctx: &Interaction,
        client: &Client,
        data: &CommandData,
    ) -> anyhow::Result<()> {
        // Acknowledge the user interaction first.
        let deferred_response = InteractionResponse {
            kind: InteractionResponseType::DeferredChannelMessageWithSource,
            data: None,
        };

        client
            .interaction(ctx.application_id)
            .create_response(ctx.id, &ctx.token, &deferred_response)
            .await?;

        // Extract message input.
        let (user_input, is_spoiler) = match data.kind {
            // For the /command.
            CommandType::ChatInput => {
                let link = data
                    .options
                    .iter()
                    .find_map(|o| match &o.value {
                        CommandOptionValue::String(s) if o.name == "link" => Some(s.as_str()),
                        _ => None,
                    })
                    .context("Missing required 'link' option")?;

                let is_spoiler = data
                    .options
                    .iter()
                    .find_map(|o| match &o.value {
                        CommandOptionValue::Boolean(b) if o.name == "spoiler" => Some(*b),
                        _ => None,
                    })
                    .unwrap_or(false);

                (link, is_spoiler)
            }
            // For the right-click on message.
            CommandType::Message => {
                let data_resolved = data
                    .resolved
                    .as_ref()
                    .context("No Resolved data for Message.")?;

                let message = data_resolved
                    .messages
                    .values()
                    .next()
                    .context("No message found in resolved data")?;

                (message.content.as_str(), false)
            }
            _ => anyhow::bail!("Unexpected CommandType: {:?}", data.kind),
        };

        let url = match UrlProcessor::try_new(user_input, is_spoiler) {
            Some(url) => url,
            None => {
                client
                    .interaction(ctx.application_id)
                    .update_response(&ctx.token)
                    .content(Some(
                        "The link provided is invalid, or the platform is currently not supported. :(",
                    ))
                    .await?;
                return Ok(());
            }
        };

        let original_url = url
            .get_original_url()
            .expect("Original URL could not be retrieved.");

        let output = match url
            .capture_url()
            .await
            .and_then(|captures| captures.format_output())
        {
            Some(output) => output,
            None => {
                client
                    .interaction(ctx.application_id)
                    .update_response(&ctx.token)
                    .content(Some(
                        "Oops! For some unknown reason, I was unable to process the URL.",
                    ))
                    .await?;
                return Ok(());
            }
        };

        let add_delete_button = ctx.is_guild()
            && ctx
                .context
                .is_some_and(|ctx_type| ctx_type == InteractionContextType::PrivateChannel);
        let buttons = Self::construct_buttons(original_url, add_delete_button);

        client
            .interaction(ctx.application_id)
            .update_response(&ctx.token)
            .content(Some(&output))
            .components(Some(&[buttons]))
            .await?;

        Ok(())
    }

    /// Constructs the open link button & Optional delete button.
    fn construct_buttons(original_url: String, add_delete_button: bool) -> Component {
        let mut buttons = vec![Component::Button(Button {
            id: None,
            custom_id: None,
            disabled: false,
            emoji: Some(EmojiReactionType::Unicode {
                name: "🔗".to_string(),
            }),
            label: Some("Open Link".to_string()),
            style: ButtonStyle::Link,
            url: Some(original_url),
            sku_id: None,
        })];

        if add_delete_button {
            buttons.push(Component::Button(Button {
                id: None,
                custom_id: Some("delete".to_string()),
                disabled: false,
                emoji: Some(EmojiReactionType::Unicode {
                    name: "🗑️".to_string(),
                }),
                label: Some("Delete".to_string()),
                style: ButtonStyle::Danger,
                url: None,
                sku_id: None,
            }));
        }

        Component::ActionRow(ActionRow {
            id: None,
            components: buttons,
        })
    }
}
