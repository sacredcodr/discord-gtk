//! Splits message text around custom emoji tags (`<:name:id>`, `<a:name:id>`).

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum Part<'a> {
    Text(&'a str),
    Emoji { name: &'a str, id: &'a str, animated: bool },
}

/// Cuts `text` into plain runs and emoji. Anything that merely looks like a
/// tag (`<b>`, `<:x:abc>`) stays text.
pub fn split(text: &str) -> Vec<Part<'_>> {
    let mut parts = Vec::new();
    let mut rest = text;
    let mut plain_from = 0; // byte offset in `text` where the pending text run starts
    let mut offset = 0;
    while let Some(found) = rest.find('<') {
        let at = offset + found;
        let tail = &text[at..];
        match tag(tail) {
            Some((emoji, len)) => {
                if plain_from < at {
                    parts.push(Part::Text(&text[plain_from..at]));
                }
                parts.push(emoji);
                plain_from = at + len;
                offset = plain_from;
            }
            None => offset = at + 1,
        }
        rest = &text[offset..];
    }
    if plain_from < text.len() {
        parts.push(Part::Text(&text[plain_from..]));
    }
    parts
}

/// Parses an emoji tag at the start of `s`, returning it and its byte length.
fn tag(s: &str) -> Option<(Part<'_>, usize)> {
    let end = s.find('>')?;
    let inner = &s[1..end];
    let (animated, inner) = match inner.strip_prefix("a:") {
        Some(rest) => (true, rest),
        None => (false, inner.strip_prefix(':')?),
    };
    let (name, id) = inner.split_once(':')?;
    let valid = !name.is_empty() && !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit());
    valid.then_some((Part::Emoji { name, id, animated }, end + 1))
}

/// The text with each emoji shown as `:name:`.
pub fn plain(text: &str) -> String {
    split(text)
        .into_iter()
        .map(|part| match part {
            Part::Text(t) => t.to_owned(),
            Part::Emoji { name, .. } => format!(":{name}:"),
        })
        .collect()
}

/// True when the text is nothing but emoji (and spaces), few enough to show large.
pub fn only_emoji(parts: &[Part<'_>]) -> bool {
    let emoji = parts.iter().filter(|p| matches!(p, Part::Emoji { .. })).count();
    emoji > 0
        && emoji <= 27
        && parts.iter().all(|p| match p {
            Part::Emoji { .. } => true,
            Part::Text(t) => t.trim().is_empty(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_text_and_emoji() {
        assert_eq!(
            split("hi <:pepe:123> and <a:dance:9>!"),
            [
                Part::Text("hi "),
                Part::Emoji { name: "pepe", id: "123", animated: false },
                Part::Text(" and "),
                Part::Emoji { name: "dance", id: "9", animated: true },
                Part::Text("!"),
            ]
        );
    }

    #[test]
    fn lookalikes_stay_text() {
        for text in ["<b>bold</b>", "<:x:abc>", "<::1>", "a < b > c", "<@123>", "<:open"] {
            assert_eq!(split(text), [Part::Text(text)], "{text}");
        }
    }

    #[test]
    fn plain_form_and_jumbo() {
        assert_eq!(plain("x <:pepe:1> y"), "x :pepe: y");
        assert!(only_emoji(&split("<:a:1> <:b:2>")));
        assert!(!only_emoji(&split("hello <:a:1>")));
        assert!(!only_emoji(&split("no emoji")));
    }
}
