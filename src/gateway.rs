//! Discord gateway client: a zstd-stream compressed websocket with resume.
//!
//! Only the events the UI consumes are decoded into typed structures. Every
//! other dispatch is skipped after reading its envelope, so a busy guild costs
//! one decompress and one envelope parse per event and no allocation.
//!
//! Connection life cycle, driven by `run`:
//!
//! 1. connect, wait for HELLO (op 10) and start the heartbeat
//! 2. IDENTIFY (op 2), or RESUME (op 6) if a previous session can be continued
//! 3. dispatch events until the socket drops or Discord asks us to reconnect
//! 4. back off, then go to 1

use std::time::Duration;

use async_channel::{Receiver, Sender};
use futures_util::{Sink, SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, value::RawValue, Value};
use tokio::time::{interval_at, Instant, Interval, MissedTickBehavior};
use zstd::stream::raw::{Decoder as ZstdDecoder, Operation};
use zstd::zstd_safe::{InBuffer, OutBuffer};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{protocol::CloseFrame, Message as Ws},
};

use crate::error::Error;
use crate::model::{Channel, Event, GuildData, Message, ReadyData, VoiceState};

const DEFAULT_URL: &str = "wss://gateway.discord.gg";
/// zstd-stream is what the official web client prefers: it costs less CPU than
/// zlib-stream for the same traffic, which matters for an always-on connection.
const QUERY: &str = "/?v=10&encoding=json&compress=zstd-stream";

/// GUILDS | GUILD_MESSAGES | DIRECT_MESSAGES | MESSAGE_CONTENT.
///
/// MESSAGE_CONTENT is a privileged intent: bots must enable it in the
/// developer portal or `Message::content` arrives empty. Sent for bot tokens
/// only; user tokens do not use intents.
const BOT_INTENTS: u32 = (1 << 0) | (1 << 9) | (1 << 12) | (1 << 15);

/// Longest pause between reconnect attempts, in seconds.
const MAX_BACKOFF_SECS: u64 = 30;

/// State that survives a dropped connection so the session can be resumed
/// instead of re-identified (which would replay the whole READY payload).
#[derive(Default)]
struct Session {
    id: Option<String>,
    /// Discord asks resumed connections to use a different host than the
    /// initial one; connecting to the default URL would be rejected.
    resume_url: Option<String>,
    /// Sequence number of the last dispatch, echoed in heartbeats and RESUME.
    seq: Option<u64>,
}

/// Envelope of every gateway frame. `d` stays as raw JSON until we know we
/// want the event.
#[derive(Deserialize)]
struct Frame<'a> {
    op: u8,
    s: Option<u64>,
    t: Option<&'a str>,
    #[serde(borrow)]
    d: Option<&'a RawValue>,
}

/// Why `connect` returned; decides what `run` does next.
enum Exit {
    /// Reconnect and try to continue the existing session.
    Resume,
    /// The session is gone: reconnect and identify from scratch.
    Reidentify,
    /// Retrying cannot help (bad token, bad intents). `run` reports and stops.
    Fatal(Error),
}

/// Runs the gateway until a fatal error or until the UI side is dropped.
///
/// `outbound` carries ready-made payloads from the rest of the app (voice
/// state updates); they are sent as they arrive while a connection is up.
pub async fn run(token: String, events: Sender<Event>, outbound: Receiver<Value>) {
    let mut session = Session::default();
    let mut backoff_secs = 1;
    loop {
        match connect(&token, &mut session, &events, &outbound, &mut backoff_secs).await {
            Exit::Fatal(error) => {
                let _ = events.send(Event::Failed(error)).await;
                return;
            }
            Exit::Reidentify => session = Session::default(),
            Exit::Resume => {}
        }
        if events.is_closed() {
            return;
        }
        tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
        backoff_secs = (backoff_secs * 2).min(MAX_BACKOFF_SECS);
    }
}

