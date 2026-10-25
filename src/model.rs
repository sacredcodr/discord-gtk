//! Wire types for the subset of the Discord API this client uses, plus the
//! two message enums that cross the UI/backend thread boundary.
//!
//! Only fields the UI reads are declared. Serde skips everything else without
//! allocating, which keeps large payloads (user accounts receive very large
//! guild objects) cheap to decode.

use serde::Deserialize;

use crate::error::Error;

/// Snowflake ids are kept as strings: they exceed 2^53 and are only ever
/// compared for equality or interpolated into URLs.
pub type Snowflake = String;

#[derive(Deserialize, Clone, Debug, PartialEq)]
pub struct User {
    pub id: Snowflake,
    pub username: String,
    /// CDN hash of the custom avatar, absent when the user kept the default.
    #[serde(default)]
    pub avatar: Option<String>,
    /// Display name chosen by the user. Absent for accounts that never set one.
    #[serde(default)]
    pub global_name: Option<String>,
}

impl User {
    /// Name to show in the UI: the display name when set, the username otherwise.
    pub fn display(&self) -> &str {
        self.global_name.as_deref().unwrap_or(&self.username)
    }

    /// CDN URL of the avatar, 64px PNG. Users without one get Discord's
    /// default, picked from the id the way Discord does.
    /// Sharp enough for the 40 px avatars on a 2x screen.
    pub fn avatar_url(&self) -> String {
        self.avatar_url_sized(128)
    }

    /// `size` must be a power of two from 16 to 4096, as the CDN requires.
    pub fn avatar_url_sized(&self, size: u32) -> String {
        match &self.avatar {
            Some(hash) => format!("https://cdn.discordapp.com/avatars/{}/{hash}.png?size={size}", self.id),
            None => {
                let index = self.id.parse::<u64>().map_or(0, |id| (id >> 22) % 6);
                format!("https://cdn.discordapp.com/embed/avatars/{index}.png")
            }
        }
    }
}

#[derive(Deserialize, Clone, Debug)]
pub struct Channel {
    pub id: Snowflake,
    /// Discord channel type, see the associated constants for the ones handled.
    #[serde(rename = "type")]
    pub kind: u8,
    /// Absent for one-to-one DMs, optional for group DMs.
    #[serde(default)]
    pub name: Option<String>,
    /// Sort order inside the parent category (or the guild when uncategorised).
    /// Not unique: ties are legal and must be broken by the caller.
    #[serde(default)]
    pub position: i32,
    /// Category this channel sits in, if any.
    #[serde(default)]
    pub parent_id: Option<Snowflake>,
    /// Only present on DM and group DM channels.
    #[serde(default)]
    pub recipients: Vec<User>,
    /// Used to order DMs most-recent-first. Snowflakes are time-ordered, but
    /// only compare them as numbers (see `Channel::recency`), never as strings.
    #[serde(default)]
    pub last_message_id: Option<Snowflake>,
    /// Absent on DMs. READY leaves it out of a guild's own channel list, so
    /// `GuildData::adopt_channels` fills it in.
    #[serde(default)]
    pub guild_id: Option<Snowflake>,
    #[serde(default)]
    pub topic: Option<String>,
    #[serde(default)]
    pub permission_overwrites: Vec<Overwrite>,
    /// CDN hash of a group DM's icon.
    #[serde(default)]
    pub icon: Option<String>,
}

/// A per-channel permission override for one role or one member.
#[derive(Deserialize, Clone, Debug)]
pub struct Overwrite {
    /// Role id, or user id when `kind` is 1.
    pub id: Snowflake,
    #[serde(rename = "type")]
    pub kind: u8,
    /// Bit sets arrive as decimal strings because they exceed 2^53.
    pub allow: String,
    pub deny: String,
}

#[derive(Deserialize, Clone, Debug)]
pub struct Role {
    pub id: Snowflake,
    pub permissions: String,
    #[serde(default)]
    pub name: String,
    /// 0xRRGGBB, 0 when the role has no colour.
    #[serde(default)]
    pub color: u32,
    #[serde(default)]
    pub position: i32,
}

/// A guild member as far as this client needs: who, and which roles.
#[derive(Deserialize, Clone, Debug)]
pub struct Member {
    #[serde(default)]
    pub roles: Vec<Snowflake>,
    #[serde(default)]
    pub user: Option<UserId>,
}

