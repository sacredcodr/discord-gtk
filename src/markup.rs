//! Discord-flavoured markdown to Pango markup.
//!
//! Supported: `**bold**`, `*italic*` / `_italic_`, `__underline__`,
//! `~~strike~~`, `` `code` ``, fenced code blocks, `> quotes`, `# headings`
//! and bare `http(s)` links. Everything else, including text a plugin
//! produced, is escaped and shown as typed.
//!
//! The output is always valid Pango markup: every `&`, `<` and `>` from the
//! input is escaped, so message text can never inject tags or links of its own.

const CODE_STYLE: &str = r##"<span font_family="monospace" background="#1e1f22">"##;
const MAX_DEPTH: u32 = 4;

pub fn to_pango(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 16);
    // Even segments are prose, odd ones sit between ``` fences.
    for (i, part) in text.split("```").enumerate() {
        if i % 2 == 1 && text.matches("```").count() > i {
            out.push_str(&code_block(part));
        } else if i % 2 == 1 {
            // Unterminated fence: show it literally rather than swallowing the rest.
            out.push_str("```");
            out.push_str(&prose(part));
        } else {
            out.push_str(&prose(part));
        }
    }
    out
}

fn code_block(body: &str) -> String {
    // A language tag may follow the fence on its own line: ```rust\n...
    let body = match body.split_once('\n') {
        Some((first, rest)) if !first.is_empty() && !first.contains(' ') => rest,
        _ => body,
    };
    format!("{CODE_STYLE}{}</span>", escape(body.trim_matches('\n')))
}

fn prose(text: &str) -> String {
    text.split('\n').map(line).collect::<Vec<_>>().join("\n")
}

fn line(line: &str) -> String {
    if let Some(quoted) = line.strip_prefix("> ") {
        return format!(r##"<span foreground="#80848e">▎ </span>{}"##, inline(quoted, 0));
    }
    for (marks, size) in [("### ", "large"), ("## ", "x-large"), ("# ", "xx-large")] {
        if let Some(heading) = line.strip_prefix(marks) {
            return format!(r#"<span size="{size}" weight="bold">{}</span>"#, inline(heading, 0));
        }
    }
    inline(line, 0)
}

fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    out
}

fn is_word(c: Option<char>) -> bool {
    c.is_some_and(char::is_alphanumeric)
}

/// Finds `marker` in `s` after at least one character, returning the byte offset.
fn closing(s: &str, marker: &str) -> Option<usize> {
    let first = s.chars().next()?.len_utf8();
    s[first..].find(marker).map(|at| at + first)
}

fn inline(s: &str, depth: u32) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    let mut rest = s;
    let mut previous: Option<char> = None;

    while let Some(c) = rest.chars().next() {
        let tag = if depth >= MAX_DEPTH {
            None
        } else {
            [("**", "b"), ("__", "u"), ("~~", "s")]
                .iter()
                .find(|(m, _)| rest.starts_with(m))
                .and_then(|&(m, t)| {
                    let inner_end = closing(&rest[m.len()..], m)?;
                    Some((m.len(), inner_end, t))
                })
                .or_else(|| match c {
                    '*' | '_' => {
                        let body = &rest[1..];
                        let end = closing(body, &c.to_string())?;
                        let boundary_ok = c == '*'
                            || (!is_word(previous) && !is_word(body[end + 1..].chars().next()));
                        let spaced = body.starts_with(char::is_whitespace);
                        (boundary_ok && !spaced).then_some((1, end, "i"))
                    }
                    _ => None,
                })
        };

        if let Some((open, inner_len, tag)) = tag {
            let inner = &rest[open..open + inner_len];
            out.push_str(&format!("<{tag}>{}</{tag}>", inline(inner, depth + 1)));
            rest = &rest[open + inner_len + open..];
            previous = Some(c);
            continue;
        }

        if c == '`' {
            if let Some(end) = closing(&rest[1..], "`") {
                out.push_str(&format!("{CODE_STYLE}{}</span>", escape(&rest[1..1 + end])));
                rest = &rest[end + 2..];
                previous = Some('`');
                continue;
            }
        }

        if rest.starts_with("http://") || rest.starts_with("https://") {
            let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
            let mut url = &rest[..end];
            // Trailing punctuation belongs to the sentence, not the link.
            url = url.trim_end_matches(['.', ',', ')', '!', '?', ';', ':', '>', '"']);
            if url.len() > "https://".len() {
                let url_text = escape(url);
                out.push_str(&format!(r##"<a href="{url_text}">{url_text}</a>"##));
                rest = &rest[url.len()..];
                previous = url.chars().last();
                continue;
            }
        }

        if c == '\\' {
            if let Some(next) = rest[1..].chars().next().filter(char::is_ascii_punctuation) {
                out.push_str(&escape(&next.to_string()));
                rest = &rest[1 + next.len_utf8()..];
                previous = Some(next);
                continue;
            }
        }

        out.push_str(&escape(&c.to_string()));
        rest = &rest[c.len_utf8()..];
        previous = Some(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::to_pango;

    #[test]
    fn styles() {
        assert_eq!(to_pango("**hi** *there* __u__ ~~s~~"), "<b>hi</b> <i>there</i> <u>u</u> <s>s</s>");
        assert_eq!(to_pango("_snake_case_word"), "_snake_case_word");
        assert_eq!(to_pango("a _b_ c"), "a <i>b</i> c");
        assert_eq!(to_pango("**a *b* c**"), "<b>a <i>b</i> c</b>");
    }

    #[test]
    fn unmatched_markers_stay_literal() {
        assert_eq!(to_pango("2 * 3 = 6"), "2 * 3 = 6");
        assert_eq!(to_pango("**open"), "**open");
        assert_eq!(to_pango("5 ** 2 ** 3"), "5 <b> 2 </b> 3");
    }

    #[test]
    fn escapes_everything_else() {
        assert_eq!(to_pango("<b>&</b>"), "&lt;b&gt;&amp;&lt;/b&gt;");
        assert_eq!(to_pango(r"\*not italic\*"), "*not italic*");
    }

    #[test]
    fn code_and_links() {
        assert!(to_pango("`a*b*c`").contains("a*b*c"));
        assert!(!to_pango("`a*b*c`").contains("<i>"));
        let t = to_pango("see https://example.com/a?b=1&c=2.");
        assert!(t.contains(r#"<a href="https://example.com/a?b=1&amp;c=2">"#), "{t}");
        assert!(t.ends_with("</a>."));
        let block = to_pango("x\n```rust\nlet a = <1>;\n```\ny");
        assert!(block.contains("let a = &lt;1&gt;;") && !block.contains("rust"), "{block}");
        assert!(to_pango("```unterminated *x*").contains("<i>x</i>"));
    }

    #[test]
    fn blocks() {
        assert!(to_pango("> quoted **x**").contains("<b>x</b>"));
        assert!(to_pango("# Title").contains("xx-large"));
        assert_eq!(to_pango("#nospace"), "#nospace");
    }
}
