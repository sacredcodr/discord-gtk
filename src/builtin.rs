//! Native versions of popular Vencord plugins.
//!
//! Vencord plugins are JavaScript that patches the code of Discord's web
//! client, so they cannot run in a native app. These do the same jobs on
//! top of our own message pipeline. Each has a switch in Settings > Plugins;
//! like Vencord, all start switched off.

use crate::prefs;

pub struct Info {
    pub id: &'static str,
    pub name: &'static str,
    pub description: &'static str,
}

pub const PLUGINS: &[Info] = &[
    Info { id: "clearURLs", name: "ClearURLs", description: "Removes tracking parameters (utm_*, fbclid, gclid...) from links you send." },
    Info { id: "unindent", name: "Unindent", description: "Removes extra indentation from code blocks you send." },
    Info { id: "moreCommands", name: "MoreCommands", description: "Adds /shrug, /lenny, /tableflip, /unflip and /sus." },
    Info { id: "fakeNitro", name: "FakeNitro", description: "Sends emoji from other servers as image links so everyone sees them." },
    Info { id: "silentMessageToggle", name: "SilentMessageToggle", description: "Start a message with @silent to send it without notifying anyone." },
    Info { id: "noReplyMention", name: "NoReplyMention", description: "Replies do not ping the person you reply to." },
    Info { id: "silentTyping", name: "SilentTyping", description: "Others no longer see you typing." },
    Info { id: "anonymiseFileNames", name: "AnonymiseFileNames", description: "Gives uploaded files a random name." },
    Info { id: "messageLogger", name: "MessageLogger", description: "Keeps deleted messages visible, marked as deleted." },
    Info { id: "characterCounter", name: "CharacterCounter", description: "Shows how many of the 2000 characters a message has used." },
];

pub fn enabled(id: &str) -> bool {
    prefs::get().plugins.get(id).copied().unwrap_or(false)
}

pub fn set_enabled(id: &str, on: bool) {
    prefs::update(|p| {
        p.plugins.insert(id.to_owned(), on);
    });
}

/// Applies every text-rewriting plugin to a message about to be sent.
/// `own_emoji` says whether an emoji id belongs to the server being written in.
pub fn rewrite_outgoing(text: &str, own_emoji: &dyn Fn(&str) -> bool) -> String {
    let mut text = text.to_owned();
    if enabled("unindent") {
        text = unindent(&text);
    }
    if enabled("clearURLs") {
        text = clear_urls(&text);
    }
    if enabled("fakeNitro") {
        text = fake_nitro(&text, own_emoji);
    }
    text
}

/// `/shrug`-style commands. `None` when the name is not one of ours or the
/// plugin is off.
pub fn command(name: &str, args: &str) -> Option<String> {
    if !enabled("moreCommands") {
        return None;
    }
    let face = match name {
        "shrug" => "¯\\_(ツ)_/¯",
        "lenny" => "( ͡° ͜ʖ ͡°)",
        "tableflip" => "(╯°□°)╯︵ ┻━┻",
        "unflip" => "┬─┬ ノ( ゜-゜ノ)",
        "sus" => "ඞ",
        _ => return None,
    };
    Some(if args.is_empty() { face.to_owned() } else { format!("{args} {face}") })
}

const TRACKING: &[&str] = &[
    "fbclid", "gclid", "gclsrc", "dclid", "msclkid", "igshid", "mc_eid", "mc_cid", "_hsenc", "_hsmi",
    "yclid", "twclid", "ttclid", "si", "feature", "ref_src", "ref_url", "spm",
];

fn is_tracking(key: &str) -> bool {
    key.starts_with("utm_") || TRACKING.contains(&key)
}

/// Removes tracking query parameters from every link in `text`.
pub fn clear_urls(text: &str) -> String {
    text.split_inclusive(char::is_whitespace)
        .map(|chunk| {
            let word = chunk.trim_end();
            let tail = &chunk[word.len()..];
            if !(word.starts_with("http://") || word.starts_with("https://")) {
                return chunk.to_owned();
            }
            let (before_fragment, fragment) = match word.split_once('#') {
                Some((a, b)) => (a, Some(b)),
                None => (word, None),
            };
            let Some((base, query)) = before_fragment.split_once('?') else { return chunk.to_owned() };
            let kept: Vec<&str> = query
                .split('&')
                .filter(|pair| !is_tracking(pair.split('=').next().unwrap_or("")))
                .collect();
            let mut out = base.to_owned();
            if !kept.is_empty() {
                out.push('?');
                out.push_str(&kept.join("&"));
            }
            if let Some(fragment) = fragment {
                out.push('#');
                out.push_str(fragment);
            }
            out + tail
        })
        .collect()
}

