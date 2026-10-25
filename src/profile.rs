//! The profile card: what Discord shows when you click someone's name.
//!
//! It opens at once with what we already know (name, avatar, status), then
//! fills in the rest (banner, bio, roles, mutual servers) when Discord's
//! profile reply arrives.

use std::{collections::HashMap, rc::Rc};

use adw::prelude::*;
use gtk::{cairo, glib, pango};
use serde_json::Value;

use crate::{
    icons,
    images::Images,
    model::{Command, RelateAction, Relationship, Role, User},
};

/// Everything known about the person before the profile arrives.
pub struct Subject {
    pub user: User,
    pub status: String,
    pub activity: Option<String>,
    /// How the logged-in user relates to them (`Relationship::FRIEND` ...).
    pub relationship: Option<u8>,
    pub is_me: bool,
    pub guild_id: Option<String>,
    /// Roles of the guild the card was opened from, for naming a member's roles.
    pub roles: Vec<Role>,
    /// Guild id -> name, for listing mutual servers.
    pub guild_names: HashMap<String, String>,
}

pub struct Card {
    banner_color: std::cell::Cell<(f64, f64, f64)>,
    banner_area: gtk::DrawingArea,
    banner_image: gtk::Picture,
    user_id: String,
    bio: gtk::Label,
    bio_section: gtk::Box,
    since: gtk::Label,
    roles: gtk::FlowBox,
    roles_section: gtk::Box,
    mutual: gtk::Label,
    mutual_section: gtk::Box,
    accounts: gtk::Box,
    pronouns: gtk::Label,
    subject: Subject,
    images: Rc<Images>,
}

fn heading(text: &str) -> gtk::Label {
    let label = gtk::Label::builder().label(text).xalign(0.0).build();
    label.add_css_class("profile-heading");
    label
}

fn section(title: &str, body: &impl IsA<gtk::Widget>) -> gtk::Box {
    let section = gtk::Box::new(gtk::Orientation::Vertical, 4);
    section.append(&heading(title));
    section.append(body);
    section.set_visible(false);
    section
}

/// When an account was created, from the time bits of its id.
pub fn created(id: &str) -> Option<String> {
    const DISCORD_EPOCH_MS: u64 = 1_420_070_400_000;
    let ms = (id.parse::<u64>().ok()? >> 22) + DISCORD_EPOCH_MS;
    let when = glib::DateTime::from_unix_utc((ms / 1000) as i64).ok()?.to_local().ok()?;
    Some(when.format("%b %e, %Y").ok()?.trim().to_owned())
}

fn paint_color(rgb: u32) -> (f64, f64, f64) {
    (
        f64::from((rgb >> 16) & 0xff) / 255.0,
        f64::from((rgb >> 8) & 0xff) / 255.0,
        f64::from(rgb & 0xff) / 255.0,
    )
}

