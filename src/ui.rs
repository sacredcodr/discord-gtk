use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    rc::Rc,
    time::{Duration, Instant},
};

use adw::prelude::*;
use async_channel::Sender;
use gtk::{gdk, gio, glib, pango};

use crate::member_list::{Item, MemberItem, MemberList};
use crate::model::{Attachment, Channel, Command, Embed, Emoji, Event, GuildEmoji, Message, Reaction, Relationship, Role, User};
use crate::profile;
use crate::state::{Entry, State, Unread, VoiceEntry, CONNECT, SEND_MESSAGES};
use crate::error::Error;
use crate::autocomplete;
use crate::emoji_text;
use crate::icons;
use crate::images::Images;
use crate::markup;
use crate::plugins::{Plugins, Shown};
use crate::prefs;
use crate::web_login;
use crate::{backend, config, settings};

/// Messages kept in memory for the open channel; older ones are dropped from
/// the front. This bounds memory no matter how long a channel stays open.
const MAX_MESSAGES: u32 = 300;

/// Pseudo guild id used for the "Direct Messages" row. Matches Discord's own
/// route name, which cannot collide with a real snowflake.
const DM_ID: &str = "@me";

/// How close to the end, in pixels, the view must be for a new message to
/// keep it pinned to the bottom. Scrolled further up means the user is
/// reading history and must not be yanked down.
const STICK_TO_BOTTOM_PX: f64 = 80.0;

/// All widgets and view state, owned on the GTK main thread only.
///
/// IMPORTANT: never moved to another thread. The backend talks to it only
/// through `commands` (out) and the `Event` channel (in).
struct Ui {
    toasts: adw::ToastOverlay,
    stack: gtk::Stack,
    split: adw::NavigationSplitView,
    guilds: gtk::ListBox,
    channels: gtk::ListBox,
    channel_icon: gtk::Image,
    channel_name: gtk::Label,
    channel_topic: gtk::Label,
    server_name: gtk::Label,
    members_toggle: gtk::ToggleButton,
    members_revealer: gtk::Revealer,
    members_list: gtk::ListBox,
    member_list: RefCell<MemberList>,
    store: gio::ListStore,
    list: gtk::ListView,
    scroller: gtk::ScrolledWindow,
    entry: gtk::Entry,
    attach_button: gtk::Button,
    suggestions: RefCell<Vec<GuildEmoji>>,
    emoji_popover: gtk::Popover,
    emoji_matches: gtk::ListBox,
    emoji_chooser: gtk::EmojiChooser,
    picker: gtk::Popover,
    picker_search: gtk::SearchEntry,
    picker_body: gtk::Box,
    commands: RefCell<Option<Sender<Command>>>,
    current_guild: RefCell<Option<String>>,
    current_channel: RefCell<Option<String>>,
    /// Holds the browser login web view while the login page is showing. Empty
    /// otherwise, so a logged-in session pays nothing for WebKit.
    web_slot: gtk::Box,
    images: Rc<Images>,
    plugins: Rc<Plugins>,
    /// Id of the logged-in user, shared with message rows (own messages get Edit/Delete).
    me: Rc<RefCell<Option<String>>>,
    actions_rx: RefCell<Option<async_channel::Receiver<RowAction>>>,
    /// (message id, author name) the next message will reply to.
    reply_to: RefCell<Option<(String, String)>>,
    /// Id of the message being edited instead of sending a new one.
    editing: RefCell<Option<String>>,
    context_bar: gtk::Box,
    context_label: gtk::Label,
    typing_label: gtk::Label,
    /// user id -> when their typing indicator expires.
    typing: RefCell<HashMap<String, Instant>>,
    /// user id -> display name, learned from messages and DMs.
    names: RefCell<HashMap<String, String>>,
    last_typing_sent: Cell<Option<Instant>>,
    state: RefCell<State>,
    /// Set while lists are rebuilt so the rebuild itself is not mistaken for a click.
    silent: Cell<bool>,
    voice_panel: gtk::Box,
    voice_status: gtk::Label,
    voice_channel: gtk::Label,
    mute_button: gtk::ToggleButton,
    deafen_button: gtk::ToggleButton,
    hang_up_button: gtk::Button,
    call_button: gtk::Button,
    settings_button: gtk::Button,
    settings: RefCell<Option<Rc<settings::Handle>>>,
    content_stack: gtk::Stack,
    friends_list: gtk::ListBox,
    friends_tabs: gtk::Box,
    friends_tab: Cell<FriendsTab>,
    friends_dirty: Cell<bool>,
    profile: RefCell<Option<Rc<profile::Card>>>,
    presence_dots: RefCell<HashMap<String, Vec<glib::WeakRef<gtk::DrawingArea>>>>,
    last_channel: RefCell<HashMap<String, String>>,
    char_counter: gtk::Label,
    clear_deleted_button: gtk::Button,
    staged: RefCell<Vec<std::path::PathBuf>>,
    staged_box: gtk::Box,
    asked_users: RefCell<std::collections::HashSet<String>>,
    user_avatar: adw::Avatar,
    user_name: gtk::Label,
    user_tag: gtk::Label,
    /// Channel of the voice connection in progress or established.
    voice_target: RefCell<Option<String>>,
    /// User ids heard speaking right now.
    speaking: RefCell<Vec<String>>,
}

pub fn build(app: &adw::Application) {
    if let Some(win) = app.active_window() {
        win.present();
        return;
    }

    icons::install();
    settings::apply_appearance();
    let ui = Rc::new(Ui::new());
    // Plugin themes sit above the built-in one so they can override it.
    load_css(&ui.plugins.css, gtk::STYLE_PROVIDER_PRIORITY_USER);
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Discord")
        .default_width(1100)
        .default_height(720)
        .content(&ui.toasts)
        .build();

    ui.connect_signals();

    // Ctrl+, opens settings, as in most desktop apps.
    let shortcuts = gtk::ShortcutController::new();
    shortcuts.set_scope(gtk::ShortcutScope::Global);
    let opener = ui.clone();
    shortcuts.add_shortcut(gtk::Shortcut::new(
        gtk::ShortcutTrigger::parse_string("<Control>comma"),
        Some(gtk::CallbackAction::new(move |_, _| {
            opener.open_settings();
            glib::Propagation::Stop
        })),
    ));
    window.add_controller(shortcuts);

    match config::load_token() {
        Some(token) => ui.start(token),
        None => ui.show_login(),
    }
    window.present();
}

fn load_css(css: &str, priority: u32) {
    let provider = gtk::CssProvider::new();
    provider.load_from_string(css);
    gtk::style_context_add_provider_for_display(
        &gtk::gdk::Display::default().expect("a display"),
        &provider,
        priority,
    );
}