/// Strips the indentation shared by all lines inside each fenced code block.
pub fn unindent(text: &str) -> String {
    let mut out = String::new();
    for (index, part) in text.split("```").enumerate() {
        if index > 0 {
            out.push_str("```");
        }
        // Odd parts are code. Keep the language line (up to the first newline) as is.
        if index % 2 == 1 {
            let (first, rest) = part.split_once('\n').unwrap_or((part, ""));
            let indent = rest
                .lines()
                .filter(|l| !l.trim().is_empty())
                .map(|l| l.len() - l.trim_start().len())
                .min()
                .unwrap_or(0);
            out.push_str(first);
            if part.contains('\n') {
                out.push('\n');
            }
            let stripped: Vec<&str> =
                rest.split('\n').map(|l| if l.len() >= indent && l[..indent].trim().is_empty() { &l[indent..] } else { l.trim_start() }).collect();
            out.push_str(&stripped.join("\n"));
        } else {
            out.push_str(part);
        }
    }
    out
}

/// Turns `<:name:id>` emoji that this server does not own into image links.
pub fn fake_nitro(text: &str, own: &dyn Fn(&str) -> bool) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find('<') {
        out.push_str(&rest[..start]);
        let tail = &rest[start..];
        let replaced = parse_emoji(tail).filter(|e| !own(&e.id));
        match (parse_emoji(tail), replaced) {
            (Some(emoji), Some(foreign)) => {
                let ext = if foreign.animated { "gif" } else { "webp" };
                out.push_str(&format!(
                    "https://cdn.discordapp.com/emojis/{}.{ext}?size=48&name={}",
                    foreign.id, foreign.name
                ));
                rest = &tail[emoji.len..];
            }
            (Some(emoji), None) => {
                out.push_str(&tail[..emoji.len]);
                rest = &tail[emoji.len..];
            }
            (None, _) => {
                out.push('<');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

struct ParsedEmoji<'a> {
    name: &'a str,
    id: &'a str,
    animated: bool,
    len: usize,
}

fn parse_emoji(s: &str) -> Option<ParsedEmoji<'_>> {
    let body = s.strip_prefix("<a:").map(|b| (b, true)).or_else(|| s.strip_prefix("<:").map(|b| (b, false)))?;
    let (inner, animated) = body;
    let end = inner.find('>')?;
    let (name, id) = inner[..end].split_once(':')?;
    if name.is_empty() || id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let prefix = if animated { 3 } else { 2 };
    Some(ParsedEmoji { name, id, animated, len: prefix + end + 1 })
}

/// A random 7-character name that keeps the file's extension.
pub fn anonymised(name: &str) -> String {
    use std::hash::{BuildHasher, Hasher};
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write(name.as_bytes());
    let mut bits = hasher.finish();
    let stem: String = (0..7)
        .map(|_| {
            let c = ALPHABET[(bits % ALPHABET.len() as u64) as usize] as char;
            bits /= ALPHABET.len() as u64;
            c
        })
        .collect();
    match name.rsplit_once('.') {
        Some((_, ext)) if !ext.is_empty() && ext.len() <= 8 => format!("{stem}.{ext}"),
        _ => stem,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracking_is_removed_but_real_parameters_stay() {
        assert_eq!(
            clear_urls("see https://a.com/p?id=3&utm_source=x&fbclid=9#top now"),
            "see https://a.com/p?id=3#top now"
        );
        assert_eq!(clear_urls("https://a.com/?utm_medium=1"), "https://a.com/");
        assert_eq!(clear_urls("no links here?utm_x=1"), "no links here?utm_x=1");
    }

    #[test]
    fn code_blocks_lose_shared_indentation() {
        let input = "hi\n```js\n    a\n      b\n```";
        assert_eq!(unindent(input), "hi\n```js\na\n  b\n```");
        assert_eq!(unindent("plain  text"), "plain  text");
    }

    #[test]
    fn foreign_emoji_become_links() {
        let own = |id: &str| id == "1";
        assert_eq!(fake_nitro("a <:mine:1> b", &own), "a <:mine:1> b");
        assert_eq!(
            fake_nitro("<a:dance:22>!", &own),
            "https://cdn.discordapp.com/emojis/22.gif?size=48&name=dance!"
        );
        assert_eq!(fake_nitro("1 < 2 and <b>", &own), "1 < 2 and <b>");
    }

    #[test]
    fn anonymised_names_keep_the_extension() {
        let name = anonymised("holiday.photo.PNG");
        assert!(name.ends_with(".PNG") && name.len() == 11, "{name}");
        assert_eq!(anonymised("README").len(), 7);
    }
}
