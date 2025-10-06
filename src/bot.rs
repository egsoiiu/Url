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
use bytes::Bytes;

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

        // Handle commands only if sent explicitly to this bot in group
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
    // Clone `msg` so we can move the clone and avoid the borrow issue
    let msg_clone = msg.clone();

    // Handle URL with custom filename syntax (url | filename)
    let text = msg.text().trim();

    // Check if the message contains pipe syntax for custom filename
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

    // Fallback: try parsing the entire message as a plain URL (without custom filename)
    if let Ok(url) = Url::parse(text) {
        return self.handle_url(msg_clone, url, None).await;
    }
}


    Ok(())
}

// --- Paste this handle_start function ---
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

/// Handle the /upload command.
/// This command should be used in groups to upload a file.
/// Supports custom filename via '|' separator.
async fn handle_upload(&self, msg: Message, cmd: Command) -> Result<()> {
    // If the argument is not specified, reply with an error
    let input = match cmd.arg {
        Some(ref arg) => arg.trim(),
        None => {
            msg.reply("Please specify a URL").await?;
            return Ok(());
        }
    };

    // Split by pipe '|' to separate URL and custom filename
    let parts: Vec<&str> = input.splitn(2, '|').collect();

    let url_str = parts[0].trim();
if url_str.is_empty() {
    msg.reply("Please specify a valid URL").await?;
    return Ok(());
}
    
    let custom_name = parts.get(1).map(|s| s.trim().to_string());

    // Parse the URL
    // Parse the URL
let url = match Url::parse(url_str) {
    Ok(url) => url,
    Err(err) => {
        msg.reply(format!("Invalid URL '{}': {}", url_str, err)).await?;
        return Ok(());
    }
};

    // Pass URL and custom name to handle_url
    self.handle_url(msg, url, custom_name).await
}

