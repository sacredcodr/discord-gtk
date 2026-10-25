//! Network side of the app.
//!
//! Everything here runs on ONE dedicated thread with a current-thread tokio
//! runtime. That is a deliberate performance choice: no work-stealing pool, no
//! idle worker threads, and the thread sleeps in epoll whenever the socket is
//! quiet. The only scheduled wakeup is the gateway heartbeat.
//!
//! IMPORTANT: nothing here may touch GTK. Results travel to the main thread
//! through the `Event` channel only.

use std::{sync::Arc, time::Duration};

use async_channel::{Receiver, Sender};
use serde_json::json;
use tokio::{
    sync::{Mutex, Semaphore},
    time::MissedTickBehavior,
};

use crate::error::Error;
use crate::model::{Panel, RelateAction};
use crate::gateway;
use crate::model::{Command, Event, Message};
use crate::rest::Rest;
use crate::voice::{Signal, Voice};

/// Concurrent image downloads. A channel full of pictures would otherwise open
/// dozens of connections at once.
const MAX_IMAGE_DOWNLOADS: usize = 6;

/// Messages fetched when a channel is opened. Discord allows up to 100; 50
/// fills a tall window and keeps the first paint fast.
const HISTORY_LIMIT: u32 = 50;

/// Starts the backend thread. The thread exits when `commands` is closed and
/// the gateway gives up, i.e. when the UI is gone.
pub fn spawn(token: String, commands: Receiver<Command>, events: Sender<Event>) {
    std::thread::Builder::new()
        .name("backend".into())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("tokio current-thread runtime");
            runtime.block_on(async move {
                let rest = Rest::new(&token);
                // Payloads for the gateway (voice state) and events coming from it.
                let (outbound_tx, outbound_rx) = async_channel::unbounded();
                let (gateway_tx, gateway_rx) = async_channel::unbounded();
                let outbound = outbound_tx.clone();
                let voice = Arc::new(Mutex::new(Voice::new(outbound_tx, events.clone())));
                tokio::join!(
                    gateway::run(token, gateway_tx, outbound_rx),
                    forward(gateway_rx, events.clone(), voice.clone()),
                    voice_clock(voice.clone()),
                    command_loop(rest, commands, events, voice, outbound)
                );
            });
        })
        .expect("spawn backend thread");
}

/// Passes gateway events on to the UI, and voice-related ones to the voice
/// manager on a task of their own: joining a call can take seconds and must
/// not hold up chat.
async fn forward(gateway: Receiver<Event>, events: Sender<Event>, voice: Arc<Mutex<Voice>>) {
    while let Ok(event) = gateway.recv().await {
        if let Some(signal) = Signal::from_event(&event) {
            let voice = voice.clone();
            tokio::spawn(async move { voice.lock().await.observe(signal).await });
        }
        if events.send(event).await.is_err() {
            return;
        }
    }
}

/// Opens and closes the microphone to follow speech, 50 times a second.
async fn voice_clock(voice: Arc<Mutex<Voice>>) {
    let mut ticker = tokio::time::interval(Duration::from_millis(20));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;
        let mut voice = voice.lock().await;
        if voice.connected() {
            voice.tick();
        }
    }
}

async fn command_loop(
    rest: Rest,
    commands: Receiver<Command>,
    events: Sender<Event>,
    voice: Arc<Mutex<Voice>>,
    outbound: Sender<serde_json::Value>,
) {
    let downloads = Arc::new(Semaphore::new(MAX_IMAGE_DOWNLOADS));
    while let Ok(command) = commands.recv().await {
        match command {
            // Voice commands can wait on a lock held by a join in progress, so
            // they run on their own tasks.
            Command::JoinVoice { guild_id, channel_id } => {
                let voice = voice.clone();
                tokio::spawn(async move { voice.lock().await.join(guild_id, channel_id).await });
            }
            Command::SubscribeMembers { guild_id, channel_id } => {
                let _ = outbound.try_send(json!({"op": 14, "d": {
                    "guild_id": guild_id,
                    "typing": true,
                    "activities": true,
                    "threads": true,
                    "channels": { channel_id: [[0, 99]] },
                }}));
            }
            Command::FetchUsers { guild_id, user_ids } => {
                // Names come back as a GUILD_MEMBERS_CHUNK on the gateway.
                let _ = outbound.try_send(json!({"op": 8, "d": {
                    "guild_id": guild_id,
                    "user_ids": user_ids,
                    "presences": true,
                    "limit": 0,
                }}));
            }
            Command::LeaveVoice => {
                let voice = voice.clone();
                tokio::spawn(async move { voice.lock().await.leave().await });
            }
            Command::SetVoiceMute { mute, deaf } => {
                let voice = voice.clone();
                tokio::spawn(async move { voice.lock().await.set_mute(mute, deaf) });
            }
            Command::FetchImage(url) => {
                let (rest, events, downloads) = (rest.clone(), events.clone(), downloads.clone());
                tokio::spawn(async move {
                    let _permit = downloads.acquire().await;
                    let event = match rest.fetch_image(&url).await {
                        Ok(bytes) => Event::Image { url, bytes },
                        Err(_) => Event::ImageFailed { url },
                    };
                    let _ = events.send(event).await;
                });
            }
            command => {
                // One task per command so a slow request (or a rate-limit wait)
                // never blocks the next click.
                let (rest, events) = (rest.clone(), events.clone());
                tokio::spawn(async move {
                    let event = match handle(command, &rest).await {
                        Ok(Some(event)) => event,
                        Ok(None) => return,
                        Err(e) => Event::Failed(e),
                    };
                    let _ = events.send(event).await;
                });
            }
        }
    }
}

