//! The settings window, laid out like Discord's.
//!
//! Account options (status, privacy, chat display, accessibility, language)
//! are read from and written to Discord's user settings, so they follow the
//! account to every client. Options that only make sense for this app
//! (devices, volumes, text size, plugins) live in `prefs`.

use std::{cell::{Cell, RefCell}, rc::Rc};

use adw::prelude::*;
use gtk::{gio, glib};
use serde_json::{json, Value};

use crate::{
    audio,
    images::Images,
    model::{Command, Panel, User},
    plugins, prefs,
};

const DARK: &str = include_str!("themes/dark.css");
const DARKER: &str = include_str!("themes/darker.css");
const MIDNIGHT: &str = include_str!("themes/midnight.css");
const LIGHT: &str = include_str!("themes/light.css");

pub struct Context {
    pub user: User,
    pub images: Rc<Images>,
    pub send: Rc<dyn Fn(Command)>,
    pub log_out: Rc<dyn Fn()>,
}

/// (page id, title, icon). `None` ids are section headings.
const SIDEBAR: [(&str, &str, &str); 17] = [
    ("", "USER SETTINGS", ""),
    ("account", "My Account", "avatar-default-symbolic"),
    ("content", "Content & Social", "emblem-people-symbolic"),
    ("devices", "Devices", "computer-symbolic"),
    ("connections", "Connections", "network-workgroup-symbolic"),
    ("", "APP SETTINGS", ""),
    ("appearance", "Appearance", "applications-graphics-symbolic"),
    ("accessibility", "Accessibility", "preferences-desktop-accessibility-symbolic"),
    ("voice", "Voice & Video", "audio-input-microphone-symbolic"),
    ("chat", "Chat", "user-available-symbolic"),
    ("notifications", "Notifications", "preferences-system-notifications-symbolic"),
    ("activity", "Activity Privacy", "security-high-symbolic"),
    ("language", "Language", "preferences-desktop-locale-symbolic"),
    ("advanced", "Advanced", "applications-engineering-symbolic"),
    ("", "THIS APP", ""),
    ("plugins", "Plugins", "application-x-addon-symbolic"),
    ("about", "About", "help-about-symbolic"),
];

fn escaped(text: &str) -> glib::GString {
    glib::markup_escape_text(text)
}

/// Keeps the widgets that mirror an account setting, so a reply from Discord
/// can fill them in without the fill itself being sent back as a change.
pub struct Handle {
    bindings: RefCell<Vec<(&'static str, Box<dyn Fn(&Value)>)>>,
    loading: Cell<bool>,
    devices: adw::PreferencesGroup,
    connections: adw::PreferencesGroup,
}

impl Handle {
    /// Shows data Discord sent for a panel.
    pub fn apply(&self, panel: Panel, value: &Value) {
        match panel {
            Panel::Settings => {
                self.loading.set(true);
                for (key, set) in self.bindings.borrow().iter() {
                    if let Some(found) = value.get(*key) {
                        set(found);
                    }
                }
                self.loading.set(false);
            }
            Panel::Sessions => fill_sessions(&self.devices, value),
            Panel::Connections => fill_connections(&self.connections, value),
        }
    }
}

/// Builds rows bound to account settings.
struct Rows {
    handle: Rc<Handle>,
    send: Rc<dyn Fn(Command)>,
}

impl Rows {
    fn patch(&self, key: &str, value: Value) {
        if !self.handle.loading.get() {
            (self.send)(Command::PatchSettings(json!({ key: value })));
        }
    }

    fn bind(&self, key: &'static str, set: impl Fn(&Value) + 'static) {
        self.handle.bindings.borrow_mut().push((key, Box::new(set)));
    }