/// Opens a profile card next to `anchor` and asks for the full profile.
pub fn show(
    anchor: &gtk::Widget,
    subject: Subject,
    images: Rc<Images>,
    send: Rc<dyn Fn(Command)>,
) -> Rc<Card> {
    let popover = gtk::Popover::new();
    popover.set_parent(anchor);
    popover.add_css_class("profile-card");
    popover.connect_closed(|p| {
        let p = p.clone();
        glib::idle_add_local_once(move || p.unparent());
    });

    // Banner with the avatar hanging over its lower edge.
    let banner_area = gtk::DrawingArea::new();
    banner_area.set_content_width(340);
    banner_area.set_content_height(96);
    let banner_image = gtk::Picture::new();
    banner_image.set_content_fit(gtk::ContentFit::Cover);
    banner_image.set_size_request(340, 96);
    banner_image.set_can_shrink(true);
    let banner = gtk::Overlay::new();
    banner.set_child(Some(&banner_area));
    banner.add_overlay(&banner_image);
    banner.set_overflow(gtk::Overflow::Hidden);

    let avatar = adw::Avatar::new(80, Some(subject.user.display()), true);
    let url = subject.user.avatar_url_sized(256);
    avatar.set_widget_name(&url);
    let (shown, wanted) = (avatar.clone(), url.clone());
    images.get(&url, move |texture| {
        if shown.widget_name() == wanted {
            shown.set_custom_image(texture.as_ref());
        }
    });
    let dot = crate::status::dot(&subject.status);
    let avatar_stack = gtk::Overlay::new();
    avatar_stack.set_child(Some(&avatar));
    avatar_stack.add_overlay(&dot);
    avatar_stack.set_halign(gtk::Align::Start);
    avatar_stack.set_margin_start(16);
    avatar_stack.set_margin_top(52);
    avatar_stack.add_css_class("profile-avatar");
    let top = gtk::Overlay::new();
    top.set_child(Some(&banner));
    top.add_overlay(&avatar_stack);
    top.set_size_request(-1, 96 + 40);
    banner.set_valign(gtk::Align::Start);

    let name = gtk::Label::builder().label(subject.user.display()).xalign(0.0).wrap(true).selectable(true).build();
    name.add_css_class("profile-name");
    let tag = gtk::Label::builder().label(&subject.user.username).xalign(0.0).selectable(true).build();
    tag.add_css_class("profile-tag");
    let pronouns = gtk::Label::builder().xalign(0.0).visible(false).build();
    pronouns.add_css_class("profile-tag");
    let identity = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    identity.append(&tag);
    identity.append(&pronouns);
    let activity = gtk::Label::builder()
        .label(subject.activity.clone().unwrap_or_default())
        .xalign(0.0)
        .wrap(true)
        .visible(subject.activity.is_some())
        .build();
    activity.add_css_class("profile-activity");

    let bio = gtk::Label::builder().xalign(0.0).wrap(true).wrap_mode(pango::WrapMode::WordChar).selectable(true).build();
    let bio_section = section("ABOUT ME", &bio);
    let since = gtk::Label::builder().xalign(0.0).build();
    let since_section = section("MEMBER SINCE", &since);
    since.set_label(&created(&subject.user.id).unwrap_or_default());
    since_section.set_visible(since.label() != "");
    let roles = gtk::FlowBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .max_children_per_line(6)
        .column_spacing(4)
        .row_spacing(4)
        .build();
    let roles_section = section("ROLES", &roles);
    let mutual = gtk::Label::builder().xalign(0.0).wrap(true).build();
    let mutual_section = section("MUTUAL SERVERS", &mutual);
    let accounts = gtk::Box::new(gtk::Orientation::Vertical, 2);
    let accounts_section = section("CONNECTIONS", &accounts);

    let body = gtk::Box::new(gtk::Orientation::Vertical, 8);
    body.set_margin_start(16);
    body.set_margin_end(16);
    body.set_margin_bottom(16);
    body.append(&name);
    body.append(&identity);
    body.append(&activity);
    body.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    body.append(&bio_section);
    body.append(&since_section);
    body.append(&roles_section);
    body.append(&mutual_section);
    body.append(&accounts_section);

    // Actions.
    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    if !subject.is_me {
        let message = gtk::Button::with_label("Message");
        message.add_css_class("suggested-action");
        let (send_dm, id, pop) = (send.clone(), subject.user.id.clone(), popover.clone());
        message.connect_clicked(move |_| {
            send_dm(Command::OpenDm(id.clone()));
            pop.popdown();
        });
        actions.append(&message);

        let (label, action) = match subject.relationship {
            Some(Relationship::FRIEND) => ("Remove Friend", RelateAction::Remove),
            Some(Relationship::INCOMING) => ("Accept", RelateAction::Add),
            Some(Relationship::OUTGOING) => ("Cancel Request", RelateAction::Remove),
            Some(Relationship::BLOCKED) => ("Unblock", RelateAction::Remove),
            _ => ("Add Friend", RelateAction::Add),
        };
        let friend = gtk::Button::with_label(label);
        let (send_rel, id, pop) = (send.clone(), subject.user.id.clone(), popover.clone());
        friend.connect_clicked(move |_| {
            send_rel(Command::Relate { user_id: id.clone(), action });
            pop.popdown();
        });
        actions.append(&friend);
    }
    let copy = icons::button("copy", "Copy User ID");
    let id = subject.user.id.clone();
    copy.connect_clicked(move |button| button.clipboard().set_text(&id));
    actions.append(&copy);
    actions.set_margin_top(4);
    body.append(&actions);
    let _ = accounts_section; // shown by `fill` through `accounts`' parent

    let column = gtk::Box::new(gtk::Orientation::Vertical, 0);
    column.set_size_request(340, -1);
    column.append(&top);
    column.append(&body);
    popover.set_child(Some(&column));

    let card = Rc::new(Card {
        banner_color: std::cell::Cell::new((0.17, 0.18, 0.19)),
        banner_area: banner_area.clone(),
        banner_image,
        user_id: subject.user.id.clone(),
        bio,
        bio_section,
        since,
        roles,
        roles_section,
        mutual,
        mutual_section,
        accounts,
        pronouns,
        subject,
        images,
    });
    let painter = card.clone();
    banner_area.set_draw_func(move |_, cr: &cairo::Context, w, h| {
        let (r, g, b) = painter.banner_color.get();
        cr.set_source_rgb(r, g, b);
        let _ = cr.rectangle(0.0, 0.0, f64::from(w), f64::from(h));
        let _ = cr.fill();
    });

    popover.popup();
    send(Command::FetchProfile { user_id: card.user_id.clone(), guild_id: card.subject.guild_id.clone() });
    card
}

