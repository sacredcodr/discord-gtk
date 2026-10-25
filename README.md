# discord-gtk

A Discord client for Linux, written in Rust with GTK4 and libadwaita. It is built from scratch and does not embed Discord's web app, apart from the login page.

> [!CAUTION]
> This is an unofficial client. Discord Inc. did not make it, doesn't endorse it, and won't support it. "Discord" and its logos belong to Discord Inc.
>
> Using it breaks Discord's Terms of Service. Discord only permits its own apps to log in as a user, and this client signs in with your account token and calls the private user API. Discord suspends and terminates accounts for this, often without warning. If that happens to you, nobody here can get it back.
>
> Use an account you can afford to lose. The author and contributors accept no responsibility for banned accounts, lost servers or anything else that follows. The software comes "as is" under the MIT license. Please don't use it to automate accounts, spam, scrape, or dodge moderation, and don't file issues asking for that.

## What works

Servers, channels, DMs and group DMs, with unread and mention badges. Messages render Markdown, custom emoji, embeds and attachments, and you can reply, edit, delete and react. Sent messages show up immediately and settle once Discord confirms them.

There is a friends list with online status, and clicking a name or avatar anywhere opens a profile card. Voice channels work, including mute and deafen, speaking indicators and Discord's end-to-end encryption (through [songbird](https://github.com/serenity-rs/songbird)). Files can be attached from the file picker, pasted from the clipboard or dropped onto the window, and they go up through the same three-step upload Discord's own client uses.

Settings cover your account, sessions, connections, privacy, appearance (Discord's four themes), voice devices and notifications.

This is a first version, so a lot is still missing: threads and forums, search, pins, stage channels, GIF and sticker pickers, and incoming call prompts.

## Building

You need a recent Rust toolchain plus development files for GTK 4 (4.16 or newer), libadwaita 1.6, WebKitGTK 6.0, libopus, cmake and PipeWire or ALSA. On Arch:

```sh
sudo pacman -S gtk4 libadwaita webkitgtk-6.0 opus cmake pipewire
cargo run --release
```

For debug output start it with `RUST_LOG=discord_gtk=debug`. Logs can contain usernames and ids, so read them before pasting them into an issue.

## Logging in

On the first start the app opens Discord's own login page in an embedded browser and keeps the token it gets back. You can also paste a token by hand.

The token lives in `~/.config/discord-gtk/token` with mode 0600. Whoever has that file controls your account. The app sends it only to discord.com, and file uploads go to Discord's storage URLs without it. Logging out in Settings deletes the file.

Keep the token out of issues, screenshots and commits. The `.gitignore` here excludes tokens, logs and screenshots, but you are the last line of defence.

## Plugins

A plugin is a folder in `~/.config/discord-gtk/plugins/` holding a `plugin.lua`, a `style.css`, or both. The folder `examples/plugins/demo` is a working starting point, and the comment at the top of `src/plugins.rs` documents the API. Lua plugins run in a sandbox with no access to files, processes or the network.

Settings has a Plugins page with native versions of some Vencord plugins: ClearURLs, Unindent, MoreCommands, FakeNitro, SilentMessageToggle, NoReplyMention, SilentTyping, AnonymiseFileNames, MessageLogger and CharacterCounter. Vencord's own plugins are JavaScript written for Discord's web app, so they can't run here. These are rewrites that do the same jobs.

## Look and assets

The colour palettes in `src/themes/` follow Discord's dark, darker, midnight and light themes, and `tools/gen_themes.py` regenerates them. Colour values aren't creative works, and the repo contains no Discord code, images or fonts.

The icons are [Material Design Icons](https://pictogrammers.com/library/mdi/) from Pictogrammers, used under the Apache License 2.0 (see [NOTICE](NOTICE)). Run `tools/gen_icons.py` to download them again and rebuild `src/icon_data.rs`.

## Security

If you find a way this app could leak a token or run untrusted code, please report it through a private security advisory on GitHub instead of a public issue.

## License

MIT, see [LICENSE](LICENSE). Each dependency keeps its own license.
