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
/// All the bot logic is implemented in this struct.
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
    /// Create a new bot instance.
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

    /// Run the bot.
    pub async fn run(self: Arc<Self>) {
        loop {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {
                    info!("Received Ctrl+C, exiting");
                    break;
                }

                Ok(update) = self.client.next_update() => {
                    let self_ = self.clone();

                    // Spawn a new task to handle the update
                    tokio::spawn(async move {
                        if let Err(err) = self_.handle_update(update).await {
                            error!("Error handling update: {}", err);
                        }
                    });
                }
            }
        }
    }

    /// Update handler.
    async fn handle_update(&self, update: Update) -> Result<()> {
        // NOTE: no ; here, so result is returned
        match update {
            Update::NewMessage(msg) => self.handle_message(msg).await,
            Update::CallbackQuery(query) => self.handle_callback(query).await,
            _ => Ok(()),
        }
    }

    /// Message handler.
    ///
    /// Ensures the message is from a user or a group, and then parses the command.
    /// If the command is not recognized, it will try to parse the message as a URL.
    async fn handle_message(&self, msg: Message) -> Result<()> {
        // Ensure the message chat is a user or a group
        match msg.chat() {
            Chat::User(_) | Chat::Group(_) => {}
            _ => return Ok(()),
        };

        // Parse the command
        let command = parse_command(msg.text());
        if let Some(command) = command {
            // Ensure the command is for this bot
            if let Some(via) = &command.via {
                if via.to_lowercase() != self.me.username().unwrap_or_default().to_lowercase() {
                    warn!("Ignoring command for unknown bot: {}", via);
                    return Ok(());
                }
            }

            // There is a chance that there are multiple bots listening
            // to /start commands in a group, so we handle commands
            // only if they are sent explicitly to this bot.
            if let Chat::Group(_) = msg.chat() {
                if command.name == "start" && command.via.is_none() {
                    return Ok(());
                }
            }

            // Handle the command
            info!("Received command: {:?}", command);
            match command.name.as_str() {
                "start" => {
                    return self.handle_start(msg).await;
                }
                "upload" => {
                    return self.handle_upload(msg, command).await;
                }
                _ => {}
            }
        }

        if let Chat::User(_) = msg.chat() {
            // If the message is not a command, try to parse it as a URL
            if let Ok(url) = Url::parse(msg.text()) {
                return self.handle_url(msg, url).await;
            }
        }

        Ok(())
    }

    /// Handle the /start command with buttons.
    async fn handle_start(&self, msg: Message) -> Result<()> {
        let keyboard = reply_markup::inline(vec![
            vec![
                button::inline("Help", "help"),
                button::inline("Sample", "sample"),
            ],
        ]);

        msg.reply(
            InputMessage::html(
                "📁 <b>Hi! Need a file uploaded? Just send the link!</b>\n\
                In groups, use <code>/upload &lt;url&gt;</code>\n\
                \n\
                🌟 <b>Features:</b>\n\
                \u{2022} Free & fast\n\
                \u{2022} <a href=\"https://github.com/altfoxie/url-uploader\">Open source</a>\n\
                \u{2022} Uploads files up to 2GB\n\
                \u{2022} Redirect-friendly",
            )
            .reply_markup(&keyboard),
        )
        .await?;
        Ok(())
    }

    /// Handle the /upload command.
    /// This command should be used in groups to upload a file.
    async fn handle_upload(&self, msg: Message, cmd: Command) -> Result<()> {
        // If the argument is not specified, reply with an error
        let url = match cmd.arg {
            Some(url) => url,
            None => {
                msg.reply("Please specify a URL").await?;
                return Ok(());
            }
        };

        // Parse the URL
        let url = match Url::parse(&url) {
            Ok(url) => url,
            Err(err) => {
                msg.reply(format!("Invalid URL: {}", err)).await?;
                return Ok(());
            }
        };

        self.handle_url(msg, url).await
    }

    /// Handle a URL.
    /// This function will download the file and upload it to Telegram.
    async fn handle_url(&self, msg: Message, url: Url) -> Result<()> {
        let sender = match msg.sender() {
            Some(sender) => sender,
            None => return Ok(()),
        };

        // Lock the chat to prevent multiple uploads at the same time
        info!("Locking chat {}", msg.chat().id());
        let _lock = self.locks.insert(msg.chat().id());
        if !_lock {
            msg.reply("✋ Whoa, slow down! There's already an active upload in this chat.")
                .await?;
            return Ok(());
        }
        self.started_by.insert(msg.chat().id(), sender.id());

        // Deferred unlock
        defer! {
            info!("Unlocking chat {}", msg.chat().id());
            self.locks.remove(&msg.chat().id());
            self.started_by.remove(&msg.chat().id());
        };

        info!("Downloading file from {}", url);
        let response = self.http.get(url).send().await?;

        // Get the file name and size
        let length = response.content_length().unwrap_or_default() as usize;
        let name = match response
            .headers()
            .get("content-disposition")
            .and_then(|value| {
                value
                    .to_str()
                    .ok()
                    .and_then(|value| {
                        value
                            .split(';')
                            .map(|value| value.trim())
                            .find(|value| value.starts_with("filename="))
                    })
                    .map(|value| value.trim_start_matches("filename="))
                    .map(|value| value.trim_matches('"'))
            }) {
            Some(name) => name.to_string(),
            None => response
                .url()
                .path_segments()
                .and_then(|segments| segments.last())
                .and_then(|name| {
                    if name.contains('.') {
                        Some(name.to_string())
                    } else {
                        // guess the extension from the content type
                        response
                            .headers()
                            .get("content-type")
                            .and_then(|value| value.to_str().ok())
                            .and_then(mime_guess::get_mime_extensions_str)
                            .and_then(|ext| ext.first())
                            .map(|ext| format!("{}.{}", name, ext))
                    }
                })
                .unwrap_or("file.bin".to_string())
                .to_string(),
        };
        let name = percent_encoding::percent_decode_str(&name)
            .decode_utf8()?
            .to_string();
        let is_video = response
            .headers()
            .get("content-type")
            .map(|value| {
                value
                    .to_str()
                    .ok()
                    .map(|value| value.starts_with("video/mp4"))
                    .unwrap_or_default()
            })
            .unwrap_or_default()
            || name.to_lowercase().ends_with(".mp4");
        info!("File {} ({} bytes, video: {})", name, length, is_video);

        // File is empty
        if length == 0 {
            msg.reply("⚠️ File is empty").await?;
            return Ok(());
        }

        // File is too large
        if length > 2 * 1024 * 1024 * 1024 {
            msg.reply("⚠️ File is too large").await?;
            return Ok(());
        }

        // Wrap the response stream in a valved stream
        let (trigger, stream) = Valved::new(
            response
                .bytes_stream()
                .map_err(|err| std::io::Error::new(std::io::ErrorKind::Other, err)),
        );
        self.triggers.insert(msg.chat().id(), trigger);

        // Deferred trigger removal
        defer! {
            self.triggers.remove(&msg.chat().id());
        };

        // Reply markup buttons
        let reply_markup = reply_markup::inline(vec![vec![button::inline(
            "⛔ Cancel",
            "cancel",
        )]]);

        // Send status message
        let status = Arc::new(Mutex::new(
            msg.reply(
                InputMessage::html(format!("🚀 Starting upload of <code>{}</code>...", name))
                    .reply_markup(&reply_markup),
            )
            .await?,
        ));

        let start_time = std::time::Instant::now();
        
        // Clone variables before moving into closure
        let name_clone = name.clone();
        let status_clone = status.clone();
        let reply_markup_clone = reply_markup.clone();

        let mut stream = stream
            .into_async_read()
            .compat()
            // Report progress every 1 second for better updates
            .report_progress(Duration::from_secs(1), move |progress| {
                let status = status_clone.clone();
                let name = name_clone.clone();
                let reply_markup = reply_markup_clone.clone();
                
                let elapsed = start_time.elapsed().as_secs_f64();
                let speed = if elapsed > 0.0 {
                    (progress as f64 / elapsed) as u64
                } else {
                    0
                };

                let time_remaining = if speed > 0 {
                    (length as u64 - progress as u64) / speed
                } else {
                    0
                };

                let progress_percent = progress as f64 / length as f64 * 100.0;
                let progress_bar = create_progress_bar(progress_percent);

                tokio::spawn(async move {
                    if let Ok(mut status_guard) = status.try_lock() {
                        let _ = status_guard
                            .edit(
                                InputMessage::html(format!(
                                    "📤 <b>Uploading...</b>\n\n{}\n\n\
                                    <b>{:.2}%</b>\n\n\
                                    ➩ {} of {}\n\
                                    ➩ Speed: {}/s\n\
                                    ➩ Time Left: {}",
                                    progress_bar,
                                    progress_percent,
                                    bytesize::to_string(progress as u64, true),
                                    bytesize::to_string(length as u64, true),
                                    bytesize::to_string(speed, true),
                                    format_duration(time_remaining)
                                ))
                                .reply_markup(&reply_markup),
                            )
                            .await;
                    }
                });
            });

        // Upload the file
        let upload_start_time = chrono::Utc::now();
        let file = self
            .client
            .upload_stream(&mut stream, length, name.clone())
            .await?;

        // Calculate upload time
        let elapsed = chrono::Utc::now() - upload_start_time;
        info!("Uploaded file {} ({} bytes) in {}", name, length, elapsed);

        // Send file
        let mut input_msg = InputMessage::html(format!(
            "✅ <b>Upload completed in {:.2} seconds</b>",
            elapsed.num_milliseconds() as f64 / 1000.0
        ));
        input_msg = input_msg.document(file);
        if is_video {
            input_msg = input_msg.attribute(Attribute::Video {
                supports_streaming: false,
                duration: Duration::ZERO,
                w: 0,
                h: 0,
                round_message: false,
            });
        }
        msg.reply(input_msg).await?;

        // Delete status message
        status.lock().await.delete().await?;

        Ok(())
    }

    /// Callback query handler.
    async fn handle_callback(&self, query: CallbackQuery) -> Result<()> {
        let data = query.data();
        match data {
            b"cancel" => self.handle_cancel(query).await,
            b"help" => self.handle_help_callback(query).await,
            b"sample" => self.handle_sample_callback(query).await,
            b"start" => self.handle_start_callback(query).await,
            _ => Ok(()),
        }
    }

    /// Handle the cancel button.
    async fn handle_cancel(&self, query: CallbackQuery) -> Result<()> {
        let started_by_user_id = match self.started_by.get(&query.chat().id()) {
            Some(id) => *id,
            None => return Ok(()),
        };

        if started_by_user_id != query.sender().id() {
            info!(
                "Some genius with ID {} tried to cancel another user's upload in chat {}",
                query.sender().id(),
                query.chat().id()
            );

            query
                .answer()
                .alert("⚠️ You can't cancel another user's upload")
                .cache_time(Duration::ZERO)
                .send()
                .await?;

            return Ok(());
        }

        if let Some((chat_id, trigger)) = self.triggers.remove(&query.chat().id()) {
            info!("Cancelling upload in chat {}", chat_id);
            drop(trigger);
            self.started_by.remove(&chat_id);

            let message = query.load_message().await?;
            message.edit("⛔ Upload cancelled").await?;

            query.answer().send().await?;
        }
        Ok(())
    }

    /// Handle help button callback.
    async fn handle_help_callback(&self, query: CallbackQuery) -> Result<()> {
        let keyboard = reply_markup::inline(vec![
            vec![
                button::inline("Back", "start"),
                button::inline("Sample", "sample"),
            ],
        ]);

        let help_text = "🆘 <b>Help Section</b>\n\n\
                       📖 <b>How to use:</b>\n\
                       \u{2022} Send any direct download link in private chat\n\
                       \u{2022} In groups, use <code>/upload &lt;url&gt;</code>\n\n\
                       🔗 <b>Supported links:</b>\n\
                       \u{2022} Direct download links\n\
                       \u{2022} Redirecting links\n\
                       \u{2022} Files up to 2GB\n\n\
                       ⚠️ <b>Note:</b> The bot will download and reupload the file to Telegram";

        let message = query.load_message().await?;
        message
            .edit(InputMessage::html(help_text).reply_markup(&keyboard))
            .await?;

        // Answer the callback query to remove the loading state
        query.answer().send().await?;

        Ok(())
    }

    /// Handle sample button callback.
    async fn handle_sample_callback(&self, query: CallbackQuery) -> Result<()> {
        let keyboard = reply_markup::inline(vec![
            vec![
                button::inline("Back", "start"),
                button::inline("Help", "help"),
            ],
        ]);

        let sample_text = "📝 <b>Sample Links</b>\n\n\
                         Here are some example links you can try:\n\n\
                         🔗 <b>Direct download examples:</b>\n\
                         \u{2022} <code>https://example.com/file.zip</code>\n\
                         \u{2022} <code>https://download.com/document.pdf</code>\n\n\
                         🔄 <b>Redirect examples:</b>\n\
                         \u{2022} <code>https://bit.ly/example-file</code>\n\
                         \u{2022} <code>https://tinyurl.com/sample-file</code>\n\n\
                         💡 <b>Try it:</b> Copy any sample link and send it to me!";

        let message = query.load_message().await?;
        message
            .edit(InputMessage::html(sample_text).reply_markup(&keyboard))
            .await?;

        // Answer the callback query to remove the loading state
        query.answer().send().await?;

        Ok(())
    }

    /// Handle start button callback.
    async fn handle_start_callback(&self, query: CallbackQuery) -> Result<()> {
        let keyboard = reply_markup::inline(vec![
            vec![
                button::inline("Help", "help"),
                button::inline("Sample", "sample"),
            ],
        ]);

        let start_text = "📁 <b>Hi! Need a file uploaded? Just send the link!</b>\n\
                In groups, use <code>/upload &lt;url&gt;</code>\n\
                \n\
                🌟 <b>Features:</b>\n\
                \u{2022} Free & fast\n\
                \u{2022} <a href=\"https://github.com/altfoxie/url-uploader\">Open source</a>\n\
                \u{2022} Uploads files up to 2GB\n\
                \u{2022} Redirect-friendly";

        let message = query.load_message().await?;
        message
            .edit(InputMessage::html(start_text).reply_markup(&keyboard))
            .await?;

        // Answer the callback query to remove the loading state
        query.answer().send().await?;

        Ok(())
    }
}

/// Create a visual progress bar
fn create_progress_bar(percentage: f64) -> String {
    const BAR_LENGTH: usize = 10;
    let filled = (percentage / 100.0 * BAR_LENGTH as f64).round() as usize;
    let empty = BAR_LENGTH - filled;

    let filled_str = "■".repeat(filled);
    let empty_str = "□".repeat(empty);

    format!("[ {} ]", filled_str + &empty_str)
}

/// Format duration in seconds to human readable format
fn format_duration(seconds: u64) -> String {
    if seconds == 0 {
        return "Calculating...".to_string();
    }

    let minutes = seconds / 60;
    let hours = minutes / 60;

    if hours > 0 {
        format!("{} hr {} min", hours, minutes % 60)
    } else if minutes > 0 {
        format!("{} min {} sec", minutes, seconds % 60)
    } else {
        format!("{} sec", seconds)
    }
}
