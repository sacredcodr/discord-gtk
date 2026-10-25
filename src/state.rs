//! Client-side view of the account: guilds, channels, permissions and unread
//! state, built from READY and kept current by gateway events.
//!
//! Pure data and logic, no GTK, so it is unit tested. Permission rules follow
//! Discord's documented algorithm (base permissions from @everyone and roles,
//! then channel overwrites for @everyone, roles and the member, in that order).

use std::collections::{HashMap, HashSet};

use crate::model::{Channel, GuildData, GuildEmoji, Presence, Relationship, Message, ReadyData, Snowflake, User, VoiceState};

pub const ADMINISTRATOR: u64 = 1 << 3;
pub const VIEW_CHANNEL: u64 = 1 << 10;
pub const SEND_MESSAGES: u64 = 1 << 11;
pub const CONNECT: u64 = 1 << 20;

fn bits(decimal: &str) -> u64 {
    decimal.parse().unwrap_or(0)
}

/// Snowflakes only ever compare as numbers.
fn id_value(id: &str) -> u64 {
    id.parse().unwrap_or(0)
}

pub struct GuildState {
    pub data: GuildData,
    /// Role ids of the logged-in user in this guild.
    my_roles: Vec<Snowflake>,
}

impl GuildState {
    fn new(data: GuildData, me: &str) -> Self {
        let my_roles = data
            .members
            .iter()
            .find(|m| m.user.as_ref().is_some_and(|u| u.id == me))
            .map(|m| m.roles.clone())
            .unwrap_or_default();
        Self { data, my_roles }
    }

    pub fn id(&self) -> &str {
        &self.data.id
    }

    fn base_permissions(&self, me: &str) -> u64 {
        if self.data.owner_id.as_deref() == Some(me) {
            return u64::MAX;
        }
        let mut permissions = self
            .data
            .roles
            .iter()
            .find(|r| r.id == self.data.id) // the @everyone role shares the guild id
            .map_or(0, |r| bits(&r.permissions));
        for role in self.data.roles.iter().filter(|r| self.my_roles.contains(&r.id)) {
            permissions |= bits(&role.permissions);
        }
        if permissions & ADMINISTRATOR != 0 { u64::MAX } else { permissions }
    }

    /// What the logged-in user may do in `channel`.
    pub fn permissions_in(&self, me: &str, channel: &Channel) -> u64 {
        let base = self.base_permissions(me);
        if base & ADMINISTRATOR != 0 {
            return u64::MAX;
        }
        let mut permissions = base;
        let overwrites = &channel.permission_overwrites;
        if let Some(everyone) = overwrites.iter().find(|o| o.id == self.data.id) {
            permissions &= !bits(&everyone.deny);
            permissions |= bits(&everyone.allow);
        }
        let (mut allow, mut deny) = (0, 0);
        for overwrite in overwrites.iter().filter(|o| o.kind == 0 && self.my_roles.contains(&o.id)) {
            allow |= bits(&overwrite.allow);
            deny |= bits(&overwrite.deny);
        }
        permissions &= !deny;
        permissions |= allow;
        if let Some(own) = overwrites.iter().find(|o| o.kind == 1 && o.id == me) {
            permissions &= !bits(&own.deny);
            permissions |= bits(&own.allow);
        }
        // Without VIEW_CHANNEL every other permission is moot.
        if permissions & VIEW_CHANNEL == 0 { 0 } else { permissions }
    }

