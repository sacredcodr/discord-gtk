mod audio;
mod autocomplete;
mod backend;
mod builtin;
mod config;
mod emoji_text;
mod error;
mod gateway;
mod icon_data;
mod icons;
mod images;
mod markup;
mod member_list;
mod model;
mod plugins;
mod prefs;
mod profile;
mod state;
mod status;
mod rest;
mod settings;
mod ui;
mod voice;
mod web_login;

use adw::prelude::*;

fn main() -> gtk::glib::ExitCode {
    // Opt-in diagnostics, e.g. RUST_LOG=discord_gtk=debug,songbird=debug
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();
    // Several dependencies bring their own TLS backend, so rustls cannot pick
    // a crypto provider by itself. Choose one before any connection is made.
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("no other crypto provider installed yet");
    // WebKit's DMA-BUF renderer aborts the web process on some NVIDIA/Wayland
    // setups, which kills the login page. Must be set before WebKit starts.
    if std::env::var_os("WEBKIT_DISABLE_DMABUF_RENDERER").is_none() {
        // SAFETY: single-threaded here; nothing else has started yet.
        unsafe { std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1") };
    }
    let app = adw::Application::builder()
        .application_id("dev.yugi.DiscordGtk")
        .build();
    app.connect_activate(ui::build);
    app.run()
}
