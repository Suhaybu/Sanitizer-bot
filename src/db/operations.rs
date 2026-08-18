//! All the database operations are stored in this module.
//! (Ported to Turso from libsql using an LLM)

use anyhow::Context;
use serde::{Deserialize, Serialize};
use twilight_model::channel::Message;
use twilight_model::id::Id;
use twilight_model::id::marker::MessageMarker;

use super::connection::{WRITE_LOCK, get_read_connection, get_write_connection, request_push};
use crate::discord::models::{DeletePermission, SanitizerMode};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct ServerConfig {
    pub guild_id: u64,
    pub sanitizer_mode: SanitizerMode,
    pub delete_permission: DeletePermission,
    pub hide_original_embed: bool,
}

impl ServerConfig {
    pub async fn save(&self) -> anyhow::Result<()> {
        let sql = r#"
            INSERT OR REPLACE INTO server_configs
            (guild_id, sanitizer_mode, delete_permission, hide_original_embed)
            VALUES (?1, ?2, ?3, ?4)
        "#;

        {
            let _guard = WRITE_LOCK.lock().await;
            let conn = get_write_connection()?;
            conn.execute(
                sql,
                (
                    self.guild_id as i64,
                    self.sanitizer_mode as i32,
                    self.delete_permission as i32,
                    self.hide_original_embed,
                ),
            )
            .await
            .context("Failed to save server config")?;
        }

        tracing::debug!("Saved config for guild {}", self.guild_id);
        tracing::debug!("{:?}", self);

        request_push();

        Ok(())
    }

    pub async fn get_or_default(guild_id: u64) -> anyhow::Result<Self> {
        match Self::get(guild_id).await? {
            Some(config) => Ok(config),
            None => {
                tracing::debug!("Using default config for guild ({})", guild_id);
                Ok(Self::new(guild_id))
            }
        }
    }

    fn new(guild_id: u64) -> Self {
        Self {
            guild_id,
            sanitizer_mode: SanitizerMode::default(),
            delete_permission: DeletePermission::default(),
            hide_original_embed: true,
        }
    }

    // Uses the read pool - no lock needed.
    async fn get(guild_id: u64) -> anyhow::Result<Option<Self>> {
        let conn = get_read_connection().await?;

        let sql = r#"
            SELECT guild_id, sanitizer_mode, delete_permission, hide_original_embed
            FROM server_configs
            WHERE guild_id = ?
        "#;

        let mut rows = conn
            .query(sql, [guild_id as i64])
            .await
            .context("Failed to execute SELECT query")?;

        let Some(row) = rows.next().await.context("Failed to fetch row")? else {
            tracing::debug!(%guild_id, "No config found for guild");
            return Ok(None);
        };

        tracing::debug!(%guild_id, "Found existing config for guild");

        Ok(Some(Self {
            guild_id,
            sanitizer_mode: row.get::<i32>(1)?.into(),
            delete_permission: row.get::<i32>(2)?.into(),
            hide_original_embed: row.get::<bool>(3)?,
        }))
    }

    // pub async fn delete(guild_id: u64) -> anyhow::Result<()> {
    //    let conn = get_connection()?;

    //     let sql = "DELETE FROM Sanitizer WHERE guild_id = ?";

    //     conn.execute(sql, [guild_id as i64])
    //         .await
    //         .context("Failed to delete server config")?;

    //     debug!("Deleted config for guild {}", guild_id);
    //     Ok(())
    // }
}

/// Represents the Author of the Message used for finding a match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageAuthor {
    User,
    Bot,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct ResponseMap {
    pub user_message_id: u64,
    pub bot_message_id: u64,
    pub guild_id: Option<u64>,
    pub channel_id: u64,
}

impl ResponseMap {
    pub fn new(user_message: &Message, bot_message_id: Id<MessageMarker>) -> Self {
        Self {
            user_message_id: user_message.id.get(),
            bot_message_id: bot_message_id.get(),
            guild_id: user_message.guild_id.map(|id| id.get()),
            channel_id: user_message.channel_id.into(),
        }
    }

    pub async fn save(&self) -> anyhow::Result<()> {
        let sql = r#"
            INSERT OR REPLACE INTO response_map
            (user_message_id, bot_message_id, guild_id, channel_id)
            VALUES (?1, ?2, ?3, ?4)
        "#;

        {
            let _guard = WRITE_LOCK.lock().await;
            let conn = get_write_connection()?;
            conn.execute(
                sql,
                (
                    self.user_message_id as i64,
                    self.bot_message_id as i64,
                    self.guild_id.map(|id| id as i64),
                    self.channel_id as i64,
                ),
            )
            .await
            .context("Failed to save response map")?;
        }

        tracing::debug!(
            "Saved response map: user_msg={}, bot_msg={}, guild_id={:?}, channel_id={}",
            self.user_message_id,
            self.bot_message_id,
            self.guild_id,
            self.channel_id
        );

        request_push();

        Ok(())
    }

    /// Finds a match for a deleted message using the message's id.
    ///
    /// Returns a tuple containing:
    ///     - ReponseMap (struct)
    ///     - MessageAuthor (enum): to identify the message author
    // Uses the read pool - no lock needed, safe to run concurrently
    // with writes/push/pull under WAL.
    pub async fn find_match(
        message_id: Id<MessageMarker>,
    ) -> anyhow::Result<Option<(Self, MessageAuthor)>> {
        let conn = get_read_connection().await?;

        let sql = r#"
            SELECT user_message_id, bot_message_id, guild_id, channel_id
            FROM response_map
            WHERE user_message_id = ?1 OR bot_message_id = ?1
        "#;

        let mut rows = conn
            .query(sql, [message_id.get() as i64])
            .await
            .context("Failed to execute SELECT statement")?;

        let Some(row) = rows.next().await.context("Failed to fetch row")? else {
            tracing::debug!(%message_id, "No response map found");
            return Ok(None);
        };

        let (user_message_id, bot_message_id, guild_id, channel_id) = (
            row.get::<i64>(0)? as u64,
            row.get::<i64>(1)? as u64,
            row.get::<Option<i64>>(2)?.map(|id| id as u64),
            row.get::<i64>(3)? as u64,
        );

        let author = match user_message_id == message_id.get() {
            true => MessageAuthor::User,
            false => MessageAuthor::Bot,
        };

        tracing::debug!(%message_id, "Found response map");

        let response_map = Self {
            user_message_id,
            bot_message_id,
            guild_id,
            channel_id,
        };

        Ok(Some((response_map, author)))
    }

    pub async fn delete_entry(user_message_id: u64) -> anyhow::Result<()> {
        let sql = "DELETE FROM response_map WHERE user_message_id = ?";

        {
            let _guard = WRITE_LOCK.lock().await;
            let conn = get_write_connection()?;
            conn.execute(sql, [user_message_id as i64])
                .await
                .context("Failed to delete from response map")?;
        }

        tracing::debug!(
            "Deleted response map for user_message_id={}",
            user_message_id
        );

        request_push();

        Ok(())
    }
}
