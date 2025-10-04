use std::{sync::Arc, time::Duration};

use anyhow::Result;
use async_read_progress::TokioAsyncReadProgressExt;
use dashmap::{DashMap, DashSet};
use futures::TryStreamExt;
use grammers_client::{
    button, reply_markup,
    types::{CallbackQuery, Chat, Message, User, Attribute},
    Client, InputMessage, Update,
};
use log::{error, info, warn};
use reqwest::Url;
use scopeguard::defer;
use stream_cancel::{Trigger, Valved};
use tokio::sync::Mutex;
use tokio_util::compat::FuturesAsyncReadCompatExt;

use crate::command::{parse_command, Command};

/// Bot is the main struct of the bot.
#[derive(Debug)]
pub struct Bot {
    client: Client,
    me: User,
    http: reqwest::Client,
    locks: Arc<DashSet<i64>>,
    started_by: Arc<DashMap<i64, i64>>,
    triggers: Arc<DashMap<i64, Trigger>>,
}

impl Bot {
    pub async fn new(client: Client) -> Result<Arc<Self>> {
        let me = client.get_me().await?;
        Ok(Arc::new(Self {
            client,
            me,
            http: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/110.0.0.0 Safari/537.36")
                .build()?,
            locks: Arc::new(DashSet::new()),
            started_by: Arc::new(DashMap::new()),
            triggers: Arc::new(DashMap::new()),
        }))
    }