    /// A toggle for a boolean setting. `invert` is for settings Discord words
    /// the other way round (e.g. "restricted" vs "allow").
    fn switch(&self, group: &adw::PreferencesGroup, key: &'static str, title: &str, subtitle: &str, invert: bool) {
        let row = adw::SwitchRow::builder().title(escaped(title)).subtitle(escaped(subtitle)).build();
        group.add(&row);
        let this = Rows { handle: self.handle.clone(), send: self.send.clone() };
        row.connect_active_notify(move |row| this.patch(key, json!(row.is_active() != invert)));
        let row = row.clone();
        self.bind(key, move |v| row.set_active(v.as_bool().unwrap_or(false) != invert));
    }

    /// A choice among fixed values.
    fn combo(&self, group: &adw::PreferencesGroup, key: &'static str, title: &str, options: &[(Value, &str)]) {
        let labels: Vec<&str> = options.iter().map(|(_, label)| *label).collect();
        let row = adw::ComboRow::builder()
            .title(escaped(title))
            .model(&gtk::StringList::new(&labels))
            .build();
        group.add(&row);
        let values: Vec<Value> = options.iter().map(|(v, _)| v.clone()).collect();
        let this = Rows { handle: self.handle.clone(), send: self.send.clone() };
        let sent = values.clone();
        row.connect_selected_notify(move |row| {
            if let Some(value) = sent.get(row.selected() as usize) {
                this.patch(key, value.clone());
            }
        });
        let row = row.clone();
        self.bind(key, move |v| {
            if let Some(at) = values.iter().position(|o| o == v) {
                row.set_selected(at as u32);
            }
        });
    }
}

pub fn present(parent: &gtk::Window, ctx: Context) -> Rc<Handle> {
    let handle = Rc::new(Handle {
        bindings: RefCell::default(),
        loading: Cell::new(false),
        devices: adw::PreferencesGroup::builder().title("Where you are logged in").build(),
        connections: adw::PreferencesGroup::builder().title("Connected accounts").build(),
    });
    let rows = Rows { handle: handle.clone(), send: ctx.send.clone() };

    let stack = gtk::Stack::new();
    stack.add_named(&account_page(&ctx, &rows), Some("account"));
    stack.add_named(&content_page(&rows), Some("content"));
    stack.add_named(&page(&[&handle.devices]), Some("devices"));
    stack.add_named(&page(&[&handle.connections]), Some("connections"));
    stack.add_named(&appearance_page(&rows), Some("appearance"));
    stack.add_named(&accessibility_page(&rows), Some("accessibility"));
    stack.add_named(&voice_page(), Some("voice"));
    stack.add_named(&chat_page(&rows), Some("chat"));
    stack.add_named(&notifications_page(), Some("notifications"));
    stack.add_named(&activity_page(&rows), Some("activity"));
    stack.add_named(&language_page(&rows), Some("language"));
    stack.add_named(&advanced_page(&rows), Some("advanced"));
    stack.add_named(&plugins_page(), Some("plugins"));
    stack.add_named(&about_page(), Some("about"));

    let content_title = adw::WindowTitle::new("My Account", "");
    let content = adw::ToolbarView::new();
    content.add_top_bar(&adw::HeaderBar::builder().title_widget(&content_title).build());
    content.set_content(Some(&stack));
    let content_page = adw::NavigationPage::new(&content, "Settings");

    let list = gtk::ListBox::new();
    list.add_css_class("navigation-sidebar");
    for (id, title, icon) in SIDEBAR {
        let row = gtk::ListBoxRow::new();
        row.set_widget_name(id);
        if id.is_empty() {
            let heading = gtk::Label::builder().label(title).xalign(0.0).build();
            heading.add_css_class("settings-heading");
            row.set_child(Some(&heading));
            row.set_selectable(false);
            row.set_activatable(false);
        } else {
            let inner = adw::ActionRow::builder().title(escaped(title)).build();
            inner.add_prefix(&gtk::Image::from_icon_name(icon));
            row.set_child(Some(&inner));
        }
        list.append(&row);
    }

    let log_out = gtk::Button::with_label("Log Out");
    log_out.add_css_class("destructive-action");
    log_out.set_margin_top(8);
    log_out.set_margin_bottom(8);
    log_out.set_margin_start(12);
    log_out.set_margin_end(12);
    let action = ctx.log_out.clone();
    log_out.connect_clicked(move |_| action());

    let sidebar = adw::ToolbarView::new();
    sidebar.add_top_bar(&adw::HeaderBar::new());
    sidebar.set_content(Some(&gtk::ScrolledWindow::builder().child(&list).build()));
    sidebar.add_bottom_bar(&log_out);
    let sidebar_page = adw::NavigationPage::new(&sidebar, "Settings");

    let split = adw::NavigationSplitView::new();
    split.set_sidebar(Some(&sidebar_page));
    split.set_content(Some(&content_page));

    list.connect_row_selected({
        let (stack, title, split) = (stack.clone(), content_title.clone(), split.clone());
        move |_, row| {
            let Some(row) = row else { return };
            let id = row.widget_name();
            if let Some((_, label, _)) = SIDEBAR.iter().find(|(i, _, _)| *i == id) {
                stack.set_visible_child_name(&id);
                title.set_title(label);
                split.set_show_content(true);
            }
        }
    });
    list.select_row(list.row_at_index(1).as_ref());

    adw::Window::builder()
        .transient_for(parent)
        .modal(true)
        .title("Settings")
        .default_width(900)
        .default_height(680)
        .content(&split)
        .build()
        .present();

    for panel in [Panel::Settings, Panel::Sessions, Panel::Connections] {
        (ctx.send)(Command::Fetch(panel));
    }
    handle
}

/// A preferences page holding the given groups.
fn page(groups: &[&adw::PreferencesGroup]) -> gtk::Widget {
    let page = adw::PreferencesPage::new();
    for group in groups {
        page.add(*group);
    }
    page.upcast()
}

fn group(title: &str, description: &str) -> adw::PreferencesGroup {
    adw::PreferencesGroup::builder().title(escaped(title)).description(escaped(description)).build()
}

fn account_page(ctx: &Context, rows: &Rows) -> gtk::Widget {
    let user = &ctx.user;
    let avatar = adw::Avatar::new(96, Some(user.display()), true);
    let url = user.avatar_url_sized(256);
    avatar.set_widget_name(&url);
    let (shown, wanted) = (avatar.clone(), url.clone());
    ctx.images.get(&url, move |texture| {
        if shown.widget_name() == wanted {
            shown.set_custom_image(texture.as_ref());
        }
    });
    avatar.set_halign(gtk::Align::Center);
    let head = adw::PreferencesGroup::new();
    head.add(&avatar);

    let details = group("Account", "");
    for (title, value) in [
        ("Display name", user.display().to_owned()),
        ("Username", user.username.clone()),
        ("User ID", user.id.clone()),
    ] {
        let row = adw::ActionRow::builder().title(title).subtitle(escaped(&value)).build();
        row.set_subtitle_selectable(true);
        details.add(&row);
    }

    const STATUSES: [(&str, &str); 4] =
        [("online", "Online"), ("idle", "Idle"), ("dnd", "Do Not Disturb"), ("invisible", "Invisible")];
    let presence = group("Status", "Shown to other people on Discord.");
    rows.combo(
        &presence,
        "status",
        "Status",
        &STATUSES.map(|(value, label)| (json!(value), label)),
    );
    let custom = adw::EntryRow::builder().title("Custom status").show_apply_button(true).build();
    presence.add(&custom);
    let send = rows.send.clone();
    let handle = rows.handle.clone();
    custom.connect_apply(move |row| {
        if handle.loading.get() {
            return;
        }
        let text = row.text().to_string();
        let value = if text.is_empty() { Value::Null } else { json!({ "text": text }) };
        send(Command::PatchSettings(json!({ "custom_status": value })));
    });
    let field = custom.clone();
    rows.bind("custom_status", move |v| {
        field.set_text(v.get("text").and_then(Value::as_str).unwrap_or(""));
    });
    page(&[&head, &details, &presence])
}

fn content_page(rows: &Rows) -> gtk::Widget {
    let filter = group("Explicit media filter", "Automatically blocks images in direct messages that may be explicit.");
    rows.combo(
        &filter,
        "explicit_content_filter",
        "Scan direct messages",
        &[
            (json!(0), "Do not scan"),
            (json!(1), "Scan from everyone except friends"),
            (json!(2), "Scan from everyone"),
        ],
    );

    let privacy = group("Server privacy defaults", "");
    rows.switch(
        &privacy,
        "default_guilds_restricted",
        "Allow direct messages from server members",
        "Applies to servers you join from now on.",
        true,
    );

    let friends = group("Who can add you as a friend", "");
    let flags = [
        ("all", "Everyone"),
        ("mutual_friends", "Friends of friends"),
        ("mutual_guilds", "Server members"),
    ];
    let switches: Rc<Vec<adw::SwitchRow>> = Rc::new(
        flags.iter().map(|(_, title)| adw::SwitchRow::builder().title(*title).build()).collect(),
    );
    for row in switches.iter() {
        friends.add(row);
        let (switches, send, handle) = (switches.clone(), rows.send.clone(), rows.handle.clone());
        row.connect_active_notify(move |_| {
            if handle.loading.get() {
                return;
            }
            let object: serde_json::Map<String, Value> = flags
                .iter()
                .zip(switches.iter())
                .map(|((key, _), row)| ((*key).to_owned(), json!(row.is_active())))
                .collect();
            send(Command::PatchSettings(json!({ "friend_source_flags": object })));
        });
    }
    let shown = switches.clone();
    rows.bind("friend_source_flags", move |v| {
        for ((key, _), row) in flags.iter().zip(shown.iter()) {
            row.set_active(v.get(*key).and_then(Value::as_bool).unwrap_or(false));
        }
    });

    let social = group("Social", "");
    rows.switch(&social, "detect_platform_accounts", "Detect accounts from other services", "Such as Steam.", false);
    rows.switch(&social, "stream_notifications_enabled", "Stream notifications", "Tell me when friends start streaming.", false);
    rows.switch(&social, "view_nsfw_commands", "Show age-restricted commands in direct messages", "", false);
    rows.switch(&social, "view_nsfw_guilds", "Show age-restricted servers", "", false);
    page(&[&filter, &privacy, &friends, &social])
}

fn fill_sessions(group: &adw::PreferencesGroup, value: &Value) {
    while let Some(child) = group_rows(group).into_iter().next() {
        group.remove(&child);
    }
    let sessions = value.get("user_sessions").and_then(Value::as_array).cloned().unwrap_or_default();
    if sessions.is_empty() {
        group.add(&adw::ActionRow::builder().title("No sessions").build());
    }
    for session in sessions {
        let info = &session["client_info"];
        let field = |name: &str| info.get(name).and_then(Value::as_str).unwrap_or("").to_owned();
        let (platform, os, location) = (field("platform"), field("os"), field("location"));
        let title = [platform, os].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" · ");
        let when = session
            .get("approx_last_used_time")
            .and_then(Value::as_str)
            .and_then(|iso| glib::DateTime::from_iso8601(iso, None).ok())
            .and_then(|t| t.to_local().ok())
            .and_then(|t| t.format("%b %e, %Y").ok())
            .map(|s| s.to_string())
            .unwrap_or_default();
        let subtitle = [location, when].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" · ");
        group.add(
            &adw::ActionRow::builder()
                .title(escaped(if title.is_empty() { "Unknown device" } else { &title }))
                .subtitle(escaped(&subtitle))
                .build(),
        );
    }
}

