//! `:name:` emoji completion in the composer: finding the half-typed token
//! and building the text Discord expects for a custom emoji.

use crate::model::GuildEmoji;

/// Fewest typed characters before suggestions appear, so a lone `:` or a
/// smiley like `:)` never opens the list.
const MIN_QUERY: usize = 2;

/// Finds an unfinished `:query` just before `cursor` (a character offset).
/// Returns where the colon sits (character offset) and the text after it.
pub fn token(text: &str, cursor: usize) -> Option<(usize, String)> {
    let before: Vec<char> = text.chars().take(cursor).collect();
    let colon = before.iter().rposition(|&c| c == ':')?;
    let query: String = before[colon + 1..].iter().collect();
    // The colon must start a word, and the query must be plain emoji-name text.
    let starts_word = colon == 0 || before[colon - 1].is_whitespace();
    let plain = query.chars().all(|c| c.is_alphanumeric() || c == '_');
    (starts_word && plain && query.chars().count() >= MIN_QUERY).then_some((colon, query))
}

/// What gets sent for a custom emoji: `<:name:id>` or `<a:name:id>`.
pub fn markup(emoji: &GuildEmoji) -> String {
    format!("<{}:{}:{}>", if emoji.animated { "a" } else { "" }, emoji.name, emoji.id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_token_before_the_cursor() {
        assert_eq!(token("hi :pe", 6), Some((3, "pe".into())));
        assert_eq!(token(":pepe", 5), Some((0, "pepe".into())));
        assert_eq!(token("hi :pe and more", 6), Some((3, "pe".into())));
    }

    #[test]
    fn ignores_things_that_are_not_emoji_names() {
        assert_eq!(token("hi :", 4), None);
        assert_eq!(token("hi :p", 5), None);
        assert_eq!(token("a:pepe", 6), None);
        assert_eq!(token("http://x", 8), None);
        assert_eq!(token("time 10:30", 10), None);
        assert_eq!(token(":pe be", 6), None);
    }

    #[test]
    fn builds_emoji_text() {
        let e = |animated| GuildEmoji { id: "9".into(), name: "pepe".into(), animated, available: true };
        assert_eq!(markup(&e(false)), "<:pepe:9>");
        assert_eq!(markup(&e(true)), "<a:pepe:9>");
    }
}