#[derive(Deserialize, Clone, Debug)]
pub struct UserId {
    pub id: Snowflake,
}

/// A guild exactly as READY and GUILD_CREATE deliver it, with channels and
/// roles included. Unavailable guilds (bots, outages) carry only `id`.
#[derive(Deserialize, Clone, Debug)]
pub struct GuildData {
    pub id: Snowflake,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub icon: Option<String>,
    #[serde(default)]
    pub owner_id: Option<Snowflake>,
    #[serde(default)]
    pub channels: Vec<Channel>,
    #[serde(default)]
    pub roles: Vec<Role>,
    /// Users get only themselves here in large guilds, so it is searched for
    /// the logged-in user rather than treated as a member list.
    #[serde(default)]
    pub members: Vec<Member>,
    #[serde(default)]
    pub unavailable: bool,
    #[serde(default)]
    pub voice_states: Vec<VoiceState>,
    #[serde(default)]
    pub emojis: Vec<GuildEmoji>,
}

/// A custom emoji a guild owns.
#[derive(Deserialize, Clone, Debug)]
pub struct GuildEmoji {
    pub id: Snowflake,
    pub name: String,
    #[serde(default)]
    pub animated: bool,
    /// False for emoji locked behind a boost level the guild lost.
    #[serde(default = "yes")]
    pub available: bool,
}

fn yes() -> bool {
    true
}

impl GuildEmoji {
    pub fn image_url(&self) -> String {
        format!("https://cdn.discordapp.com/emojis/{}.png?size=48", self.id)
    }
}

impl GuildData {
    pub fn adopt_channels(&mut self) {
        for state in &mut self.voice_states {
            state.guild_id = Some(self.id.clone());
        }
        for channel in &mut self.channels {
            channel.guild_id = Some(self.id.clone());
        }
    }

    pub fn icon_url(&self) -> Option<String> {
        let hash = self.icon.as_ref()?;
        Some(format!("https://cdn.discordapp.com/icons/{}/{hash}.png?size=128", self.id))
    }
}

/// Who is in which voice channel, as READY and VOICE_STATE_UPDATE report it.
#[derive(Deserialize, Clone, Debug)]
pub struct VoiceState {
    #[serde(default)]
    pub guild_id: Option<Snowflake>,
    /// `None` means the user left voice.
    #[serde(default)]
    pub channel_id: Option<Snowflake>,
    pub user_id: Snowflake,
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub self_mute: bool,
    #[serde(default)]
    pub self_deaf: bool,
    #[serde(default)]
    pub mute: bool,
    #[serde(default)]
    pub deaf: bool,
    /// Present on updates; READY omits it, so names fall back to what is known.
    #[serde(default)]
    pub member: Option<VoiceMember>,
}

#[derive(Deserialize, Clone, Debug)]
pub struct VoiceMember {
    #[serde(default)]
    pub user: Option<User>,
}

#[derive(Deserialize, Clone, Debug)]
pub struct ReadState {
    pub id: Snowflake,
    #[serde(default)]
    pub last_message_id: Option<Snowflake>,
    #[serde(default)]
    pub mention_count: u32,
}

#[derive(Deserialize, Clone, Debug, Default)]
pub struct GuildSettings {
    #[serde(default)]
    pub guild_id: Option<Snowflake>,
    #[serde(default)]
    pub muted: bool,
    #[serde(default)]
    pub channel_overrides: Vec<ChannelOverride>,
}

#[derive(Deserialize, Clone, Debug)]
pub struct ChannelOverride {
    pub channel_id: Snowflake,
    #[serde(default)]
    pub muted: bool,
}

#[derive(Deserialize, Clone, Debug, Default)]
pub struct GuildFolder {
    #[serde(default)]
    pub guild_ids: Vec<Snowflake>,
}

#[derive(Deserialize, Clone, Debug, Default)]
pub struct UserSettings {
    #[serde(default)]
    pub guild_folders: Vec<GuildFolder>,
}

/// The parts of the READY payload the UI uses.
#[derive(Deserialize, Clone, Debug)]
pub struct ReadyData {
    pub user: User,
    #[serde(default)]
    pub guilds: Vec<GuildData>,
    #[serde(default)]
    pub private_channels: Vec<Channel>,
    #[serde(default)]
    pub read_state: Vec<ReadState>,
    #[serde(default)]
    pub user_guild_settings: Vec<GuildSettings>,
    #[serde(default)]
    pub user_settings: Option<UserSettings>,
    #[serde(default)]
    pub relationships: Vec<Relationship>,
    #[serde(default)]
    pub presences: Vec<Presence>,
}

