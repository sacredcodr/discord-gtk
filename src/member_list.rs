//! The right-hand member list, kept in sync from GUILD_MEMBER_LIST_UPDATE.
//!
//! Discord sends a window of the list (a "range") as indexed items, then edits
//! it with SYNC / INSERT / UPDATE / DELETE / INVALIDATE operations. The list
//! alternates group headers ("Online — 3") and members.
//!
//! IMPORTANT: this wire format is not in the public docs; it follows what the
//! gateway actually sends. Anything unrecognised is ignored, never fatal.

use serde::Deserialize;

use crate::model::{Snowflake, User};

#[derive(Deserialize, Clone, Debug, PartialEq)]
pub struct Group {
    pub id: String,
    #[serde(default)]
    pub count: u32,
}

#[derive(Deserialize, Clone, Debug, PartialEq)]
pub struct Presence {
    #[serde(default)]
    pub status: String,
}

#[derive(Deserialize, Clone, Debug, PartialEq)]
pub struct MemberItem {
    pub user: User,
    #[serde(default)]
    pub nick: Option<String>,
    #[serde(default)]
    pub roles: Vec<Snowflake>,
    #[serde(default)]
    pub presence: Option<Presence>,
}

impl MemberItem {
    pub fn name(&self) -> &str {
        self.nick.as_deref().unwrap_or_else(|| self.user.display())
    }

    pub fn status(&self) -> &str {
        self.presence.as_ref().map_or("offline", |p| p.status.as_str())
    }
}

#[derive(Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Item {
    Group(Group),
    Member(MemberItem),
}

#[derive(Deserialize, Debug)]
#[serde(tag = "op", rename_all = "UPPERCASE")]
enum Op {
    Sync { range: [usize; 2], items: Vec<Item> },
    Insert { index: usize, item: Item },
    Update { index: usize, item: Item },
    Delete { index: usize },
    Invalidate { range: [usize; 2] },
}

#[derive(Deserialize, Debug)]
pub struct Update {
    pub guild_id: Snowflake,
    #[serde(default)]
    pub online_count: u32,
    #[serde(default)]
    ops: Vec<Op>,
}

/// The list as far as it has been received. Holes (not yet synced ranges)
/// are `None` so indexes keep matching the server's.
#[derive(Default, Debug)]
pub struct MemberList {
    pub guild_id: Snowflake,
    pub online_count: u32,
    items: Vec<Option<Item>>,
}

impl MemberList {
    pub fn items(&self) -> impl Iterator<Item = &Item> {
        self.items.iter().flatten()
    }

    /// Applies an update; returns false if it belongs to another guild.
    pub fn apply(&mut self, update: Update) -> bool {
        if update.guild_id != self.guild_id {
            self.items.clear();
            self.guild_id = update.guild_id;
        }
        self.online_count = update.online_count;
        for op in update.ops {
            match op {
                Op::Sync { range: [start, end], items } => {
                    let end = (end + 1).max(start);
                    self.fill_to(end);
                    for slot in &mut self.items[start..end] {
                        *slot = None;
                    }
                    for (i, item) in items.into_iter().enumerate() {
                        if let Some(slot) = self.items.get_mut(start + i) {
                            *slot = Some(item);
                        }
                    }
                }
                Op::Insert { index, item } => {
                    self.fill_to(index);
                    self.items.insert(index, Some(item));
                }
                Op::Update { index, item } => {
                    self.fill_to(index + 1);
                    self.items[index] = Some(item);
                }
                Op::Delete { index } => {
                    if index < self.items.len() {
                        self.items.remove(index);
                    }
                }
                Op::Invalidate { range: [start, end] } => {
                    for slot in self.items.iter_mut().skip(start).take(end + 1 - start.min(end + 1)) {
                        *slot = None;
                    }
                }
            }
        }
        true
    }

    fn fill_to(&mut self, len: usize) {
        if self.items.len() < len {
            self.items.resize(len, None);
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn update(ops: serde_json::Value) -> Update {
        serde_json::from_value(json!({"guild_id": "1", "online_count": 2, "ops": ops})).unwrap()
    }

    fn member(id: &str) -> serde_json::Value {
        json!({"member": {"user": {"id": id, "username": format!("u{id}")}, "presence": {"status": "online"}}})
    }

    fn names(list: &MemberList) -> Vec<String> {
        list.items()
            .map(|i| match i {
                Item::Group(g) => format!("#{}", g.id),
                Item::Member(m) => m.name().to_owned(),
            })
            .collect()
    }

    #[test]
    fn sync_then_edit() {
        let mut list = MemberList::default();
        list.apply(update(json!([{"op": "SYNC", "range": [0, 2],
            "items": [{"group": {"id": "online", "count": 2}}, member("5"), member("6")]}])));
        assert_eq!(names(&list), ["#online", "u5", "u6"]);

        list.apply(update(json!([{"op": "INSERT", "index": 1, "item": member("7")}])));
        assert_eq!(names(&list), ["#online", "u7", "u5", "u6"]);
        list.apply(update(json!([{"op": "DELETE", "index": 2}, {"op": "UPDATE", "index": 1,
            "item": {"member": {"user": {"id": "7", "username": "u7"}, "nick": "Seven"}}}])));
        assert_eq!(names(&list), ["#online", "Seven", "u6"]);
    }

    #[test]
    fn other_guild_resets_the_list() {
        let mut list = MemberList::default();
        list.apply(update(json!([{"op": "SYNC", "range": [0, 0], "items": [member("5")]}])));
        let other: Update = serde_json::from_value(json!({"guild_id": "2", "ops": []})).unwrap();
        list.apply(other);
        assert!(names(&list).is_empty());
    }

    #[test]
    fn missing_presence_is_offline() {
        let m: MemberItem = serde_json::from_value(member("9")["member"].clone()).unwrap();
        assert_eq!(m.status(), "online");
        let bare: MemberItem = serde_json::from_value(json!({"user": {"id": "1", "username": "a"}})).unwrap();
        assert_eq!(bare.status(), "offline");
    }
}
