-- Demo plugin. Copy this folder to ~/.config/discord-gtk/plugins/ to use it.

discord.log("demo plugin loaded")

-- /shrug <text>  sends "<text> ¯\_(ツ)_/¯"
discord.command("shrug", function(args)
  return args .. " ¯\\_(ツ)_/¯"
end)

-- /hello  shows a toast and sends nothing
discord.command("hello", function()
  discord.notify("Plugins are working!")
end)

-- Rewrites how messages containing "lol" are shown (local only).
discord.on("message", function(msg)
  if msg.content:lower():find("lol", 1, true) then
    return msg.content .. "  😂"
  end
end)

-- Stops you sending an empty-looking message by accident.
discord.on("send", function(text)
  if text:match("^%s*$") then return false end
end)