    /// Channels in Discord's display order, hidden ones removed. Uncategorised
    /// channels come first, then each category with its children.
    pub fn entries(&self, me: &str) -> Vec<Entry> {
        let visible = |c: &Channel| self.permissions_in(me, c) & VIEW_CHANNEL != 0;
        let order = |c: &Channel| (is_voice(c), c.position, id_value(&c.id));
        let listable = |c: &Channel| {
            matches!(
                c.kind,
                Channel::TEXT | Channel::ANNOUNCEMENT | Channel::VOICE | Channel::STAGE | Channel::FORUM
            )
        };

        let mut loose: Vec<&Channel> = self
            .data
            .channels
            .iter()
            .filter(|c| listable(c) && c.parent_id.is_none() && visible(c))
            .collect();
        loose.sort_by_key(|c| order(c));

        let mut categories: Vec<&Channel> =
            self.data.channels.iter().filter(|c| c.kind == Channel::CATEGORY).collect();
        categories.sort_by_key(|c| (c.position, id_value(&c.id)));

        let mut out: Vec<Entry> = loose.into_iter().map(|c| Entry::Channel(c.clone())).collect();
        for category in categories {
            let mut children: Vec<&Channel> = self
                .data
                .channels
                .iter()
                .filter(|c| listable(c) && c.parent_id.as_deref() == Some(&category.id) && visible(c))
                .collect();
            if children.is_empty() {
                continue;
            }
            children.sort_by_key(|c| order(c));
            out.push(Entry::Category(category.clone()));
            out.extend(children.into_iter().map(|c| Entry::Channel(c.clone())));
        }
        out
    }
}

fn is_voice(c: &Channel) -> bool {
    matches!(c.kind, Channel::VOICE | Channel::STAGE)
}

pub enum Entry {
    Category(Channel),
    Channel(Channel),
}

/// How a channel should look in the list.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Unread {
    pub unread: bool,
    pub mentions: u32,
}

impl Unread {
    fn add(self, other: Unread) -> Unread {
        Unread { unread: self.unread || other.unread, mentions: self.mentions + other.mentions }
    }
}

/// Someone connected to a voice channel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VoiceEntry {
    pub user_id: Snowflake,
    pub name: String,
    pub avatar_url: Option<String>,
    pub muted: bool,
    pub deafened: bool,
}

#[derive(Default)]
pub struct State {
    pub me: Snowflake,
    /// In the order the user arranged them.
    pub guilds: Vec<GuildState>,
    pub dms: Vec<Channel>,
    /// channel id -> (last acknowledged message id, mention count)
    read: HashMap<Snowflake, (u64, u32)>,
    /// channel id -> newest message id seen
    latest: HashMap<Snowflake, u64>,
    muted_guilds: HashSet<Snowflake>,
    muted_channels: HashSet<Snowflake>,
    pub relationships: Vec<Relationship>,
    presences: HashMap<Snowflake, Presence>,
    /// voice channel id -> who is connected, in join order
    voice: HashMap<Snowflake, Vec<VoiceState>>,
    /// Everyone seen so far, for names and avatars of people without a message.
    users: HashMap<Snowflake, User>,
}

impl State {
    pub fn from_ready(ready: ReadyData) -> Self {
        let me = ready.user.id.clone();
        let mut state = State { me: me.clone(), ..Self::default() };
        state.remember_user(&ready.user);

        for entry in ready.read_state {
            let last = entry.last_message_id.as_deref().map_or(0, id_value);
            state.read.insert(entry.id, (last, entry.mention_count));
        }
        for settings in ready.user_guild_settings {
            match settings.guild_id {
                Some(id) if settings.muted => {
                    state.muted_guilds.insert(id);
                }
                _ => {}
            }
            state
                .muted_channels
                .extend(settings.channel_overrides.into_iter().filter(|o| o.muted).map(|o| o.channel_id));
        }

        let order: Vec<Snowflake> = ready
            .user_settings
            .map(|s| s.guild_folders.into_iter().flat_map(|f| f.guild_ids).collect())
            .unwrap_or_default();
        let mut guilds: Vec<GuildData> = ready.guilds;
        guilds.sort_by_key(|g| order.iter().position(|id| *id == g.id).unwrap_or(usize::MAX));
        for guild in guilds {
            state.add_guild(guild);
        }

        let voice_states: Vec<VoiceState> =
            state.guilds.iter().flat_map(|g| g.data.voice_states.clone()).collect();
        tracing::debug!(count = voice_states.len(), with_member = voice_states.iter().filter(|v| v.member.is_some()).count(), with_user = voice_states.iter().filter(|v| v.member.as_ref().is_some_and(|m| m.user.is_some())).count(), "voice states in READY");
        for voice_state in &voice_states {
            state.apply_voice_state(voice_state);
        }

        for relationship in ready.relationships {
            state.update_relationship(relationship);
        }
        for presence in ready.presences {
            state.set_presence(presence);
        }

        let mut dms = ready.private_channels;
        for dm in &dms {
            dm.recipients.iter().for_each(|u| state.remember_user(u));
            state.note_latest(dm);
        }
        dms.sort_by_key(|c| std::cmp::Reverse(c.recency()));
        state.dms = dms;
        state
    }

