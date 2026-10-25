//! Plugin host: sandboxed Lua scripts plus optional CSS themes.
//!
//! Layout, under `$XDG_CONFIG_HOME/discord-gtk/plugins/`:
//!
//! ```text
//! my-plugin/
//!   plugin.lua   optional, runs once at startup
//!   style.css    optional, applied over the built-in theme
//! ```
//!
//! Lua API (global `discord`):
//!
//! - `discord.on("message", function(msg) ... end)`
//!   Runs whenever a message is drawn. `msg` has `id`, `channel_id`,
//!   `content` and `author = {id, username, name}`. Return a string to change
//!   the text shown, `false` to hide the message, or nothing to leave it.
//!   Only the local view changes; nothing is sent to Discord.
//! - `discord.on("send", function(text) ... end)`
//!   Runs on text the user is about to send. Return the replacement text, or
//!   `false` to cancel sending.
//! - `discord.command("name", function(args) ... end)`
//!   Handles `/name args` typed in the message box. A returned string is sent
//!   as a message; returning nothing sends nothing.
//! - `discord.log(text)` writes to stderr; `discord.notify(text)` shows a toast.
//!
//! SECURITY: scripts get only the `string`, `table`, `math` and `utf8`
//! libraries. There is no `io`, `os`, `package` or `debug`, so a plugin
//! cannot read files, run programs or reach the network. CSS is applied as-is.
//!
//! IMPORTANT: Lua is not thread safe. The host lives on the GTK main thread
//! and handlers must stay fast, because "message" handlers run while rows
//! are bound during scrolling.

use std::{cell::RefCell, collections::HashSet, fs, path::PathBuf, rc::Rc};

use mlua::{Function, Lua, LuaOptions, StdLib, Table, Value};

use crate::model::Message;

const BOOTSTRAP: &str = r#"
local handlers, commands = {}, {}
discord._handlers, discord._commands = handlers, commands
function discord.on(event, fn)
  handlers[event] = handlers[event] or {}
  table.insert(handlers[event], fn)
end
function discord.command(name, fn)
  commands[name] = fn
end
"#;

/// What a "message" hook decided about a message.
pub enum Shown {
    Text(String),
    Hidden,
}

pub struct Plugins {
    lua: Lua,
    /// Toasts queued by `discord.notify`, drained by the UI.
    notices: Rc<RefCell<Vec<String>>>,
    /// Errors already reported, so a broken handler is logged once, not on every redraw.
    reported: RefCell<HashSet<String>>,
    /// CSS of every loaded plugin, concatenated in load order.
    pub css: String,
}

pub fn config_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("discord-gtk").join("plugins"))
}

impl Plugins {
    /// Loads every plugin directory. A plugin that fails to load is reported
    /// and skipped; it never stops the app from starting.
    pub fn load() -> Self {
        let lua = Lua::new_with(
            StdLib::STRING | StdLib::TABLE | StdLib::MATH | StdLib::UTF8,
            LuaOptions::default(),
        )
        .expect("Lua with safe standard libraries");
        let notices = Rc::new(RefCell::new(Vec::new()));
        let mut plugins = Self {
            lua,
            notices,
            reported: RefCell::default(),
            css: String::new(),
        };
        if let Err(e) = plugins.install_api() {
            eprintln!("plugins: cannot set up the Lua API: {e}");
            return plugins;
        }

        let Some(dir) = config_dir() else { return plugins };
        let mut found: Vec<_> = fs::read_dir(&dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.path().is_dir())
            .map(|e| e.path())
            .collect();
        found.sort();
        for path in found {
            plugins.load_one(&path);
        }
        plugins
    }

    fn install_api(&self) -> mlua::Result<()> {
        let api = self.lua.create_table()?;
        api.set(
            "log",
            self.lua.create_function(|_, text: String| {
                eprintln!("[plugin] {text}");
                Ok(())
            })?,
        )?;
        let notices = self.notices.clone();
        api.set(
            "notify",
            self.lua.create_function(move |_, text: String| {
                notices.borrow_mut().push(text);
                Ok(())
            })?,
        )?;
        self.lua.globals().set("discord", api)?;
        self.lua.load(BOOTSTRAP).set_name("bootstrap").exec()
    }

    fn load_one(&mut self, dir: &std::path::Path) {
        let name = dir.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        if let Ok(css) = fs::read_to_string(dir.join("style.css")) {
            self.css.push_str(&css);
            self.css.push('\n');
        }
        let Ok(source) = fs::read_to_string(dir.join("plugin.lua")) else { return };
        if let Err(e) = self.lua.load(&source).set_name(format!("{name}/plugin.lua")).exec() {
            eprintln!("plugins: {name} failed to load: {e}");
        }
    }

    /// Toasts requested by plugins since the last call.
    pub fn take_notices(&self) -> Vec<String> {
        self.notices.take()
    }

