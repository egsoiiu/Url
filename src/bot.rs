use std::{sync::Arc, time::Duration};

use anyhow::Result;
use async_read_progress::TokioAsyncReadProgressExt;
use dashmap::{DashMap, DashSet};
use futures::TryStreamExt;
use grammers_client::{
    button, reply_markup,
    types::{CallbackQuery, Chat, Message, User},
    Client, InputMessage, Update,
};
use log::{error, info, warn};
use reqwest::Url;
use scopeguard::defer;
use stream_cancel::{Trigger, Valved};
use tokio::sync::Mutex;
use tokio_util::compat::FuturesAsyncReadCompatExt;

use crate::command::{parse_command, Command};

/// Main bot structure handling Telegram file uploads from URLs
#[derive(Debug)]
pub struct Bot {
    client: Client,
    me: User,
    http: reqwest::Client,
    locks: Arc<DashSet<i64>>,           // Prevent multiple uploads per chat
    started_by: Arc<DashMap<i64, i64>>, // Track who started uploads
    triggers: Arc<DashMap<i64, Trigger>>, // Cancel upload streams
}

impl Bot {
    /// Create a new bot instance
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

    /// Main bot event loop
    pub async fn run(self: Arc<Self>) {
        loop {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {
                    info!("Received Ctrl+C, exiting");
                    break;
                }
                Ok(update) = self.client.next_update() => {
                    let self_ = self.clone();
                    tokio::spawn(async move {
                        if let Err(err) = self_.handle_update(update).await {
                            error!("Error handling update: {}", err);
                        }
                    });
                }
            }
        }
    }

    /// Route updates to appropriate handlers
    async fn handle_update(&self, update: Update) -> Result<()> {
        match update {
            Update::NewMessage(msg) => self.handle_message(msg).await,
            Update::CallbackQuery(query) => self.handle_callback(query).await,
            _ => Ok(()),
        }
    }

    /// Handle incoming messages - commands and URLs
    async fn handle_message(&self, msg: Message) -> Result<()> {
        // Only process user and group chats
        match msg.chat() {
            Chat::User(_) | Chat::Group(_) => {}
            _ => return Ok(()),
        };

        // Process commands
        if let Some(command) = parse_command(msg.text()) {
            // Validate bot mention in groups
            if let Some(via) = &command.via {
                if via.to_lowercase() != self.me.username().unwrap_or_default().to_lowercase() {
                    warn!("Ignoring command for unknown bot: {}", via);
                    return Ok(());
                }
            }

            // In groups, only process /start if explicitly mentioned
            if let Chat::Group(_) = msg.chat() {
                if command.name == "start" && command.via.is_none() {
                    return Ok(());
                }
            }

            info!("Received command: {:?}", command);
            match command.name.as_str() {
                "start" => return self.handle_start(msg).await,
                "upload" => return self.handle_upload(msg, command).await,
                _ => {}
            }
        }

        // Handle direct messages as potential URLs
        if let Chat::User(_) = msg.chat() {
            let text = msg.text().trim();
            let msg_clone = msg.clone();

            // Check for URL with custom filename syntax: "url | filename"
            if let Some(pipe_pos) = text.find('|') {
                let (url_part, name_part) = text.split_at(pipe_pos);
                let url_str = url_part.trim();
                let custom_name = name_part[1..].trim();

                if !url_str.is_empty() && !custom_name.is_empty() {
                    if let Ok(url) = Url::parse(url_str) {
                        return self.handle_url(msg_clone, url, Some(custom_name.to_string())).await;
                    }
                }
            }

            // Try parsing as plain URL
            if let Ok(url) = Url::parse(text) {
                return self.handle_url(msg.clone(), url, None).await;
            }
        }

        Ok(())
    }

    /// Handle /start command - show welcome message
    async fn handle_start(&self, msg: Message) -> Result<()> {
        msg.reply(InputMessage::html(
            "𝑊𝑒𝑙𝑐𝑜𝑚𝑒 𝑡𝑜 𝑈𝑅𝐿 𝑈𝑝𝑙𝑜𝑎𝑑𝑒𝑟 𝑏𝑜𝑡\n\
            \n\
            𝐶𝑢𝑠𝑡𝑜𝑚 𝑓𝑖𝑙𝑒 𝑛𝑎𝑚𝑖𝑛𝑔\n\
                ➠ 𝑈𝑅𝐿 | 𝐹𝑖𝑙𝑒_𝑁𝑎𝑚𝑒\n\
            \n\
            <blockquote>𝐹𝑒𝑎𝑡𝑢𝑟𝑒𝑠:\n\
            ㅤ➠ 𝑐𝑟𝑎𝑧𝑦 𝑓𝑎𝑠𝑡 & 𝑓𝑟𝑒𝑒\n\
            ㅤ➠ 𝑢𝑝 𝑡𝑜 2𝐺𝐵\n\
            ㅤ➠ 𝑐𝑢𝑠𝑡𝑜𝑚 𝑓𝑖𝑙𝑒 𝑛𝑎𝑚𝑒 𝑤𝑖𝑡ℎ 𝑎𝑢𝑡𝑜 𝑒𝑥𝑡𝑒𝑛𝑠𝑖𝑜𝑛</blockquote>\n\
            \n\
            ✨ 𝐶𝑜𝑝𝑦 𝑎𝑛𝑑 𝑃𝑎𝑠𝑡𝑒 𝑦𝑜𝑢𝑟 𝑈𝑅𝐿 𝑡𝑜 𝑔𝑒𝑡 𝑠𝑡𝑎𝑟𝑡𝑒𝑑!",
        ))
        .await?;

        Ok(())
    }

    /// Handle /upload command with URL and optional custom filename
    async fn handle_upload(&self, msg: Message, cmd: Command) -> Result<()> {
        let input = match cmd.arg {
            Some(ref arg) => arg.trim(),
            None => {
                msg.reply("Please specify a URL").await?;
                return Ok(());
            }
        };

        // Split URL and custom filename by pipe separator
        let parts: Vec<&str> = input.splitn(2, '|').collect();
        let url_str = parts[0].trim();

        if url_str.is_empty() {
            msg.reply("Please specify a valid URL").await?;
            return Ok(());
        }

        let custom_name = parts.get(1).map(|s| s.trim().to_string());
        let url = match Url::parse(url_str) {
            Ok(url) => url,
            Err(err) => {
                msg.reply(format!("Invalid URL '{}': {}", url_str, err)).await?;
                return Ok(());
            }
        };

        self.handle_url(msg, url, custom_name).await
    }

    /// Core URL handling: download from URL and upload to Telegram
    async fn handle_url(&self, msg: Message, url: Url, custom_name: Option<String>) -> Result<()> {
        let sender = match msg.sender() {
            Some(sender) => sender,
            None => return Ok(()),
        };

        // Acquire chat lock to prevent concurrent uploads
        info!("Locking chat {}", msg.chat().id());
        let lock_acquired = self.locks.insert(msg.chat().id());
        if !lock_acquired {
            msg.reply("✋ Whoa, slow down! There's already an active upload in this chat.")
                .await?;
            return Ok(());
        }
        self.started_by.insert(msg.chat().id(), sender.id());

        // Auto-unlock when function exits
        defer! {
            info!("Unlocking chat {}", msg.chat().id());
            self.locks.remove(&msg.chat().id());
            self.started_by.remove(&msg.chat().id());
        };

        // Download file from URL
        info!("Downloading file from {}", url);
        let response = self.http.get(url.clone()).send().await?;
        let length = response.content_length().unwrap_or_default() as usize;

        // Determine filename with proper extension
        let (original_name, ext) = Self::extract_filename(&response).await;
        let name = Self::build_final_filename(&original_name, ext, custom_name);

        // Percent decode filename
        let name = percent_encoding::percent_decode_str(&name)
            .decode_utf8()?
            .to_string();

        let is_video = Self::is_video_file(&response, &name);
        info!("File {} ({} bytes, video: {})", name, length, is_video);

        // Validate file
        if length == 0 {
            msg.reply("⚠️ File is empty").await?;
            return Ok(());
        }
        if length > 2 * 1024 * 1024 * 1024 {
            msg.reply("⚠️ File is too large").await?;
            return Ok(());
        }

        // Create cancelable stream
        let (trigger, stream) = Valved::new(
            response
                .bytes_stream()
                .map_err(|err| std::io::Error::new(std::io::ErrorKind::Other, err)),
        );
        self.triggers.insert(msg.chat().id(), trigger);

        defer! {
            self.triggers.remove(&msg.chat().id());
        };

        // Cancel button
        let reply_markup = Arc::new(reply_markup::inline(vec![vec![button::inline(
            "Cancel ✗",
            "cancel",
        )]]));

        // Send initial status message
        let status = Arc::new(Mutex::new(
            msg.reply(
                InputMessage::html(format!("🚀 Starting upload of <code>{}</code>...", name))
                    .reply_markup(reply_markup.clone().as_ref()),
            )
            .await?,
        ));

        let start_time = Arc::new(chrono::Utc::now());

        // Create progress-tracking stream
        let mut stream = stream
            .into_async_read()
            .compat()
            .report_progress(Duration::from_secs(3), {
                let status = status.clone();
                let name = name.clone();
                let reply_markup = reply_markup.clone();
                let start_time = start_time.clone();

                move |progress| {
                    let status = status.clone();
                    let name = name.clone();
                    let reply_markup = reply_markup.clone();
                    let start_time = start_time.clone();

                    tokio::spawn(async move {
                        let now = chrono::Utc::now();
                        let elapsed_secs = (now - *start_time).num_seconds().max(1) as f64;
                        let percent = progress as f64 / length as f64;

                        let progress_text = Self::format_progress_update(
                            progress, length, percent, elapsed_secs,
                        );

                        let msg_text = format!(
                            "\n\n⏳ <b>Uploading...</b>\n\n{}\n\n",
                            progress_text
                        );

                        status
                            .lock()
                            .await
                            .edit(InputMessage::html(msg_text).reply_markup(reply_markup.as_ref()))
                            .await
                            .ok();
                    });
                }
            });

        // Upload to Telegram
        let upload_start = chrono::Utc::now();
        let file = self
            .client
            .upload_stream(&mut stream, length, name.clone())
            .await?;

        let elapsed = chrono::Utc::now() - upload_start;
        info!("Uploaded file {} ({} bytes) in {}", name, length, elapsed);

        // Send completed file
        let input_msg = InputMessage::html(name.clone()).document(file);
        msg.reply(input_msg).await?;

        // Clean up status message
        status.lock().await.delete().await?;

        Ok(())
    }

    /// Extract filename from response headers or URL
    async fn extract_filename(response: &reqwest::Response) -> (String, &'static str) {
        // Try content-disposition header first
        if let Some(name) = response
            .headers()
            .get("content-disposition")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| {
                value
                    .split(';')
                    .map(|v| v.trim())
                    .find(|v| v.starts_with("filename="))
            })
            .map(|filename| filename.trim_start_matches("filename=").trim_matches('"'))
        {
            return (name.to_string(), "bin");
        }

        // Fallback to URL path segments
        if let Some(segments) = response.url().path_segments() {
            if let Some(last_segment) = segments.last() {
                if last_segment.contains('.') {
                    return (last_segment.to_string(), "bin");
                } else {
                    // Guess extension from content-type
                    if let Some(ext) = response
                        .headers()
                        .get("content-type")
                        .and_then(|value| value.to_str().ok())
                        .and_then(mime_guess::get_mime_extensions_str)
                        .and_then(|exts| exts.first())
                    {
                        return (format!("{}.{}", last_segment, ext), ext);
                    }
                }
            }
        }

        ("file.bin".to_string(), "bin")
    }

    /// Build final filename with custom name and proper extension
    fn build_final_filename(
        original_name: &str,
        ext: &str,
        custom_name: Option<String>,
    ) -> String {
        match custom_name {
            Some(custom) => {
                if custom.to_lowercase().ends_with(&format!(".{}", ext.to_lowercase())) {
                    custom
                } else {
                    format!("{}.{}", custom, ext)
                }
            }
            None => original_name.to_string(),
        }
    }

    /// Check if file is a video based on content-type or extension
    fn is_video_file(response: &reqwest::Response, filename: &str) -> bool {
        response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .map(|ctype| ctype.starts_with("video/mp4"))
            .unwrap_or(false)
            || filename.to_lowercase().ends_with(".mp4")
    }

    /// Format progress update with bar, stats, and ETA
    fn format_progress_update(
        progress: usize,
        total: usize,
        percent: f64,
        elapsed_secs: f64,
    ) -> String {
        let progress_bar = Self::create_progress_bar(percent, 10);
        
        // Format bytes to human-readable with MB/GB
        let format_bytes = |bytes: u64| {
            bytesize::to_string(bytes, true)
                .replace("MiB", "MB")
                .replace("GiB", "GB")
        };

        let uploaded = format_bytes(progress as u64);
        let total_size = format_bytes(total as u64);

        let speed = progress as f64 / elapsed_secs;
        let speed_str = format!("{}/s", format_bytes(speed as u64));

        let remaining = total.saturating_sub(progress);
        let eta_secs = if speed > 0.0 {
            (remaining as f64 / speed).round() as u64
        } else {
            0
        };
        let eta_str = Self::format_eta(eta_secs);

        format!(
            "[ {} ] {:.2}%\n\n\
             ➩ {} of {}\n\n\
             ➩ Speed : {}\n\n\
             ➩ Time Left : {}",
            progress_bar, percent * 100.0, uploaded, total_size, speed_str, eta_str
        )
    }

    /// Create visual progress bar
    fn create_progress_bar(percent: f64, width: usize) -> String {
        let filled = (percent * width as f64).floor() as usize;
        let empty = width.saturating_sub(filled);
        format!("{}{}", "▣".repeat(filled), "□".repeat(empty))
    }

    /// Format ETA as "X min Y sec"
    fn format_eta(seconds: u64) -> String {
        let mins = seconds / 60;
        let secs = seconds % 60;
        if mins > 0 {
            format!("{} min {} sec", mins, secs)
        } else {
            format!("{} sec", secs)
        }
    }

    /// Handle callback queries from inline buttons
    async fn handle_callback(&self, query: CallbackQuery) -> Result<()> {
        match query.data() {
            b"cancel" => self.handle_cancel(query).await,
            _ => Ok(()),
        }
    }

    /// Handle upload cancellation
    async fn handle_cancel(&self, query: CallbackQuery) -> Result<()> {
        let started_by_user_id = match self.started_by.get(&query.chat().id()) {
            Some(id) => *id,
            None => return Ok(()),
        };

        // Verify user can cancel this upload
        if started_by_user_id != query.sender().id() {
            info!(
                "User {} tried to cancel another user's upload in chat {}",
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

        // Cancel the upload stream
        if let Some((chat_id, trigger)) = self.triggers.remove(&query.chat().id()) {
            info!("Cancelling upload in chat {}", chat_id);
            drop(trigger);
            self.started_by.remove(&chat_id);

            query
                .load_message()
                .await?
                .edit("Upload cancelled ✗")
                .await?;

            query.answer().send().await?;
        }

        Ok(())
    }
}