/// Handle a URL.
/// This function will download the file and upload it to Telegram.
/// Supports optional custom file name (without extension).
/// Handles downloading a file from a URL and uploading it to Telegram.
/// - If Content-Length is present: streams upload instantly.
/// - If Content-Length is missing: prefetches some bytes; if any, starts streaming upload.
async fn handle_url(&self, msg: Message, url: Url, custom_name: Option<String>) -> Result<()> {
    // 1. Identify sender for permission checks (if needed)
    let sender = match msg.sender() {
        Some(sender) => sender,
        None => return Ok(()),
    };

    // 2. Lock the chat to prevent concurrent uploads (per-chat mutex)
    info!("Locking chat {}", msg.chat().id());
    let _lock = self.locks.insert(msg.chat().id());
    if !_lock {
        msg.reply("✋ Whoa, slow down! There's already an active upload in this chat.").await?;
        return Ok(());
    }
    self.started_by.insert(msg.chat().id(), sender.id());

    // Ensure unlock even on error/return
    defer! {
        info!("Unlocking chat {}", msg.chat().id());
        self.locks.remove(&msg.chat().id());
        self.started_by.remove(&msg.chat().id());
    };

    // 3. Start HTTP GET request for the file
    info!("Downloading file from {}", url);
    let response = self.http.get(url.clone()).send().await?;

    // 4. Guess filename from Content-Disposition header or URL path
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

    // 5. Extract or guess file extension
    let ext = original_name
        .rsplit_once('.')
        .map(|(_, ext)| ext)
        .unwrap_or("bin");

    // 6. Compose final file name (use custom if provided, else use guessed)
    let name = if let Some(custom) = custom_name {
        if custom.to_lowercase().ends_with(&format!(".{}", ext.to_lowercase())) {
            custom
        } else {
            format!("{}.{}", custom, ext)
        }
    } else {
        original_name
    };
    let name = percent_encoding::percent_decode_str(&name).decode_utf8()?.to_string();

    // 7. Detect if the file is video/mp4 by content-type or filename extension
    let is_video = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .map(|ctype| ctype.starts_with("video/mp4"))
        .unwrap_or(false)
        || name.to_lowercase().ends_with(".mp4");

    info!("File {} (video: {})", name, is_video);

    // 8. Prepare a cancel button
    let reply_markup = Arc::new(reply_markup::inline(vec![vec![button::inline(
        "Cancel ✗",
        "cancel",
    )]]));

    // Helper function to format file sizes
    fn format_file_size(bytes: u64) -> String {
        bytesize::to_string(bytes, true)
            .replace("MiB", "MB")
            .replace("GiB", "GB")
    }

    // Helper function to create progress bar
    fn create_progress_bar(percent: f64, width: usize) -> String {
        let filled = (percent * width as f64).floor() as usize;
        let empty = width.saturating_sub(filled);
        format!("{}{}", "▣".repeat(filled), "□".repeat(empty))
    }

    // 9. Main streaming/upload logic
    let length = response.content_length();
    let mut http_stream = response.bytes_stream();

    if let Some(len) = length {
        // 10a. Content-Length present: stream upload directly
        info!("File size known: {} bytes", len);
        
        // Check for empty file
        if len == 0 {
            msg.reply("⚠️ File is empty").await?;
            return Ok(());
        }

        // File is too large
        if len > 2 * 1024 * 1024 * 1024 {
            msg.reply("⚠️ File is too large").await?;
            return Ok(());
        }

        // Send initial status message
        let status = Arc::new(Mutex::new(
            msg.reply(
                InputMessage::html(format!(
                    "<blockquote>📥 <b>Download Started</b>\n\n<b>File Name:</b> {}\n<b>Size:</b> {}</blockquote>",
                    name,
                    format_file_size(len)
                ))
                .reply_markup(reply_markup.clone().as_ref()),
            )
            .await?,
        ));

        let start_time = Arc::new(chrono::Utc::now());

        // Wrap the response stream in a valved stream for cancellation
        let (trigger, stream) = Valved::new(
            http_stream
                .map_err(|err| std::io::Error::new(std::io::ErrorKind::Other, err)),
        );
        self.triggers.insert(msg.chat().id(), trigger);

        // Deferred trigger removal
        defer! {
            self.triggers.remove(&msg.chat().id());
        };

        let mut stream = stream
            .into_async_read()
            .compat()
            // Report progress every 3 seconds
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

                        let percent = progress as f64 / len as f64;
                        let progress_bar = create_progress_bar(percent, 10);

                        // Inline function to format ETA as "1 min 45 sec" or "45 sec"
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
                        let total = format_file_size(len);

                        let speed = progress as f64 / elapsed_secs;
                        let speed_str = format!("{}/s", format_file_size(speed as u64));

                        let remaining = len.saturating_sub(progress as u64);
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

        // Upload the file
        let start_time = chrono::Utc::now();
        let file = self
            .client
            .upload_stream(&mut stream, len as usize, name.clone())
            .await?;

        // Calculate upload time
        let elapsed = chrono::Utc::now() - start_time;
        info!("Uploaded file {} ({} bytes) in {}", name, len, elapsed);

        // Send file
        let mut input_msg = InputMessage::html(name.clone());
        input_msg = input_msg.document(file); // Always upload as document

        msg.reply(input_msg).await?;

        // Delete status message
        status.lock().await.delete().await?;

        return Ok(());
    }

    // 10b. Content-Length missing: prefetch up to 32KB to check file is not empty
    info!("Content-Length missing, prefetching to check file content");
    
    // Inform user we're starting download
    let status = Arc::new(Mutex::new(
        msg.reply(
            InputMessage::html(format!(
                "<blockquote>📥 <b>Starting download...</b>\n\n<b>File Name:</b> {}\n<b>Size:</b> Unknown</blockquote>",
                name
            )).reply_markup(reply_markup.clone().as_ref()),
        ).await?,
    ));

    let mut buffer = Vec::new();
    let mut buffered_size = 0usize;
    let prefetch_size = 32 * 1024; // 32 KB

    while let Some(chunk) = http_stream.try_next().await? {
        buffered_size += chunk.len();
        buffer.extend_from_slice(&chunk);
        if buffered_size >= prefetch_size {
            break;
        }
    }

    if buffer.is_empty() {
        // No data at all -- file is empty or inaccessible
        status.lock().await.edit(InputMessage::html("⚠️ File is empty or could not be downloaded.")).await?;
        return Ok(());
    }

    info!("Prefetched {} bytes, file has content - proceeding with upload", buffered_size);

    // Update status to show we're uploading
    status.lock().await.edit(
        InputMessage::html(format!(
            "<blockquote>📤 <b>Uploading file...</b>\n\n<b>File Name:</b> {}\n<b>Size:</b> Unknown (streaming)</blockquote>",
            name
        )).reply_markup(reply_markup.clone().as_ref())
    ).await?;

    // 11. Combine prefetched buffer with the rest of the HTTP stream for upload
    let prefetched = futures::stream::once(async move { 
        Ok::<_, reqwest::Error>(Bytes::from(buffer)) 
    });
    let combined_stream = prefetched.chain(http_stream);
    
    // Wrap the combined stream in a valved stream for cancellation
    let (trigger, stream) = Valved::new(
        combined_stream
            .map_err(|err| std::io::Error::new(std::io::ErrorKind::Other, err)),
    );
    self.triggers.insert(msg.chat().id(), trigger);

    // Deferred trigger removal
    defer! {
        self.triggers.remove(&msg.chat().id());
    };

    let mut combined_reader = stream.into_async_read().compat();

    // 12. Start upload to Telegram (unknown total length: pass 0)
    let start_time = chrono::Utc::now();
    let file = self
        .client
        .upload_stream(&mut combined_reader, 0, name.clone())
        .await?;

    // Calculate upload time
    let elapsed = chrono::Utc::now() - start_time;
    info!("Uploaded file {} (unknown size) in {}", name, elapsed);

    // Send file
    let mut input_msg = InputMessage::html(name.clone());
    input_msg = input_msg.document(file);
    msg.reply(input_msg).await?;

    // Delete status message
    status.lock().await.delete().await?;

    Ok(())
}
// Helper function to create progress bar
fn create_progress_bar(percent: f64, width: usize) -> String {
    let filled = (percent * width as f64).floor() as usize;
    let empty = width.saturating_sub(filled);
    format!("{}{}", "▣".repeat(filled), "□".repeat(empty))
}


// --- Paste this in your handle_callback function ---
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
        // Add your other callback handlers (e.g. "cancel") here
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