    /// Custom emoji whose name contains `query`, those of `preferred` first.
    /// Nitro lets you use emoji from every server you are in, so all count.
    pub fn emoji_matches(&self, query: &str, preferred: Option<&str>, limit: usize) -> Vec<GuildEmoji> {
        let query = query.to_lowercase();
        let mut found: Vec<(bool, bool, &GuildEmoji)> = self
            .guilds
            .iter()
            .flat_map(|g| g.data.emojis.iter().map(move |e| (Some(g.data.id.as_str()) == preferred, e)))
            .filter(|(_, e)| e.available && e.name.to_lowercase().contains(&query))
            .map(|(here, e)| (here, e.name.to_lowercase().starts_with(&query), e))
            .collect();
        // Own server first, then names that start with the query.
        found.sort_by_key(|(here, prefix, e)| (!here, !prefix, e.name.to_lowercase()));
        found.into_iter().take(limit).map(|(_, _, e)| e.clone()).collect()
    }

    pub fn update_relationship(&mut self, relationship: Relationship) {
        if let Some(user) = &relationship.user {
            self.remember_user(user);
        }
        match self.relationships.iter_mut().find(|r| r.id == relationship.id) {
            Some(existing) => {
                // A partial update (e.g. a new nickname) may omit the user.
                let user = relationship.user.clone().or_else(|| existing.user.clone());
                *existing = Relationship { user, ..relationship };
            }
            None => self.relationships.push(relationship),
        }
    }

    pub fn remove_relationship(&mut self, id: &str) {
        self.relationships.retain(|r| r.id != id);
    }

    pub fn relationship(&self, id: &str) -> Option<&Relationship> {
        self.relationships.iter().find(|r| r.id == id)
    }

    pub fn set_presence(&mut self, presence: Presence) {
        if presence.status == "offline" {
            self.presences.remove(&presence.user.id);
        } else {
            self.presences.insert(presence.user.id.clone(), presence);
        }
    }

    /// "online", "idle", "dnd" or "offline" (also for anyone never seen).
    pub fn status(&self, user_id: &str) -> &str {
        self.presences.get(user_id).map_or("offline", |p| match p.status.as_str() {
            "invisible" => "offline",
            other => other,
        })
    }

    /// The most interesting thing a user is doing, as text.
    pub fn activity_text(&self, user_id: &str) -> Option<String> {
        let presence = self.presences.get(user_id)?;
        presence
            .activities
            .iter()
            .map(crate::model::Activity::describe)
            .find(|text| !text.is_empty())
    }

    /// Friends (or another relationship kind) with their users, by name.
    pub fn related(&self, kind: u8) -> Vec<(&Relationship, &crate::model::User)> {
        let mut found: Vec<_> = self
            .relationships
            .iter()
            .filter(|r| r.kind == kind)
            .filter_map(|r| r.user.as_ref().or_else(|| self.users.get(&r.id)).map(|u| (r, u)))
            .collect();
        found.sort_by_key(|(_, u)| u.display().to_lowercase());
        found
    }

    pub fn remember_user(&mut self, user: &User) {
        self.users.insert(user.id.clone(), user.clone());
    }

    pub fn user(&self, id: &str) -> Option<&User> {
        self.users.get(id)
    }

    pub fn user_name(&self, id: &str) -> Option<&str> {
        self.users.get(id).map(User::display)
    }

