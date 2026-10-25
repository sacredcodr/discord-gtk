//! Voice and call connections.
//!
//! Joining is a three-way handshake, driven by gateway events:
//!
//! 1. we send opcode 4 (voice state update) naming the channel
//! 2. Discord answers with VOICE_STATE_UPDATE (our own, carrying the session id)
//!    and VOICE_SERVER_UPDATE (a token and the media server's address)
//! 3. with both in hand we hand a `ConnectionInfo` to songbird's `Driver`, which
//!    runs the voice websocket, UDP/RTP, encryption (including DAVE end-to-end
//!    encryption) and Opus
//!
//! Audio comes back as decoded 20 ms ticks per speaker; we mix them to the
//! speakers (`audio`). The microphone goes the other way as a live input.
//!
//! IMPORTANT: lives on the backend thread; the UI hears about it only through
//! `Event::VoiceLink` and `Event::VoiceSpeaking`.

use std::{
    collections::{HashMap, HashSet},
    num::NonZeroU64,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

use async_channel::Sender;
use async_trait::async_trait;
use serde_json::{json, Value};
use songbird::{
    driver::{DecodeConfig, DecodeMode},
    id::{ChannelId, GuildId, UserId},
    input::{Input, RawAdapter},
    Config, ConnectionInfo, CoreEvent, Driver, Event as SbEvent, EventContext, EventHandler,
};

use crate::audio::{Audio, RATE};
use crate::model::{Event, Snowflake, VoiceState};

/// The slice of a gateway event the voice manager cares about. Cloneable, so
/// it can be handled on its own task without holding up the event stream.
pub enum Signal {
    Ready { user_id: Snowflake },
    State(VoiceState),
    Server { token: String, guild_id: Option<Snowflake>, channel_id: Option<Snowflake>, endpoint: Option<String> },
}

impl Signal {
    pub fn from_event(event: &Event) -> Option<Self> {
        Some(match event {
            Event::Ready(ready) => Self::Ready { user_id: ready.user.id.clone() },
            Event::VoiceState(state) => Self::State(state.clone()),
            Event::VoiceServer { token, guild_id, channel_id, endpoint } => Self::Server {
                token: token.clone(),
                guild_id: guild_id.clone(),
                channel_id: channel_id.clone(),
                endpoint: endpoint.clone(),
            },
            _ => return None,
        })
    }
}

/// Keep transmitting this long after the last loud sample so words do not end abruptly.
const HANG_TIME: Duration = Duration::from_millis(400);

#[derive(Clone)]
struct Target {
    /// `None` for DM and group calls.
    guild_id: Option<Snowflake>,
    channel_id: Snowflake,
}

/// Everything the audio-thread handlers share with the manager.
struct Shared {
    /// RTP source id -> user id, learned from speaking updates.
    ssrc: Mutex<HashMap<u32, Snowflake>>,
    /// Users currently heard, plus whether we are transmitting.
    speaking: Mutex<(HashSet<Snowflake>, bool, Vec<Snowflake>)>,
    deaf: AtomicBool,
}

pub struct Voice {
    outbound: Sender<Value>,
    events: Sender<Event>,
    me: Snowflake,
    target: Option<Target>,
    session_id: Option<String>,
    server: Option<(String, String)>,
    driver: Option<Driver>,
    audio: Option<Arc<Audio>>,
    shared: Arc<Shared>,
    self_mute: bool,
    self_deaf: bool,
    /// Last time the microphone was loud enough to transmit.
    last_voice: Option<Instant>,
    transmitting: bool,
}

impl Voice {
    pub fn new(outbound: Sender<Value>, events: Sender<Event>) -> Self {
        Self {
            outbound,
            events,
            me: String::new(),
            target: None,
            session_id: None,
            server: None,
            driver: None,
            audio: None,
            shared: Arc::new(Shared {
                ssrc: Mutex::default(),
                speaking: Mutex::default(),
                deaf: AtomicBool::new(false),
            }),
            self_mute: false,
            self_deaf: false,
            last_voice: None,
            transmitting: false,
        }
    }

    /// Starts joining a channel. Completion arrives as `Event::VoiceLink`.
    pub async fn join(&mut self, guild_id: Option<Snowflake>, channel_id: Snowflake) {
        tracing::debug!(?guild_id, %channel_id, "joining voice");
        self.leave().await;
        self.target = Some(Target { guild_id, channel_id });
        self.send_voice_state();
    }

    pub async fn leave(&mut self) {
        let had_target = self.target.take().is_some();
        self.session_id = None;
        self.server = None;
        self.transmitting = false;
        if let Some(mut driver) = self.driver.take() {
            driver.leave();
            driver.remove_all_global_events();
        }
        self.audio = None;
        self.shared.ssrc.lock().expect("ssrc map").clear();
        self.publish_speaking(true);
        if had_target {
            self.outbound.try_send(Self::voice_state_payload(None, None, self.self_mute, self.self_deaf)).ok();
        }
    }

    pub fn set_mute(&mut self, mute: bool, deaf: bool) {
        self.self_mute = mute;
        self.self_deaf = deaf;
        self.shared.deaf.store(deaf, Ordering::Relaxed);
        if self.target.is_some() {
            self.send_voice_state();
        }
    }

    fn voice_state_payload(guild_id: Option<&str>, channel_id: Option<&str>, mute: bool, deaf: bool) -> Value {
        json!({"op": 4, "d": {
            "guild_id": guild_id,
            "channel_id": channel_id,
            "self_mute": mute,
            "self_deaf": deaf,
            "self_video": false,
        }})
    }

    fn send_voice_state(&self) {
        let Some(target) = &self.target else { return };
        let payload = Self::voice_state_payload(
            target.guild_id.as_deref(),
            Some(&target.channel_id),
            self.self_mute,
            self.self_deaf,
        );
        let _ = self.outbound.try_send(payload);
    }

    /// Feeds every gateway event through; reacts to the voice-related ones.
    pub async fn observe(&mut self, signal: Signal) {
        match &signal {
            Signal::Ready { .. } => tracing::debug!("voice: ready"),
            Signal::State(s) => tracing::debug!(user = %s.user_id, channel = ?s.channel_id, mine = s.user_id == self.me, "voice: state update"),
            Signal::Server { guild_id, channel_id, endpoint, .. } => {
                tracing::debug!(?guild_id, ?channel_id, ?endpoint, "voice: server update")
            }
        }
        match signal {
            Signal::Ready { user_id } => {
                self.me = user_id;
                // A fresh session lost our voice state: say it again.
                if self.target.is_some() {
                    self.send_voice_state();
                }
            }
            Signal::State(state) if state.user_id == self.me => {
                let Some(target) = &self.target else { return };
                match state.channel_id.clone() {
                    Some(id) if id == target.channel_id => {
                        self.session_id = Some(state.session_id);
                        self.try_connect().await;
                    }
                    // Moved or kicked by someone else: our call is over.
                    other => {
                        if other.is_none() && self.session_id.is_some() {
                            self.leave().await;
                            let _ = self.events.send(Event::VoiceLink(Err("Disconnected from voice".into()))).await;
                        }
                    }
                }
            }
            Signal::Server { token, guild_id, channel_id, endpoint } => {
                let Some(target) = &self.target else { return };
                let matches = match &target.guild_id {
                    Some(guild) => guild_id.as_deref() == Some(guild),
                    None => channel_id.as_deref() == Some(&target.channel_id),
                };
                if !matches {
                    return;
                }
                match endpoint {
                    Some(endpoint) => {
                        self.server = Some((token, endpoint));
                        self.try_connect().await;
                    }
                    // The server went away and is being replaced; wait for the next update.
                    None => {
                        self.server = None;
                        if let Some(driver) = self.driver.as_mut() {
                            driver.leave();
                        }
                    }
                }
            }
            _ => {}
        }
    }

    async fn try_connect(&mut self) {
        let (Some(target), Some(session_id), Some((token, endpoint))) =
            (self.target.clone(), self.session_id.clone(), self.server.clone())
        else {
            return;
        };
        tracing::debug!("voice: have session and server, connecting");
        let Some(info) = self.connection_info(&target, session_id, token, endpoint) else {
            let _ = self.events.send(Event::VoiceLink(Err("Invalid voice server details".into()))).await;
            return;
        };

        let audio = match self.audio.clone() {
            Some(audio) => audio,
            None => match Audio::start() {
                Ok(audio) => {
                    let audio = Arc::new(audio);
                    self.audio = Some(audio.clone());
                    audio
                }
                Err(e) => {
                    let _ = self.events.send(Event::VoiceLink(Err(e))).await;
                    self.leave().await;
                    return;
                }
            },
        };

        let mut driver = self.driver.take().unwrap_or_else(|| {
            let config = Config::default().decode_mode(DecodeMode::Decode(DecodeConfig::default()));
            let mut driver = Driver::new(config);
            let handler = Handler {
                audio: audio.clone(),
                shared: self.shared.clone(),
                events: self.events.clone(),
                me: self.me.clone(),
            };
            driver.add_global_event(SbEvent::Core(CoreEvent::VoiceTick), handler.clone());
            driver.add_global_event(SbEvent::Core(CoreEvent::SpeakingStateUpdate), handler.clone());
            driver.add_global_event(SbEvent::Core(CoreEvent::DriverDisconnect), handler);
            if let Some(mic) = audio.take_mic() {
                driver.play_input(Input::from(RawAdapter::new(mic, RATE, 2)));
            }
            driver
        });

        let result = driver.connect(info).await;
        tracing::debug!(ok = result.is_ok(), "voice: driver connect finished");
        self.driver = Some(driver);
        let event = match result {
            Ok(()) => Event::VoiceLink(Ok(())),
            Err(e) => Event::VoiceLink(Err(format!("Could not connect to voice: {e}"))),
        };
        let failed = matches!(event, Event::VoiceLink(Err(_)));
        let _ = self.events.send(event).await;
        if failed {
            self.leave().await;
        }
    }

    fn connection_info(
        &self,
        target: &Target,
        session_id: String,
        token: String,
        endpoint: String,
    ) -> Option<ConnectionInfo> {
        let id = |s: &str| s.parse::<u64>().ok().and_then(NonZeroU64::new);
        let channel = id(&target.channel_id)?;
        // Calls have no guild: Discord identifies them by channel id there.
        let guild = target.guild_id.as_deref().and_then(id).unwrap_or(channel);
        Some(ConnectionInfo {
            channel_id: ChannelId(channel),
            endpoint,
            guild_id: GuildId(guild),
            session_id,
            token,
            user_id: UserId(id(&self.me)?),
        })
    }

    /// Call every ~20 ms while connected: opens or closes our microphone.
    pub fn tick(&mut self) {
        let (Some(driver), Some(audio)) = (self.driver.as_mut(), self.audio.as_ref()) else { return };
        let now = Instant::now();
        if audio.mic_level() > crate::prefs::get().mic_threshold {
            self.last_voice = Some(now);
        }
        let talking = self.last_voice.is_some_and(|t| now.duration_since(t) < HANG_TIME);
        let transmit = talking && !self.self_mute && !self.self_deaf;
        if transmit != self.transmitting {
            self.transmitting = transmit;
            driver.mute(!transmit);
            self.shared.speaking.lock().expect("speaking").1 = transmit;
            self.publish_speaking(false);
        }
    }

    pub fn connected(&self) -> bool {
        self.driver.is_some()
    }

    /// Tells the UI who is speaking, but only when that changed.
    fn publish_speaking(&self, force_clear: bool) {
        publish(&self.shared, &self.events, &self.me, force_clear);
    }
}

fn publish(shared: &Shared, events: &Sender<Event>, me: &str, clear: bool) {
    let mut state = shared.speaking.lock().expect("speaking");
    if clear {
        state.0.clear();
        state.1 = false;
    }
    let mut now: Vec<Snowflake> = state.0.iter().cloned().collect();
    if state.1 {
        now.push(me.to_owned());
    }
    now.sort();
    if now != state.2 {
        state.2 = now.clone();
        let _ = events.try_send(Event::VoiceSpeaking(now));
    }
}

#[derive(Clone)]
struct Handler {
    audio: Arc<Audio>,
    shared: Arc<Shared>,
    events: Sender<Event>,
    me: Snowflake,
}

#[async_trait]
impl EventHandler for Handler {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<SbEvent> {
        match ctx {
            EventContext::SpeakingStateUpdate(update) => {
                if let Some(user) = update.user_id {
                    self.shared.ssrc.lock().expect("ssrc map").insert(update.ssrc, user.0.to_string());
                }
            }
            EventContext::VoiceTick(tick) => {
                let deaf = self.shared.deaf.load(Ordering::Relaxed);
                let mut mixed: Vec<i32> = Vec::new();
                let mut heard = HashSet::new();
                {
                    let ssrc = self.shared.ssrc.lock().expect("ssrc map");
                    for (id, data) in &tick.speaking {
                        if let Some(user) = ssrc.get(id) {
                            heard.insert(user.clone());
                        }
                        let Some(pcm) = &data.decoded_voice else { continue };
                        if mixed.len() < pcm.len() {
                            mixed.resize(pcm.len(), 0);
                        }
                        for (sum, sample) in mixed.iter_mut().zip(pcm) {
                            *sum += i32::from(*sample);
                        }
                    }
                }
                if !deaf && !mixed.is_empty() {
                    let samples: Vec<f32> =
                        mixed.iter().map(|s| (*s).clamp(-32768, 32767) as f32 / 32768.0 * crate::audio::gain(false)).collect();
                    self.audio.play(&samples);
                }
                self.shared.speaking.lock().expect("speaking").0 = heard;
                publish(&self.shared, &self.events, &self.me, false);
            }
            EventContext::DriverDisconnect(data) => {
                if data.reason.is_some() {
                    let _ = self
                        .events
                        .try_send(Event::VoiceLink(Err("The voice connection was lost".into())));
                }
            }
            _ => {}
        }
        None
    }
}