impl Channel {
    pub const TEXT: u8 = 0;
    pub const DM: u8 = 1;
    pub const VOICE: u8 = 2;
    pub const GROUP_DM: u8 = 3;
    pub const CATEGORY: u8 = 4;
    pub const ANNOUNCEMENT: u8 = 5;
    pub const STAGE: u8 = 13;
    pub const FORUM: u8 = 15;

    /// Text to show in the channel list.
    pub fn label(&self) -> String {
        match self.kind {
            Self::DM | Self::GROUP_DM => match &self.name {
                Some(name) if !name.is_empty() => name.clone(),
                _ if self.recipients.is_empty() && self.kind == Self::DM => "Unknown user".to_owned(),
                // Everyone else left: Discord calls it an unnamed group.
                _ if self.recipients.is_empty() => "Unnamed group".to_owned(),
                _ => self
                    .recipients
                    .iter()
                    .map(User::display)
                    .collect::<Vec<_>>()
                    .join(", "),
            },
            _ => format!("# {}", self.name.as_deref().unwrap_or("unknown")),
        }
    }

    /// Sort key for "most recently active first". Channels that never had a
    /// message sort last.
    ///
    /// WAR: ids are compared numerically. Comparing the strings is wrong
    /// whenever two ids have a different number of digits.
    pub fn recency(&self) -> u64 {
        self.last_message_id
            .as_deref()
            .and_then(|id| id.parse().ok())
            .unwrap_or(0)
    }
}

#[derive(Deserialize, Clone, Debug)]
pub struct Attachment {
    pub filename: String,
    pub url: String,
    /// Resizable copy served by Discord's media proxy.
    #[serde(default)]
    pub proxy_url: Option<String>,
    #[serde(default)]
    pub content_type: Option<String>,
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
}

impl Attachment {
    /// True for raster images the UI can show inline. SVG is excluded: GTK
    /// cannot decode it from the proxy's PNG conversion path reliably.
    pub fn is_image(&self) -> bool {
        self.content_type
            .as_deref()
            .is_some_and(|t| t.starts_with("image/") && !t.contains("svg"))
            && self.width.is_some()
            && self.height.is_some()
    }
}

/// An image attached to an embed (link preview, tenor gif, ...).
#[derive(Deserialize, Clone, Debug)]
pub struct EmbedMedia {
    pub url: String,
    #[serde(default)]
    pub proxy_url: Option<String>,
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
}

#[derive(Deserialize, Clone, Debug)]
pub struct Embed {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub image: Option<EmbedMedia>,
    #[serde(default)]
    pub thumbnail: Option<EmbedMedia>,
}

impl Embed {
    /// The picture to show for this embed: the large image, else the thumbnail.
    pub fn media(&self) -> Option<&EmbedMedia> {
        self.image.as_ref().or(self.thumbnail.as_ref())
    }
}

#[derive(Deserialize, Clone, Debug)]
pub struct Message {
    pub id: Snowflake,
    pub channel_id: Snowflake,
    /// Empty for attachment-only messages. Requires the MESSAGE_CONTENT intent
    /// for bots; user tokens always receive it.
    #[serde(default)]
    pub content: String,
    pub author: User,
    /// ISO 8601 with offset, as sent by Discord.
    pub timestamp: String,
    #[serde(default)]
    pub attachments: Vec<Attachment>,
    #[serde(default)]
    pub embeds: Vec<Embed>,
    /// Users pinged by this message, used to turn `<@id>` into a name.
    #[serde(default)]
    pub mentions: Vec<User>,
    #[serde(default)]
    pub mention_everyone: bool,
    #[serde(default)]
    pub reactions: Vec<Reaction>,
    /// Set once the message has been edited.
    #[serde(default)]
    pub edited_timestamp: Option<String>,
    /// The message this one replies to. Absent when it was deleted.
    #[serde(default)]
    pub referenced_message: Option<Box<Message>>,
    /// Echoed back for messages we sent, so the local copy can be swapped for it.
    #[serde(default)]
    pub nonce: Option<String>,
    /// True for the local copy shown while a message is still being sent.
    #[serde(skip)]
    pub pending: bool,
    /// Deleted on Discord but kept on screen by the MessageLogger plugin.
    #[serde(skip)]
    pub deleted: bool,
}