    /// Applies a voice state update. A user is in at most one voice channel,
    /// so they are removed everywhere first.
    pub fn apply_voice_state(&mut self, update: &VoiceState) {
        if let Some(user) = update.member.as_ref().and_then(|m| m.user.as_ref()) {
            self.remember_user(user);
        }
        for members in self.voice.values_mut() {
            members.retain(|m| m.user_id != update.user_id);
        }
        self.voice.retain(|_, members| !members.is_empty());
        if let Some(channel) = &update.channel_id {
            self.voice.entry(channel.clone()).or_default().push(update.clone());
        }
    }

    /// Voice participants whose names we have not learned yet.
    pub fn unknown_voice_users(&self, channel_ids: &[&str]) -> Vec<Snowflake> {
        channel_ids
            .iter()
            .filter_map(|c| self.voice.get(*c))
            .flatten()
            .filter(|s| !self.users.contains_key(&s.user_id))
            .map(|s| s.user_id.clone())
            .collect()
    }

    /// Who is connected to a voice channel right now.
    pub fn voice_members(&self, channel_id: &str) -> Vec<VoiceEntry> {
        self.voice
            .get(channel_id)
            .into_iter()
            .flatten()
            .map(|s| {
                let user = self.users.get(&s.user_id);
                VoiceEntry {
                    user_id: s.user_id.clone(),
                    name: user.map_or_else(|| "Unknown user".to_owned(), |u| u.display().to_owned()),
                    avatar_url: user.map(User::avatar_url),
                    muted: s.self_mute || s.mute,
                    deafened: s.self_deaf || s.deaf,
                }
            })
            .collect()
    }

    fn note_latest(&mut self, channel: &Channel) {
        if let Some(id) = channel.last_message_id.as_deref() {
            let value = id_value(id);
            let entry = self.latest.entry(channel.id.clone()).or_insert(0);
            *entry = (*entry).max(value);
        }
    }

    pub fn add_guild(&mut self, data: GuildData) {
        if data.unavailable {
            return;
        }
        for channel in &data.channels {
            self.note_latest(channel);
        }
        let guild = GuildState::new(data, &self.me);
        match self.guilds.iter_mut().find(|g| g.id() == guild.id()) {
            Some(existing) => *existing = guild,
            None => self.guilds.push(guild),
        }
    }

    pub fn remove_guild(&mut self, id: &str) {
        self.guilds.retain(|g| g.id() != id);
    }

    pub fn guild(&self, id: &str) -> Option<&GuildState> {
        self.guilds.iter().find(|g| g.id() == id)
    }

    pub fn upsert_channel(&mut self, channel: Channel) {
        self.note_latest(&channel);
        match channel.guild_id.clone() {
            Some(guild_id) => {
                let Some(guild) = self.guilds.iter_mut().find(|g| g.id() == guild_id) else { return };
                match guild.data.channels.iter_mut().find(|c| c.id == channel.id) {
                    Some(existing) => *existing = channel,
                    None => guild.data.channels.push(channel),
                }
            }
            None => {
                match self.dms.iter_mut().find(|c| c.id == channel.id) {
                    Some(existing) => *existing = channel,
                    None => self.dms.push(channel),
                }
                self.dms.sort_by_key(|c| std::cmp::Reverse(self.latest.get(&c.id).copied().unwrap_or(0)));
            }
        }
    }

    pub fn delete_channel(&mut self, channel: &Channel) {
        match &channel.guild_id {
            Some(guild_id) => {
                if let Some(g) = self.guilds.iter_mut().find(|g| g.id() == guild_id) {
                    g.data.channels.retain(|c| c.id != channel.id);
                }
            }
            None => self.dms.retain(|c| c.id != channel.id),
        }
    }

    /// The channel with this id, wherever it lives, with its guild if any.
    pub fn channel(&self, id: &str) -> Option<(&Channel, Option<&GuildState>)> {
        if let Some(dm) = self.dms.iter().find(|c| c.id == id) {
            return Some((dm, None));
        }
        self.guilds
            .iter()
            .find_map(|g| g.data.channels.iter().find(|c| c.id == id).map(|c| (c, Some(g))))
    }

