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
use percent_encoding::percent_decode_str;
use reqwest::Url;
use scopeguard::defer;
use stream_cancel::{Trigger, Valved};
use tokio::sync::Mutex;
use tokio_util::compat::FuturesAsyncReadCompatExt;

use crate::command::{parse_command, Command};

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
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .user_agent("Mozilla/5.0 (compatible)")
            .build()?;

        Ok(Arc::new(Self {
            client,
            me,
            http,
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

    async fn handle_update(&self, update: Update) -> Result<()> {
        match update {
            Update::NewMessage(msg) => self.handle_message(msg).await,
            Update::CallbackQuery(query) => self.handle_callback(query).await,
            _ => Ok(()),
        }
    }

    async fn handle_message(&self, msg: Message) -> Result<()> {
        match msg.chat() {
            Chat::User(_) | Chat::Group(_) => {},
            _ => return Ok(()),
        }

        let command = parse_command(msg.text());
        if let Some(command) = command {
            if let Some(via) = &command.via {
                if via.to_lowercase() != self.me.username().unwrap_or_default().to_lowercase() {
                    warn!("Ignoring command for another bot: {}", via);
                    return Ok(());
                }
            }

            if let Chat::Group(_) = msg.chat() {
                if command.name == "start" && command.via.is_none() {
                    return Ok(());
                }
            }

            match command.name.as_str() {
                "start" => return self.handle_start(msg).await,
                "upload" => return self.handle_upload(msg, command).await,
                _ => {}
            }
        }

        if let Chat::User(_) = msg.chat() {
            let msg_clone = msg.clone();
            let text = msg.text().trim();

            if let Some(pos) = text.find('|') {
                let (url_part, name_part) = text.split_at(pos);
                let url_str = url_part.trim();
                let custom_name = name_part[1..].trim();

                if let Ok(url) = Url::parse(url_str) {
                    return self.handle_url(msg_clone, url, Some(custom_name.to_string())).await;
                }
            }

            if let Ok(url) = Url::parse(text) {
                return self.handle_url(msg_clone, url, None).await;
            }
        }

        Ok(())
    }

    async fn handle_start(&self, msg: Message) -> Result<()> {
        msg.reply(InputMessage::html(
            "𝑊𝑒𝑙𝑐𝑜𝑚𝑒 𝑡𝑜 𝑈𝑅𝐿 𝑈𝑝𝑙𝑜𝑎𝑑𝑒𝑟 𝑏𝑜𝑡\n\n\
            𝐶𝑢𝑠𝑡𝑜𝑚 𝑓𝑖𝑙𝑒 𝑛𝑎𝑚𝑖𝑛𝑔:\n➠ 𝑈𝑅𝐿 | 𝐹𝑖𝑙𝑒_𝑁𝑎𝑚𝑒\n\n\
            <blockquote>𝐹𝑒𝑎𝑡𝑢𝑟𝑒𝑠:\n\
            ➠ 𝑐𝑟𝑎𝑧𝑦 𝑓𝑎𝑠𝑡 & 𝑓𝑟𝑒𝑒\n\
            ➠ 𝑢𝑝 𝑡𝑜 2𝐺𝐵\n\
            ➠ 𝑐𝑢𝑠𝑡𝑜𝑚 𝑓𝑖𝑙𝑒 𝑛𝑎𝑚𝑒 𝑤𝑖𝑡ℎ 𝑎𝑢𝑡𝑜 𝑒𝑥𝑡𝑒𝑛𝑠𝑖𝑜𝑛</blockquote>\n\n\
            ✨ 𝐶𝑜𝑝𝑦 𝑎𝑛𝑑 𝑃𝑎𝑠𝑡𝑒 𝑦𝑜𝑢𝑟 𝑈𝑅𝐿 𝑡𝑜 𝑔𝑒𝑡 𝑠𝑡𝑎𝑟𝑡𝑒𝑑!"
        ))
        .await?;
        Ok(())
    }

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

    async fn handle_url(&self, msg: Message, url: Url, custom_name: Option<String>) -> Result<()> {
        let sender = match msg.sender() {
            Some(sender) => sender,
            None => return Ok(()),
        };

        let chat_id = msg.chat().id();

        if !self.locks.insert(chat_id) {
            msg.reply("✋ Whoa, slow down! There's already an active upload in this chat.")
                .await?;
            return Ok(());
        }

        self.started_by.insert(chat_id, sender.id());
        defer! {
            self.locks.remove(&chat_id);
            self.started_by.remove(&chat_id);
        }

        info!("Downloading file from {}", url);
        let response = self.http.get(url.clone()).send().await?;
        let length = response.content_length().unwrap_or_default() as usize;

        if length == 0 {
            msg.reply("⚠️ File is empty").await?;
            return Ok(());
        }

        if length > 2 * 1024 * 1024 * 1024 {
            msg.reply("⚠️ File is too large").await?;
            return Ok(());
        }

        let original_name = response
            .headers()
            .get("content-disposition")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(';').find(|s| s.trim().starts_with("filename=")))
            .and_then(|s| Some(s.trim().trim_start_matches("filename=").trim_matches('"')))
            .map(|s| s.to_string())
            .or_else(|| {
                response
                    .url()
                    .path_segments()
                    .and_then(|segments| segments.last())
                    .map(|s| s.to_string())
            })
            .unwrap_or_else(|| "file.bin".to_string());

        let ext = original_name
            .rsplit_once('.')
            .map(|(_, ext)| ext)
            .unwrap_or("bin");

        let final_name = custom_name
            .map(|name| format!("{}.{}", name, ext))
            .unwrap_or(original_name);

        let (trigger, tripwire) = stream_cancel::Tripwire::new();
        let mut stream = Valved::new(response.bytes_stream(), tripwire);

        self.triggers.insert(chat_id, trigger.clone());
        defer! {
            self.triggers.remove(&chat_id);
        }

        let mut progress = msg.reply(format!("⬇️ Downloading: `{}`", final_name)).await?;
        let mut last_update = tokio::time::Instant::now();

        let mut reader = stream.into_async_read().progress_with(move |curr| {
            let now = tokio::time::Instant::now();
            if now.duration_since(last_update).as_secs() >= 1 {
                last_update = now;
                let percent = (curr as f64 / length as f64) * 100.0;
                let _ = progress.edit(format!("⬇️ Downloading: `{}`\nProgress: {:.2}%", final_name, percent));
            }
        });

        let mut data = Vec::with_capacity(length);
        reader.read_to_end(&mut data).await?;

        progress
            .edit(format!("⬆️ Uploading: `{}`", final_name))
            .await?;

        msg.reply(InputMessage::file(data).file_name(final_name)).await?;

        Ok(())
    }

    async fn handle_callback(&self, query: CallbackQuery) -> Result<()> {
        let chat_id = query.chat().id();
        let sender_id = query.sender().id();

        if query.data() == "cancel" {
            if let Some(&starter_id) = self.started_by.get(&chat_id).as_deref() {
                if starter_id != sender_id {
                    query.answer("You're not authorized to cancel this upload.").await?;
                    return Ok(());
                }
            }

            if let Some(trigger) = self.triggers.get(&chat_id) {
                trigger.cancel();
                query.answer("Upload cancelled.").await?;
            } else {
                query.answer("Nothing to cancel.").await?;
            }
        }

        Ok(())
    }
}