/// One connection, from handshake to disconnect. `backoff_secs` is reset to 1
/// once HELLO arrives, so a connection that was healthy never inherits the
/// penalty of earlier failures.
async fn connect(
    token: &str,
    session: &mut Session,
    events: &Sender<Event>,
    outbound: &Receiver<Value>,
    backoff_secs: &mut u64,
) -> Exit {
    let base = session.resume_url.as_deref().unwrap_or(DEFAULT_URL);
    let Ok((socket, _)) = connect_async(format!("{base}{QUERY}")).await else {
        return Exit::Resume;
    };
    let (mut sink, mut stream) = socket.split();

    // One decoder per connection: the compression context is shared by all
    // messages on it, so it must never be reset or shared across sockets.
    let Ok(mut decoder) = ZstdDecoder::new() else { return Exit::Reidentify };
    let mut inflated: Vec<u8> = Vec::with_capacity(64 * 1024);
    let mut heartbeat: Option<Interval> = None;
    let mut heartbeat_acked = true;

    loop {
        tokio::select! {
            incoming = stream.next() => {
                let bytes = match incoming {
                    Some(Ok(Ws::Binary(bytes))) => bytes,
                    Some(Ok(Ws::Close(frame))) => return exit_for_close(frame),
                    Some(Ok(_)) => continue,
                    _ => return Exit::Resume,
                };
                // Each websocket message is one complete payload, so it can be
                // decoded as soon as it arrives.
                if inflate(&mut decoder, &bytes, &mut inflated).is_err() {
                    // The stream state is out of sync; nothing on this socket is trustworthy.
                    return Exit::Reidentify;
                }

                let Ok(frame) = serde_json::from_slice::<Frame>(&inflated) else { continue };
                if let Some(seq) = frame.s {
                    session.seq = Some(seq);
                }
                match frame.op {
                    0 => dispatch(&frame, session, events).await,
                    // Discord may request an immediate heartbeat.
                    1 => {
                        if send(&mut sink, json!({"op": 1, "d": session.seq})).await.is_err() {
                            return Exit::Resume;
                        }
                    }
                    7 => return Exit::Resume,
                    9 => {
                        // INVALID_SESSION: `d` says whether the session can be resumed.
                        let resumable = frame.d.is_some_and(|d| d.get() == "true");
                        return if resumable { Exit::Resume } else { Exit::Reidentify };
                    }
                    10 => {
                        #[derive(Deserialize)]
                        struct Hello { heartbeat_interval: u64 }
                        let hello = frame
                            .d
                            .and_then(|d| serde_json::from_str::<Hello>(d.get()).ok());
                        let Some(hello) = hello else { return Exit::Resume };

                        // The first beat goes out after half an interval; Discord
                        // asks for a jittered start so clients do not all beat at once.
                        let period = Duration::from_millis(hello.heartbeat_interval);
                        let mut timer = interval_at(Instant::now() + period / 2, period);
                        timer.set_missed_tick_behavior(MissedTickBehavior::Delay);
                        heartbeat = Some(timer);
                        heartbeat_acked = true;
                        *backoff_secs = 1;

                        let reply = match (&session.id, session.seq) {
                            (Some(id), Some(seq)) => resume_payload(token, id, seq),
                            _ => identify_payload(token),
                        };
                        if send(&mut sink, reply).await.is_err() {
                            return Exit::Resume;
                        }
                    }
                    11 => heartbeat_acked = true,
                    _ => {}
                }
            }
            queued = outbound.recv() => {
                // Closed means the backend is shutting down; the heartbeat arm
                // keeps the loop alive until the socket ends.
                let Ok(payload) = queued else { continue };
                tracing::debug!(op = %payload["op"], "gateway: sending queued payload");
                if send(&mut sink, payload).await.is_err() {
                    return Exit::Resume;
                }
            }
            _ = next_heartbeat(&mut heartbeat) => {
                // No ACK for the previous beat means the connection is a zombie
                // (half-open TCP); reconnect rather than wait for the OS to notice.
                if !heartbeat_acked {
                    return Exit::Resume;
                }
                heartbeat_acked = false;
                if send(&mut sink, json!({"op": 1, "d": session.seq})).await.is_err() {
                    return Exit::Resume;
                }
            }
        }
    }
}

/// Resolves on each heartbeat tick, and never before HELLO has set the interval.
async fn next_heartbeat(heartbeat: &mut Option<Interval>) {
    match heartbeat {
        Some(timer) => {
            timer.tick().await;
        }
        None => std::future::pending().await,
    }
}

