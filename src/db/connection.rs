//! Responsible for establishing the everything to do with the database connection.
//! (Ported to Turso from libsql using an LLM)

use std::ops::Deref;
use std::time::Duration;

use anyhow::Context;
use tokio::sync::{Mutex, Notify, OnceCell, mpsc};
use turso::Connection;
use turso::sync::{Builder, Database};

const READ_POOL_SIZE: usize = 4;
const BUSY_TIMEOUT: Duration = Duration::from_millis(5000);

static DB: OnceCell<Database> = OnceCell::const_new();
static WRITE_CONN: OnceCell<Connection> = OnceCell::const_new();
static READ_POOL: OnceCell<ReadPool> = OnceCell::const_new();
static PUSH_NOTIFY: Notify = Notify::const_new();

pub static WRITE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct ReadPool {
    sender: mpsc::Sender<Connection>,
    receiver: Mutex<mpsc::Receiver<Connection>>,
}

/// A checked-out read connection. Returned to the pool automatically on drop.
pub struct ReadConnGuard {
    conn: Option<Connection>,
    returner: mpsc::Sender<Connection>,
}

impl Deref for ReadConnGuard {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        self.conn.as_ref().expect("connection taken before drop")
    }
}

impl Drop for ReadConnGuard {
    fn drop(&mut self) {
        if let Some(conn) = self.conn.take() {
            // Capacity always has room for this return since it can only be
            // full when every connection is checked out.
            let _ = self.returner.try_send(conn);
        }
    }
}

unsafe impl Send for ReadConnGuard {}

/// This function exists only as a safeguard for keeping track of Turso's crate code.
/// If this function fails to compile, this means turso's internal code has changed.
fn _assert_fields_send() {
    fn is_send<T: Send>() {}
    is_send::<turso::Connection>();
    is_send::<tokio::sync::mpsc::Sender<turso::Connection>>();
}

/// Initialize database connection during pre run (main.rs).
pub async fn init() -> anyhow::Result<()> {
    let db = init_database_internal().await?;

    let write_conn = db
        .connect()
        .await
        .context("Failed to open write connection")?;
    write_conn
        .busy_timeout(BUSY_TIMEOUT)
        .context("Failed to set busy_timeout on write connection")?;

    create_tables(&write_conn).await?;

    let (tx, rx) = mpsc::channel(READ_POOL_SIZE);
    for i in 0..READ_POOL_SIZE {
        let conn = db
            .connect()
            .await
            .with_context(|| format!("Failed to open read connection {i}"))?;
        conn.busy_timeout(BUSY_TIMEOUT)
            .with_context(|| format!("Failed to set busy_timeout on read connection {i}"))?;
        tx.send(conn)
            .await
            .expect("channel just created, cannot be closed");
    }

    DB.set(db)
        .map_err(|_| anyhow::anyhow!("Database already initialized"))?;
    WRITE_CONN
        .set(write_conn)
        .map_err(|_| anyhow::anyhow!("Write connection already initialized"))?;
    READ_POOL
        .set(ReadPool {
            sender: tx,
            receiver: Mutex::new(rx),
        })
        .map_err(|_| anyhow::anyhow!("Read pool already initialized"))?;

    tokio::spawn(push_worker());

    tokio::spawn(async {
        let mut delay = Duration::from_millis(250);
        let mut last_err: Option<anyhow::Error> = None;
        for attempt in 1..=3 {
            match pull_database().await {
                Ok(()) => {
                    tracing::debug!(
                        "Initial database pull completed successfully (attempt {})",
                        attempt
                    );
                    return;
                }
                Err(e) => {
                    last_err = Some(e);
                    tracing::debug!(
                        "Initial database pull failed (attempt {}), retrying after {:?}",
                        attempt,
                        delay
                    );
                    tokio::time::sleep(delay).await;
                    delay *= 2;
                }
            }
        }
        if let Some(e) = last_err {
            tracing::error!("Failed initial database pull after retries: {:?}", e);
        }
    });

    Ok(())
}