impl Ui {
    fn new() -> Self {
        let store = gio::ListStore::new::<glib::BoxedAnyObject>();
        let images = Rc::new(Images::default());
        let plugins = Rc::new(Plugins::load());
        let me: Rc<RefCell<Option<String>>> = Rc::default();
        let (actions_tx, actions_rx) = async_channel::unbounded();
        let list = gtk::ListView::new(
            Some(gtk::NoSelection::new(Some(store.clone()))),
            Some(message_factory(store.clone(), images.clone(), plugins.clone(), me.clone(), actions_tx)),
        );
        list.add_css_class("messages");
        let scroller = gtk::ScrolledWindow::builder()
            .child(&list)
            .vexpand(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .build();

        let entry = gtk::Entry::builder()
            .placeholder_text("Message")
            .hexpand(true)
            .has_frame(false)
            .sensitive(false)
            .build();
        entry.add_css_class("composer");
        // Discord's composer: attach on the left, emoji on the right, in one rounded bar.
        let attach_button = icons::button("circle-plus", "Upload a File");
        let emoji_button = gtk::MenuButton::new();
        emoji_button.set_child(Some(&icons::image("emoji-slightly-smiling-face", 22)));
        emoji_button.set_tooltip_text(Some("Select Emoji"));
        emoji_button.add_css_class("flat");
        // Server emoji picker with a link to the standard emoji chooser.
        let emoji_chooser = gtk::EmojiChooser::new();
        let picker_search = gtk::SearchEntry::builder().placeholder_text("Find the perfect emoji").build();
        let picker_body = gtk::Box::new(gtk::Orientation::Vertical, 6);
        let picker_scroll = gtk::ScrolledWindow::builder()
            .child(&picker_body)
            .min_content_height(320)
            .min_content_width(340)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .build();
        let standard_button = gtk::Button::with_label("Standard emoji…");
        standard_button.add_css_class("flat");
        emoji_chooser.set_parent(&standard_button);
        standard_button.connect_clicked({
            let chooser = emoji_chooser.clone();
            move |_| chooser.popup()
        });
        let picker_column = gtk::Box::new(gtk::Orientation::Vertical, 6);
        picker_column.set_margin_top(8);
        picker_column.set_margin_bottom(8);
        picker_column.set_margin_start(8);
        picker_column.set_margin_end(8);
        picker_column.append(&picker_search);
        picker_column.append(&picker_scroll);
        picker_column.append(&standard_button);
        let picker = gtk::Popover::new();
        picker.set_child(Some(&picker_column));
        emoji_button.set_popover(Some(&picker));
        // Suggestions for `:name` appear above the bar. They must not steal
        // focus, or typing would stop mid-word.
        let emoji_matches = gtk::ListBox::new();
        let emoji_popover = gtk::Popover::new();
        emoji_popover.set_child(Some(&emoji_matches));
        emoji_popover.set_autohide(false);
        emoji_popover.set_can_focus(false);
        emoji_popover.set_position(gtk::PositionType::Top);
        emoji_popover.set_has_arrow(false);
        emoji_popover.set_parent(&entry);
        let composer_box = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        composer_box.add_css_class("composer-box");
        composer_box.append(&attach_button);
        composer_box.append(&entry);
        let counter = gtk::Label::new(None);
        counter.add_css_class("char-counter");
        counter.set_visible(false);
        composer_box.append(&counter);
        composer_box.append(&emoji_button);

        // Discord's channel header: "# name | topic" on the left, tools on the right.
        let channel_icon = icons::image("hashmark", 24);
        channel_icon.add_css_class("channel-hash");
        let channel_name = gtk::Label::builder().ellipsize(pango::EllipsizeMode::End).build();
        channel_name.add_css_class("channel-title");
        let channel_topic = gtk::Label::builder()
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(pango::EllipsizeMode::End)
            .build();
        channel_topic.add_css_class("channel-topic");
        let channel_info = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        channel_info.append(&channel_icon);
        channel_info.append(&channel_name);
        channel_info.append(&channel_topic);
        let chat_header = adw::HeaderBar::builder()
            .title_widget(&gtk::Box::new(gtk::Orientation::Horizontal, 0))
            .show_title(false)
            .build();
        chat_header.pack_start(&channel_info);
        let members_toggle = gtk::ToggleButton::new();
        members_toggle.set_child(Some(&icons::image("group", 22)));
        members_toggle.set_tooltip_text(Some("Member List"));
        members_toggle.set_active(true);
        members_toggle.add_css_class("flat");
        chat_header.pack_end(&members_toggle);
        let clear_deleted_button = icons::button("trash", "Clear deleted messages (MessageLogger)");
        clear_deleted_button.set_visible(false);
        chat_header.pack_end(&clear_deleted_button);
        let call_button = icons::button("phone-call", "Start a call");
        call_button.set_tooltip_text(Some("Start a call"));
        call_button.set_visible(false);
        chat_header.pack_end(&call_button);
        let chat_body = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let typing_label = gtk::Label::builder().xalign(0.0).label(" ").build();
        typing_label.add_css_class("typing");
        let context_label = gtk::Label::builder().xalign(0.0).hexpand(true).build();
        let context_close = gtk::Button::from_icon_name("window-close-symbolic");
        context_close.add_css_class("flat");
        context_close.set_widget_name("context-close");
        let context_bar = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        context_bar.add_css_class("context-bar");
        context_bar.append(&context_label);
        context_bar.append(&context_close);
        context_bar.set_visible(false);
        chat_body.append(&scroller);
        chat_body.append(&typing_label);
        chat_body.append(&context_bar);
        let staged_box = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        staged_box.add_css_class("staged-files");
        staged_box.set_visible(false);
        chat_body.append(&staged_box);
        chat_body.append(&composer_box);
        let chat_view = adw::ToolbarView::new();
        chat_view.add_top_bar(&chat_header);
        chat_body.set_hexpand(true);
        let members_list = gtk::ListBox::new();
        members_list.add_css_class("member-list");
        members_list.set_selection_mode(gtk::SelectionMode::None);
        let members_scroll = gtk::ScrolledWindow::builder()
            .child(&members_list)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .width_request(240)
            .build();
        let members_revealer = gtk::Revealer::builder()
            .transition_type(gtk::RevealerTransitionType::SlideLeft)
            .child(&members_scroll)
            .reveal_child(false)
            .build();
        let chat_row = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        chat_row.append(&chat_body);
        chat_row.append(&members_revealer);
        // The Friends home page shares the content area with the chat.
        let friends_list = gtk::ListBox::new();
        friends_list.add_css_class("friends-list");
        friends_list.set_selection_mode(gtk::SelectionMode::None);
        let friends_scroll = gtk::ScrolledWindow::builder()
            .child(&friends_list)
            .vexpand(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .build();
        let tabs = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        tabs.add_css_class("friends-tabs");
        let friends_view = gtk::Box::new(gtk::Orientation::Vertical, 0);
        friends_view.append(&tabs);
        friends_view.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        friends_view.append(&friends_scroll);
        let content_stack = gtk::Stack::new();
        content_stack.add_named(&chat_row, Some("chat"));
        content_stack.add_named(&friends_view, Some("friends"));
        chat_view.set_content(Some(&content_stack));
        let content = adw::NavigationPage::new(&chat_view, "Chat");

        let guilds = gtk::ListBox::new();
        guilds.add_css_class("guild-strip");
        let channels = gtk::ListBox::new();
        channels.add_css_class("channel-pane");
        let guild_scroll = gtk::ScrolledWindow::builder()
            .child(&guilds)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .build();
        let channel_scroll = gtk::ScrolledWindow::builder()
            .child(&channels)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .hexpand(true)
            .vexpand(true)
            .build();
        let side_body = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        side_body.append(&guild_scroll);
        side_body.append(&gtk::Separator::new(gtk::Orientation::Vertical));
        let server_name = gtk::Label::builder()
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(pango::EllipsizeMode::End)
            .label("Direct Messages")
            .build();
        server_name.add_css_class("server-name");
        let server_header = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        server_header.add_css_class("server-header");
        server_header.append(&server_name);
        server_header.append(&icons::image("chevron-small-down", 20));
        let channel_column = gtk::Box::new(gtk::Orientation::Vertical, 0);
        channel_column.add_css_class("channel-column");
        channel_column.append(&server_header);
        channel_column.append(&channel_scroll);
        side_body.append(&channel_column);
        let voice_status = gtk::Label::builder().xalign(0.0).label("Voice Connected").build();
        voice_status.add_css_class("voice-status");
        let voice_channel = gtk::Label::builder()
            .xalign(0.0)
            .ellipsize(pango::EllipsizeMode::End)
            .build();
        voice_channel.add_css_class("dim-label");
        let voice_labels = gtk::Box::new(gtk::Orientation::Vertical, 0);
        voice_labels.set_hexpand(true);
        voice_labels.append(&voice_status);
        voice_labels.append(&voice_channel);
        let mute_button = gtk::ToggleButton::new();
        let mute_icon = icons::image("microphone", 20);
        mute_button.set_child(Some(&mute_icon));
        mute_button.set_tooltip_text(Some("Mute"));
        let deafen_button = gtk::ToggleButton::new();
        let deafen_icon = icons::image("headphones", 20);
        deafen_button.set_child(Some(&deafen_icon));
        deafen_button.set_tooltip_text(Some("Deafen"));
        let hang_up_button = icons::button("phone-hang-up", "Disconnect");
        hang_up_button.set_tooltip_text(Some("Disconnect"));
        hang_up_button.add_css_class("destructive-action");
        let voice_panel = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        voice_panel.add_css_class("voice-panel");
        voice_panel.append(&voice_labels);
        voice_panel.append(&hang_up_button);
        voice_panel.set_visible(false);

        // Discord's user panel: who you are, plus mute, deafen and settings.
        let user_avatar = adw::Avatar::new(32, None, true);
        let user_name = gtk::Label::builder().xalign(0.0).ellipsize(pango::EllipsizeMode::End).build();
        user_name.add_css_class("user-name");
        let user_tag = gtk::Label::builder().xalign(0.0).ellipsize(pango::EllipsizeMode::End).build();
        user_tag.add_css_class("user-tag");
        let user_labels = gtk::Box::new(gtk::Orientation::Vertical, 0);
        user_labels.set_hexpand(true);
        user_labels.set_valign(gtk::Align::Center);
        user_labels.append(&user_name);
        user_labels.append(&user_tag);
        let settings_button = icons::button("settings", "User Settings");
        settings_button.set_tooltip_text(Some("User Settings"));
        for button in [mute_button.upcast_ref::<gtk::Widget>(), deafen_button.upcast_ref(), settings_button.upcast_ref()] {
            button.add_css_class("flat");
            button.add_css_class("panel-button");
        }
        let user_panel = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        user_panel.add_css_class("user-panel");
        user_panel.append(&user_avatar);
        user_panel.append(&user_labels);
        user_panel.append(&mute_button);
        user_panel.append(&deafen_button);
        user_panel.append(&settings_button);
        let bottom = gtk::Box::new(gtk::Orientation::Vertical, 0);
        bottom.append(&voice_panel);
        bottom.append(&user_panel);

        let side_view = adw::ToolbarView::new();
        side_view.set_content(Some(&side_body));
        side_view.add_bottom_bar(&bottom);
        let sidebar = adw::NavigationPage::new(&side_view, "Servers");

        let split = adw::NavigationSplitView::builder()
            .sidebar(&sidebar)
            .content(&content)
            .min_sidebar_width(300.0)
            .max_sidebar_width(380.0)
            .sidebar_width_fraction(0.3)
            .build();

        let stack = gtk::Stack::new();
        stack.add_named(&split, Some("main"));
        let toasts = adw::ToastOverlay::new();
        toasts.set_child(Some(&stack));

        Self {
            toasts,
            stack,
            split,
            guilds,
            channels,
            channel_icon,
            channel_name,
            channel_topic,
            server_name,
            members_toggle,
            members_revealer,
            members_list,
            member_list: RefCell::default(),
            store,
            list,
            scroller,
            entry,
            attach_button,
            suggestions: RefCell::default(),
            emoji_popover,
            emoji_matches,
            emoji_chooser,
            picker,
            picker_search,
            picker_body,
            commands: RefCell::new(None),
            current_guild: RefCell::new(None),
            current_channel: RefCell::new(None),
            web_slot: gtk::Box::new(gtk::Orientation::Vertical, 0),
            images,
            plugins,
            me,
            actions_rx: RefCell::new(Some(actions_rx)),
            reply_to: RefCell::new(None),
            editing: RefCell::new(None),
            context_bar,
            context_label,
            typing_label,
            typing: RefCell::default(),
            names: RefCell::default(),
            last_typing_sent: Cell::new(None),
            state: RefCell::default(),
            silent: Cell::new(false),
            voice_panel,
            voice_status,
            voice_channel,
            mute_button,
            deafen_button,
            hang_up_button,
            call_button,
            settings_button,
            settings: RefCell::new(None),
            content_stack,
            friends_list,
            friends_tabs: tabs,
            friends_tab: Cell::new(FriendsTab::Online),
            friends_dirty: Cell::new(false),
            profile: RefCell::new(None),
            presence_dots: RefCell::default(),
            last_channel: RefCell::default(),
            char_counter: counter,
            clear_deleted_button,
            staged: RefCell::default(),
            staged_box,
            asked_users: RefCell::default(),
            user_avatar,
            user_name,
            user_tag,
            voice_target: RefCell::new(None),
            speaking: RefCell::new(Vec::new()),
        }
    }

    fn connect_signals(self: &Rc<Self>) {
        self.stack.add_named(&self.web_login_page(), Some("web-login"));
        self.build_friend_tabs();

        // Your own profile opens from the user panel.
        for widget in [self.user_avatar.upcast_ref::<gtk::Widget>(), self.user_name.upcast_ref()] {
            let ui = self.clone();
            on_click(widget, move |anchor| {
                let me = ui.me.borrow().clone();
                let user = me.as_deref().and_then(|id| ui.state.borrow().user(id).cloned());
                if let Some(user) = user {
                    ui.open_profile(anchor, user, None);
                }
            });
        }

        let ui = self.clone();
        self.guilds.connect_row_selected(move |_, row| {
            let Some(row) = row else { return };
            if ui.silent.get() {
                return;
            }
            let id = row.widget_name().to_string();
            ui.current_guild.replace(Some(id.clone()));
            ui.show_channels(&id);
            ui.enter_guild(&id);
        });

        let ui = self.clone();
        self.channels.connect_row_selected(move |_, row| {
            let Some(row) = row else { return };
            let id = row.widget_name().to_string();
            if ui.silent.get() || id.is_empty() {
                return;
            }
            if id == FRIENDS_ID {
                ui.show_friends();
                return;
            }
            ui.open_channel(&id);
        });

        // Voice channels are not "opened"; clicking one joins the call.
        let ui = self.clone();
        self.channels.connect_row_activated(move |_, row| {
            let id = row.widget_name().to_string();
            // Only guild voice channels join on click. DMs open as chats; a
            // DM call starts from the call button, never from opening it.
            let is_voice = ui
                .state
                .borrow()
                .channel(&id)
                .is_some_and(|(c, _)| c.kind == Channel::VOICE);
            if is_voice {
                ui.join_voice(&id);
            }
        });

        let ui = self.clone();
        self.mute_button.connect_toggled(move |button| {
            if let Some(image) = button.child().and_downcast::<gtk::Image>() {
                icons::set(&image, if button.is_active() { "microphone-slash" } else { "microphone" }, 20);
            }
            ui.send_mute_state();
        });
        let ui = self.clone();
        self.deafen_button.connect_toggled(move |button| {
            if let Some(image) = button.child().and_downcast::<gtk::Image>() {
                icons::set(&image, if button.is_active() { "headphones-slash" } else { "headphones" }, 20);
            }
            // Deafening implies muting, as in the official client.
            if button.is_active() {
                ui.mute_button.set_active(true);
            }
            ui.send_mute_state();
        });
        let ui = self.clone();
        self.members_toggle.connect_toggled(move |toggle| {
            ui.members_revealer.set_reveal_child(toggle.is_active());
        });
        let ui = self.clone();
        self.settings_button.connect_clicked(move |_| ui.open_settings());
        let ui = self.clone();
        self.hang_up_button.connect_clicked(move |_| ui.leave_voice());
        let ui = self.clone();
        let clearing = ui.clone();
        self.clear_deleted_button.connect_clicked(move |_| clearing.clear_deleted());
        self.call_button.connect_clicked(move |_| {
            if let Some(id) = ui.current_channel.borrow().clone() {
                ui.join_voice(&id);
                ui.send(Command::Ring(id));
            }
        });

        let ui = self.clone();
        self.emoji_chooser.connect_emoji_picked(move |_, emoji| {
            ui.insert_into_composer(emoji);
            ui.picker.popdown();
        });
        let ui = self.clone();
        self.picker.connect_show(move |_| {
            ui.picker_search.set_text("");
            ui.fill_picker();
        });
        let ui = self.clone();
        self.picker_search.connect_search_changed(move |_| ui.fill_picker());

        // `:name` completion for custom emoji.
        let ui = self.clone();
        self.entry.connect_changed(move |entry| ui.suggest_emoji(entry));
        let counter = self.char_counter.clone();
        self.entry.connect_changed(move |entry| {
            let used = entry.text().chars().count();
            counter.set_visible(crate::builtin::enabled("characterCounter") && used > 0);
            counter.set_label(&format!("{used}/2000"));
            set_class(&counter, "over", used > 2000);
        });
        let ui = self.clone();
        self.emoji_matches.connect_row_activated(move |_, row| {
            let index = row.index() as usize;
            ui.accept_emoji(index);
        });

        let ui = self.clone();
        self.attach_button.connect_clicked(move |_| ui.pick_file());

        let ui = self.clone();
        self.entry.connect_activate(move |entry| {
            let content = entry.text().trim().to_owned();
            let Some(channel_id) = ui.current_channel.borrow().clone() else { return };
            if content.is_empty() && ui.staged.borrow().is_empty() {
                return;
            }
            entry.set_text("");
            ui.typing_label_refresh();
            if let Some(id) = ui.editing.take() {
                ui.update_context();
                ui.send(Command::Edit { channel_id, id, content });
                return;
            }
            let reply_to = ui.reply_to.take().map(|(id, _)| id);
            ui.update_context();
            let Some(content) = ui.apply_send_plugins(content) else {
                ui.flush_notices();
                return;
            };
            ui.flush_notices();
            let nonce = new_nonce();
            let files = ui.staged.take();
            if !files.is_empty() {
                ui.render_staged();
                ui.toast("Uploading…");
                ui.send(Command::SendFiles { channel_id, files, content, reply_to, nonce });
                return;
            }
            ui.show_pending(&channel_id, &content, &nonce);
            ui.send(Command::Send { channel_id, content, reply_to, nonce });
        });

        // Escape abandons a reply or an edit.
        let keys = gtk::EventControllerKey::new();
        let ui = self.clone();
        keys.connect_key_pressed(move |_, key, _, _| {
            // Tab takes the top suggestion.
            if key == gtk::gdk::Key::Tab && ui.emoji_popover.is_visible() {
                ui.accept_emoji(0);
                return glib::Propagation::Stop;
            }
            if key == gtk::gdk::Key::Escape && ui.emoji_popover.is_visible() {
                ui.emoji_popover.popdown();
                return glib::Propagation::Stop;
            }
            if key == gtk::gdk::Key::Escape && ui.cancel_context() {
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        self.entry.add_controller(keys);

        // Pasting files or pictures attaches them.
        let shortcuts = gtk::ShortcutController::new();
        shortcuts.set_propagation_phase(gtk::PropagationPhase::Capture);
        let ui = self.clone();
        let paste = gtk::Shortcut::new(
            gtk::ShortcutTrigger::parse_string("<Control>v"),
            Some(gtk::CallbackAction::new(move |_, _| {
                if ui.paste_attachments() {
                    glib::Propagation::Stop
                } else {
                    glib::Propagation::Proceed
                }
            })),
        );
        shortcuts.add_shortcut(paste);
        self.entry.add_controller(shortcuts);

        // So does dropping them anywhere on the chat.
        let drop = gtk::DropTarget::new(gdk::FileList::static_type(), gdk::DragAction::COPY);
        let ui = self.clone();
        drop.connect_drop(move |_, value, _, _| {
            let Ok(list) = value.get::<gdk::FileList>() else { return false };
            for file in list.files() {
                if let Some(path) = file.path() {
                    ui.stage_file(path);
                }
            }
            true
        });
        self.content_stack.add_controller(drop);

        if let Some(close) = self.context_bar.last_child().and_downcast::<gtk::Button>() {
            let ui = self.clone();
            close.connect_clicked(move |_| {
                ui.cancel_context();
            });
        }

        // Tell Discord we are typing, at most every few seconds.
        let ui = self.clone();
        self.entry.connect_changed(move |entry| {
            let text = entry.text();
            if text.is_empty() || text.starts_with('/') || ui.editing.borrow().is_some() {
                return;
            }
            let due = ui.last_typing_sent.get().is_none_or(|t| t.elapsed() >= TYPING_SEND_EVERY);
            if let (true, Some(channel_id)) = (due, ui.current_channel.borrow().clone()) {
                ui.last_typing_sent.set(Some(Instant::now()));
                ui.send(Command::Typing(channel_id));
            }
        });

        let Some(actions) = self.actions_rx.take() else { return };
        let ui = self.clone();
        glib::spawn_future_local(async move {
            while let Ok(action) = actions.recv().await {
                ui.row_action(action);
            }
        });
    }

    /// Removes every message the MessageLogger kept in the open channel.
    fn clear_deleted(&self) {
        let mut pos = 0;
        while pos < self.store.n_items() {
            let deleted = self
                .store
                .item(pos)
                .and_downcast::<glib::BoxedAnyObject>()
                .is_some_and(|o| o.borrow::<Message>().deleted);
            if deleted {
                self.store.remove(pos);
            } else {
                pos += 1;
            }
        }
        self.refresh_deleted_button();
    }

    /// The header's "clear deleted" button shows only while there is something to clear.
    fn refresh_deleted_button(&self) {
        let any = (0..self.store.n_items()).any(|i| {
            self.store
                .item(i)
                .and_downcast::<glib::BoxedAnyObject>()
                .is_some_and(|o| o.borrow::<Message>().deleted)
        });
        self.clear_deleted_button.set_visible(any);
    }

    fn message_by_id(&self, id: &str) -> Option<Message> {
        let pos = self.position_of(id)?;
        let obj = self.store.item(pos).and_downcast::<glib::BoxedAnyObject>()?;
        let message = obj.borrow::<Message>().clone();
        Some(message)
    }

    fn row_action(self: &Rc<Self>, action: RowAction) {
        let Some(channel_id) = self.current_channel.borrow().clone() else { return };
        match action {
            RowAction::Reply(id) => {
                let Some(m) = self.message_by_id(&id) else { return };
                self.editing.replace(None);
                self.reply_to.replace(Some((id, m.author.display().to_owned())));
                self.update_context();
                self.entry.grab_focus();
            }
            RowAction::Edit(id) => {
                let Some(m) = self.message_by_id(&id) else { return };
                self.reply_to.replace(None);
                self.editing.replace(Some(id));
                self.update_context();
                self.entry.set_text(&m.content);
                self.entry.grab_focus();
                self.entry.set_position(-1);
            }
            RowAction::Delete(id) => self.confirm_delete(channel_id, id),
            RowAction::Copy(id) => {
                if let Some(m) = self.message_by_id(&id) {
                    self.entry.clipboard().set_text(&m.content);
                }
            }
            RowAction::React { id, emoji, add } => {
                self.send(Command::React { channel_id, id, emoji, add });
            }
            RowAction::Dismiss(id) => {
                if let Some(pos) = self.position_of(&id) {
                    self.store.remove(pos);
                }
                self.refresh_deleted_button();
            }
            RowAction::ClearDeleted => self.clear_deleted(),
            RowAction::Profile { id, anchor } => {
                let (Some(m), Some(anchor)) = (self.message_by_id(&id), anchor.upgrade()) else { return };
                let guild = self.current_guild.borrow().clone().filter(|g| g != DM_ID);
                self.open_profile(&anchor, m.author, guild);
            }
        }
    }

    fn confirm_delete(self: &Rc<Self>, channel_id: String, id: String) {
        let Some(m) = self.message_by_id(&id) else { return };
        let preview: String = m.content.chars().take(120).collect();
        let dialog = adw::AlertDialog::new(Some("Delete message?"), Some(&preview));
        dialog.add_responses(&[("cancel", "Cancel"), ("delete", "Delete")]);
        dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        let ui = self.clone();
        dialog.choose(Some(&self.entry), gio::Cancellable::NONE, move |response| {
            if response == "delete" {
                ui.send(Command::Delete { channel_id, id });
            }
        });
    }

    /// Shows or hides the "Replying to" / "Editing" bar to match the state.
    fn update_context(&self) {
        let text = match (&*self.editing.borrow(), &*self.reply_to.borrow()) {
            (Some(_), _) => Some("Editing message  ·  Esc to cancel".to_owned()),
            (None, Some((_, name))) => Some(format!("Replying to {name}")),
            (None, None) => None,
        };
        self.context_bar.set_visible(text.is_some());
        self.context_label.set_label(text.as_deref().unwrap_or(""));
    }

    /// Returns true if there was something to cancel.
    fn cancel_context(&self) -> bool {
        let was_editing = self.editing.take().is_some();
        let was_replying = self.reply_to.take().is_some();
        if was_editing {
            self.entry.set_text("");
        }
        self.update_context();
        was_editing || was_replying
    }

    fn remember_names<'a>(&self, messages: impl IntoIterator<Item = &'a Message>) {
        let mut names = self.names.borrow_mut();
        let mut state = self.state.borrow_mut();
        for m in messages {
            names.insert(m.author.id.clone(), m.author.display().to_owned());
            state.remember_user(&m.author);
        }
    }

    /// Rewrites "X is typing…" from the live set and schedules the next expiry.
    fn typing_label_refresh(&self) {
        let me = self.me.borrow().clone();
        let mut typing = self.typing.borrow_mut();
        typing.retain(|_, until| *until > Instant::now());
        let names = self.names.borrow();
        let state = self.state.borrow();
        let mut who: Vec<&str> = typing
            .keys()
            .filter(|id| Some(*id) != me.as_ref())
            .map(|id| {
                names
                    .get(id)
                    .map(String::as_str)
                    .or_else(|| state.user_name(id))
                    .unwrap_or("Someone")
            })
            .collect();
        who.sort_unstable();
        let text = match who.as_slice() {
            [] => " ".to_owned(),
            [one] => format!("{one} is typing…"),
            [a, b] => format!("{a} and {b} are typing…"),
            _ => "Several people are typing…".to_owned(),
        };
        self.typing_label.set_label(&text);
    }

    /// Page holding Discord's own login form (see `web_login`), with an escape
    /// hatch to the QR code and token entry for when the web page misbehaves.
    fn web_login_page(self: &Rc<Self>) -> gtk::Box {
        // Fallback for bot tokens, and for when the web page misbehaves.
        let token_entry = gtk::PasswordEntry::builder()
            .placeholder_text("Token  (bots: \"Bot <token>\")")
            .show_peek_icon(true)
            .hexpand(true)
            .build();
        let token_button = gtk::Button::with_label("Log in");
        token_button.add_css_class("suggested-action");
        let token_form = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        token_form.set_margin_top(6);
        token_form.set_margin_bottom(6);
        token_form.set_margin_start(8);
        token_form.set_margin_end(8);
        token_form.append(&token_entry);
        token_form.append(&token_button);
        let expander = gtk::Expander::new(Some("Log in with a token instead"));
        expander.set_child(Some(&token_form));
        expander.set_margin_start(8);

        let ui = self.clone();
        let submit = {
            let entry = token_entry.clone();
            move || {
                let token = entry.text().trim().to_owned();
                if token.is_empty() {
                    return;
                }
                entry.set_text("");
                ui.accept_token(token);
            }
        };
        let on_click = submit.clone();
        token_button.connect_clicked(move |_| on_click());
        token_entry.connect_activate(move |_| submit());

        let page = gtk::Box::new(gtk::Orientation::Vertical, 0);
        page.append(&self.web_slot);
        page.append(&expander);
        page
    }

    /// Shows the browser login, starting it fresh.
    fn show_login(self: &Rc<Self>) {
        self.drop_web_login();
        let ui = self.clone();
        let web_view = web_login::new(move |token| ui.accept_token(token));
        self.web_slot.append(&web_view);
        self.stack.set_visible_child_name("web-login");
    }

    /// Destroys the login web view, ending its WebKit process.
    fn drop_web_login(&self) {
        while let Some(child) = self.web_slot.first_child() {
            if let Some(web_view) = child.downcast_ref::<webkit6::WebView>() {
                web_login::shut_down(web_view);
            }
            self.web_slot.remove(&child);
        }
    }

    /// Stores the token (mode 0600) and connects with it.
    fn accept_token(self: &Rc<Self>, token: String) {
        self.drop_web_login();
        if let Err(e) = config::save_token(&token) {
            self.toast(&format!("Could not save token: {e}"));
        }
        self.start(token);
    }

    fn start(self: &Rc<Self>, token: String) {
        let (cmd_tx, cmd_rx) = async_channel::unbounded();
        let (ev_tx, ev_rx) = async_channel::unbounded();
        self.images.set_sender(Some(cmd_tx.clone()));
        self.commands.replace(Some(cmd_tx));
        backend::spawn(token, cmd_rx, ev_tx);
        self.stack.set_visible_child_name("main");

        let ui = self.clone();
        glib::spawn_future_local(async move {
            while let Ok(ev) = ev_rx.recv().await {
                ui.handle(ev);
            }
        });
    }

    /// Runs `/commands` and "send" hooks. `None` means nothing should be sent.
    fn apply_send_plugins(&self, content: String) -> Option<String> {
        if let Some(rest) = content.strip_prefix('/') {
            let (name, args) = rest.split_once(' ').unwrap_or((rest, ""));
            if let Some(outcome) = self.plugins.run_command(name, args.trim()) {
                return outcome;
            }
            if let Some(text) = crate::builtin::command(name, args.trim()) {
                return Some(text);
            }
        }
        let content = self.plugins.on_send(content)?;
        let own: Vec<String> = {
            let state = self.state.borrow();
            let guild = self.current_guild.borrow();
            guild
                .as_deref()
                .and_then(|g| state.guild(g))
                .map(|g| g.data.emojis.iter().map(|e| e.id.clone()).collect())
                .unwrap_or_default()
        };
        Some(crate::builtin::rewrite_outgoing(&content, &|id| own.iter().any(|e| e == id)))
    }

    /// Shows toasts that plugins queued with `discord.notify`.
    fn flush_notices(&self) {
        for notice in self.plugins.take_notices() {
            self.toast(&notice);
        }
    }

    fn send(&self, cmd: Command) {
        if let Some(tx) = self.commands.borrow().as_ref() {
            let _ = tx.try_send(cmd);
        }
    }

    fn toast(&self, msg: &str) {
        self.toasts.add_toast(adw::Toast::new(msg));
    }

    /// Reports a backend failure. A rejected token is not retryable, so the
    /// stored copy is discarded and the user is sent back to the login page;
    /// leaving it would fail the same way on every launch.
    fn fail(self: &Rc<Self>, error: Error) {
        if error.is_auth_failure() {
            config::forget_token();
            self.commands.replace(None);
            self.images.set_sender(None);
            self.show_login();
        }
        self.toast(&error.to_string());
    }

    fn handle(self: &Rc<Self>, ev: Event) {
        self.flush_notices();
        match ev {
            Event::Ready(ready) => {
                let name = ready.user.display().to_owned();
                self.user_name.set_label(&name);
                self.user_tag.set_label(&ready.user.username);
                self.user_avatar.set_text(Some(&name));
                load_avatar(&self.images, &ready.user.avatar_url(), &self.user_avatar);
                self.me.replace(Some(ready.user.id.clone()));
                self.state.replace(State::from_ready(*ready));
                self.toast(&format!("Connected as {name}"));
                self.rebuild_guilds();
                if self.current_guild.borrow().is_none() {
                    if let Some(first) = self.guilds.row_at_index(0) {
                        self.guilds.select_row(Some(&first));
                    }
                }
            }
            Event::GuildCreate(guild) => {
                self.state.borrow_mut().add_guild(*guild);
                self.rebuild_guilds();
            }
            Event::GuildDelete(id) => {
                self.state.borrow_mut().remove_guild(&id);
                self.rebuild_guilds();
            }
            Event::ChannelUpsert(channel) => {
                self.state.borrow_mut().upsert_channel(channel);
                self.refresh_channels();
            }
            Event::ChannelDelete(channel) => {
                self.state.borrow_mut().delete_channel(&channel);
                self.refresh_channels();
            }
            Event::MessageAck { channel_id, message_id, mention_count } => {
                self.state.borrow_mut().ack(&channel_id, &message_id, mention_count);
                self.refresh_badges();
            }
            Event::History { channel_id, messages } => {
                if !self.is_current(&channel_id) {
                    return;
                }
                self.store.remove_all();
                self.typing.borrow_mut().clear();
                self.typing_label_refresh();
                self.remember_names(&messages);
                let items: Vec<glib::BoxedAnyObject> =
                    messages.into_iter().map(glib::BoxedAnyObject::new).collect();
                self.store.extend_from_slice(&items);
                self.scroll_to_end();
                self.ack_current();
            }
            Event::MessageCreate(m) => {
                if let Ok(sent) = glib::DateTime::from_iso8601(&m.timestamp, None) {
                    let ms = glib::real_time() / 1000 - sent.to_unix() * 1000 - i64::from(sent.microsecond()) / 1000;
                    tracing::debug!(ms, "message delivery delay (server timestamp to now)");
                }
                let viewing = self.is_current(&m.channel_id);
                self.notify_message(&m, viewing);
                self.state.borrow_mut().on_message(&m, viewing);
                if !viewing {
                    self.refresh_badges();
                    if self.current_guild.borrow().as_deref() == Some(DM_ID) {
                        self.refresh_channels();
                    }
                    return;
                }
                self.typing.borrow_mut().remove(&m.author.id);
                self.remember_names([&m]);
                self.typing_label_refresh();
                let stick = self.near_bottom();
                // Our own message arriving: swap it for the dimmed local copy.
                if let Some(at) = m.nonce.as_deref().and_then(|n| self.position_of(n)) {
                    tracing::debug!("pending message confirmed");
                    self.store.splice(at, 1, &[glib::BoxedAnyObject::new(m)]);
                    if stick {
                        self.scroll_to_end();
                    }
                    self.ack_current();
                    return;
                }
                self.store.append(&glib::BoxedAnyObject::new(m));
                let excess = self.store.n_items().saturating_sub(MAX_MESSAGES);
                if excess > 0 {
                    self.store.splice(0, excess, &[] as &[glib::BoxedAnyObject]);
                }
                if stick {
                    self.scroll_to_end();
                }
                self.ack_current();
            }
            Event::MessageUpdate(m) => {
                if let Some(pos) = self.position_of(&m.id) {
                    self.store.splice(pos, 1, &[glib::BoxedAnyObject::new(m)]);
                }
            }
            Event::MessageDelete { channel_id, id } => {
                if self.is_current(&channel_id) {
                    if let Some(pos) = self.position_of(&id) {
                        match self.message_by_id(&id).filter(|_| crate::builtin::enabled("messageLogger")) {
                            Some(mut kept) => {
                                kept.deleted = true;
                                self.store.splice(pos, 1, &[glib::BoxedAnyObject::new(kept)]);
                                self.refresh_deleted_button();
                            }
                            None => self.store.remove(pos),
                        }
                    }
                }
            }
            Event::MessageStale { channel_id, id } => {
                if self.is_current(&channel_id) && self.position_of(&id).is_some() {
                    self.send(Command::Refresh { channel_id, id });
                }
            }
            Event::Typing { channel_id, user_id } => {
                if !self.is_current(&channel_id) {
                    return;
                }
                let until = Instant::now() + Duration::from_secs(TYPING_SECS);
                self.typing.borrow_mut().insert(user_id, until);
                self.typing_label_refresh();
                let ui = self.clone();
                glib::timeout_add_local_once(Duration::from_secs(TYPING_SECS + 1), move || {
                    ui.typing_label_refresh();
                });
            }
            Event::Users(users) => {
                for user in &users {
                    self.state.borrow_mut().remember_user(user);
                }
                if !users.is_empty() {
                    self.refresh_channels();
                }
            }
            Event::Presence(presence) => {
                let id = presence.user.id.clone();
                self.state.borrow_mut().set_presence(presence);
                self.update_presence_dots(&id);
                self.mark_friends_dirty();
            }
            Event::RelationshipUpdate(relationship) => {
                self.state.borrow_mut().update_relationship(relationship);
                self.mark_friends_dirty();
            }
            Event::RelationshipRemove(id) => {
                self.state.borrow_mut().remove_relationship(&id);
                self.mark_friends_dirty();
            }
            Event::Profile { user_id, value } => {
                if let Some(card) = self.profile.borrow().as_ref().filter(|c| c.user_id() == user_id) {
                    card.fill(&value);
                }
            }
            Event::DmOpened(channel) => {
                let id = channel.id.clone();
                self.state.borrow_mut().upsert_channel(channel);
                self.go_to_dm(&id);
            }
            Event::Panel(panel, value) => {
                if let Some(handle) = self.settings.borrow().as_ref() {
                    handle.apply(panel, &value);
                }
            }
            Event::SendFailed { nonce, error } => {
                if let Some(at) = self.position_of(&nonce) {
                    self.store.remove(at);
                }
                self.fail(error);
            }
            Event::MemberList(update) => {
                let current = self.current_guild.borrow().clone();
                if current.as_deref() == Some(update.guild_id.as_str()) {
                    self.member_list.borrow_mut().apply(*update);
                    self.render_members();
                }
            }
            Event::VoiceState(update) => {
                self.state.borrow_mut().apply_voice_state(&update);
                self.refresh_channels();
            }
            Event::VoiceServer { .. } => {}
            Event::VoiceLink(Ok(())) => {
                self.voice_status.set_label("Voice Connected");
                self.voice_status.add_css_class("connected");
            }
            Event::VoiceLink(Err(reason)) => {
                self.voice_target.replace(None);
                self.voice_panel.set_visible(false);
                self.speaking.borrow_mut().clear();
                self.toast(&reason);
                self.refresh_speaking();
            }
            Event::VoiceSpeaking(ids) => {
                self.speaking.replace(ids);
                self.refresh_speaking();
            }
            Event::Image { url, bytes } => self.images.deliver(url, bytes),
            Event::ImageFailed { url } => self.images.fail(url),
            Event::Failed(e) => self.fail(e),
        }
    }

    /// Rebuilds the server strip from `state`, keeping the selection.
    fn rebuild_guilds(self: &Rc<Self>) {
        let keep = self.current_guild.borrow().clone();
        self.silent.set(true);
        self.guilds.remove_all();
        self.guilds.append(&self.strip_row(DM_ID, "Direct Messages", None));
        for guild in &self.state.borrow().guilds {
            let data = &guild.data;
            self.guilds.append(&self.strip_row(&data.id, &data.name, data.icon_url()));
        }
        if let Some(row) = keep.and_then(|id| find_row(&self.guilds, &id)) {
            self.guilds.select_row(Some(&row));
        }
        self.silent.set(false);
        self.refresh_badges();
    }

    /// One entry of the server strip: icon, an unread pill on the left and a
    /// mention badge in the corner. `refresh_badges` keeps the last two current.
    fn strip_row(&self, id: &str, name: &str, icon: Option<String>) -> gtk::ListBoxRow {
        let avatar = adw::Avatar::new(48, Some(name), true);
        if id == DM_ID {
            avatar.set_show_initials(false);
            avatar.set_icon_name(Some("mail-message-new-symbolic"));
        }
        if let Some(url) = icon {
            load_avatar(&self.images, &url, &avatar);
        }
        let pill = gtk::Box::new(gtk::Orientation::Vertical, 0);
        pill.add_css_class("pill");
        pill.set_widget_name("pill");
        pill.set_halign(gtk::Align::Start);
        pill.set_valign(gtk::Align::Center);
        avatar.set_margin_start(12);
        let badge = gtk::Label::new(None);
        badge.add_css_class("badge");
        badge.set_widget_name("badge");
        badge.set_halign(gtk::Align::End);
        badge.set_valign(gtk::Align::End);
        badge.set_visible(false);

        let overlay = gtk::Overlay::new();
        overlay.set_child(Some(&avatar));
        overlay.add_overlay(&pill);
        overlay.add_overlay(&badge);

        let row = gtk::ListBoxRow::new();
        row.set_widget_name(id);
        row.set_tooltip_text(Some(name));
        row.set_child(Some(&overlay));
        row
    }

    /// Fills the channel pane for a server, or the DM list for `@me`.
    fn show_channels(self: &Rc<Self>, guild_id: &str) {
        let keep = self.current_channel.borrow().clone();
        let in_guild = guild_id != DM_ID;
        let server_title = if in_guild {
            self.state.borrow().guild(guild_id).map(|g| g.data.name.clone()).unwrap_or_default()
        } else {
            "Direct Messages".to_owned()
        };
        self.server_name.set_label(&server_title);
        self.member_list.replace(MemberList::default());
        self.members_list.remove_all();
        self.members_toggle.set_visible(in_guild);
        self.members_revealer.set_reveal_child(in_guild && self.members_toggle.is_active());
        let scroll = self.channels.ancestor(gtk::ScrolledWindow::static_type()).and_downcast::<gtk::ScrolledWindow>();
        let scroll_to = scroll.as_ref().map(|s| s.vadjustment().value());
        self.silent.set(true);
        self.channels.remove_all();
        {
            let state = self.state.borrow();
            if guild_id == DM_ID {
                self.channels.append(&self.friends_row(&state));
                self.presence_dots.borrow_mut().clear();
                for dm in &state.dms {
                    self.channels.append(&self.dm_row(dm, &state));
                }
            } else if let Some(guild) = state.guild(guild_id) {
                for entry in guild.entries(&state.me) {
                    match entry {
                        Entry::Category(c) => self.channels.append(&category_row(&c)),
                        Entry::Channel(c) => {
                            self.channels.append(&channel_row(&c));
                            for member in state.voice_members(&c.id) {
                                self.channels.append(&self.voice_member_row(&member));
                            }
                        }
                    }
                }
            }
        }
        if let Some(row) = keep.and_then(|id| find_row(&self.channels, &id)) {
            self.channels.select_row(Some(&row));
        }
        self.silent.set(false);
        self.refresh_badges();
        self.refresh_speaking();
        self.fetch_unknown_voice_users(guild_id);
        // Rebuilding resets the scroll position; put it back once laid out.
        if let (Some(scroll), Some(value)) = (scroll, scroll_to) {
            glib::idle_add_local_once(move || scroll.vadjustment().set_value(value));
        }
    }

    /// READY lists who is in voice but not who they are; ask Discord once.
    fn fetch_unknown_voice_users(&self, guild_id: &str) {
        let wanted = {
            let state = self.state.borrow();
            let Some(guild) = state.guild(guild_id) else { return };
            let channels: Vec<&str> = guild.data.channels.iter().map(|c| c.id.as_str()).collect();
            state.unknown_voice_users(&channels)
        };
        let mut asked = self.asked_users.borrow_mut();
        let fresh: Vec<String> = wanted.into_iter().filter(|id| asked.insert(id.clone())).take(40).collect();
        if !fresh.is_empty() {
            drop(asked);
            self.send(Command::FetchUsers { guild_id: guild_id.to_owned(), user_ids: fresh });
        }
    }

    fn voice_member_row(self: &Rc<Self>, member: &VoiceEntry) -> gtk::ListBoxRow {
        let avatar = adw::Avatar::new(24, Some(&member.name), true);
        if let Some(url) = &member.avatar_url {
            load_avatar(&self.images, url, &avatar);
        }
        let name = gtk::Label::builder()
            .label(&member.name)
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(pango::EllipsizeMode::End)
            .build();
        let inner = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        inner.set_margin_start(24);
        inner.append(&avatar);
        inner.append(&name);
        if member.deafened {
            inner.append(&icons::image("headphones-slash", 16));
        } else if member.muted {
            inner.append(&icons::image("microphone-slash", 16));
        }
        let row = gtk::ListBoxRow::new();
        row.set_widget_name(&format!("{VOICE_ROW}{}", member.user_id));
        row.set_selectable(false);
        row.set_activatable(false);
        row.add_css_class("voice-member");
        row.set_child(Some(&inner));
        let (ui, id) = (self.clone(), member.user_id.clone());
        on_click(&row, move |anchor| {
            let user = ui.state.borrow().user(&id).cloned();
            let guild = ui.current_guild.borrow().clone().filter(|g| g != DM_ID);
            if let Some(user) = user {
                ui.open_profile(anchor, user, guild);
            }
        });
        row
    }

    /// Re-renders the channel pane for whichever server is open.
    fn refresh_channels(self: &Rc<Self>) {
        if let Some(id) = self.current_guild.borrow().clone() {
            self.show_channels(&id);
        }
    }

    fn dm_row(&self, dm: &Channel, state: &State) -> gtk::ListBoxRow {
        let label = dm.label();
        let avatar = adw::Avatar::new(32, Some(&label), true);
        let url = match dm.recipients.as_slice() {
            [one] if dm.kind == Channel::DM => Some(one.avatar_url()),
            _ => dm
                .icon
                .as_ref()
                .map(|hash| format!("https://cdn.discordapp.com/channel-icons/{}/{hash}.png?size=128", dm.id)),
        };
        if let Some(url) = url {
            load_avatar(&self.images, &url, &avatar);
        }
        let name = gtk::Label::builder()
            .label(&label)
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(pango::EllipsizeMode::End)
            .build();
        let badge = badge_label();
        let inner = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        inner.set_margin_top(3);
        inner.set_margin_bottom(3);
        // One-to-one chats show whether the other person is around.
        let avatar_widget: gtk::Widget = match dm.recipients.as_slice() {
            [one] if dm.kind == Channel::DM => {
                let (stack, dot) = with_status_dot(&avatar, state.status(&one.id));
                self.presence_dots.borrow_mut().entry(one.id.clone()).or_default().push(dot.downgrade());
                stack.upcast()
            }
            _ => avatar.clone().upcast(),
        };
        inner.append(&avatar_widget);
        inner.append(&name);
        inner.append(&badge);
        let row = gtk::ListBoxRow::new();
        row.set_widget_name(&dm.id);
        row.set_child(Some(&inner));
        row
    }

    /// Updates unread pills, mention badges and bold/muted styling everywhere.
    fn refresh_badges(&self) {
        let state = self.state.borrow();
        for row in rows(&self.guilds) {
            let id = row.widget_name();
            let unread = if id == DM_ID { state.dm_unread() } else { state.guild_unread(&id) };
            let Some(overlay) = row.child().and_downcast::<gtk::Overlay>() else { continue };
            let mut child = overlay.first_child();
            while let Some(widget) = child {
                match widget.widget_name().as_str() {
                    "pill" => set_class(&widget, "unread", unread.unread || unread.mentions > 0),
                    "badge" => set_badge(&widget, unread.mentions),
                    _ => {}
                }
                child = widget.next_sibling();
            }
        }
        for row in rows(&self.channels) {
            let id = row.widget_name();
            if id.is_empty() || id.starts_with(VOICE_ROW) {
                continue;
            }
            let Unread { unread, mentions } = state.unread(&id);
            set_class(&row, "unread", unread || mentions > 0);
            set_class(&row, "muted", state.is_muted(&id));
            if let Some(badge) = row.child().and_then(last_child) {
                set_badge(&badge, mentions);
            }
        }
    }

    /// Opens a channel: loads history and sets up the composer for what the
    /// user is allowed to do there.
    fn open_channel(self: &Rc<Self>, id: &str) {
        let (title, topic, can_send) = {
            let state = self.state.borrow();
            let found = state.channel(id);
            let title = found.as_ref().map(|(c, _)| c.label()).unwrap_or_default();
            let topic = found.as_ref().and_then(|(c, _)| c.topic.clone()).unwrap_or_default();
            (title, topic, state.permissions_in(id) & SEND_MESSAGES != 0)
        };
        let icon = match self.state.borrow().channel(id).map(|(c, _)| c.kind) {
            Some(Channel::DM | Channel::GROUP_DM) => "at",
            Some(Channel::VOICE) => "voice-normal",
            Some(Channel::ANNOUNCEMENT) => "announcements",
            _ => "hashmark",
        };
        icons::set(&self.channel_icon, icon, 24);
        self.content_stack.set_visible_child_name("chat");
        if let Some(guild) = self.state.borrow().channel(id).and_then(|(c, _)| c.guild_id.clone()) {
            self.last_channel.borrow_mut().insert(guild, id.to_owned());
        }
        self.channel_name.set_label(&title);
        self.channel_topic.set_label(topic.lines().next().unwrap_or(""));
        self.channel_topic.set_tooltip_text((!topic.is_empty()).then_some(topic.as_str()));
        self.store.remove_all();
        self.refresh_deleted_button();
        self.cancel_context();
        self.current_channel.replace(Some(id.to_owned()));
        self.entry.set_sensitive(can_send);
        self.attach_button.set_sensitive(can_send);
        self.entry.set_placeholder_text(Some(if can_send {
            "Message"
        } else {
            "You do not have permission to send messages in this channel"
        }));
        self.split.set_show_content(true);
        let is_dm = self.state.borrow().channel(id).is_some_and(|(c, _)| matches!(c.kind, Channel::DM | Channel::GROUP_DM));
        self.call_button.set_visible(is_dm);
        if let Some(guild) = self.current_guild.borrow().clone().filter(|g| g != DM_ID) {
            self.send(Command::SubscribeMembers { guild_id: guild, channel_id: id.to_owned() });
        }
        self.send(Command::LoadHistory(id.to_owned()));
    }

    /// Opens a profile card next to `anchor`.
    fn open_profile(self: &Rc<Self>, anchor: &gtk::Widget, user: User, guild_id: Option<String>) {
        let (subject, send) = {
            let state = self.state.borrow();
            let roles = guild_id
                .as_deref()
                .and_then(|id| state.guild(id))
                .map(|g| g.data.roles.clone())
                .unwrap_or_default();
            let subject = profile::Subject {
                status: state.status(&user.id).to_owned(),
                activity: state.activity_text(&user.id),
                relationship: state.relationship(&user.id).map(|r| r.kind),
                is_me: self.me.borrow().as_deref() == Some(user.id.as_str()),
                guild_names: state.guilds.iter().map(|g| (g.data.id.clone(), g.data.name.clone())).collect(),
                user,
                guild_id,
                roles,
            };
            let ui = self.clone();
            (subject, Rc::new(move |command| ui.send(command)) as Rc<dyn Fn(Command)>)
        };
        let card = profile::show(anchor, subject, self.images.clone(), send);
        self.profile.replace(Some(card));
    }

    /// Switches to a direct message conversation, opening the list if needed.
    fn go_to_dm(self: &Rc<Self>, channel_id: &str) {
        self.current_guild.replace(Some(DM_ID.to_owned()));
        self.silent.set(true);
        if let Some(row) = self.guilds.row_at_index(0) {
            self.guilds.select_row(Some(&row));
        }
        self.silent.set(false);
        self.show_channels(DM_ID);
        if let Some(row) = find_row(&self.channels, channel_id) {
            self.channels.select_row(Some(&row));
        } else {
            self.open_channel(channel_id);
        }
    }

    /// Updates the status dots of one person everywhere they appear.
    fn update_presence_dots(&self, user_id: &str) {
        let status = self.state.borrow().status(user_id).to_owned();
        if let Some(dots) = self.presence_dots.borrow_mut().get_mut(user_id) {
            dots.retain(|dot| dot.upgrade().is_some());
            for dot in dots.iter().filter_map(glib::WeakRef::upgrade) {
                crate::status::set(&dot, &status);
            }
        }
    }

    /// The "Friends" entry at the top of the direct message list.
    fn friends_row(&self, state: &State) -> gtk::ListBoxRow {
        let online = state
            .related(Relationship::FRIEND)
            .iter()
            .filter(|(_, u)| state.status(&u.id) != "offline")
            .count();
        let pending = state.related(Relationship::INCOMING).len();
        let label = gtk::Label::builder().label("Friends").xalign(0.0).hexpand(true).build();
        let inner = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        inner.set_margin_top(6);
        inner.set_margin_bottom(6);
        inner.append(&icons::image("group", 24));
        inner.append(&label);
        if pending > 0 {
            let badge = badge_label();
            badge.set_label(&pending.to_string());
            badge.set_visible(true);
            inner.append(&badge);
        }
        inner.set_tooltip_text(Some(&format!("{online} online")));
        let row = gtk::ListBoxRow::new();
        row.set_widget_name(FRIENDS_ID);
        row.set_child(Some(&inner));
        row
    }

    fn build_friend_tabs(self: &Rc<Self>) {
        let mut first: Option<gtk::ToggleButton> = None;
        for (tab, title) in [
            (FriendsTab::Online, "Online"),
            (FriendsTab::All, "All"),
            (FriendsTab::Pending, "Pending"),
            (FriendsTab::Blocked, "Blocked"),
        ] {
            let button = gtk::ToggleButton::with_label(title);
            button.add_css_class("flat");
            button.set_group(first.as_ref());
            if first.is_none() {
                button.set_active(true);
                first = Some(button.clone());
            }
            let ui = self.clone();
            button.connect_toggled(move |b| {
                if b.is_active() {
                    ui.friends_tab.set(tab);
                    ui.refresh_friends();
                }
            });
            self.friends_tabs.append(&button);
        }
        let add = gtk::Entry::builder()
            .placeholder_text("Add friend by username")
            .hexpand(true)
            .halign(gtk::Align::End)
            .width_chars(26)
            .build();
        let ui = self.clone();
        add.connect_activate(move |entry| {
            let name = entry.text().trim().to_owned();
            if !name.is_empty() {
                entry.set_text("");
                ui.send(Command::AddFriend(name));
                ui.toast("Friend request sent");
            }
        });
        self.friends_tabs.append(&add);
    }

    /// Discord opens a server on the channel you last used there, or its
    /// first text channel; Home goes back to the Friends page.
    fn enter_guild(self: &Rc<Self>, guild_id: &str) {
        let current_in_guild = {
            let state = self.state.borrow();
            let current = self.current_channel.borrow();
            current.as_deref().and_then(|c| state.channel(c)).is_some_and(|(c, _)| {
                if guild_id == DM_ID {
                    c.guild_id.is_none()
                } else {
                    c.guild_id.as_deref() == Some(guild_id)
                }
            })
        };
        if current_in_guild {
            return;
        }
        if guild_id == DM_ID {
            self.show_friends();
            return;
        }
        self.leave_friends();
        let target = self.last_channel.borrow().get(guild_id).cloned().or_else(|| {
            let state = self.state.borrow();
            state.guild(guild_id)?.entries(&state.me).into_iter().find_map(|e| match e {
                Entry::Channel(c) if matches!(c.kind, Channel::TEXT | Channel::ANNOUNCEMENT) => Some(c.id),
                _ => None,
            })
        });
        self.current_channel.replace(None);
        self.store.remove_all();
        self.channel_name.set_label("");
        self.channel_topic.set_label("");
        self.entry.set_sensitive(false);
        self.call_button.set_visible(false);
        match target {
            Some(id) => {
                if let Some(row) = find_row(&self.channels, &id) {
                    self.channels.select_row(Some(&row));
                } else {
                    self.open_channel(&id);
                }
            }
            None => {}
        }
    }

    /// Back from the Friends page to a chat view.
    fn leave_friends(&self) {
        if self.content_stack.visible_child_name().as_deref() == Some("friends") {
            self.content_stack.set_visible_child_name("chat");
            self.channel_name.set_label("");
            self.channel_topic.set_label("");
            self.store.remove_all();
            self.entry.set_sensitive(false);
        }
    }

    fn show_friends(self: &Rc<Self>) {
        self.current_channel.replace(None);
        self.content_stack.set_visible_child_name("friends");
        icons::set(&self.channel_icon, "group", 24);
        self.channel_name.set_label("Friends");
        self.channel_topic.set_label("");
        self.call_button.set_visible(false);
        self.refresh_friends();
    }

    /// Redraws the friends list soon, once, however many events asked for it.
    fn mark_friends_dirty(self: &Rc<Self>) {
        if self.content_stack.visible_child_name().as_deref() != Some("friends") || self.friends_dirty.replace(true) {
            return;
        }
        let ui = self.clone();
        glib::timeout_add_local_once(std::time::Duration::from_millis(300), move || {
            ui.friends_dirty.set(false);
            ui.refresh_friends();
        });
    }

    fn refresh_friends(self: &Rc<Self>) {
        self.friends_list.remove_all();
        let tab = self.friends_tab.get();
        let state = self.state.borrow();
        let mut shown = 0;
        let kinds: &[u8] = match tab {
            FriendsTab::Online | FriendsTab::All => &[Relationship::FRIEND],
            FriendsTab::Pending => &[Relationship::INCOMING, Relationship::OUTGOING],
            FriendsTab::Blocked => &[Relationship::BLOCKED],
        };
        for &kind in kinds {
            for (relationship, user) in state.related(kind) {
                let status = state.status(&user.id);
                if tab == FriendsTab::Online && status == "offline" {
                    continue;
                }
                let subtitle = match kind {
                    Relationship::INCOMING => "Incoming friend request".to_owned(),
                    Relationship::OUTGOING => "Outgoing friend request".to_owned(),
                    Relationship::BLOCKED => "Blocked".to_owned(),
                    _ => state.activity_text(&user.id).unwrap_or_else(|| status_label(status).to_owned()),
                };
                self.friends_list.append(&self.friend_row(user, relationship, &subtitle, status));
                shown += 1;
            }
        }
        if shown == 0 {
            let empty = gtk::Label::new(Some(match tab {
                FriendsTab::Online => "No friends online right now.",
                FriendsTab::All => "You have no friends yet.",
                FriendsTab::Pending => "There are no pending friend requests.",
                FriendsTab::Blocked => "You have not blocked anyone.",
            }));
            empty.set_margin_top(48);
            empty.add_css_class("dim-label");
            let row = gtk::ListBoxRow::new();
            row.set_activatable(false);
            row.set_child(Some(&empty));
            self.friends_list.append(&row);
        }
    }

    fn friend_row(self: &Rc<Self>, user: &User, relationship: &Relationship, subtitle: &str, status: &str) -> gtk::ListBoxRow {
        let avatar = adw::Avatar::new(36, Some(user.display()), true);
        load_avatar(&self.images, &user.avatar_url(), &avatar);
        let (stack, dot) = with_status_dot(&avatar, status);
        self.presence_dots.borrow_mut().entry(user.id.clone()).or_default().push(dot.downgrade());
        let name = gtk::Label::builder().label(user.display()).xalign(0.0).build();
        name.add_css_class("member-name");
        let detail = gtk::Label::builder().label(subtitle).xalign(0.0).ellipsize(pango::EllipsizeMode::End).build();
        detail.add_css_class("profile-tag");
        let labels = gtk::Box::new(gtk::Orientation::Vertical, 0);
        labels.set_hexpand(true);
        labels.set_valign(gtk::Align::Center);
        labels.append(&name);
        labels.append(&detail);
        let inner = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        inner.set_margin_top(6);
        inner.set_margin_bottom(6);
        inner.set_margin_start(20);
        inner.set_margin_end(20);
        inner.append(&stack);
        inner.append(&labels);

        let id = user.id.clone();
        match relationship.kind {
            Relationship::INCOMING => {
                let accept = icons::button("checkmark-small", "Accept");
                let (ui, who) = (self.clone(), id.clone());
                accept.connect_clicked(move |_| {
                    ui.send(Command::Relate { user_id: who.clone(), action: crate::model::RelateAction::Add });
                });
                inner.append(&accept);
                let decline = icons::button("x-small", "Ignore");
                let (ui, who) = (self.clone(), id.clone());
                decline.connect_clicked(move |_| {
                    ui.send(Command::Relate { user_id: who.clone(), action: crate::model::RelateAction::Remove });
                });
                inner.append(&decline);
            }
            Relationship::OUTGOING | Relationship::BLOCKED => {
                let cancel = icons::button("x-small", if relationship.kind == Relationship::BLOCKED { "Unblock" } else { "Cancel" });
                let (ui, who) = (self.clone(), id.clone());
                cancel.connect_clicked(move |_| {
                    ui.send(Command::Relate { user_id: who.clone(), action: crate::model::RelateAction::Remove });
                });
                inner.append(&cancel);
            }
            _ => {
                let message = icons::button("forum", "Message");
                let (ui, who) = (self.clone(), id.clone());
                message.connect_clicked(move |_| ui.open_dm_with(&who));
                inner.append(&message);
            }
        }
        let row = gtk::ListBoxRow::new();
        row.set_activatable(false);
        row.set_child(Some(&inner));
        let (ui, user) = (self.clone(), user.clone());
        on_click(&labels, move |anchor| ui.open_profile(anchor, user.clone(), None));
        row
    }

    /// Opens the DM with a user: the existing channel, or a new one.
    fn open_dm_with(self: &Rc<Self>, user_id: &str) {
        let existing = self
            .state
            .borrow()
            .dms
            .iter()
            .find(|c| c.kind == Channel::DM && c.recipients.iter().any(|u| u.id == user_id))
            .map(|c| c.id.clone());
        match existing {
            Some(id) => self.go_to_dm(&id),
            None => self.send(Command::OpenDm(user_id.to_owned())),
        }
    }

    /// Shows a desktop notification for a DM or a mention, unless the person is
    /// already looking at that channel or the channel is muted.
    fn notify_message(&self, m: &Message, viewing: bool) {
        let Some(window) = self.stack.root().and_downcast::<gtk::Window>() else { return };
        let me = self.me.borrow().clone();
        if !prefs::get().notifications
            || me.as_deref() == Some(m.author.id.as_str())
            || (viewing && window.is_active())
        {
            return;
        }
        let state = self.state.borrow();
        let is_dm = state
            .channel(&m.channel_id)
            .is_some_and(|(c, _)| matches!(c.kind, Channel::DM | Channel::GROUP_DM));
        let mentioned = m.mention_everyone || m.mentions.iter().any(|u| Some(u.id.as_str()) == me.as_deref());
        if !(is_dm || mentioned) || state.is_muted(&m.channel_id) {
            return;
        }
        let Some(app) = window.application() else { return };
        let body = if m.content.is_empty() { "Sent an attachment".to_owned() } else { emoji_text::plain(&mention_text(m)) };
        let notification = gio::Notification::new(m.author.display());
        notification.set_body(Some(&body));
        app.send_notification(Some(&format!("message-{}", m.id)), &notification);
    }

    /// Redraws the right-hand member list from what the server has sent.
    fn render_members(self: &Rc<Self>) {
        self.members_list.remove_all();
        let state = self.state.borrow();
        let list = self.member_list.borrow();
        let roles = state.guild(&list.guild_id).map(|g| g.data.roles.clone()).unwrap_or_default();
        for item in list.items() {
            match item {
                Item::Group(group) => {
                    let name = match group.id.as_str() {
                        "online" => "Online".to_owned(),
                        "offline" => "Offline".to_owned(),
                        id => roles.iter().find(|r| r.id == id).map_or_else(|| "Members".to_owned(), |r| r.name.clone()),
                    };
                    let label = gtk::Label::builder()
                        .label(format!("{} — {}", name.to_uppercase(), group.count))
                        .xalign(0.0)
                        .build();
                    label.add_css_class("member-group");
                    let row = gtk::ListBoxRow::new();
                    row.set_activatable(false);
                    row.set_child(Some(&label));
                    self.members_list.append(&row);
                }
                Item::Member(member) => self.members_list.append(&self.member_row(member, &roles)),
            }
        }
    }

    fn member_row(self: &Rc<Self>, member: &MemberItem, roles: &[Role]) -> gtk::ListBoxRow {
        let avatar = adw::Avatar::new(32, Some(member.name()), true);
        load_avatar(&self.images, &member.user.avatar_url(), &avatar);
        let dot = crate::status::dot(member.status());
        let overlay = gtk::Overlay::new();
        overlay.set_child(Some(&avatar));
        overlay.add_overlay(&dot);

        let name = gtk::Label::builder()
            .label(member.name())
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(pango::EllipsizeMode::End)
            .build();
        name.add_css_class("member-name");
        // The name takes the colour of the member's highest coloured role.
        let colour = roles
            .iter()
            .filter(|r| r.color != 0 && member.roles.contains(&r.id))
            .max_by_key(|r| r.position)
            .map(|r| r.color);
        if let Some(rgb) = colour {
            name.set_markup(&format!(
                r##"<span foreground="#{rgb:06x}">{}</span>"##,
                glib::markup_escape_text(member.name())
            ));
        }
        let inner = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        inner.append(&overlay);
        inner.append(&name);
        let row = gtk::ListBoxRow::new();
        row.set_activatable(false);
        row.add_css_class("member-row");
        set_class(&row, "offline", member.status() == "offline");
        row.set_child(Some(&inner));
        let (ui, user) = (self.clone(), member.user.clone());
        on_click(&row, move |anchor| {
            let guild = ui.current_guild.borrow().clone();
            ui.open_profile(anchor, user.clone(), guild);
        });
        row
    }

    fn insert_into_composer(&self, text: &str) {
        let position = self.entry.position();
        self.entry.insert_text(text, &mut { position });
        self.entry.set_position(position + text.chars().count() as i32);
        self.entry.grab_focus();
    }

    /// Fills the emoji picker: search results when something is typed,
    /// otherwise the open server's emoji, with the other servers collapsed
    /// (loading every server's emoji at once would mean thousands of images).
    fn fill_picker(self: &Rc<Self>) {
        while let Some(child) = self.picker_body.first_child() {
            self.picker_body.remove(&child);
        }
        let query = self.picker_search.text().to_string();
        let current = self.current_guild.borrow().clone();
        let state = self.state.borrow();
        if !query.is_empty() {
            let found = state.emoji_matches(&query, current.as_deref(), 80);
            if found.is_empty() {
                self.picker_body.append(&gtk::Label::new(Some("No emoji found")));
            } else {
                self.picker_body.append(&self.emoji_grid(&found));
            }
            return;
        }
        let mut guilds: Vec<_> = state.guilds.iter().filter(|g| !g.data.emojis.is_empty()).collect();
        guilds.sort_by_key(|g| Some(g.data.id.as_str()) != current.as_deref());
        if guilds.is_empty() {
            self.picker_body.append(&gtk::Label::new(Some("No server emoji")));
        }
        for (n, guild) in guilds.into_iter().enumerate() {
            let emojis: Vec<GuildEmoji> = guild.data.emojis.iter().filter(|e| e.available).cloned().collect();
            let expander = gtk::Expander::new(Some(&format!("{} ({})", guild.data.name, emojis.len())));
            let built = Cell::new(false);
            let ui = self.clone();
            expander.connect_expanded_notify(move |expander| {
                if expander.is_expanded() && !built.replace(true) {
                    expander.set_child(Some(&ui.emoji_grid(&emojis)));
                }
            });
            self.picker_body.append(&expander);
            expander.set_expanded(n == 0);
        }
    }

    fn emoji_grid(self: &Rc<Self>, emojis: &[GuildEmoji]) -> gtk::FlowBox {
        let grid = gtk::FlowBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .max_children_per_line(8)
            .min_children_per_line(8)
            .homogeneous(true)
            .build();
        for emoji in emojis {
            let button = gtk::Button::new();
            button.add_css_class("flat");
            button.set_tooltip_text(Some(&format!(":{}:", emoji.name)));
            let image = gtk::Picture::new();
            image.set_size_request(32, 32);
            image.set_content_fit(gtk::ContentFit::Contain);
            load_into(&self.images, &emoji.image_url(), &image);
            button.set_child(Some(&image));
            let (ui, text) = (self.clone(), format!("{} ", autocomplete::markup(emoji)));
            button.connect_clicked(move |_| {
                ui.insert_into_composer(&text);
                ui.picker.popdown();
            });
            grid.append(&button);
        }
        grid
    }

    /// Shows custom-emoji suggestions for the `:name` being typed, if any.
    fn suggest_emoji(self: &Rc<Self>, entry: &gtk::Entry) {
        let found = autocomplete::token(&entry.text(), entry.position() as usize).map(|(_, query)| {
            let guild = self.current_guild.borrow().clone();
            self.state.borrow().emoji_matches(&query, guild.as_deref(), 8)
        });
        let matches = found.unwrap_or_default();
        self.suggestions.replace(matches.clone());
        self.emoji_matches.remove_all();
        if matches.is_empty() {
            self.emoji_popover.popdown();
            return;
        }
        for emoji in &matches {
            let picture = picture(&self.images, &emoji.image_url(), 24, 24);
            let label = gtk::Label::builder().label(format!(":{}:", emoji.name)).xalign(0.0).build();
            let row_box = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            row_box.append(&picture);
            row_box.append(&label);
            self.emoji_matches.append(&row_box);
        }
        self.emoji_popover.popup();
    }

    /// Replaces the half-typed `:name` with the chosen emoji.
    fn accept_emoji(&self, index: usize) {
        let Some(emoji) = self.suggestions.borrow().get(index).cloned() else { return };
        let cursor = self.entry.position() as usize;
        let Some((start, _)) = autocomplete::token(&self.entry.text(), cursor) else { return };
        self.entry.delete_text(start as i32, cursor as i32);
        let text = format!("{} ", autocomplete::markup(&emoji));
        self.entry.insert_text(&text, &mut (start as i32));
        self.entry.set_position((start + text.chars().count()) as i32);
        self.emoji_popover.popdown();
        self.entry.grab_focus();
    }

    /// Lets the user choose a file and sends it to the open channel, with
    /// whatever is typed as its message.
    /// Chooses files to attach; they wait in the composer until Enter.
    fn pick_file(self: &Rc<Self>) {
        let parent = self.stack.root().and_downcast::<gtk::Window>();
        let ui = self.clone();
        gtk::FileDialog::builder().title("Upload a File").build().open_multiple(
            parent.as_ref(),
            gio::Cancellable::NONE,
            move |result| {
                let Ok(files) = result else { return };
                for file in files.iter::<gio::File>().flatten() {
                    if let Some(path) = file.path() {
                        ui.stage_file(path);
                    }
                }
            },
        );
    }

    /// Adds a file to the message being written.
    fn stage_file(self: &Rc<Self>, path: std::path::PathBuf) {
        if self.current_channel.borrow().is_none() {
            return;
        }
        if self.staged.borrow().len() >= 10 {
            return self.toast("You can attach up to 10 files per message");
        }
        self.staged.borrow_mut().push(path);
        self.render_staged();
        self.entry.grab_focus();
    }

    fn render_staged(self: &Rc<Self>) {
        while let Some(child) = self.staged_box.first_child() {
            self.staged_box.remove(&child);
        }
        let files = self.staged.borrow().clone();
        self.staged_box.set_visible(!files.is_empty());
        for (index, path) in files.iter().enumerate() {
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let is_image = matches!(
                path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).as_deref(),
                Some("png" | "jpg" | "jpeg" | "gif" | "webp")
            );
            let card = gtk::Box::new(gtk::Orientation::Vertical, 4);
            card.add_css_class("staged-file");
            if is_image {
                let picture = gtk::Picture::for_filename(path);
                picture.set_size_request(96, 72);
                picture.set_content_fit(gtk::ContentFit::Cover);
                picture.set_can_shrink(true);
                card.append(&picture);
            } else {
                card.append(&icons::image("copy", 32));
            }
            let label = gtk::Label::builder()
                .label(&name)
                .max_width_chars(14)
                .ellipsize(pango::EllipsizeMode::Middle)
                .build();
            card.append(&label);
            let remove = icons::button("trash", "Remove attachment");
            remove.add_css_class("flat");
            let ui = self.clone();
            remove.connect_clicked(move |_| {
                let mut staged = ui.staged.borrow_mut();
                if index < staged.len() {
                    staged.remove(index);
                }
                drop(staged);
                ui.render_staged();
            });
            card.append(&remove);
            self.staged_box.append(&card);
        }
    }

    /// Ctrl+V: files and pictures on the clipboard become attachments.
    /// Returns whether it took the paste (plain text is left to the entry).
    fn paste_attachments(self: &Rc<Self>) -> bool {
        let clipboard = self.entry.clipboard();
        let formats = clipboard.formats();
        if formats.contains_type(gdk::FileList::static_type()) {
            let ui = self.clone();
            glib::spawn_future_local(async move {
                if let Ok(value) = ui.entry.clipboard().read_value_future(gdk::FileList::static_type(), glib::Priority::DEFAULT).await {
                    if let Ok(list) = value.get::<gdk::FileList>() {
                        for file in list.files() {
                            if let Some(path) = file.path() {
                                ui.stage_file(path);
                            }
                        }
                    }
                }
            });
            return true;
        }
        if formats.contains_type(gdk::Texture::static_type()) && !formats.contains_type(String::static_type()) {
            let ui = self.clone();
            glib::spawn_future_local(async move {
                let Ok(Some(texture)) = ui.entry.clipboard().read_texture_future().await else { return };
                let dir = glib::user_cache_dir().join("discord-gtk").join("pasted");
                let _ = std::fs::create_dir_all(&dir);
                let path = dir.join(format!("image-{}.png", glib::real_time() / 1000));
                match texture.save_to_png(&path) {
                    Ok(()) => ui.stage_file(path),
                    Err(e) => ui.toast(&format!("Could not read the pasted image: {e}")),
                }
            });
            return true;
        }
        false
    }

    fn open_settings(self: &Rc<Self>) {
        let Some(parent) = self.stack.root().and_downcast::<gtk::Window>() else { return };
        let Some(user) = self.me.borrow().as_deref().and_then(|id| self.state.borrow().user(id).cloned()) else {
            return self.toast("Still connecting, try again in a moment");
        };
        let ui = self.clone();
        let log_out = parent.clone();
        let handle = settings::present(
            &parent,
            settings::Context {
                user,
                images: self.images.clone(),
                send: Rc::new(move |command| ui.send(command)),
                log_out: Rc::new(move || {
                    config::forget_token();
                    if let Some(app) = log_out.application() {
                        app.quit();
                    }
                }),
            },
        );
        self.settings.replace(Some(handle));
    }

    fn join_voice(self: &Rc<Self>, channel_id: &str) {
        let (guild_id, kind, name) = {
            let state = self.state.borrow();
            let Some((channel, guild)) = state.channel(channel_id) else { return };
            (guild.map(|g| g.id().to_owned()), channel.kind, channel.label())
        };
        if kind == Channel::STAGE {
            return self.toast("Stage channels are not supported yet");
        }
        if !matches!(kind, Channel::VOICE | Channel::DM | Channel::GROUP_DM) {
            return;
        }
        let permissions = self.state.borrow().permissions_in(channel_id);
        tracing::debug!(channel_id, permissions, "join voice requested");
        // Discord ignores a join without CONNECT instead of answering, which
        // would leave the panel stuck on "Connecting".
        if permissions & CONNECT == 0 {
            return self.toast("You do not have permission to join this voice channel");
        }
        self.voice_target.replace(Some(channel_id.to_owned()));
        self.voice_status.remove_css_class("connected");
        self.voice_status.set_label("Connecting…");
        self.voice_channel.set_label(&name);
        self.voice_panel.set_visible(true);
        self.send(Command::JoinVoice { guild_id, channel_id: channel_id.to_owned() });
    }

    fn leave_voice(&self) {
        self.voice_target.replace(None);
        self.voice_panel.set_visible(false);
        self.speaking.borrow_mut().clear();
        self.send(Command::LeaveVoice);
        self.refresh_speaking();
    }

    fn send_mute_state(&self) {
        self.send(Command::SetVoiceMute {
            mute: self.mute_button.is_active(),
            deaf: self.deafen_button.is_active(),
        });
    }

    /// Rings around the avatars of whoever is talking.
    fn refresh_speaking(&self) {
        let speaking = self.speaking.borrow();
        for row in rows(&self.channels) {
            let Some(user) = row.widget_name().strip_prefix(VOICE_ROW).map(str::to_owned) else { continue };
            set_class(&row, "speaking", speaking.contains(&user));
        }
    }

    /// Marks the open channel read up to its newest message, here and on Discord.
    fn ack_current(&self) {
        let Some(channel_id) = self.current_channel.borrow().clone() else { return };
        let message_id = {
            let mut state = self.state.borrow_mut();
            let Unread { unread, mentions } = state.unread(&channel_id);
            let Some(newest) = state.newest_message(&channel_id) else { return };
            if !unread && mentions == 0 {
                return;
            }
            state.ack(&channel_id, &newest, Some(0));
            newest
        };
        self.send(Command::Ack { channel_id, message_id });
        self.refresh_badges();
    }

    fn is_current(&self, channel_id: &str) -> bool {
        self.current_channel.borrow().as_deref() == Some(channel_id)
    }

    /// Shows a message right away, dimmed, until Discord confirms it. This is
    /// what the official client does; waiting for the round trip feels broken.
    fn show_pending(&self, channel_id: &str, content: &str, nonce: &str) {
        let Some(author) = self.me.borrow().as_deref().and_then(|id| self.state.borrow().user(id).cloned()) else {
            return;
        };
        let timestamp = glib::DateTime::now_utc().ok().and_then(|t| t.format_iso8601().ok()).unwrap_or_default();
        let pending = serde_json::from_value::<Message>(serde_json::json!({
            "id": nonce,
            "channel_id": channel_id,
            "content": content,
            "timestamp": timestamp.to_string(),
            "author": {"id": author.id, "username": author.username, "avatar": author.avatar, "global_name": author.global_name},
        }));
        if let Ok(mut message) = pending {
            message.pending = true;
            self.store.append(&glib::BoxedAnyObject::new(message));
            self.scroll_to_end();
        }
    }

    fn position_of(&self, id: &str) -> Option<u32> {
        (0..self.store.n_items()).find(|&i| {
            self.store
                .item(i)
                .and_downcast::<glib::BoxedAnyObject>()
                .is_some_and(|o| o.borrow::<Message>().id == id)
        })
    }

    /// True when the view is at (or within `STICK_TO_BOTTOM_PX` of) the newest message.
    fn near_bottom(&self) -> bool {
        let adj = self.scroller.vadjustment();
        adj.value() + adj.page_size() >= adj.upper() - STICK_TO_BOTTOM_PX
    }

    /// Deferred to idle: the list has not been measured yet when items are added,
    /// and scrolling before layout lands on the wrong row.
    fn scroll_to_end(&self) {
        let list = self.list.clone();
        let store = self.store.clone();
        let adjustment = self.scroller.vadjustment();
        glib::idle_add_local_once(move || {
            let n = store.n_items();
            if n > 0 {
                list.scroll_to(n - 1, gtk::ListScrollFlags::NONE, None);
            }
            // The new row is only sized after this first pass, which can leave
            // it just below the view: go to the true end once layout settled.
            glib::timeout_add_local_once(std::time::Duration::from_millis(80), move || {
                adjustment.set_value(adjustment.upper() - adjustment.page_size());
            });
        });
    }
}

/// Runs `action` with the widget when it is clicked with the primary button.
fn on_click(widget: &impl IsA<gtk::Widget>, action: impl Fn(&gtk::Widget) + 'static) {
    let click = gtk::GestureClick::builder().button(1).build();
    click.connect_released(move |gesture, _, _, _| {
        if let Some(widget) = gesture.widget() {
            action(&widget);
        }
    });
    widget.add_controller(click);
}

fn status_label(status: &str) -> &'static str {
    match status {
        "online" => "Online",
        "idle" => "Idle",
        "dnd" => "Do Not Disturb",
        _ => "Offline",
    }
}

/// An avatar with Discord's status dot in its corner.
fn with_status_dot(avatar: &adw::Avatar, status: &str) -> (gtk::Overlay, gtk::DrawingArea) {
    let dot = crate::status::dot(status);
    let stack = gtk::Overlay::new();
    stack.set_child(Some(avatar));
    stack.add_overlay(&dot);
    (stack, dot)
}

fn rows(list: &gtk::ListBox) -> Vec<gtk::ListBoxRow> {
    let mut out = Vec::new();
    let mut child = list.first_child();
    while let Some(widget) = child {
        child = widget.next_sibling();
        if let Ok(row) = widget.downcast::<gtk::ListBoxRow>() {
            out.push(row);
        }
    }
    out
}

fn find_row(list: &gtk::ListBox, id: &str) -> Option<gtk::ListBoxRow> {
    rows(list).into_iter().find(|r| r.widget_name() == id)
}

fn last_child(widget: gtk::Widget) -> Option<gtk::Widget> {
    widget.last_child()
}

fn set_class(widget: &impl IsA<gtk::Widget>, class: &str, on: bool) {
    if on {
        widget.add_css_class(class);
    } else {
        widget.remove_css_class(class);
    }
}

fn badge_label() -> gtk::Label {
    let badge = gtk::Label::new(None);
    badge.add_css_class("badge");
    badge.set_visible(false);
    badge
}

fn set_badge(widget: &gtk::Widget, mentions: u32) {
    let Some(label) = widget.downcast_ref::<gtk::Label>() else { return };
    label.set_visible(mentions > 0);
    label.set_label(&if mentions > 99 { "99+".to_owned() } else { mentions.to_string() });
}

fn category_row(category: &Channel) -> gtk::ListBoxRow {
    let label = gtk::Label::builder()
        .label(category.name.as_deref().unwrap_or("").to_uppercase())
        .xalign(0.0)
        .margin_top(14)
        .margin_bottom(2)
        .build();
    label.add_css_class("category");
    let row = gtk::ListBoxRow::new();
    row.set_selectable(false);
    row.set_activatable(false);
    row.set_child(Some(&label));
    row
}

fn channel_row(channel: &Channel) -> gtk::ListBoxRow {
    let voice = matches!(channel.kind, Channel::VOICE | Channel::STAGE);
    let forum = channel.kind == Channel::FORUM;
    let icon = match channel.kind {
        Channel::VOICE => "voice-normal",
        Channel::STAGE => "stage",
        Channel::FORUM => "forum",
        Channel::ANNOUNCEMENT => "announcements",
        _ => "hashmark",
    };
    let name = gtk::Label::builder()
        .label(channel.name.as_deref().unwrap_or("unknown"))
        .xalign(0.0)
        .hexpand(true)
        .ellipsize(pango::EllipsizeMode::End)
        .margin_top(4)
        .margin_bottom(4)
        .build();
    let inner = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    inner.append(&icons::image(icon, 18));
    inner.append(&name);
    inner.append(&badge_label());
    let row = gtk::ListBoxRow::new();
    row.set_widget_name(&channel.id);
    // Voice and forum channels need their own views; for now they are listed but inert.
    row.set_selectable(!voice && !forum);
    // Voice rows are clicked to join; forums need a view we do not have yet.
    row.set_activatable(!forum);
    row.set_child(Some(&inner));
    row
}

const MEDIA_MAX_W: f64 = 400.0;
const MEDIA_MAX_H: f64 = 300.0;

/// Messages by the same author closer together than this collapse under one header.
const GROUP_WINDOW_SECS: i64 = 7 * 60;

const AVATAR_SIZE: i32 = 40;

/// Name of the "Friends" row at the top of the direct message list.
const FRIENDS_ID: &str = "@friends";

#[derive(Clone, Copy, PartialEq, Eq)]
enum FriendsTab {
    Online,
    All,
    Pending,
    Blocked,
}

/// Prefix of channel-list row names that stand for a person in a voice channel.
const VOICE_ROW: &str = "voice:";

/// Discord shows "is typing" for about ten seconds after the last signal, and
/// accepts a new signal at most that often.
const TYPING_SECS: u64 = 10;
const TYPING_SEND_EVERY: Duration = Duration::from_secs(8);

/// Quick reactions offered in the message menu.
const QUICK_REACTIONS: [&str; 6] = ["👍", "❤️", "😂", "😮", "😢", "🎉"];

/// What the user asked for from a message row. Rows live in a recycled list
/// and cannot reach the `Ui`, so they report through a channel.
enum RowAction {
    Reply(String),
    Edit(String),
    Delete(String),
    Copy(String),
    React { id: String, emoji: Emoji, add: bool },
    /// Forget one message the MessageLogger kept.
    Dismiss(String),
    /// Forget every kept deleted message in this channel.
    ClearDeleted,
    /// Show the author's profile card, anchored to the clicked widget.
    Profile { id: String, anchor: glib::WeakRef<gtk::Widget> },
}

fn message_factory(
    store: gio::ListStore,
    images: Rc<Images>,
    plugins: Rc<Plugins>,
    me: Rc<RefCell<Option<String>>>,
    actions: async_channel::Sender<RowAction>,
) -> gtk::SignalListItemFactory {
    let factory = gtk::SignalListItemFactory::new();

    let (setup_actions, setup_me) = (actions.clone(), me);
    let actions_for_bind = actions;
    factory.connect_setup(move |_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().expect("ListItem");

        // gutter | column(reply, head, body, media, reactions)
        let avatar = adw::Avatar::new(AVATAR_SIZE, None, true);
        let gutter = gtk::Box::new(gtk::Orientation::Vertical, 0);
        gutter.set_size_request(AVATAR_SIZE, -1);
        gutter.set_valign(gtk::Align::Start);
        gutter.append(&avatar);

        let reply = gtk::Label::builder()
            .xalign(0.0)
            .ellipsize(pango::EllipsizeMode::End)
            .build();
        reply.add_css_class("reply-preview");
        let author = gtk::Label::builder().xalign(0.0).build();
        author.add_css_class("message-author");
        let time = gtk::Label::builder().xalign(0.0).build();
        time.add_css_class("message-time");
        let head = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        head.append(&author);
        head.append(&time);

        let body = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .wrap_mode(pango::WrapMode::WordChar)
            .use_markup(true)
            .build();
        body.add_css_class("message-body");
        // Messages with custom emoji need inline images, which a label cannot hold.
        let rich = gtk::TextView::builder()
            .editable(false)
            .cursor_visible(false)
            .wrap_mode(gtk::WrapMode::WordChar)
            .hexpand(true)
            .can_target(false)
            .focusable(false)
            .visible(false)
            .build();
        rich.add_css_class("rich-body");
        body.connect_activate_link(|_, uri| {
            let _ = gio::AppInfo::launch_default_for_uri(uri, gio::AppLaunchContext::NONE);
            glib::Propagation::Stop
        });
        let media = gtk::Box::new(gtk::Orientation::Vertical, 6);
        let reactions = gtk::FlowBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .max_children_per_line(30)
            .column_spacing(4)
            .row_spacing(4)
            .halign(gtk::Align::Start)
            .homogeneous(false)
            .build();

        let col = gtk::Box::new(gtk::Orientation::Vertical, 2);
        col.set_hexpand(true);
        col.append(&reply);
        col.append(&head);
        col.append(&body);
        col.append(&rich);
        col.append(&media);
        col.append(&reactions);

        let row = gtk::Box::new(gtk::Orientation::Horizontal, 16);
        row.set_margin_start(16);
        row.set_margin_end(16);
        row.append(&gutter);
        row.append(&col);

        // Right click opens the message menu.
        let click = gtk::GestureClick::builder().button(3).build();
        let (weak_item, me, actions) = (item.downgrade(), setup_me.clone(), setup_actions.clone());
        click.connect_pressed(move |gesture, _, x, y| {
            let Some(item) = weak_item.upgrade() else { return };
            let Some(obj) = item.item().and_downcast::<glib::BoxedAnyObject>() else { return };
            let Some(row) = gesture.widget() else { return };
            let m = obj.borrow::<Message>();
            let mine = me.borrow().as_deref() == Some(m.author.id.as_str());
            show_menu(&row, (x, y), &m, mine, &actions);
        });
        row.add_controller(click);

        // Clicking the avatar or the name opens the author's profile.
        for widget in [avatar.upcast_ref::<gtk::Widget>(), author.upcast_ref()] {
            let (weak_item, actions) = (item.downgrade(), setup_actions.clone());
            on_click(widget, move |anchor| {
                let Some(item) = weak_item.upgrade() else { return };
                let Some(obj) = item.item().and_downcast::<glib::BoxedAnyObject>() else { return };
                let id = obj.borrow::<Message>().id.clone();
                let _ = actions.try_send(RowAction::Profile { id, anchor: anchor.downgrade() });
            });
        }

        // Discord's hover toolbar: react, reply, edit (own messages), more.
        let toolbar = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        toolbar.add_css_class("message-toolbar");
        toolbar.set_halign(gtk::Align::End);
        toolbar.set_valign(gtk::Align::Start);
        toolbar.set_margin_end(16);
        // Hidden by opacity rather than visibility: a popover opened from a
        // toolbar button must outlive the pointer leaving the row.
        toolbar.set_opacity(0.0);
        toolbar.set_can_target(false);
        let tool = |icon: &str, tip: &str, act: Rc<dyn Fn(&gtk::Widget, &Message, bool)>| {
            let button = icons::button(icon, tip);
            let (weak_item, me) = (item.downgrade(), setup_me.clone());
            button.connect_clicked(move |button| {
                let Some(item) = weak_item.upgrade() else { return };
                let Some(obj) = item.item().and_downcast::<glib::BoxedAnyObject>() else { return };
                let m = obj.borrow::<Message>();
                let mine = me.borrow().as_deref() == Some(m.author.id.as_str());
                act(button.upcast_ref(), &m, mine);
            });
            toolbar.append(&button);
            button
        };
        let a = setup_actions.clone();
        tool("emoji-slightly-smiling-face", "Add Reaction", Rc::new(move |w, m, _| show_reactions(w, m, &a)));
        let a = setup_actions.clone();
        tool("arrow-angle-left-up", "Reply", Rc::new(move |_, m, _| {
            let _ = a.try_send(RowAction::Reply(m.id.clone()));
        }));
        let a = setup_actions.clone();
        let edit_button = tool("pencil", "Edit", Rc::new(move |_, m, _| {
            let _ = a.try_send(RowAction::Edit(m.id.clone()));
        }));
        edit_button.set_widget_name("edit-tool");
        let a = setup_actions.clone();
        tool("more-horizontal", "More", Rc::new(move |w, m, mine| show_menu(w, (0.0, 0.0), m, mine, &a)));

        let overlay = gtk::Overlay::new();
        overlay.set_child(Some(&row));
        overlay.add_overlay(&toolbar);
        let hover = gtk::EventControllerMotion::new();
        let (shown, weak_item, me) = (toolbar.clone(), item.downgrade(), setup_me.clone());
        hover.connect_enter(move |_, _, _| {
            // Only your own messages can be edited.
            if let Some(item) = weak_item.upgrade() {
                if let Some(obj) = item.item().and_downcast::<glib::BoxedAnyObject>() {
                    let mine = me.borrow().as_deref() == Some(obj.borrow::<Message>().author.id.as_str());
                    let mut child = shown.first_child();
                    while let Some(button) = child {
                        if button.widget_name() == "edit-tool" {
                            button.set_visible(mine);
                        }
                        child = button.next_sibling();
                    }
                }
            }
            shown.set_opacity(1.0);
            shown.set_can_target(true);
        });
        let shown = toolbar.clone();
        hover.connect_leave(move |_| {
            shown.set_opacity(0.0);
            shown.set_can_target(false);
        });
        overlay.add_controller(hover);
        item.set_child(Some(&overlay));
    });

    factory.connect_bind(move |_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().expect("ListItem");
        let Some(obj) = item.item().and_downcast::<glib::BoxedAnyObject>() else { return };
        let m = obj.borrow::<Message>();

        let overlay = item.child().and_downcast::<gtk::Overlay>().expect("overlay");
        let row = overlay.child().and_downcast::<gtk::Box>().expect("row");
        let mut text = display_content(&m);
        row.set_visible(true);
        match plugins.on_message(&m, &text) {
            Some(Shown::Text(replaced)) => text = replaced,
            Some(Shown::Hidden) => return row.set_visible(false),
            None => {}
        }
        let gutter = row.first_child().and_downcast::<gtk::Box>().expect("gutter");
        let avatar = gutter.first_child().and_downcast::<adw::Avatar>().expect("avatar");
        let col = gutter.next_sibling().and_downcast::<gtk::Box>().expect("column");
        let reply = col.first_child().and_downcast::<gtk::Label>().expect("reply");
        let head = reply.next_sibling().and_downcast::<gtk::Box>().expect("head");
        let author = head.first_child().and_downcast::<gtk::Label>().expect("author");
        let time = author.next_sibling().and_downcast::<gtk::Label>().expect("time");
        let body = head.next_sibling().and_downcast::<gtk::Label>().expect("body");
        let rich = body.next_sibling().and_downcast::<gtk::TextView>().expect("rich body");
        let media = rich.next_sibling().and_downcast::<gtk::Box>().expect("media");
        let reactions = media.next_sibling().and_downcast::<gtk::FlowBox>().expect("reactions");

        // Needed by the row's context menu and by tests of grouping.
        row.set_widget_name(&m.id);

        let previous = item
            .position()
            .checked_sub(1)
            .and_then(|p| store.item(p))
            .and_downcast::<glib::BoxedAnyObject>();
        let replying = m.referenced_message.is_some();
        let grouped = !replying
            && previous.is_some_and(|p| continues(&p.borrow::<Message>(), &m));

        row.set_margin_top(if grouped { 1 } else { 16 });
        avatar.set_visible(!grouped);
        head.set_visible(!grouped);
        reply.set_visible(replying);
        if let Some(original) = &m.referenced_message {
            let snippet: String = original.content.lines().next().unwrap_or("").chars().take(100).collect();
            reply.set_markup(&format!(
                "↪ <b>{}</b>  {}",
                gtk::glib::markup_escape_text(original.author.display()),
                gtk::glib::markup_escape_text(&snippet),
            ));
        }
        if !grouped {
            author.set_label(m.author.display());
            time.set_label(&local_time(&m.timestamp));
            avatar.set_text(Some(m.author.display()));
            avatar.set_custom_image(None::<&gdk::Texture>);
            load_avatar(&images, &m.author.avatar_url(), &avatar);
        }

        row.set_opacity(if m.pending { 0.55 } else { 1.0 });
        set_class(&row, "deleted", m.deleted);
        let raw = mention_text(&m);
        let parts = emoji_text::split(&raw);
        // A plugin may have rewritten the text; then the plain label shows its version.
        let rich_body = text == emoji_text::plain(&raw)
            && parts.iter().any(|p| matches!(p, emoji_text::Part::Emoji { .. }));
        rich.set_visible(rich_body);
        if rich_body {
            fill_rich(&rich, &images, &parts, m.edited_timestamp.is_some());
            body.set_visible(false);
        } else {
            let mut markup = markup::to_pango(&text);
            if m.deleted {
                markup.push_str(r##"<span size="small" foreground="#f23f43"> (deleted)</span>"##);
            }
            if m.edited_timestamp.is_some() {
                markup.push_str(r##"<span size="small" foreground="#949ba4"> (edited)</span>"##);
            }
            body.set_markup(&markup);
            body.set_visible(!text.is_empty());
        }

        while let Some(child) = media.first_child() {
            media.remove(&child);
        }
        for attachment in &m.attachments {
            media.append(&attachment_widget(&images, attachment));
        }
        for embed in &m.embeds {
            if let Some(widget) = embed_widget(&images, embed) {
                media.append(&widget);
            }
        }
        media.set_visible(media.first_child().is_some());

        reactions.remove_all();
        for reaction in &m.reactions {
            reactions.append(&reaction_button(&images, &m.id, reaction, &actions_for_bind));
        }
        reactions.set_visible(!m.reactions.is_empty());
    });

    factory
}

fn reaction_button(
    images: &Rc<Images>,
    message_id: &str,
    reaction: &Reaction,
    actions: &async_channel::Sender<RowAction>,
) -> gtk::Button {
    let inner = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    match reaction.emoji.image_url() {
        Some(url) => {
            let picture = gtk::Picture::new();
            picture.set_size_request(18, 18);
            load_into(images, &url, &picture);
            inner.append(&picture);
        }
        None => inner.append(&gtk::Label::new(reaction.emoji.name.as_deref())),
    }
    inner.append(&gtk::Label::new(Some(&reaction.count.to_string())));

    let button = gtk::Button::new();
    button.set_child(Some(&inner));
    button.add_css_class("reaction");
    if reaction.me {
        button.add_css_class("reaction-me");
    }
    let (id, emoji, add, actions) = (message_id.to_owned(), reaction.emoji.clone(), !reaction.me, actions.clone());
    button.connect_clicked(move |_| {
        let _ = actions.try_send(RowAction::React { id: id.clone(), emoji: emoji.clone(), add });
    });
    button
}

/// The right-click menu of a message.
/// A small popover with just the quick reactions.
fn show_reactions(anchor: &gtk::Widget, m: &Message, actions: &async_channel::Sender<RowAction>) {
    let popover = gtk::Popover::new();
    popover.set_parent(anchor);
    popover.connect_closed(|p| {
        let p = p.clone();
        glib::idle_add_local_once(move || p.unparent());
    });
    let quick = gtk::Box::new(gtk::Orientation::Horizontal, 2);
    for name in QUICK_REACTIONS {
        let button = gtk::Button::with_label(name);
        button.add_css_class("flat");
        let (id, actions, popover) = (m.id.clone(), actions.clone(), popover.clone());
        let emoji = Emoji { id: None, name: Some(name.to_owned()) };
        button.connect_clicked(move |_| {
            let _ = actions.try_send(RowAction::React { id: id.clone(), emoji: emoji.clone(), add: true });
            popover.popdown();
        });
        quick.append(&button);
    }
    popover.set_child(Some(&quick));
    popover.popup();
}

fn show_menu(
    row: &gtk::Widget,
    at: (f64, f64),
    m: &Message,
    mine: bool,
    actions: &async_channel::Sender<RowAction>,
) {
    let popover = gtk::Popover::new();
    popover.set_parent(row);
    popover.set_has_arrow(false);
    popover.set_pointing_to(Some(&gdk::Rectangle::new(at.0 as i32, at.1 as i32, 1, 1)));
    popover.connect_closed(|p| {
        let p = p.clone();
        glib::idle_add_local_once(move || p.unparent());
    });

    let column = gtk::Box::new(gtk::Orientation::Vertical, 2);
    let quick = gtk::Box::new(gtk::Orientation::Horizontal, 2);
    for name in QUICK_REACTIONS {
        let button = gtk::Button::with_label(name);
        button.add_css_class("flat");
        let (id, actions, popover) = (m.id.clone(), actions.clone(), popover.clone());
        let emoji = Emoji { id: None, name: Some(name.to_owned()) };
        button.connect_clicked(move |_| {
            let _ = actions.try_send(RowAction::React { id: id.clone(), emoji: emoji.clone(), add: true });
            popover.popdown();
        });
        quick.append(&button);
    }
    column.append(&quick);

    let entry = |label: &str, make: Box<dyn Fn(String) -> RowAction>| {
        let button = gtk::Button::with_label(label);
        button.add_css_class("flat");
        if let Some(inner) = button.child().and_downcast::<gtk::Label>() {
            inner.set_xalign(0.0);
        }
        let (id, actions, popover) = (m.id.clone(), actions.clone(), popover.clone());
        button.connect_clicked(move |_| {
            let _ = actions.try_send(make(id.clone()));
            popover.popdown();
        });
        column.append(&button);
    };
    if m.deleted {
        entry("Remove from log", Box::new(RowAction::Dismiss));
        entry("Copy text", Box::new(RowAction::Copy));
        entry("Clear all deleted messages", Box::new(|_| RowAction::ClearDeleted));
        quick.set_visible(false);
        popover.set_child(Some(&column));
        popover.popup();
        return;
    }
    entry("Reply", Box::new(RowAction::Reply));
    if mine {
        entry("Edit message", Box::new(RowAction::Edit));
    }
    entry("Copy text", Box::new(RowAction::Copy));
    if mine {
        entry("Delete message", Box::new(RowAction::Delete));
    }

    popover.set_child(Some(&column));
    popover.popup();
}

/// True when `m` should sit under the previous message's header.
fn continues(previous: &Message, m: &Message) -> bool {
    previous.author.id == m.author.id
        && (unix_time(&m.timestamp) - unix_time(&previous.timestamp)).abs() < GROUP_WINDOW_SECS
}

fn unix_time(iso: &str) -> i64 {
    glib::DateTime::from_iso8601(iso, None).map_or(0, |d| d.to_unix())
}

/// Message text with Discord's `<@id>` and `<:name:id>` markup made readable.
/// The message text with user mentions turned into names; custom emoji tags stay.
/// A message id of our own making, shaped like a snowflake (milliseconds since
/// Discord's epoch in the high bits) so it sorts with real ids.
fn new_nonce() -> String {
    const DISCORD_EPOCH_MS: i64 = 1_420_070_400_000;
    thread_local!(static COUNTER: Cell<u64> = const { Cell::new(0) });
    let ms = glib::real_time() / 1000 - DISCORD_EPOCH_MS;
    let count = COUNTER.with(|c| {
        c.set(c.get() + 1);
        c.get() & 0xfff
    });
    (((ms as u64) << 22) | count).to_string()
}

fn mention_text(m: &Message) -> String {
    let mut text = m.content.clone();
    for user in &m.mentions {
        for tag in [format!("<@{}>", user.id), format!("<@!{}>", user.id)] {
            text = text.replace(&tag, &format!("@{}", user.display()));
        }
    }
    text
}

fn display_content(m: &Message) -> String {
    emoji_text::plain(&mention_text(m))
}

/// Fills a read-only text view with a message that contains custom emoji,
/// showing each as an inline image. Emoji-only messages get big emoji.
fn fill_rich(view: &gtk::TextView, images: &Rc<Images>, parts: &[emoji_text::Part<'_>], edited: bool) {
    let buffer = view.buffer();
    buffer.set_text("");
    let size = if emoji_text::only_emoji(parts) { 48 } else { 22 };
    for part in parts {
        let mut end = buffer.end_iter();
        match part {
            emoji_text::Part::Text(text) => buffer.insert_markup(&mut end, &markup::to_pango(text)),
            emoji_text::Part::Emoji { id, .. } => {
                let picture = gtk::Picture::new();
                picture.set_size_request(size, size);
                picture.set_content_fit(gtk::ContentFit::Contain);
                picture.set_can_shrink(true);
                picture.set_valign(gtk::Align::Center);
                load_into(images, &format!("https://cdn.discordapp.com/emojis/{id}.png?size=64"), &picture);
                let anchor = buffer.create_child_anchor(&mut end);
                view.add_child_at_anchor(&picture, &anchor);
            }
        }
    }
    if edited {
        let mut end = buffer.end_iter();
        buffer.insert_markup(&mut end, r##"<span size="small" foreground="#949ba4"> (edited)</span>"##);
    }
}

/// Shrinks `w`x`h` to fit the inline limits, never enlarging.
fn fit(w: u32, h: u32) -> (i32, i32) {
    let (w, h) = (f64::from(w.max(1)), f64::from(h.max(1)));
    let scale = (MEDIA_MAX_W / w).min(MEDIA_MAX_H / h).min(1.0);
    ((w * scale).round() as i32, (h * scale).round() as i32)
}

/// Asks Discord's media proxy for a PNG no bigger than needed (2x for sharpness).
fn proxied(url: &str, proxy: Option<&str>, w: i32, h: i32) -> String {
    let Some(proxy) = proxy else { return url.to_owned() };
    let sep = if proxy.contains('?') { '&' } else { '?' };
    format!("{proxy}{sep}format=png&width={}&height={}", w * 2, h * 2)
}

fn picture(images: &Rc<Images>, url: &str, w: i32, h: i32) -> gtk::Picture {
    let picture = gtk::Picture::new();
    picture.set_size_request(w, h);
    picture.set_halign(gtk::Align::Start);
    picture.set_can_shrink(true);
    picture.set_content_fit(gtk::ContentFit::Cover);
    picture.add_css_class("message-media");
    picture.set_overflow(gtk::Overflow::Hidden);
    load_into(images, url, &picture);
    picture
}

fn attachment_widget(images: &Rc<Images>, a: &Attachment) -> gtk::Widget {
    if let (true, Some(w), Some(h)) = (a.is_image(), a.width, a.height) {
        let (dw, dh) = fit(w, h);
        return picture(images, &proxied(&a.url, a.proxy_url.as_deref(), dw, dh), dw, dh).upcast();
    }
    let label = gtk::Label::builder().label(format!("📎 {}", a.filename)).xalign(0.0).build();
    label.add_css_class("message-body");
    label.upcast()
}

fn embed_widget(images: &Rc<Images>, embed: &Embed) -> Option<gtk::Widget> {
    let media = embed.media();
    if embed.title.is_none() && embed.description.is_none() && media.is_none() {
        return None;
    }
    let card = gtk::Box::new(gtk::Orientation::Vertical, 4);
    card.add_css_class("embed");
    card.set_halign(gtk::Align::Start);
    if let Some(title) = &embed.title {
        let label = gtk::Label::builder()
            .label(title)
            .xalign(0.0)
            .wrap(true)
            .max_width_chars(60)
            .build();
        label.add_css_class("embed-title");
        card.append(&label);
    }
    if let Some(description) = &embed.description {
        let label = gtk::Label::builder()
            .label(description)
            .xalign(0.0)
            .wrap(true)
            .max_width_chars(60)
            .lines(6)
            .ellipsize(pango::EllipsizeMode::End)
            .build();
        label.add_css_class("embed-description");
        card.append(&label);
    }
    if let Some(media) = media {
        let (dw, dh) = fit(media.width.unwrap_or(400), media.height.unwrap_or(300));
        card.append(&picture(images, &proxied(&media.url, media.proxy_url.as_deref(), dw, dh), dw, dh));
    }
    Some(card.upcast())
}

/// Shows `url` in `picture` once it is available. The URL is remembered on
/// the widget so a row recycled for another message is not overwritten late.
fn load_into(images: &Images, url: &str, picture: &gtk::Picture) {
    picture.set_widget_name(url);
    let (picture, wanted) = (picture.clone(), url.to_owned());
    images.get(url, move |texture| {
        if picture.widget_name() == wanted {
            picture.set_paintable(texture.as_ref());
        }
    });
}

fn load_avatar(images: &Images, url: &str, avatar: &adw::Avatar) {
    avatar.set_widget_name(url);
    let (avatar, wanted) = (avatar.clone(), url.to_owned());
    images.get(url, move |texture| {
        if avatar.widget_name() == wanted {
            avatar.set_custom_image(texture.as_ref());
        }
    });
}

fn local_time(iso: &str) -> String {
    glib::DateTime::from_iso8601(iso, None)
        .ok()
        .and_then(|d| d.to_local().ok())
        .and_then(|d| d.format("%H:%M").ok())
        .map(|s| s.to_string())
        .unwrap_or_default()
}