async fn send<S: Sink<Ws> + Unpin>(sink: &mut S, payload: Value) -> Result<(), ()> {
    sink.send(Ws::text(payload.to_string())).await.map_err(drop)
}

fn is_bot(token: &str) -> bool {
    token.starts_with("Bot ")
}

/// The gateway wants the bare token; the "Bot " prefix is a REST-only convention.
fn bare_token(token: &str) -> &str {
    token.strip_prefix("Bot ").unwrap_or(token)
}

fn identify_payload(token: &str) -> Value {
    let mut data = json!({
        "token": bare_token(token),
        "properties": {"os": "linux", "browser": "discord-gtk", "device": "discord-gtk"},
    });
    if is_bot(token) {
        data["intents"] = json!(BOT_INTENTS);
    }
    json!({"op": 2, "d": data})
}

fn resume_payload(token: &str, session_id: &str, seq: u64) -> Value {
    json!({
        "op": 6,
        "d": {"token": bare_token(token), "session_id": session_id, "seq": seq}
    })
}

fn exit_for_close(frame: Option<CloseFrame>) -> Exit {
    match frame.map(|f| u16::from(f.code)) {
        Some(code @ (4004 | 4010..=4014)) => Exit::Fatal(Error::GatewayRejected { code }),
        // 4007 invalid seq, 4009 session timed out: the old session is unusable.
        Some(4007 | 4009) => Exit::Reidentify,
        _ => Exit::Resume,
    }
}

/// Decodes one zstd-stream message into `out` (cleared first).
///
/// The decoder writes only into spare capacity and never grows `out`, so we
/// loop, reserving more room, until all input is consumed with space to spare.
fn inflate(
    decoder: &mut ZstdDecoder<'_>,
    input: &[u8],
    out: &mut Vec<u8>,
) -> std::io::Result<()> {
    out.clear();
    let mut source = InBuffer::around(input);
    loop {
        out.reserve(32 * 1024);
        let start = out.len();
        let mut sink = OutBuffer::around_pos(&mut *out, start);
        decoder.run(&mut source, &mut sink)?;
        let filled_capacity = sink.pos() == sink.capacity();
        if source.pos() >= input.len() && !filled_capacity {
            return Ok(());
        }
    }
}

