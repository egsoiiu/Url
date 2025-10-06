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

/// Main bot struct containing all state.
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

    /// Run the bot, polling for updates.
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

    /// Handle incoming updates (messages, button presses, etc).
    async fn handle_update(&self, update: Update) -> Result<()> {
        match update {
            Update::NewMessage(msg) => self.handle_message(msg).await,
            Update::CallbackQuery(query) => self.handle_callback(query).await,
            _ => Ok(()),
        }
    }

    /// Handle incoming messages, including commands and URLs.
    async fn handle_message(&self, msg: Message) -> Result<()> {
        // Only react to user and group chats
        match msg.chat() {
            Chat::User(_) | Chat::Group(_) => {},
            _ => return Ok(()),
        };

        // Try to parse a command
        let command = parse_command(msg.text());
        if let Some(command) = command {
            // Ignore commands for other bots
            if let Some(via) = &command.via {
                if via.to_lowercase() != self.me.username().unwrap_or_default().to_lowercase() {
                    warn!("Ignoring command for unknown bot: {}", via);
                    return Ok(());
                }
            }
            // In groups, only handle commands sent directly to this bot
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

        // If not a command, handle as URL (private chats only)
        if let Chat::User(_) = msg.chat() {
            let msg_clone = msg.clone();
            let text = msg.text().trim();

            // Support "url | filename" syntax
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
            // Fallback: treat as plain URL
            if let Ok(url) = Url::parse(text) {
                return self.handle_url(msg_clone, url, None).await;
            }
        }
        Ok(())
    }

    /// Send the /start welcome message with inline buttons.
    async fn handle_start(&self, msg: Message) -> Result<()> {
        let reply_markup = reply_markup::inline(vec![
            vec![
                button::inline("Help", "help"),
                button::inline("Sample", "sample"),
            ],
        ]);
        msg.reply(
            InputMessage::html(
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
                ✨ 𝐶𝑜𝑝𝑦 𝑎𝑛𝑑 𝑃𝑎𝑠𝑡𝑒 𝑦𝑜𝑢𝑟 𝑈𝑅𝐿 𝑡𝑜 𝑔𝑒𝑡 𝑠𝑡𝑎𝑟𝑡𝑒𝑑!"
            ).reply_markup(&reply_markup)
        ).await?;
        Ok(())
    }

    /// Handle the /upload command (parse URL and optional custom filename).
    async fn handle_upload(&self, msg: Message, cmd: Command) -> Result<()> {
        let input = match cmd.arg {
            Some(ref arg) => arg.trim(),
            None => {
                msg.reply("Please specify a URL").await?;
                return Ok(());
            }
        };
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

    /// Download the file from URL, show progress, and upload to Telegram.
    async fn handle_url(&self, msg: Message, url: Url, custom_name: Option<String>) -> Result<()> {
        let sender = match msg.sender() {
            Some(sender) => sender,
            None => return Ok(()),
        };

        // Lock chat to prevent concurrent uploads
        info!("Locking chat {}", msg.chat().id());
        let _lock = self.locks.insert(msg.chat().id());
        if !_lock {
            msg.reply("✋ Whoa, slow down! There's already an active upload in this chat.").await?;
            return Ok(());
        }
        self.started_by.insert(msg.chat().id(), sender.id());

        // Unlock chat when done
        defer! {
            info!("Unlocking chat {}", msg.chat().id());
            self.locks.remove(&msg.chat().id());
            self.started_by.remove(&msg.chat().id());
        };

        info!("Downloading file from {}", url);
        let response = self.http.get(url.clone()).send().await?;

        let length = response.content_length().unwrap_or_default() as usize;

        // Extract filename or guess from content-type
        let original_name = match response
            .headers()
            .get("content-disposition")
            .and_then(|value| {
                value
                    .to_str()
                    .ok()
                    .and_then(|value| {
                        value
                            .split(';')
                            .map(|v| v.trim())
                            .find(|v| v.starts_with("filename="))
                    })
                    .map(|filename| filename.trim_start_matches("filename=").trim_matches('"'))
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
                        response
                            .headers()
                            .get("content-type")
                            .and_then(|value| value.to_str().ok())
                            .and_then(mime_guess::get_mime_extensions_str)
                            .and_then(|exts| exts.first())
                            .map(|ext| format!("{}.{}", name, ext))
                    }
                })
                .unwrap_or_else(|| "file.bin".to_string()),
        };

        // Extract extension
        let ext = original_name
            .rsplit_once('.')
            .map(|(_, ext)| ext)
            .unwrap_or("bin");

        // Determine final filename
        let name = if let Some(custom) = custom_name {
            if custom.to_lowercase().ends_with(&format!(".{}", ext.to_lowercase())) {
                custom
            } else {
                format!("{}.{}", custom, ext)
            }
        } else {
            original_name
        };

        // Percent decode filename
        let name = percent_encoding::percent_decode_str(&name)
            .decode_utf8()?
            .to_string();

        // Detect if the file is an mp4 video
        let is_video = response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .map(|ctype| ctype.starts_with("video/mp4"))
            .unwrap_or(false)
            || name.to_lowercase().ends_with(".mp4");

        info!("File {} ({} bytes, video: {})", name, length, is_video);

        if length == 0 {
            msg.reply("⚠️ File is empty").await?;
            return Ok(());
        }
        if length > 2 * 1024 * 1024 * 1024 {
            msg.reply("⚠️ File is too large").await?;
            return Ok(());
        }

        // Prepare streaming for progress reporting & cancellation
        let (trigger, stream) = Valved::new(
            response
                .bytes_stream()
                .map_err(|err| std::io::Error::new(std::io::ErrorKind::Other, err)),
        );
        self.triggers.insert(msg.chat().id(), trigger);

        defer! {
            self.triggers.remove(&msg.chat().id());
        };

        // Cancel button reply markup
        let reply_markup = Arc::new(reply_markup::inline(vec![vec![button::inline(
            "Cancel ✗",
            "cancel",
        )]]));

        // Helper to format file sizes
        fn format_file_size(bytes: u64) -> String {
            bytesize::to_string(bytes, true)
                .replace("MiB", "MB")
                .replace("GiB", "GB")
        }

        // Send initial status message
        let status = Arc::new(Mutex::new(
            msg.reply(
                InputMessage::html(format!(
                    "<blockquote>📥 <b>Download Started</b>\n\n<b>File Name:</b> {}\n<b>Size:</b> {}</blockquote>",
                    name,
                    format_file_size(length as u64)
                ))
                .reply_markup(reply_markup.clone().as_ref()),
            )
            .await?,
        ));

        let start_time = Arc::new(chrono::Utc::now());

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
                        let progress_bar = create_progress_bar(percent, 10);

                        // Inline ETA formatter
                        let format_eta = |seconds: u64| -> String {
                            if seconds >= 60 {
                                let minutes = seconds / 60;
                                let secs = seconds % 60;
                                if secs > 0 {
                                    format!("{} min {} sec", minutes, secs)
                                } else {
                                    format!("{} min", minutes)
                                }
                            } else {
                                format!("{} sec", seconds)
                            }
                        };

                        let uploaded = format_file_size(progress as u64);
                        let total = format_file_size(length as u64);
                        let speed = progress as f64 / elapsed_secs;
                        let speed_str = format!("{}/s", format_file_size(speed as u64));
                        let remaining = length.saturating_sub(progress);
                        let eta_secs = if speed > 0.0 {
                            (remaining as f64 / speed).round() as u64
                        } else {
                            0
                        };
                        let eta_str = format_eta(eta_secs);

                        let msg_text = format!(
                            "ㅤ<br> \n <blockquote>ㅤ\n <br><br> 𝐹𝑖𝑙𝑒 𝑛𝑎𝑚𝑒 :  <a href=\"https://example.com/file.zip\">{}</a><br><br> \n\n ㅤㅤ𝑆𝑖𝑧𝑒 :  {}<br><br><br></blockquote>\n\nDownload Completed ✓\n\n\n⏳ Uploading...\n\n[ {} ] {:.2}%\n\n➩ {} of {}\n\n➩ Speed : {}\n\n➩ Time Left : {}",
                            name,
                            total,
                            progress_bar,
                            percent * 100.0,
                            uploaded,
                            total,
                            speed_str,
                            eta_str,
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

        // Progress bar helper
        fn create_progress_bar(percent: f64, width: usize) -> String {
            let filled = (percent * width as f64).floor() as usize;
            let empty = width.saturating_sub(filled);
            format!("{}{}", "▣".repeat(filled), "□".repeat(empty))
        }

        // Upload the file
        let start_time = chrono::Utc::now();
        let file = self
            .client
            .upload_stream(&mut stream, length, name.clone())
            .await?;

        let elapsed = chrono::Utc::now() - start_time;
        info!("Uploaded file {} ({} bytes) in {}", name, length, elapsed);

        // Send file as Telegram document
        let mut input_msg = InputMessage::html(name.clone());
        input_msg = input_msg.document(file);
        msg.reply(input_msg).await?;

        // Delete status message
        status.lock().await.delete().await?;
        Ok(())
    }

    /// Handle inline button presses (help, sample, back, cancel, etc).
    async fn handle_callback(&self, query: CallbackQuery) -> Result<()> {
        match query.data() {
            b"help" => {
                let reply_markup = reply_markup::inline(vec![
                    vec![
                        button::inline("Back", "back"),
                        button::inline("Sample", "sample"),
                    ],
                ]);
                query
                    .load_message()
                    .await?
                    .edit(
                        InputMessage::html(
                            "<b>Help</b>\n\nSend me a URL or use the /upload command!\n\nYou can specify a custom filename using <code>URL | filename</code>."
                        ).reply_markup(&reply_markup)
                    )
                    .await?;
                query.answer().send().await?;
                Ok(())
            }
            b"sample" => {
                let reply_markup = reply_markup::inline(vec![
                    vec![
                        button::inline("Back", "back"),
                        button::inline("Help", "help"),
                    ],
                ]);
                query
                    .load_message()
                    .await?
                    .edit(
                        InputMessage::html(
                            "<b>Sample Usage</b>\n\nExample:\n<code>https://example.com/file.zip | MyFile.zip</code>\n\nThis will download and upload the file as 'MyFile.zip'."
                        ).reply_markup(&reply_markup)
                    )
                    .await?;
                query.answer().send().await?;
                Ok(())
            }
            b"back" => {
                let reply_markup = reply_markup::inline(vec![
                    vec![
                        button::inline("Help", "help"),
                        button::inline("Sample", "sample"),
                    ],
                ]);
                query
                    .load_message()
                    .await?
                    .edit(
                        InputMessage::html(
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
                            ✨ 𝐶𝑜𝑝𝑦 𝑎𝑛𝑑 𝑃𝑎𝑠𝑡𝑒 𝑦𝑜𝑢𝑟 𝑈𝑅𝐿 𝑡𝑜 𝑔𝑒𝑡 𝑠𝑡𝑎𝑟𝑡𝑒𝑑!"
                        ).reply_markup(&reply_markup)
                    )
                    .await?;
                query.answer().send().await?;
                Ok(())
            }
            b"cancel" => self.handle_cancel(query).await,
            _ => Ok(()),
        }
    }

    /// Cancel an ongoing upload if requested by the original user.
    async fn handle_cancel(&self, query: CallbackQuery) -> Result<()> {
        let started_by_user_id = match self.started_by.get(&query.chat().id()) {
            Some(id) => *id,
            None => return Ok(()),
        };
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