    pub async fn run(self: Arc<Self>) {
        loop {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {
                    info!("Received Ctrl+C, exiting");
                    break;
                }

                Ok(update) = self.client.next_update() => {
                    let bot = self.clone();
                    tokio::spawn(async move {
                        if let Err(err) = bot.handle_update(update).await {
                            error!("Error handling update: {}", err);
                        }
                    });
                }
            }
        }
    }

    async fn handle_update(&self, update: Update) -> Result<()> {
        match update {
            Update::NewMessage(msg) => self.handle_message(msg).await,
            Update::CallbackQuery(query) => self.handle_callback(query).await,
            _ => Ok(()),
        }
    }

    async fn handle_message(&self, msg: Message) -> Result<()> {
        // Only handle user or group chats
        match msg.chat() {
            Chat::User(_) | Chat::Group(_) => {}
            _ => return Ok(()),
        }

        if let Some(cmd) = parse_command(msg.text()) {
            // If command has via (@botname) and it's not for us, ignore
            if let Some(via) = &cmd.via {
                if via.to_lowercase() != self.me.username().unwrap_or_default().to_lowercase() {
                    warn!("Command via different bot: {}", via);
                    return Ok(());
                }
            }

            // In group, if /start without via, ignore to avoid conflicts
            if let Chat::Group(_) = msg.chat() {
                if cmd.name == "start" && cmd.via.is_none() {
                    return Ok(());
                }
            }

            match cmd.name.as_str() {
                "start" => return self.handle_start(msg).await,
                "upload" => return self.handle_upload(msg, cmd).await,
                _ => {}
            }
        }

        // If it's not a command and in a user chat, try to parse as URL
        if let Chat::User(_) = msg.chat() {
            if let Ok(url) = Url::parse(msg.text()) {
                return self.handle_url(msg, url).await;
            }
        }

        Ok(())
    }

    async fn handle_start(&self, msg: Message) -> Result<()> {
        let keyboard = reply_markup::inline(vec![
            vec![
                button::inline("❓ Help", "help"),
                button::inline("📄 Sample", "sample"),
            ],
        ]);

        let text = "📁 <b>Hi! Need a file uploaded? Just send the link!</b>\n\
            In groups, use <code>/upload &lt;url&gt;</code>\n\n\
            🌟 <b>Features:</b>\n\
            \u{2022} Free & fast\n\
            \u{2022} <a href=\"https://github.com/altfoxie/url-uploader\">Open source</a>\n\
            \u{2022} Uploads files up to 2GB\n\
            \u{2022} Redirect-friendly";

        msg.reply(InputMessage::html(text).reply_markup(&keyboard)).await?;
        Ok(())
    }

    async fn handle_upload(&self, msg: Message, cmd: Command) -> Result<()> {
        let url_str = match cmd.arg {
            Some(u) => u,
            None => {
                msg.reply("Please specify a URL").await?;
                return Ok(());
            }
        };

        let url = match Url::parse(&url_str) {
            Ok(u) => u,
            Err(err) => {
                msg.reply(format!("Invalid URL: {}", err)).await?;
                return Ok(());
            }
        };

        self.handle_url(msg, url).await
    }

    async fn handle_url(&self, msg: Message, url: Url) -> Result<()> {
        let sender = match msg.sender() {
            Some(s) => s,
            None => return Ok(()),
        };

        info!("Locking chat {}", msg.chat().id());
        let lock_inserted = self.locks.insert(msg.chat().id());
        if !lock_inserted {
            msg.reply("✋ Whoa, slow down! There's already an active upload in this chat.")
                .await?;
            return Ok(());
        }
        self.started_by.insert(msg.chat().id(), sender.id());

        defer! {
            info!("Unlocking chat {}", msg.chat().id());
            self.locks.remove(&msg.chat().id());
            self.started_by.remove(&msg.chat().id());
        };

        let response = self.http.get(url).send().await?;

        let length = response.content_length().unwrap_or_default() as usize;
        let name = {
            // derive filename
            if let Some(value) = response
                .headers()
                .get("content-disposition")
                .and_then(|hdr| hdr.to_str().ok())
            {
                // try parse filename=...
                value
                    .split(';')
                    .map(|p| p.trim())
                    .find_map(|part| {
                        if part.starts_with("filename=") {
                            Some(part.trim_start_matches("filename=").trim_matches('"').to_string())
                        } else {
                            None
                        }
                    })
                    .unwrap_or_else(|| {
                        // fallback: last segment of URL
                        response
                            .url()
                            .path_segments()
                            .and_then(|seg| seg.last())
                            .map(|s| s.to_string())
                            .unwrap_or_else(|| "file.bin".to_string())
                    })
            } else {
                response
                    .url()
                    .path_segments()
                    .and_then(|seg| seg.last())
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "file.bin".to_string())
            }
        };

        let name = percent_encoding::percent_decode_str(&name).decode_utf8()?.to_string();

        let is_video = {
            let ct = response.headers().get("content-type").and_then(|v| v.to_str().ok());
            ct.map(|ct| ct.starts_with("video/mp4")).unwrap_or(false)
                || name.to_lowercase().ends_with(".mp4")
        };

        info!("File {} ({} bytes, video: {})", name, length, is_video);

        if length == 0 {
            msg.reply("⚠️ File is empty").await?;
            return Ok(());
        }
        if length > 2 * 1024 * 1024 * 1024 {
            msg.reply("⚠️ File is too large").await?;
            return Ok(());
        }

        let (trigger, stream) = Valved::new(
            response
                .bytes_stream()
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e)),
        );
        self.triggers.insert(msg.chat().id(), trigger);

        defer! {
            self.triggers.remove(&msg.chat().id());
        };

        let reply_markup = reply_markup::inline(vec![vec![button::inline("⛔ Cancel", "cancel")]]);

        let status_msg = msg
            .reply(InputMessage::html(format!("🚀 Starting upload of <code>{}</code>...", name))
                .reply_markup(&reply_markup))
            .await?;
        let status = Arc::new(Mutex::new(status_msg));

        let start_instant = std::time::Instant::now();

        let name_clone = name.clone();
        let status_clone = status.clone();
        let reply_markup_clone = reply_markup.clone();

        let mut stream = stream
            .into_async_read()
            .compat()
            .report_progress(Duration::from_secs(1), move |progress| {
                let status = status_clone.clone();
                let name = name_clone.clone();
                let reply_markup = reply_markup_clone.clone();

                let elapsed = start_instant.elapsed().as_secs_f64();
                let speed = if elapsed > 0.0 {
                    (progress as f64 / elapsed) as u64
                } else {
                    0
                };

                let time_remaining_secs = if speed > 0 {
                    ((length as u64).saturating_sub(progress as u64)) / speed
                } else {
                    0
                };

                let percent = (progress as f64 / length as f64) * 100.0;
                let bar = create_progress_bar(percent);

                let progress_text = format!(
                    "📤 <b>Uploading...</b>\n\n{}\n\n\
                     <b>{:.2}%</b>\n\n\
                     ➩ {} of {}\n\
                     ➩ Speed: {}/s\n\
                     ➩ Time Left: {}",
                    bar,
                    percent,
                    bytesize::to_string(progress as u64, true),
                    bytesize::to_string(length as u64, true),
                    bytesize::to_string(speed, true),
                    format_duration(time_remaining_secs),
                );

                tokio::spawn(async move {
                    if let Ok(mut guard) = status.try_lock() {
                        let _ = guard
                            .edit(InputMessage::html(progress_text).reply_markup(&reply_markup))
                            .await;
                    }
                });
            });

        let upload_start = chrono::Utc::now();
        let file = self
            .client
            .upload_stream(&mut stream, length, name.clone())
            .await?;

        let elapsed = chrono::Utc::now() - upload_start;
        info!("Uploaded file {} ({} bytes) in {}", name, length, elapsed);

        let mut reply = InputMessage::html(format!(
            "✅ <b>Upload completed in {:.2} secs</b>",
            elapsed.num_milliseconds() as f64 / 1000.0
        ));
        reply = reply.document(file);
        if is_video {
            reply = reply.attribute(Attribute::Video {
                supports_streaming: false,
                duration: Duration::ZERO,
                w: 0,
                h: 0,
                round_message: false,
            });
        }
        msg.reply(reply).await?;

        // Delete status (progress) message
        status.lock().await.delete().await?;

        Ok(())
    }

    async fn handle_callback(&self, query: CallbackQuery) -> Result<()> {
        match query.data() {
            b"cancel" => self.handle_cancel(query).await,
            b"help" => self.handle_help_callback(query).await,
            b"sample" => self.handle_sample_callback(query).await,
            b"start" => self.handle_start_callback(query).await,
            _ => {
                // Just answer to clear “loading” if needed
                query.answer().send().await?;
                Ok(())
            }
        }
    }

    async fn handle_cancel(&self, query: CallbackQuery) -> Result<()> {
        let started = match self.started_by.get(&query.chat().id()) {
            Some(id) => *id,
            None => return Ok(()),
        };

        if started != query.sender().id() {
            query
                .answer()
                .alert("⚠️ You can't cancel another user's upload")
                .cache_time(Duration::ZERO)
                .send()
                .await?;
            return Ok(());
        }

        if let Some((chat_id, trigger)) = self.triggers.remove(&query.chat().id()) {
            drop(trigger);
            self.started_by.remove(&chat_id);

            let message = query.load_message().await?;
            message.edit("⛔ Upload cancelled").await?;

            query.answer().send().await?;
        }

        Ok(())
    }

    async fn handle_help_callback(&self, query: CallbackQuery) -> Result<()> {
        let keyboard = reply_markup::inline(vec![
            vec![
                button::inline("🔙 Back", "start"),
                button::inline("📄 Sample", "sample"),
            ],
        ]);
        let help_text = "🆘 <b>Help Section</b>\n\n\
            📖 <b>How to use:</b>\n\
            \u{2022} Send direct download link\n\
            \u{2022} In groups: /upload <url>\n\n\
            🔗 <b>Supported:</b> direct or redirecting links, up to 2GB\n\n\
            ⚠️ <b>Note:</b> Bot downloads and reuploads";

        let message = query.load_message().await?;
        message.edit(InputMessage::html(help_text).reply_markup(&keyboard)).await?;
        query.answer().send().await?;

        Ok(())
    }

    async fn handle_sample_callback(&self, query: CallbackQuery) -> Result<()> {
        let keyboard = reply_markup::inline(vec![
            vec![
                button::inline("🔙 Back", "start"),
                button::inline("❓ Help", "help"),
            ],
        ]);
        let sample_text = "📝 <b>Sample Links</b>\n\n\
            Try these examples:\n\
            • https://example.com/file.zip\n\
            • https://download.com/doc.pdf\n\
            • https://bit.ly/redirect\n\
            • https://tinyurl.com/sample";

        let message = query.load_message().await?;
        message.edit(InputMessage::html(sample_text).reply_markup(&keyboard)).await?;
        query.answer().send().await?;

        Ok(())
    }

    async fn handle_start_callback(&self, query: CallbackQuery) -> Result<()> {
        let keyboard = reply_markup::inline(vec![
            vec![
                button::inline("❓ Help", "help"),
                button::inline("📄 Sample", "sample"),
            ],
        ]);
        let text = "📁 <b>Hi! Need a file uploaded? Just send the link!</b>\n\
            In groups, /upload <url>\n\n\
            🌟 <b>Features:</b>\n\
            • Free & fast\n\
            • <a href=\"https://github.com/altfoxie/url-uploader\">Open source</a>\n\
            • Upload up to 2GB\n\
            • Redirect-friendly";

        let message = query.load_message().await?;
        message.edit(InputMessage::html(text).reply_markup(&keyboard)).await?;
        query.answer().send().await?;

        Ok(())
    }
}

/// Create a visual progress bar string
fn create_progress_bar(percent: f64) -> String {
    const LEN: usize = 10;
    let filled = ((percent / 100.0) * (LEN as f64)).round() as usize;
    let filled = filled.min(LEN);
    let empty = LEN - filled;

    format!(
        "[ {}{} ]",
        "■".repeat(filled),
        "□".repeat(empty)
    )
}

/// Format seconds into human-readable time
fn format_duration(seconds: u64) -> String {
    if seconds == 0 {
        return "Calculating...".into();
    }
    let mins = seconds / 60;
    let secs = seconds % 60;
    if mins > 0 {
        format!("{} min {} sec", mins, secs)
    } else {
        format!("{} sec", secs)
    }
}