/// Decodes the dispatches the UI cares about; ignores the rest.
async fn dispatch(frame: &Frame<'_>, session: &mut Session, events: &Sender<Event>) {
    let (Some(name), Some(data)) = (frame.t, frame.d) else { return };
    let data = data.get();
    let event = match name {
        "READY" => {
            #[derive(Deserialize)]
            struct Meta {
                session_id: String,
                resume_gateway_url: String,
            }
            let (Ok(meta), Ok(mut ready)) =
                (serde_json::from_str::<Meta>(data), serde_json::from_str::<ReadyData>(data))
            else {
                return;
            };
            session.id = Some(meta.session_id);
            session.resume_url = Some(meta.resume_gateway_url);
            ready.guilds.iter_mut().for_each(GuildData::adopt_channels);
            Event::Ready(Box::new(ready))
        }
        "GUILD_CREATE" => match serde_json::from_str::<GuildData>(data) {
            Ok(mut guild) => {
                guild.adopt_channels();
                Event::GuildCreate(Box::new(guild))
            }
            Err(_) => return,
        },
        "GUILD_DELETE" => {
            #[derive(Deserialize)]
            struct Gone {
                id: String,
            }
            match serde_json::from_str::<Gone>(data) {
                Ok(g) => Event::GuildDelete(g.id),
                Err(_) => return,
            }
        }
        "CHANNEL_CREATE" | "CHANNEL_UPDATE" => match serde_json::from_str::<Channel>(data) {
            Ok(channel) => Event::ChannelUpsert(channel),
            Err(_) => return,
        },
        "CHANNEL_DELETE" => match serde_json::from_str::<Channel>(data) {
            Ok(channel) => Event::ChannelDelete(channel),
            Err(_) => return,
        },
        "MESSAGE_ACK" => {
            #[derive(Deserialize)]
            struct Ack {
                channel_id: String,
                message_id: String,
                #[serde(default)]
                mention_count: Option<u32>,
            }
            match serde_json::from_str::<Ack>(data) {
                Ok(a) => Event::MessageAck {
                    channel_id: a.channel_id,
                    message_id: a.message_id,
                    mention_count: a.mention_count,
                },
                Err(_) => return,
            }
        }
        "MESSAGE_CREATE" => match serde_json::from_str::<Message>(data) {
            Ok(message) => Event::MessageCreate(message),
            Err(_) => return,
        },
        // Updates are partial (an embed unfurl carries no author). Those fail
        // to decode as a full `Message` and are intentionally dropped.
        "MESSAGE_UPDATE" => match serde_json::from_str::<Message>(data) {
            Ok(message) => Event::MessageUpdate(message),
            Err(_) => return,
        },
        "MESSAGE_DELETE" => {
            #[derive(Deserialize)]
            struct Deleted {
                id: String,
                channel_id: String,
            }
            match serde_json::from_str::<Deleted>(data) {
                Ok(d) => Event::MessageDelete { channel_id: d.channel_id, id: d.id },
                Err(_) => return,
            }
        }
        "MESSAGE_REACTION_ADD" | "MESSAGE_REACTION_REMOVE" | "MESSAGE_REACTION_REMOVE_ALL"
        | "MESSAGE_REACTION_REMOVE_EMOJI" => {
            #[derive(Deserialize)]
            struct Reacted {
                message_id: String,
                channel_id: String,
            }
            match serde_json::from_str::<Reacted>(data) {
                Ok(r) => Event::MessageStale { channel_id: r.channel_id, id: r.message_id },
                Err(_) => return,
            }
        }
        "VOICE_STATE_UPDATE" => match serde_json::from_str::<VoiceState>(data) {
            Ok(state) => Event::VoiceState(state),
            Err(e) => {
                tracing::debug!(%e, "unreadable voice state");
                return;
            }
        },
        "VOICE_SERVER_UPDATE" => {
            #[derive(Deserialize)]
            struct Server {
                token: String,
                #[serde(default)]
                guild_id: Option<String>,
                #[serde(default)]
                channel_id: Option<String>,
                #[serde(default)]
                endpoint: Option<String>,
            }
            match serde_json::from_str::<Server>(data) {
                Ok(s) => Event::VoiceServer {
                    token: s.token,
                    guild_id: s.guild_id,
                    channel_id: s.channel_id,
                    endpoint: s.endpoint,
                },
                Err(_) => return,
            }
        }
        "GUILD_MEMBERS_CHUNK" => {
            #[derive(Deserialize)]
            struct Chunk {
                #[serde(default)]
                members: Vec<crate::model::VoiceMember>,
            }
            match serde_json::from_str::<Chunk>(data) {
                Ok(chunk) => Event::Users(chunk.members.into_iter().filter_map(|m| m.user).collect()),
                Err(e) => {
                    tracing::debug!(%e, "unreadable member chunk");
                    return;
                }
            }
        }
        "PRESENCE_UPDATE" => match serde_json::from_str(data) {
            Ok(presence) => Event::Presence(presence),
            Err(_) => return,
        },
        "RELATIONSHIP_ADD" | "RELATIONSHIP_UPDATE" => match serde_json::from_str(data) {
            Ok(relationship) => Event::RelationshipUpdate(relationship),
            Err(_) => return,
        },
        "RELATIONSHIP_REMOVE" => {
            #[derive(Deserialize)]
            struct Gone {
                id: String,
            }
            match serde_json::from_str::<Gone>(data) {
                Ok(gone) => Event::RelationshipRemove(gone.id),
                Err(_) => return,
            }
        }
        "GUILD_MEMBER_LIST_UPDATE" => match serde_json::from_str::<crate::member_list::Update>(data) {
            Ok(update) => Event::MemberList(Box::new(update)),
            Err(e) => {
                tracing::debug!(error = %e, "unreadable member list update");
                return;
            }
        },
        "TYPING_START" => {
            #[derive(Deserialize)]
            struct Typing {
                channel_id: String,
                user_id: String,
            }
            match serde_json::from_str::<Typing>(data) {
                Ok(t) => Event::Typing { channel_id: t.channel_id, user_id: t.user_id },
                Err(_) => return,
            }
        }
        _ => return,
    };
    let _ = events.send(event).await;
}