/// The single write connection. Callers MUST hold WRITE_LOCK for the
/// entire duration they use it.
pub fn get_write_connection() -> anyhow::Result<&'static Connection> {
    WRITE_CONN
        .get()
        .context("Write connection has not been initialized; call init_database() first")
}

/// Checks out one connection from the read pool. Waits if all are
/// currently in use. Safe to call concurrently from any number of tasks —
/// the returned guard hands the connection back automatically on drop.
pub async fn get_read_connection() -> anyhow::Result<ReadConnGuard> {
    let pool = READ_POOL
        .get()
        .context("Read pool has not been initialized; call init_database() first")?;

    let mut receiver = pool.receiver.lock().await;
    let conn = receiver
        .recv()
        .await
        .context("Read connection pool is closed")?;

    Ok(ReadConnGuard {
        conn: Some(conn),
        returner: pool.sender.clone(),
    })
}

/// Ask the background worker to push local writes to the remote.
pub fn request_push() {
    PUSH_NOTIFY.notify_one();
}

pub async fn pull_database() -> anyhow::Result<()> {
    let _guard = WRITE_LOCK.lock().await;
    tracing::debug!("Pulling changes from remote");
    db()?
        .pull()
        .await
        .context("Failed to pull changes from remote")?;
    tracing::debug!("Database pull completed");
    Ok(())
}

fn db() -> anyhow::Result<&'static Database> {
    DB.get()
        .context("Database has not been initialized; call init_database() first")
}

async fn push_worker() {
    loop {
        PUSH_NOTIFY.notified().await;
        if let Err(e) = push_database().await {
            tracing::warn!("Failed to push database after write: {:?}", e);
        }
    }
}

async fn push_database() -> anyhow::Result<()> {
    let _guard = WRITE_LOCK.lock().await;
    tracing::debug!("Pushing local writes to remote");
    db()?
        .push()
        .await
        .context("Failed to push local writes to remote")?;
    Ok(())
}

async fn init_database_internal() -> anyhow::Result<Database> {
    let url = std::env::var("TURSO_DATABASE_URL").context("TURSO_DATABASE_URL must be set")?;
    let auth_token = std::env::var("TURSO_AUTH_TOKEN").context("TURSO_AUTH_TOKEN must be set")?;

    tracing::info!("Initializing Turso synced database");

    let db = Builder::new_remote("local.db")
        .with_remote_url(&url)
        .with_auth_token(&auth_token)
        .bootstrap_if_empty(true)
        .build()
        .await
        .context("Failed to build database connection")?;

    Ok(db)
}

async fn create_tables(conn: &Connection) -> anyhow::Result<()> {
    tracing::debug!("Ensuring database schema exists");

    let create_server_configs_table = r#"
        CREATE TABLE IF NOT EXISTS server_configs (
            guild_id INTEGER PRIMARY KEY,
            sanitizer_mode INTEGER NOT NULL DEFAULT 0,
            delete_permission INTEGER NOT NULL DEFAULT 0,
            hide_original_embed BOOLEAN NOT NULL DEFAULT true
        )
    "#;

    conn.execute(create_server_configs_table, ())
        .await
        .context("Failed to create server_configs table")?;

    let create_response_map_table = r#"
        CREATE TABLE IF NOT EXISTS response_map (
            user_message_id INTEGER PRIMARY KEY,
            bot_message_id INTEGER NOT NULL UNIQUE,
            guild_id INTEGER,
            channel_id INTEGER NOT NULL
        )
        "#;

    conn.execute(create_response_map_table, ())
        .await
        .context("Failed to create response_map table")?;

    let create_index = r#"
            CREATE INDEX IF NOT EXISTS idx_response_map_location
            ON response_map (guild_id, channel_id)
        "#;

    conn.execute(create_index, ())
        .await
        .context("Failed to create index for response_map")?;

    tracing::debug!("Database schema set.");
    Ok(())
}