    pub fn permissions_in(&self, channel_id: &str) -> u64 {
        match self.channel(channel_id) {
            Some((channel, Some(guild))) => guild.permissions_in(&self.me, channel),
            // DMs: everything chat-related is allowed.
            Some((_, None)) => u64::MAX,
            None => 0,
        }
    }

    fn channel_unread(&self, channel: &Channel) -> Unread {
        let latest = self.latest.get(&channel.id).copied().unwrap_or(0);
        let (read, mentions) = self.read.get(&channel.id).copied().unwrap_or((u64::MAX, 0));
        let muted = self.muted_channels.contains(&channel.id);
        Unread { unread: latest > read && !muted, mentions }
    }

    pub fn unread(&self, channel_id: &str) -> Unread {
        self.channel(channel_id).map(|(c, _)| self.channel_unread(c)).unwrap_or_default()
    }

    pub fn is_muted(&self, channel_id: &str) -> bool {
        self.muted_channels.contains(channel_id)
    }

    /// Unread summary of a whole guild, counting only channels the user can see.
    pub fn guild_unread(&self, guild_id: &str) -> Unread {
        if self.muted_guilds.contains(guild_id) {
            return Unread::default();
        }
        let Some(guild) = self.guild(guild_id) else { return Unread::default() };
        guild
            .data
            .channels
            .iter()
            .filter(|c| !is_voice(c) && c.kind != Channel::CATEGORY)
            .filter(|c| guild.permissions_in(&self.me, c) & VIEW_CHANNEL != 0)
            .map(|c| self.channel_unread(c))
            .fold(Unread::default(), Unread::add)
    }

    pub fn dm_unread(&self) -> Unread {
        self.dms.iter().map(|c| self.channel_unread(c)).fold(Unread::default(), Unread::add)
    }

    /// Records a new message. `viewing` is true when the user is looking at
    /// that channel right now, in which case it counts as read.
    pub fn on_message(&mut self, m: &Message, viewing: bool) {
        let id = id_value(&m.id);
        let entry = self.latest.entry(m.channel_id.clone()).or_insert(0);
        *entry = (*entry).max(id);
        if let Some(dm) = self.dms.iter_mut().find(|c| c.id == m.channel_id) {
            dm.last_message_id = Some(m.id.clone());
            self.dms.sort_by_key(|c| std::cmp::Reverse(self.latest.get(&c.id).copied().unwrap_or(0)));
        }
        if viewing {
            return;
        }
        let pinged = m.mention_everyone || m.mentions.iter().any(|u| u.id == self.me);
        if pinged && m.author.id != self.me {
            let read = self.read.entry(m.channel_id.clone()).or_insert((u64::MAX, 0));
            read.1 += 1;
        }
    }

    /// Marks a channel read up to `message_id` (this client or another session).
    pub fn ack(&mut self, channel_id: &str, message_id: &str, mentions: Option<u32>) {
        let entry = self.read.entry(channel_id.to_owned()).or_insert((0, 0));
        let id = id_value(message_id);
        if entry.0 == u64::MAX || id > entry.0 {
            entry.0 = id;
        }
        entry.1 = mentions.unwrap_or(0);
    }

