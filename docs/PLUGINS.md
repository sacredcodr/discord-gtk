# Writing plugins

A plugin is a folder in `~/.config/discord-gtk/plugins/`. Inside it you can put a `plugin.lua`, a `style.css`, or both. The app loads every folder when it starts, in alphabetical order, so restart it after you change something.

```
~/.config/discord-gtk/plugins/
  my-plugin/
    plugin.lua
    style.css
```

Settings has a Plugins page with a button that opens this folder. `examples/plugins/demo` in the repository is a small working plugin you can copy.

If a plugin fails to load, the app prints the reason to stderr and carries on without it. A handler that raises an error is reported once and then ignored for that error, so a broken plugin can't flood the log or stop the app.

## The `discord` table

Every plugin gets a global table called `discord`.

### `discord.on(event, handler)`

Registers a handler for an event. You can register as many as you like, and they run in the order they were loaded. There are two events.

**`"message"`** runs whenever a message is drawn on screen. The handler receives a table:

| field | meaning |
|-------|---------|
| `id` | message id, as a string |
| `channel_id` | channel id, as a string |
| `content` | the message text |
| `author.id` | author's user id |
| `author.username` | the account name |
| `author.name` | the display name, falling back to the username |

What the handler returns decides what you see:

- a string replaces the text shown
- `false` hides the message
- anything else, including nothing, leaves it alone

When one handler changes the text, the next handler sees the new text in `content`.

Only your own view changes. Nothing is sent to Discord, and other people still see the original. Message handlers run while rows are drawn during scrolling, so keep them quick.

**`"send"`** runs on text you are about to send. The handler receives the text as a string. Return a string to replace it, `false` to cancel the send, or nothing to leave it alone. This one does change what other people see.

### `discord.command(name, handler)`

Handles `/name args` typed in the message box. The handler receives everything after the command name as one string, with surrounding spaces removed.

- If it returns a string, that string is sent as a message.
- If it returns nothing, nothing is sent. Use this for commands that only show a toast.

A plugin command takes priority over the built-in ones, so you can override `/shrug` if you want to.

### `discord.log(text)`

Writes a line to stderr, prefixed with `[plugin]`. Start the app from a terminal to see it.

### `discord.notify(text)`

Shows a short toast at the bottom of the window.

## The sandbox

Plugins run in a restricted Lua. They get the basic language functions plus the `string`, `table`, `math` and `utf8` libraries. There is no `io`, `os`, `package` or `debug`, so a plugin can't read files, run programs or reach the network. That is deliberate: it lets you try other people's plugins without handing them your account.

A plugin also can't call Discord on your behalf, read your token, or see messages other than the ones passed to its handlers.

Style sheets have no such limit. CSS is applied as written, so read what a theme does before you install it.

## Examples

Turn a command into a message:

```lua
discord.command("shrug", function(args)
  return args .. " ¯\\_(ツ)_/¯"
end)
```

Show a toast and send nothing:

```lua
discord.command("hello", function()
  discord.notify("Plugins are working!")
end)
```

Hide messages from one account, locally:

```lua
discord.on("message", function(msg)
  if msg.author.username == "annoying_bot" then
    return false
  end
end)
```

Add a note to messages that mention a word:

```lua
discord.on("message", function(msg)
  if msg.content:lower():find("lol", 1, true) then
    return msg.content .. "  😂"
  end
end)
```

Stop yourself sending a blank message, and turn your messages into lower case:

```lua
discord.on("send", function(text)
  if text:match("^%s*$") then return false end
  return text:lower()
end)
```

Be careful with `"send"` handlers that rewrite text: they also run on text that contains Markdown, mentions like `<@123>` and emoji like `<:name:123>`. Changing those can break them.

## Themes

Anything in `style.css` is applied on top of the built-in theme, so a plugin can be only a style sheet. It uses GTK's CSS, which is close to web CSS but not the same: there is no `display: flex`, and some properties differ. The [GTK CSS overview](https://docs.gtk.org/gtk4/css-overview.html) lists what is supported.

These class names are stable enough to target:

| class | what it is |
|-------|-----------|
| `.messages` | the message list |
| `.message-author`, `.message-time`, `.message-body` | parts of a message |
| `.message-media` | pictures and attachments in a message |
| `.reaction`, `.reaction-me` | reaction buttons, and the ones you added |
| `.embed`, `.embed-title`, `.embed-description` | link embeds |
| `.composer`, `.composer-box` | the message box |
| `.guild-strip` | the server icons on the left |
| `.channel-pane` | the channel and DM list |
| `.member-list`, `.member-row`, `.member-name` | the member list |
| `.user-panel`, `.user-name`, `.user-tag` | your name at the bottom left |
| `.profile-card`, `.profile-name` | the profile popup |
| `.status-dot` | the online status badge |

Example, to tint every username:

```css
.message-author { color: #c9cdfb; }
```

Class names can change between versions, since this is an early release. If a theme stops working after an update, check whether a name changed in `src/style.css`.

## Built-in plugins

The Plugins page in Settings also lists plugins that ship with the app (ClearURLs, MessageLogger and others). Those are written in Rust, live in `src/builtin.rs`, and are separate from Lua plugins. They are switched off until you turn them on.

## Sharing a plugin

Zip the folder or put it in a git repository. Whoever installs it drops the folder into `~/.config/discord-gtk/plugins/`. Say in the README what the plugin does and whether its `style.css` changes anything beyond colours.