/// How the logged-in user relates to another account.
#[derive(Deserialize, Clone, Debug)]
pub struct Relationship {
    pub id: Snowflake,
    /// 1 friend, 2 blocked, 3 incoming request, 4 outgoing request.
    #[serde(rename = "type")]
    pub kind: u8,
    #[serde(default)]
    pub user: Option<User>,
}

impl Relationship {
    pub const FRIEND: u8 = 1;
    pub const BLOCKED: u8 = 2;
    pub const INCOMING: u8 = 3;
    pub const OUTGOING: u8 = 4;
}

#[derive(Deserialize, Clone, Debug)]
pub struct PresenceUser {
    pub id: Snowflake,
}

/// What someone is doing: playing, listening, a custom status...
#[derive(Deserialize, Clone, Debug)]
pub struct Activity {
    pub name: String,
    #[serde(rename = "type", default)]
    pub kind: u8,
    #[serde(default)]
    pub state: Option<String>,
}

impl Activity {
    /// "Playing X", "Listening to X", or the custom status text.
    pub fn describe(&self) -> String {
        match self.kind {
            0 => format!("Playing {}", self.name),
            1 => format!("Streaming {}", self.name),
            2 => format!("Listening to {}", self.name),
            3 => format!("Watching {}", self.name),
            4 => self.state.clone().unwrap_or_default(),
            5 => format!("Competing in {}", self.name),
            _ => self.name.clone(),
        }
    }
}

#[derive(Deserialize, Clone, Debug)]
pub struct Presence {
    pub user: PresenceUser,
    #[serde(default = "offline")]
    pub status: String,
    #[serde(default)]
    pub activities: Vec<Activity>,
}

fn offline() -> String {
    "offline".into()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RelateAction {
    Add,
    Remove,
}

/// Account data the settings window shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Panel {
    Settings,
    Sessions,
    Connections,
}

#[derive(Deserialize, Clone, Debug)]
pub struct Emoji {
    /// Present for custom (server) emoji only.
    #[serde(default)]
    pub id: Option<Snowflake>,
    /// The unicode character, or the name of a custom emoji.
    #[serde(default)]
    pub name: Option<String>,
}

impl Emoji {
    /// How the REST API addresses this emoji in a URL path (percent-encoded).
    pub fn api_path(&self) -> String {
        match (&self.id, &self.name) {
            (Some(id), name) => format!("{}:{id}", name.as_deref().unwrap_or("_")),
            (None, Some(name)) => name
                .bytes()
                .map(|b| format!("%{b:02X}"))
                .collect(),
            (None, None) => String::new(),
        }
    }

    pub fn image_url(&self) -> Option<String> {
        let id = self.id.as_ref()?;
        Some(format!("https://cdn.discordapp.com/emojis/{id}.png?size=48"))
    }
}

#[derive(Deserialize, Clone, Debug)]
pub struct Reaction {
    pub emoji: Emoji,
    pub count: u32,
    /// True when the logged-in user has added this reaction.
    #[serde(default)]
    pub me: bool,
}