    /// Runs the "message" hooks. `None` means no plugin cares and the message
    /// is shown unchanged.
    pub fn on_message(&self, m: &Message, content: &str) -> Option<Shown> {
        let handlers = self.handlers("message")?;
        let table = self.message_table(m, content).ok()?;
        let mut shown = None;
        for handler in handlers {
            match handler.call::<Value>(table.clone()) {
                Ok(Value::String(text)) => {
                    let text = text.to_string_lossy().to_string();
                    let _ = table.set("content", text.as_str());
                    shown = Some(Shown::Text(text));
                }
                Ok(Value::Boolean(false)) => return Some(Shown::Hidden),
                Ok(_) => {}
                Err(e) => self.report("message", &e),
            }
        }
        shown
    }

    /// Runs the "send" hooks. `None` cancels the send.
    pub fn on_send(&self, text: String) -> Option<String> {
        let Some(handlers) = self.handlers("send") else { return Some(text) };
        let mut text = text;
        for handler in handlers {
            match handler.call::<Value>(text.as_str()) {
                Ok(Value::String(replacement)) => text = replacement.to_string_lossy().to_string(),
                Ok(Value::Boolean(false)) => return None,
                Ok(_) => {}
                Err(e) => self.report("send", &e),
            }
        }
        Some(text)
    }

    /// Runs `/name args`. `Some(reply)` is the outcome when a plugin owns the
    /// command (the reply may be empty); `None` when no plugin does.
    pub fn run_command(&self, name: &str, args: &str) -> Option<Option<String>> {
        let commands: Table = self.lua.globals().get::<Table>("discord").ok()?.get("_commands").ok()?;
        let handler: Function = commands.get(name).ok()?;
        match handler.call::<Value>(args) {
            Ok(Value::String(reply)) => Some(Some(reply.to_string_lossy().to_string())),
            Ok(_) => Some(None),
            Err(e) => {
                self.report(&format!("/{name}"), &e);
                Some(None)
            }
        }
    }

    fn handlers(&self, event: &str) -> Option<Vec<Function>> {
        let all: Table = self.lua.globals().get::<Table>("discord").ok()?.get("_handlers").ok()?;
        let list: Table = all.get(event).ok()?;
        let functions: Vec<Function> = list.sequence_values().flatten().collect();
        (!functions.is_empty()).then_some(functions)
    }

    fn message_table(&self, m: &Message, content: &str) -> mlua::Result<Table> {
        let author = self.lua.create_table()?;
        author.set("id", m.author.id.as_str())?;
        author.set("username", m.author.username.as_str())?;
        author.set("name", m.author.display())?;
        let table = self.lua.create_table()?;
        table.set("id", m.id.as_str())?;
        table.set("channel_id", m.channel_id.as_str())?;
        table.set("content", content)?;
        table.set("author", author)?;
        Ok(table)
    }

    fn report(&self, what: &str, error: &mlua::Error) {
        let line = format!("plugins: {what} handler failed: {error}");
        if self.reported.borrow_mut().insert(line.clone()) {
            eprintln!("{line}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host_with(lua_source: &str, css: &str) -> Plugins {
        let dir = std::env::temp_dir().join(format!("discord-gtk-test-{}", std::process::id()));
        let plugin = dir.join("discord-gtk/plugins/t");
        fs::create_dir_all(&plugin).unwrap();
        fs::write(plugin.join("plugin.lua"), lua_source).unwrap();
        fs::write(plugin.join("style.css"), css).unwrap();
        // SAFETY: only this test module touches the variable, and it runs before any thread reads it.
        unsafe { std::env::set_var("XDG_CONFIG_HOME", &dir) };
        Plugins::load()
    }

    fn message(content: &str) -> Message {
        serde_json::from_value(serde_json::json!({
            "id": "1", "channel_id": "2", "content": content,
            "author": {"id": "3", "username": "u", "global_name": "User"},
            "timestamp": "2026-01-01T00:00:00+00:00",
        }))
        .unwrap()
    }

    #[test]
    fn hooks_commands_css_and_sandbox() {
        let host = host_with(
            r#"
            discord.command("shrug", function(a) return a .. " x" end)
            discord.command("quiet", function() discord.notify("hi") end)
            discord.on("message", function(m)
              if m.content == "hide" then return false end
              if m.content == "lol" then return m.author.name .. ": lol" end
            end)
            discord.on("send", function(t) if t == "" then return false end return t:upper() end)
            assert(io == nil and os == nil and package == nil and debug == nil)
            assert(type(pairs) == "function" and type(tostring) == "function" and type(pcall) == "function")
            "#,
            ".a { color: red; }",
        );
        assert_eq!(host.run_command("shrug", "hey"), Some(Some("hey x".into())));
        assert_eq!(host.run_command("quiet", ""), Some(None));
        assert_eq!(host.take_notices(), vec!["hi".to_owned()]);
        assert_eq!(host.run_command("nope", ""), None);
        assert!(matches!(host.on_message(&message("hide"), "hide"), Some(Shown::Hidden)));
        assert!(matches!(host.on_message(&message("lol"), "lol"), Some(Shown::Text(t)) if t == "User: lol"));
        assert!(host.on_message(&message("plain"), "plain").is_none());
        assert_eq!(host.on_send("abc".into()), Some("ABC".into()));
        assert_eq!(host.on_send(String::new()), None);
        assert!(host.css.contains("color: red"));
    }
}