fn fill_connections(group: &adw::PreferencesGroup, value: &Value) {
    while let Some(child) = group_rows(group).into_iter().next() {
        group.remove(&child);
    }
    let list = value.as_array().cloned().unwrap_or_default();
    if list.is_empty() {
        group.add(&adw::ActionRow::builder().title("No connected accounts").build());
    }
    for item in list {
        let name = item.get("name").and_then(Value::as_str).unwrap_or("");
        let kind = item.get("type").and_then(Value::as_str).unwrap_or("");
        group.add(&adw::ActionRow::builder().title(escaped(name)).subtitle(escaped(kind)).build());
    }
}

/// The rows currently in a preferences group (it has no accessor for them).
fn group_rows(group: &adw::PreferencesGroup) -> Vec<gtk::Widget> {
    let mut found = Vec::new();
    fn walk(widget: &gtk::Widget, found: &mut Vec<gtk::Widget>) {
        let mut child = widget.first_child();
        while let Some(c) = child {
            if c.is::<adw::ActionRow>() {
                found.push(c.clone());
            } else {
                walk(&c, found);
            }
            child = c.next_sibling();
        }
    }
    walk(group.upcast_ref(), &mut found);
    found
}

fn device_row(title: &str, input: bool) -> adw::ComboRow {
    let devices = audio::devices(input);
    let mut labels = vec!["System default".to_owned()];
    labels.extend(devices.iter().map(|d| d.label.clone()));
    let wanted = {
        let prefs = prefs::get();
        if input { prefs.input_device } else { prefs.output_device }
    };
    let row = adw::ComboRow::builder()
        .title(title)
        .model(&gtk::StringList::new(&labels.iter().map(String::as_str).collect::<Vec<_>>()))
        .build();
    if let Some(at) = wanted.and_then(|w| devices.iter().position(|d| d.id == w)) {
        row.set_selected(at as u32 + 1);
    }
    row.connect_selected_notify(move |row| {
        let choice = match row.selected() {
            0 => None,
            n => devices.get(n as usize - 1).map(|d| d.id.clone()),
        };
        prefs::update(|p| if input { p.input_device = choice } else { p.output_device = choice });
    });
    row
}