/// UI -> backend requests. Every command is answered by at most one `Event`
/// (or an `Event::Failed`); `Send` is answered by the gateway echo instead.
pub enum Command {
    LoadHistory(Snowflake),
    Send { channel_id: Snowflake, content: String, reply_to: Option<Snowflake>, nonce: String },
    /// Sends a local file as an attachment, with optional message text.
    /// Sends a message with files, the way Discord's own client does: reserve
    /// upload slots, PUT the bytes to storage, then post the message.
    SendFiles { channel_id: Snowflake, files: Vec<std::path::PathBuf>, content: String, reply_to: Option<Snowflake>, nonce: String },
    Edit { channel_id: Snowflake, id: Snowflake, content: String },
    Delete { channel_id: Snowflake, id: Snowflake },
    /// Adds or removes the user's own reaction.
    React { channel_id: Snowflake, id: Snowflake, emoji: Emoji, add: bool },
    /// Tells Discord the user is typing. Fire and forget.
    Typing(Snowflake),
    /// Marks everything up to `message_id` in a channel as read.
    Ack { channel_id: Snowflake, message_id: Snowflake },
    /// Joins a voice channel (or DM call when `guild_id` is `None`).
    JoinVoice { guild_id: Option<Snowflake>, channel_id: Snowflake },
    LeaveVoice,
    /// Changes the local mute/deafen state; also announced to Discord.
    SetVoiceMute { mute: bool, deaf: bool },
    /// Asks for a guild's member list for the given channel.
    SubscribeMembers { guild_id: Snowflake, channel_id: Snowflake },
    /// Loads a user's profile card (`guild_id` adds their server profile).
    FetchProfile { user_id: Snowflake, guild_id: Option<Snowflake> },
    /// Looks up accounts we only know by id (voice participants from READY).
    FetchUsers { guild_id: Snowflake, user_ids: Vec<Snowflake> },
    /// Opens (or creates) the direct message channel with a user.
    OpenDm(Snowflake),
    /// Friend actions: add, remove/decline/cancel, or block.
    Relate { user_id: Snowflake, action: RelateAction },
    /// Sends a friend request to `username`.
    AddFriend(String),
    /// Loads one of the account panels shown in settings.
    Fetch(Panel),
    /// Changes Discord-side user settings (status, custom status).
    PatchSettings(serde_json::Value),
    /// Rings the other people in a DM or group call.
    Ring(Snowflake),
    /// Re-downloads one message; answered with `Event::MessageUpdate`.
    Refresh { channel_id: Snowflake, id: Snowflake },
    /// Downloads an image for the UI. Answered by `Event::Image`, or by
    /// `Event::ImageFailed` so the UI can stop waiting.
    FetchImage(String),
}

/// Backend -> UI notifications, delivered on the GTK main thread.
pub enum Event {
    MemberList(Box<crate::member_list::Update>),
    Ready(Box<ReadyData>),
    GuildCreate(Box<GuildData>),
    GuildDelete(Snowflake),
    /// A channel was created or changed. Guild channels carry `guild_id`.
    ChannelUpsert(Channel),
    ChannelDelete(Channel),
    /// Another session of this account read a channel.
    MessageAck { channel_id: Snowflake, message_id: Snowflake, mention_count: Option<u32> },
    /// Oldest message first.
    History { channel_id: Snowflake, messages: Vec<Message> },
    MessageCreate(Message),
    MessageUpdate(Message),
    MessageDelete { channel_id: Snowflake, id: Snowflake },
    /// Something about the message changed that the gateway only hints at
    /// (reactions); the UI should `Command::Refresh` it if it is on screen.
    MessageStale { channel_id: Snowflake, id: Snowflake },
    Typing { channel_id: Snowflake, user_id: Snowflake },
    VoiceState(VoiceState),
    /// Voice server details for the join in progress. Consumed by the
    /// backend's voice manager and also forwarded to the UI.
    VoiceServer { token: String, guild_id: Option<Snowflake>, channel_id: Option<Snowflake>, endpoint: Option<String> },
    /// The voice connection came up (`Ok`) or ended (`Err(reason)`).
    VoiceLink(Result<(), String>),
    /// User ids currently transmitting audio, ourselves included.
    VoiceSpeaking(Vec<Snowflake>),
    /// Raw encoded bytes (PNG/JPEG) for a `Command::FetchImage`.
    Image { url: String, bytes: Vec<u8> },
    ImageFailed { url: String },
    Failed(Error),
    Presence(Presence),
    Users(Vec<User>),
    RelationshipUpdate(Relationship),
    RelationshipRemove(Snowflake),
    /// A user's profile as Discord sent it.
    Profile { user_id: Snowflake, value: serde_json::Value },
    /// A DM channel that was just opened or created.
    DmOpened(Channel),
    /// Data for a settings panel, as Discord sent it.
    Panel(Panel, serde_json::Value),
    /// A message could not be sent; its local copy (`nonce`) must go.
    SendFailed { nonce: Snowflake, error: Error },
}

#[cfg(test)]
mod tests {
    use super::Emoji;

    #[test]
    fn reaction_paths() {
        let unicode = Emoji { id: None, name: Some("👍".into()) };
        assert_eq!(unicode.api_path(), "%F0%9F%91%8D");
        let custom = Emoji { id: Some("123".into()), name: Some("party".into()) };
        assert_eq!(custom.api_path(), "party:123");
        assert_eq!(custom.image_url().as_deref(), Some("https://cdn.discordapp.com/emojis/123.png?size=48"));
    }
}