impl Card {
    pub fn user_id(&self) -> &str {
        &self.user_id
    }

    /// Fills in the parts that need Discord's profile reply.
    pub fn fill(&self, value: &Value) {
        let profile = &value["user_profile"];
        let guild_profile = &value["guild_member_profile"];
        let text = |v: &Value| v.as_str().filter(|s| !s.is_empty()).map(str::to_owned);

        if let Some(color) = value["user"]["accent_color"].as_u64() {
            self.banner_color.set(paint_color(color as u32));
            self.banner_area.queue_draw();
        }
        if let Some(banner) = text(&value["user"]["banner"]).or_else(|| text(&profile["banner"])) {
            let url = format!("https://cdn.discordapp.com/banners/{}/{banner}.png?size=480", self.user_id);
            let picture = self.banner_image.clone();
            picture.set_widget_name(&url);
            let wanted = url.clone();
            self.images.get(&url, move |texture| {
                if picture.widget_name() == wanted {
                    picture.set_paintable(texture.as_ref());
                }
            });
        }
        if let Some(pronouns) = text(&guild_profile["pronouns"]).or_else(|| text(&profile["pronouns"])) {
            self.pronouns.set_label(&format!("· {pronouns}"));
            self.pronouns.set_visible(true);
        }
        if let Some(bio) = text(&guild_profile["bio"])
            .or_else(|| text(&value["user"]["bio"]))
            .or_else(|| text(&profile["bio"]))
        {
            self.bio.set_label(&bio);
            self.bio_section.set_visible(true);
        }

        // "Member since" shows when the account and, in a server, the membership began.
        let mut since = created(&self.user_id).unwrap_or_default();
        if let Some(joined) = value["guild_member"]["joined_at"]
            .as_str()
            .and_then(|iso| glib::DateTime::from_iso8601(iso, None).ok())
            .and_then(|t| t.to_local().ok())
            .and_then(|t| t.format("%b %e, %Y").ok())
        {
            since = format!("{since}  ·  server: {}", joined.trim());
        }
        self.since.set_label(&since);

        // Roles in the server the card was opened from.
        if let Some(ids) = value["guild_member"]["roles"].as_array() {
            let mut shown: Vec<&Role> = self
                .subject
                .roles
                .iter()
                .filter(|r| ids.iter().any(|id| id.as_str() == Some(r.id.as_str())))
                .collect();
            shown.sort_by_key(|r| std::cmp::Reverse(r.position));
            for role in shown {
                let colour = if role.color == 0 { 0x99_aa_b5 } else { role.color };
                let chip = gtk::Label::new(None);
                chip.set_markup(&format!(
                    r##"<span foreground="#{colour:06x}">●</span> {}"##,
                    glib::markup_escape_text(&role.name)
                ));
                chip.add_css_class("role-chip");
                self.roles.append(&chip);
            }
            self.roles_section.set_visible(self.roles.first_child().is_some());
        }

        if let Some(guilds) = value["mutual_guilds"].as_array().filter(|g| !g.is_empty()) {
            let names: Vec<String> = guilds
                .iter()
                .filter_map(|g| g["id"].as_str())
                .map(|id| self.subject.guild_names.get(id).cloned().unwrap_or_else(|| "A server".into()))
                .collect();
            let more = names.len().saturating_sub(5);
            let mut text = names.iter().take(5).cloned().collect::<Vec<_>>().join(", ");
            if more > 0 {
                text.push_str(&format!(" and {more} more"));
            }
            if let Some(friends) = value["mutual_friends_count"].as_u64().filter(|n| *n > 0) {
                text.push_str(&format!("\n{friends} mutual friends"));
            }
            self.mutual.set_label(&text);
            self.mutual_section.set_visible(true);
        }

        for account in value["connected_accounts"].as_array().into_iter().flatten() {
            let kind = account["type"].as_str().unwrap_or("");
            let name = account["name"].as_str().unwrap_or("");
            let label = gtk::Label::builder().label(format!("{name}  ({kind})")).xalign(0.0).build();
            self.accounts.append(&label);
        }
        if let Some(parent) = self.accounts.parent() {
            parent.set_visible(self.accounts.first_child().is_some());
        }
    }

}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_age_comes_from_the_id() {
        // 175928847299117063 is a well-known early snowflake: April 2016.
        let text = created("175928847299117063").unwrap();
        assert!(text.contains("2016"), "{text}");
        assert!(created("not a number").is_none());
    }

    #[test]
    fn colours_unpack() {
        assert_eq!(paint_color(0xff0000), (1.0, 0.0, 0.0));
        assert_eq!(paint_color(0x000000), (0.0, 0.0, 0.0));
    }
}