fn slider(title: &str, min: f64, max: f64, step: f64, value: f64, on_change: impl Fn(f64) + 'static) -> adw::ActionRow {
    let scale = gtk::Scale::with_range(gtk::Orientation::Horizontal, min, max, step);
    scale.set_value(value);
    scale.set_size_request(260, -1);
    scale.set_valign(gtk::Align::Center);
    scale.connect_value_changed(move |s| on_change(s.value()));
    let row = adw::ActionRow::builder().title(title).build();
    row.add_suffix(&scale);
    row.set_activatable_widget(Some(&scale));
    row
}

fn voice_page() -> gtk::Widget {
    let devices = group("Devices", "Microphone and speaker changes apply the next time you join a call.");
    devices.add(&device_row("Input device", true));
    devices.add(&device_row("Output device", false));
    let prefs_now = prefs::get();
    devices.add(&slider("Input volume", 0.0, 2.0, 0.05, f64::from(prefs_now.input_volume), |v| {
        prefs::update(|p| p.input_volume = v as f32);
        audio::set_gain(true, v as f32);
    }));
    devices.add(&slider("Output volume", 0.0, 2.0, 0.05, f64::from(prefs_now.output_volume), |v| {
        prefs::update(|p| p.output_volume = v as f32);
        audio::set_gain(false, v as f32);
    }));

    let gate = group("Input sensitivity", "Your microphone is only sent while it is louder than this.");
    gate.add(&slider("Threshold", 0.0, 0.2, 0.005, f64::from(prefs_now.mic_threshold), |v| {
        prefs::update(|p| p.mic_threshold = v as f32);
    }));
    page(&[&devices, &gate])
}