/// Returns the event to deliver, or `None` when the command has no direct
/// answer (a sent message comes back through the gateway).
async fn handle(command: Command, rest: &Rest) -> Result<Option<Event>, Error> {
    let event = match command {
        Command::LoadHistory(channel_id) => {
            let path = format!("/channels/{channel_id}/messages?limit={HISTORY_LIMIT}");
            let mut messages: Vec<Message> = rest.get(&path).await?;
            // Discord returns newest first; the list wants oldest first.
            messages.reverse();
            Event::History { channel_id, messages }
        }
        Command::Send { channel_id, content, reply_to, nonce } => {
            let mut content = content;
            let mut body = json!({ "nonce": nonce, "enforce_nonce": true });
            if crate::builtin::enabled("silentMessageToggle") {
                if let Some(rest) = content.strip_prefix("@silent ") {
                    body["flags"] = json!(4096); // SUPPRESS_NOTIFICATIONS
                    content = rest.to_owned();
                }
            }
            body["content"] = json!(content);
            if let Some(id) = reply_to {
                body["message_reference"] = json!({ "message_id": id });
                if crate::builtin::enabled("noReplyMention") {
                    body["allowed_mentions"] = json!({ "parse": ["users", "roles", "everyone"], "replied_user": false });
                }
            }
            let started = std::time::Instant::now();
            let sent = rest.post(&format!("/channels/{channel_id}/messages"), &body).await;
            tracing::debug!(ms = started.elapsed().as_millis() as u64, ok = sent.is_ok(), "message POST finished");
            return Ok(sent.err().map(|error| Event::SendFailed { nonce, error }));
        }
        Command::SendFiles { channel_id, files, content, reply_to, nonce } => {
            /// Beyond this the file is almost certainly over Discord's limit anyway,
            /// and holding it in memory would be wasteful.
            const MAX_UPLOAD: u64 = 100 * 1024 * 1024;
            let mut loaded: Vec<(String, Vec<u8>)> = Vec::new();
            for path in &files {
                let mut name = path.file_name().map_or_else(|| "file".into(), |n| n.to_string_lossy().into_owned());
                if crate::builtin::enabled("anonymiseFileNames") {
                    name = crate::builtin::anonymised(&name);
                }
                let bad = |why: String| Error::Decode { path: name.clone(), reason: why };
                let meta = tokio::fs::metadata(path).await.map_err(|e| bad(e.to_string()))?;
                if meta.len() > MAX_UPLOAD {
                    return Err(bad("file is larger than 100 MB".into()));
                }
                let data = tokio::fs::read(path).await.map_err(|e| bad(e.to_string()))?;
                loaded.push((name, data));
            }

            // 1. Ask for an upload slot per file.
            let wanted: Vec<_> = loaded
                .iter()
                .enumerate()
                .map(|(i, (name, data))| json!({ "id": i.to_string(), "filename": name, "file_size": data.len() }))
                .collect();
            let slots: serde_json::Value =
                rest.post_json(&format!("/channels/{channel_id}/attachments"), &json!({ "files": wanted })).await?;
            let slots = slots["attachments"].as_array().cloned().unwrap_or_default();

            // 2. Put the bytes where Discord said, 3. then refer to them in the message.
            let mut attachments = Vec::new();
            for (i, (name, data)) in loaded.into_iter().enumerate() {
                let slot = slots
                    .iter()
                    .find(|s| s["id"].as_str().map_or_else(|| s["id"].as_u64() == Some(i as u64), |id| id == i.to_string()))
                    .ok_or_else(|| Error::Decode { path: name.clone(), reason: "no upload slot".into() })?;
                let (Some(url), Some(stored)) = (slot["upload_url"].as_str(), slot["upload_filename"].as_str()) else {
                    return Err(Error::Decode { path: name, reason: "upload slot incomplete".into() });
                };
                rest.upload_to(url, data, mime_for(&name)).await?;
                attachments.push(json!({ "id": i.to_string(), "filename": name, "uploaded_filename": stored }));
            }
            let mut body = json!({
                "content": content,
                "nonce": nonce,
                "attachments": attachments,
                "flags": 0,
            });
            if let Some(id) = reply_to {
                body["message_reference"] = json!({ "message_id": id });
            }
            rest.post(&format!("/channels/{channel_id}/messages"), &body).await?;
            return Ok(None);
        }
        Command::Edit { channel_id, id, content } => {
            let path = format!("/channels/{channel_id}/messages/{id}");
            rest.patch(&path, &json!({ "content": content })).await?;
            return Ok(None);
        }
        Command::Delete { channel_id, id } => {
            rest.delete(&format!("/channels/{channel_id}/messages/{id}")).await?;
            return Ok(None);
        }
        Command::React { channel_id, id, emoji, add } => {
            let path = format!(
                "/channels/{channel_id}/messages/{id}/reactions/{}/@me",
                emoji.api_path()
            );
            if add { rest.put(&path).await? } else { rest.delete(&path).await? }
            // The gateway also announces this, but refreshing here makes the
            // click feel instant even when the event is slow or dropped.
            let message: Message = rest.get(&format!("/channels/{channel_id}/messages/{id}")).await?;
            Event::MessageUpdate(message)
        }
        Command::Ack { channel_id, message_id } => {
            let path = format!("/channels/{channel_id}/messages/{message_id}/ack");
            rest.post(&path, &json!({ "token": null })).await?;
            return Ok(None);
        }
        Command::FetchProfile { user_id, guild_id } => {
            let mut path = format!("/users/{user_id}/profile?with_mutual_guilds=true&with_mutual_friends_count=true");
            if let Some(guild) = guild_id {
                path.push_str(&format!("&guild_id={guild}"));
            }
            let value: serde_json::Value = rest.get(&path).await?;
            return Ok(Some(Event::Profile { user_id, value }));
        }
        Command::OpenDm(user_id) => {
            let channel: crate::model::Channel =
                rest.post_json("/users/@me/channels", &json!({ "recipient_id": user_id })).await?;
            return Ok(Some(Event::DmOpened(channel)));
        }
        Command::Relate { user_id, action } => {
            let path = format!("/users/@me/relationships/{user_id}");
            match action {
                RelateAction::Add => rest.put_json(&path, &json!({})).await?,
                RelateAction::Remove => rest.delete(&path).await?,
            }
            return Ok(None);
        }
        Command::AddFriend(username) => {
            rest.post("/users/@me/relationships", &json!({ "username": username, "discriminator": null })).await?;
            return Ok(None);
        }
        Command::Fetch(panel) => {
            let path = match panel {
                Panel::Settings => "/users/@me/settings",
                Panel::Sessions => "/auth/sessions",
                Panel::Connections => "/users/@me/connections",
            };
            let value: serde_json::Value = rest.get(path).await?;
            return Ok(Some(Event::Panel(panel, value)));
        }
        Command::PatchSettings(body) => {
            rest.patch("/users/@me/settings", &body).await?;
            return Ok(None);
        }
        Command::Ring(channel_id) => {
            rest.post(&format!("/channels/{channel_id}/call/ring"), &json!({ "recipients": null })).await?;
            return Ok(None);
        }
        Command::Typing(_) if crate::builtin::enabled("silentTyping") => return Ok(None),
        Command::Typing(channel_id) => {
            rest.post(&format!("/channels/{channel_id}/typing"), &json!({})).await?;
            return Ok(None);
        }
        Command::Refresh { channel_id, id } => {
            let message: Message = rest.get(&format!("/channels/{channel_id}/messages/{id}")).await?;
            Event::MessageUpdate(message)
        }
        // Routed in `command_loop`.
        Command::FetchImage(_) | Command::FetchUsers { .. } | Command::SubscribeMembers { .. } | Command::JoinVoice { .. } | Command::LeaveVoice | Command::SetVoiceMute { .. } => {
            return Ok(None)
        }
    };
    Ok(Some(event))
}

fn mime_for(name: &str) -> &'static str {
    match name.rsplit('.').next().map(str::to_ascii_lowercase).as_deref() {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("mp4") => "video/mp4",
        Some("webm") => "video/webm",
        Some("mp3") => "audio/mpeg",
        Some("ogg") => "audio/ogg",
        Some("txt") => "text/plain",
        Some("pdf") => "application/pdf",
        _ => "application/octet-stream",
    }
}