    /// Id of the newest message known in a channel, for acknowledging it.
    pub fn newest_message(&self, channel_id: &str) -> Option<String> {
        self.latest.get(channel_id).filter(|v| **v > 0).map(u64::to_string)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const ME: &str = "100";

    fn ready(value: serde_json::Value) -> State {
        let mut ready: ReadyData = serde_json::from_value(value).unwrap();
        ready.guilds.iter_mut().for_each(GuildData::adopt_channels);
        State::from_ready(ready)
    }

    fn base(guild: serde_json::Value) -> State {
        ready(json!({
            "user": {"id": ME, "username": "me"},
            "guilds": [guild],
            "read_state": [
                {"id": "11", "last_message_id": "500", "mention_count": 2},
                {"id": "13", "last_message_id": "1"},
            ],
            "user_guild_settings": [{"guild_id": "1", "muted": false,
                "channel_overrides": [{"channel_id": "13", "muted": true}]}],
        }))
    }

    fn guild() -> serde_json::Value {
        json!({
            "id": "1", "name": "G", "owner_id": "999",
            "roles": [
                {"id": "1", "permissions": "1024"},           // @everyone: VIEW_CHANNEL
                {"id": "7", "permissions": "2048"},           // role 7: SEND_MESSAGES
            ],
            "members": [{"roles": ["7"], "user": {"id": ME}}],
            "channels": [
                {"id": "10", "type": 4, "name": "Cat", "position": 1},
                {"id": "11", "type": 0, "name": "general", "parent_id": "10", "position": 0, "last_message_id": "600"},
                {"id": "12", "type": 0, "name": "secret", "parent_id": "10", "position": 1,
                 "permission_overwrites": [{"id": "1", "type": 0, "allow": "0", "deny": "1024"}]},
                {"id": "13", "type": 0, "name": "quiet", "parent_id": "10", "position": 2, "last_message_id": "900"},
                {"id": "14", "type": 2, "name": "voice", "parent_id": "10", "position": 0},
                {"id": "15", "type": 0, "name": "top", "position": 5},
                {"id": "16", "type": 4, "name": "EmptyCat", "position": 2},
                {"id": "17", "type": 0, "name": "readonly", "parent_id": "16", "position": 0,
                 "permission_overwrites": [{"id": "7", "type": 0, "allow": "0", "deny": "2048"}]},
            ],
        })
    }

    fn names(state: &State) -> Vec<String> {
        state.guild("1").unwrap().entries(ME).iter().map(|e| match e {
            Entry::Category(c) => format!("[{}]", c.name.clone().unwrap()),
            Entry::Channel(c) => c.name.clone().unwrap(),
        }).collect()
    }

    #[test]
    fn hides_denied_channels_and_orders_like_discord() {
        let state = base(guild());
        // "secret" is hidden, loose channels come first, text before voice.
        assert_eq!(names(&state), ["top", "[Cat]", "general", "quiet", "voice", "[EmptyCat]", "readonly"]);
    }

    #[test]
    fn send_permission_comes_from_roles_and_overwrites() {
        let state = base(guild());
        assert!(state.permissions_in("11") & SEND_MESSAGES != 0, "role 7 grants sending");
        assert!(state.permissions_in("17") & SEND_MESSAGES == 0, "overwrite denies the role");
        assert!(state.permissions_in("17") & VIEW_CHANNEL != 0);
        assert_eq!(state.permissions_in("12"), 0, "no view means no permissions at all");
    }

    #[test]
    fn owner_and_admin_see_everything() {
        let mut g = guild();
        g["owner_id"] = json!(ME);
        assert!(names(&base(g)).contains(&"secret".to_owned()));
        let mut g = guild();
        g["roles"][1]["permissions"] = json!((ADMINISTRATOR).to_string());
        assert_eq!(base(g).permissions_in("12"), u64::MAX);
    }

    #[test]
    fn unread_and_mentions() {
        let mut state = base(guild());
        assert!(state.unread("11").unread, "latest 600 is past the read mark 500");
        assert_eq!(state.unread("11").mentions, 2);
        assert!(!state.unread("13").unread, "muted channel does not light up");
        assert!(!state.unread("15").unread, "no read state means nothing to catch up on");
        assert_eq!(state.guild_unread("1").mentions, 2);

        state.ack("11", "600", Some(0));
        assert_eq!(state.unread("11"), Unread::default());
        assert!(!state.guild_unread("1").unread);

        let m: Message = serde_json::from_value(json!({
            "id": "700", "channel_id": "11", "content": "hey @me",
            "author": {"id": "5", "username": "x"}, "timestamp": "2026-01-01T00:00:00+00:00",
            "mentions": [{"id": ME, "username": "me"}],
        })).unwrap();
        state.on_message(&m, false);
        assert!(state.unread("11").unread);
        assert_eq!(state.unread("11").mentions, 1);
        state.on_message(&m, true);
        assert_eq!(state.unread("11").mentions, 1, "viewing does not add pings");
    }

    #[test]
    fn presence_and_relationships() {
        let mut state = ready(json!({
            "user": {"id": ME, "username": "me"},
            "relationships": [
                {"id": "20", "type": 1, "user": {"id": "20", "username": "bob", "global_name": "Bob"}},
                {"id": "21", "type": 1, "user": {"id": "21", "username": "al"}},
                {"id": "22", "type": 3, "user": {"id": "22", "username": "cy"}},
            ],
            "presences": [
                {"user": {"id": "20"}, "status": "online", "activities": [{"name": "Chess", "type": 0}]},
                {"user": {"id": "21"}, "status": "idle"},
            ],
        }));
        assert_eq!(state.status("20"), "online");
        assert_eq!(state.status("99"), "offline");
        assert_eq!(state.activity_text("20").as_deref(), Some("Playing Chess"));
        let friends: Vec<_> = state.related(1).into_iter().map(|(_, u)| u.display().to_owned()).collect();
        assert_eq!(friends, ["al", "Bob"]);
        assert_eq!(state.related(3).len(), 1);

        let offline: Presence = serde_json::from_value(json!({"user": {"id": "20"}, "status": "offline"})).unwrap();
        state.set_presence(offline);
        assert_eq!(state.status("20"), "offline");
        state.remove_relationship("21");
        assert_eq!(state.related(1).len(), 1);
    }

    #[test]
    fn emoji_search_prefers_prefix_and_skips_unavailable() {
        let mut g = guild();
        g["emojis"] = json!([
            {"id": "1", "name": "sad_pepe"},
            {"id": "2", "name": "pepe"},
            {"id": "3", "name": "pepelocked", "available": false},
            {"id": "4", "name": "cat"},
        ]);
        let state = base(g);
        let names: Vec<String> = state.emoji_matches("PEPE", None, 10).into_iter().map(|e| e.name).collect();
        assert_eq!(names, ["pepe", "sad_pepe"]);
        assert_eq!(state.emoji_matches("pepe", None, 1).len(), 1);
        assert!(state.emoji_matches("dog", None, 10).is_empty());
    }

    #[test]
    fn voice_roster_follows_updates() {
        let mut state = base(guild());
        let update = |user: &str, channel: Option<&str>| -> VoiceState {
            serde_json::from_value(json!({
                "user_id": user, "channel_id": channel, "session_id": "s", "self_mute": user == "9",
                "member": {"user": {"id": user, "username": format!("u{user}")}},
            })).unwrap()
        };
        state.apply_voice_state(&update("9", Some("14")));
        state.apply_voice_state(&update("8", Some("14")));
        let members = state.voice_members("14");
        assert_eq!(members.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(), ["u9", "u8"]);
        assert!(members[0].muted && !members[1].muted);

        state.apply_voice_state(&update("9", Some("20"))); // moved
        assert_eq!(state.voice_members("14").len(), 1);
        assert_eq!(state.voice_members("20").len(), 1);
        state.apply_voice_state(&update("9", None)); // left
        assert!(state.voice_members("20").is_empty());
    }

    #[test]
    fn guild_order_follows_folders() {
        let state = ready(json!({
            "user": {"id": ME, "username": "me"},
            "guilds": [{"id": "1", "name": "a"}, {"id": "2", "name": "b"}, {"id": "3", "name": "c"}],
            "user_settings": {"guild_folders": [{"guild_ids": ["3"]}, {"guild_ids": ["1"]}]},
        }));
        let ids: Vec<&str> = state.guilds.iter().map(|g| g.id()).collect();
        assert_eq!(ids, ["3", "1", "2"]);
    }
}