/// Applies the theme and message text size from the saved preferences.
pub fn apply_appearance() {
    let prefs = prefs::get();
    let manager = adw::StyleManager::default();
    let light = match prefs.theme.as_str() {
        "light" => true,
        "system" => !manager.is_dark(),
        _ => false,
    };
    manager.set_color_scheme(if light { adw::ColorScheme::ForceLight } else { adw::ColorScheme::ForceDark });
    let palette = match prefs.theme.as_str() {
        _ if light => LIGHT,
        "darker" => DARKER,
        "midnight" => MIDNIGHT,
        _ => DARK,
    };
    thread_local!(static THEME: gtk::CssProvider = {
        let provider = gtk::CssProvider::new();
        if let Some(display) = gtk::gdk::Display::default() {
            gtk::style_context_add_provider_for_display(&display, &provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
        }
        provider
    });
    THEME.with(|p| p.load_from_string(&format!("{palette}\n{}", include_str!("style.css"))));
    thread_local!(static ZOOM: gtk::CssProvider = {
        let provider = gtk::CssProvider::new();
        if let Some(display) = gtk::gdk::Display::default() {
            gtk::style_context_add_provider_for_display(&display, &provider, gtk::STYLE_PROVIDER_PRIORITY_USER);
        }
        provider
    });
    ZOOM.with(|p| p.load_from_string(&format!(".messages {{ font-size: {}%; }}", (prefs.zoom * 100.0).round())));
}

fn appearance_page(rows: &Rows) -> gtk::Widget {
    const THEMES: [(&str, &str); 5] = [
        ("dark", "Dark"),
        ("darker", "Darker"),
        ("midnight", "Midnight"),
        ("light", "Light"),
        ("system", "Sync with computer"),
    ];
    let group = group("Theme", "");
    let theme = adw::ComboRow::builder()
        .title("Theme")
        .model(&gtk::StringList::new(&THEMES.map(|(_, label)| label)))
        .build();
    let current = prefs::get().theme;
    theme.set_selected(THEMES.iter().position(|(v, _)| *v == current).unwrap_or(0) as u32);
    let send = rows.send.clone();
    theme.connect_selected_notify(move |row| {
        let choice = THEMES[row.selected() as usize].0;
        prefs::update(|p| p.theme = choice.to_owned());
        apply_appearance();
        // Discord only knows its own four themes, so "sync" is kept here.
        if choice != "system" {
            send(Command::PatchSettings(json!({ "theme": choice })));
        }
    });
    group.add(&theme);

    let size = self::group("Message text", "");
    size.add(&slider("Text size", 0.8, 1.6, 0.05, prefs::get().zoom, |v| {
        prefs::update(|p| p.zoom = v);
        apply_appearance();
    }));
    page(&[&group, &size])
}

fn accessibility_page(rows: &Rows) -> gtk::Widget {
    let motion = group("Motion", "");
    rows.switch(&motion, "animate_emoji", "Play animated emoji", "", false);
    rows.switch(&motion, "gif_auto_play", "Automatically play GIFs", "When Discord is focused.", false);
    rows.combo(
        &motion,
        "animate_stickers",
        "Sticker animation",
        &[(json!(0), "Always animate"), (json!(1), "Animate on interaction"), (json!(2), "Never animate")],
    );
    let text = group("Text", "");
    rows.switch(&text, "convert_emoticons", "Convert emoticons to emoji", "For example :) becomes 🙂.", false);
    rows.switch(&text, "enable_tts_command", "Allow /tts playback", "", false);
    let privacy = group("Privacy", "");
    rows.switch(&privacy, "allow_accessibility_detection", "Let Discord detect screen readers", "", false);
    page(&[&motion, &text, &privacy])
}

fn chat_page(rows: &Rows) -> gtk::Widget {
    let display = group("Display", "");
    rows.switch(&display, "inline_attachment_media", "Show uploaded images and videos", "", false);
    rows.switch(&display, "inline_embed_media", "Show images and videos from links", "", false);
    rows.switch(&display, "render_embeds", "Show website previews", "", false);
    rows.switch(&display, "render_reactions", "Show emoji reactions", "", false);
    page(&[&display])
}

fn notifications_page() -> gtk::Widget {
    let group = group("Desktop notifications", "For direct messages and mentions while you are not looking at that channel.");
    let toggle = adw::SwitchRow::builder().title("Enable desktop notifications").build();
    toggle.set_active(prefs::get().notifications);
    toggle.connect_active_notify(|row| prefs::update(|p| p.notifications = row.is_active()));
    group.add(&toggle);
    page(&[&group])
}

fn activity_page(rows: &Rows) -> gtk::Widget {
    let group = group("Activity", "");
    rows.switch(&group, "show_current_game", "Share your activity as a status", "Show what you are playing.", false);
    rows.switch(&group, "allow_activity_party_privacy_friends", "Friends can join without asking", "", false);
    rows.switch(&group, "allow_activity_party_privacy_voice_channel", "People in your voice channel can join without asking", "", false);
    page(&[&group])
}

fn language_page(rows: &Rows) -> gtk::Widget {
    const LOCALES: [(&str, &str); 31] = [
        ("bg", "Български"), ("cs", "Čeština"), ("da", "Dansk"), ("de", "Deutsch"),
        ("el", "Ελληνικά"), ("en-GB", "English (UK)"), ("en-US", "English (US)"),
        ("es-419", "Español (América Latina)"), ("es-ES", "Español"), ("fi", "Suomi"),
        ("fr", "Français"), ("hi", "हिन्दी"), ("hr", "Hrvatski"), ("hu", "Magyar"),
        ("it", "Italiano"), ("ja", "日本語"), ("ko", "한국어"), ("lt", "Lietuviškai"),
        ("nl", "Nederlands"), ("no", "Norsk"), ("pl", "Polski"), ("pt-BR", "Português (Brasil)"),
        ("ro", "Română"), ("ru", "Русский"), ("sv-SE", "Svenska"), ("th", "ไทย"),
        ("tr", "Türkçe"), ("uk", "Українська"), ("vi", "Tiếng Việt"), ("zh-CN", "中文"),
        ("zh-TW", "繁體中文"),
    ];
    let group = group("Language", "Changes the language of Discord's own messages. This app's menus stay in English.");
    rows.combo(&group, "locale", "Language", &LOCALES.map(|(code, label)| (json!(code), label)));
    page(&[&group])
}

fn advanced_page(rows: &Rows) -> gtk::Widget {
    let group = group("Advanced", "");
    rows.switch(&group, "developer_mode", "Developer mode", "Lets you copy IDs from menus.", false);
    page(&[&group])
}

fn plugins_page() -> gtk::Widget {
    let builtin = group("Built-in plugins", "Native versions of popular Vencord plugins. Vencord's own plugins are JavaScript for Discord's web app and cannot run here.");
    for info in crate::builtin::PLUGINS {
        let row = adw::SwitchRow::builder()
            .title(escaped(info.name))
            .subtitle(escaped(info.description))
            .active(crate::builtin::enabled(info.id))
            .build();
        let id = info.id;
        row.connect_active_notify(move |row| crate::builtin::set_enabled(id, row.is_active()));
        builtin.add(&row);
    }
    let group = group("Installed plugins", "Each plugin is a folder with plugin.lua and an optional style.css.");
    let dir = plugins::config_dir();
    let mut names: Vec<String> = dir
        .as_ref()
        .and_then(|d| std::fs::read_dir(d).ok())
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().join("plugin.lua").exists() || e.path().join("style.css").exists())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    if names.is_empty() {
        group.add(&adw::ActionRow::builder().title("No plugins installed").build());
    }
    for name in names {
        group.add(&adw::ActionRow::builder().title(escaped(&name)).build());
    }
    let open = gtk::Button::with_label("Open Plugins Folder");
    open.set_halign(gtk::Align::Start);
    open.connect_clicked(move |_| {
        if let Some(dir) = &dir {
            let _ = std::fs::create_dir_all(dir);
            gtk::FileLauncher::new(Some(&gio::File::for_path(dir))).launch(gtk::Window::NONE, gio::Cancellable::NONE, |_| {});
        }
    });
    let actions = adw::PreferencesGroup::new();
    actions.add(&open);
    page(&[&builtin, &group, &actions])
}

fn about_page() -> gtk::Widget {
    let group = group("About", "");
    group.add(&adw::ActionRow::builder().title("Version").subtitle(env!("CARGO_PKG_VERSION")).build());
    group.add(
        &adw::ActionRow::builder()
            .title("Unofficial client")
            .subtitle("Uses your account token directly, which Discord's terms do not allow. Use at your own risk.")
            .build(),
    );
    page(&[&group])
}
