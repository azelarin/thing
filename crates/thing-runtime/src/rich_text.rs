//! styled spans and the safe markup subset for text widgets.
//!
//! markup supports `<b>`, `<strong>`, `<i>`, `<em>`, `<u>`, `<a href="...">`,
//! `<br>`, and `<img alt="...">` (shown as its alt text, never fetched), with
//! the `&amp;`, `&lt;`, `&gt;`, `&quot;`, and `&apos;` entities and numeric
//! character references. other tags are dropped and their content kept.
//! markup that is malformed, nested too deeply, or beyond the span and link
//! limits is shown as its literal source instead. nothing in it is executed,
//! fetched, or opened.

use mlua::{Error, Result, Table, Value};
use thing_core::{Color, MAX_LINK_BYTES, MAX_TEXT_LINKS, MAX_TEXT_SPANS, TextSpan};

/// the largest markup source accepted.
pub(crate) const MAX_MARKUP_BYTES: usize = 64 * 1024;
const MAX_MARKUP_DEPTH: usize = 16;

/// a text widget's resolved text, spans, and links.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct RichText {
    pub(crate) text: String,
    pub(crate) spans: Vec<TextSpan>,
    pub(crate) links: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
struct SpanStyle {
    weight: Option<f32>,
    italic: bool,
    underline: bool,
    color: Option<Color>,
    link: Option<String>,
}

impl SpanStyle {
    fn is_plain(&self) -> bool {
        *self == Self::default()
    }
}

impl RichText {
    fn push(&mut self, text: &str, style: &SpanStyle) -> std::result::Result<(), String> {
        if text.is_empty() {
            return Ok(());
        }
        let start = self.text.len();
        self.text.push_str(text);
        if style.is_plain() {
            return Ok(());
        }
        let link = match &style.link {
            Some(target) => {
                if target.is_empty() || target.len() > MAX_LINK_BYTES || target.contains('\0') {
                    return Err(format!(
                        "links must be 1 to {MAX_LINK_BYTES} bytes without NUL"
                    ));
                }
                let index = match self.links.iter().position(|link| link == target) {
                    Some(index) => index,
                    None => {
                        if self.links.len() >= MAX_TEXT_LINKS {
                            return Err(format!(
                                "a text widget may hold at most {MAX_TEXT_LINKS} links"
                            ));
                        }
                        self.links.push(target.clone());
                        self.links.len() - 1
                    }
                };
                Some(index as u16)
            }
            None => None,
        };
        let span = TextSpan {
            range: start..self.text.len(),
            weight: style.weight,
            italic: style.italic,
            underline: style.underline,
            color: style.color,
            link,
        };
        // adjacent text with one style is one span.
        let same_style = |previous: &TextSpan| {
            (
                previous.weight,
                previous.italic,
                previous.underline,
                previous.color,
                previous.link,
            ) == (
                span.weight,
                span.italic,
                span.underline,
                span.color,
                span.link,
            )
        };
        if let Some(previous) = self.spans.last_mut()
            && previous.range.end == start
            && same_style(previous)
        {
            previous.range.end = span.range.end;
            return Ok(());
        }
        if self.spans.len() >= MAX_TEXT_SPANS {
            return Err(format!(
                "a text widget may hold at most {MAX_TEXT_SPANS} spans"
            ));
        }
        self.spans.push(span);
        Ok(())
    }
}

/// parses the `spans` property: a list of strings and span tables.
pub(crate) fn spans(value: Value) -> Result<RichText> {
    let Value::Table(list) = value else {
        return Err(Error::runtime(
            "spans must be a list of strings and span tables",
        ));
    };
    let length = list.raw_len();
    if length > MAX_TEXT_SPANS {
        return Err(Error::runtime(format!(
            "a text widget may hold at most {MAX_TEXT_SPANS} spans"
        )));
    }
    if list.clone().pairs::<Value, Value>().count() != length {
        return Err(Error::runtime("spans must be a list"));
    }
    let mut rich = RichText::default();
    for index in 1..=length {
        let (text, style) = match list.raw_get::<Value>(index)? {
            Value::String(text) => (text.to_str()?.to_owned(), SpanStyle::default()),
            Value::Table(span) => span_table(&span)?,
            _ => {
                return Err(Error::runtime("each span must be a string or a span table"));
            }
        };
        rich.push(&text, &style).map_err(Error::runtime)?;
    }
    Ok(rich)
}

fn span_table(span: &Table) -> Result<(String, SpanStyle)> {
    for pair in span.clone().pairs::<Value, Value>() {
        let (key, _) = pair?;
        let known = match &key {
            Value::String(key) => matches!(
                &*key.to_str()?,
                "text" | "weight" | "bold" | "italic" | "underline" | "color" | "link"
            ),
            _ => false,
        };
        if !known {
            return Err(Error::runtime(format!(
                "unknown span property {}",
                key.to_string().unwrap_or_default()
            )));
        }
    }
    let Value::String(text) = span.raw_get::<Value>("text")? else {
        return Err(Error::runtime("span.text must be a string"));
    };
    let flag = |name: &str| -> Result<Option<bool>> {
        match span.raw_get::<Value>(name)? {
            Value::Nil => Ok(None),
            Value::Boolean(value) => Ok(Some(value)),
            _ => Err(Error::runtime(format!("span.{name} must be a boolean"))),
        }
    };
    let weight = match span.raw_get::<Value>("weight")? {
        Value::Nil => flag("bold")?.and_then(|bold| bold.then_some(700.0)),
        Value::Integer(weight) => Some(weight as f32),
        Value::Number(weight) => Some(weight as f32),
        _ => return Err(Error::runtime("span.weight must be a number")),
    };
    if weight.is_some_and(|weight| !weight.is_finite() || !(1.0..=1000.0).contains(&weight)) {
        return Err(Error::runtime("span.weight must be between 1 and 1000"));
    }
    let color = match span.raw_get::<Value>("color")? {
        Value::Nil => None,
        value => Some(crate::properties::parse_color(value, "span.color")?),
    };
    let link = match span.raw_get::<Value>("link")? {
        Value::Nil => None,
        Value::String(link) => Some(link.to_str()?.to_owned()),
        _ => return Err(Error::runtime("span.link must be a string")),
    };
    let underline = flag("underline")?.unwrap_or(link.is_some());
    Ok((
        text.to_str()?.to_owned(),
        SpanStyle {
            weight,
            italic: flag("italic")?.unwrap_or(false),
            underline,
            color,
            link,
        },
    ))
}

/// parses markup, falling back to the literal source when it is malformed
/// or exceeds the limits. only oversized input is an error.
pub(crate) fn markup(source: &str) -> Result<RichText> {
    if source.len() > MAX_MARKUP_BYTES {
        return Err(Error::runtime(format!(
            "markup may be at most {} KiB",
            MAX_MARKUP_BYTES / 1024
        )));
    }
    Ok(parse_markup(source).unwrap_or_else(|| RichText {
        text: source.to_owned(),
        ..RichText::default()
    }))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tag {
    Bold,
    Italic,
    Underline,
    Link,
    Other,
}

fn parse_markup(source: &str) -> Option<RichText> {
    let mut rich = RichText::default();
    let mut stack: Vec<(String, Tag, SpanStyle)> = Vec::new();
    let mut style = SpanStyle::default();
    let mut rest = source;
    while !rest.is_empty() {
        let next = rest.find(['<', '&']).unwrap_or(rest.len());
        rich.push(&rest[..next], &style).ok()?;
        rest = &rest[next..];
        if rest.starts_with('&') {
            let (text, used) = entity(rest);
            rich.push(&text, &style).ok()?;
            rest = &rest[used..];
            continue;
        }
        if rest.is_empty() {
            break;
        }
        let end = rest.find('>')?;
        let tag = &rest[1..end];
        rest = &rest[end + 1..];
        if tag.contains('<') {
            return None;
        }
        if let Some(name) = tag.strip_prefix('/') {
            let name = name.trim().to_ascii_lowercase();
            let (open, _, previous) = stack.pop()?;
            if open != name {
                return None;
            }
            style = previous;
            continue;
        }
        let self_closing = tag.ends_with('/');
        let tag = tag.trim_end_matches('/');
        let (name, attributes) = tag
            .split_once(|character: char| character.is_ascii_whitespace())
            .unwrap_or((tag, ""));
        let name = name.to_ascii_lowercase();
        if name.is_empty() || !name.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
            return None;
        }
        let attributes = parse_attributes(attributes)?;
        match name.as_str() {
            "br" => {
                rich.push("\n", &style).ok()?;
                continue;
            }
            "img" => {
                // embedded images are never fetched; their alt text stands in.
                if let Some(alt) = attribute(&attributes, "alt") {
                    rich.push(&alt, &style).ok()?;
                }
                if !self_closing {
                    stack.push((name, Tag::Other, style.clone()));
                }
                continue;
            }
            _ => {}
        }
        if self_closing {
            continue;
        }
        if stack.len() >= MAX_MARKUP_DEPTH {
            return None;
        }
        let kind = match name.as_str() {
            "b" | "strong" => Tag::Bold,
            "i" | "em" => Tag::Italic,
            "u" => Tag::Underline,
            "a" => Tag::Link,
            _ => Tag::Other,
        };
        stack.push((name, kind, style.clone()));
        match kind {
            Tag::Bold => style.weight = Some(700.0),
            Tag::Italic => style.italic = true,
            Tag::Underline => style.underline = true,
            Tag::Link => {
                let href = attribute(&attributes, "href")?;
                style.link = Some(href);
                style.underline = true;
            }
            Tag::Other => {}
        }
    }
    // unclosed tags end with the text.
    Some(rich)
}

fn attribute(attributes: &[(String, String)], name: &str) -> Option<String> {
    attributes
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.clone())
}

/// `name="value"` or `name='value'` pairs, with entities decoded in values.
fn parse_attributes(mut source: &str) -> Option<Vec<(String, String)>> {
    let mut attributes = Vec::new();
    loop {
        source = source.trim_start();
        if source.is_empty() {
            return Some(attributes);
        }
        let equals = source.find('=')?;
        let name = source[..equals].trim().to_ascii_lowercase();
        if name.is_empty()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        {
            return None;
        }
        source = source[equals + 1..].trim_start();
        let quote = source
            .chars()
            .next()
            .filter(|quote| matches!(quote, '"' | '\''))?;
        let close = source[1..].find(quote)? + 1;
        let mut value = String::new();
        let mut raw = &source[1..close];
        while !raw.is_empty() {
            let next = raw.find('&').unwrap_or(raw.len());
            value.push_str(&raw[..next]);
            raw = &raw[next..];
            if !raw.is_empty() {
                let (text, used) = entity(raw);
                value.push_str(&text);
                raw = &raw[used..];
            }
        }
        attributes.push((name, value));
        source = &source[close + 1..];
    }
}

/// decodes one entity at the start of `source`, or keeps a bare `&`.
fn entity(source: &str) -> (String, usize) {
    let literal = ("&".to_owned(), 1);
    let Some(end) = source.bytes().take(12).position(|byte| byte == b';') else {
        return literal;
    };
    let name = &source[1..end];
    let character = match name {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        _ => name
            .strip_prefix("#x")
            .or_else(|| name.strip_prefix("#X"))
            .map(|hex| u32::from_str_radix(hex, 16).ok())
            .or_else(|| name.strip_prefix('#').map(|decimal| decimal.parse().ok()))
            .flatten()
            .and_then(char::from_u32)
            .filter(|character| *character != '\0'),
    };
    match character {
        Some(character) => (character.to_string(), end + 1),
        None => literal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(text: &str) -> RichText {
        RichText {
            text: text.into(),
            ..RichText::default()
        }
    }

    #[test]
    fn markup_styles_links_and_entities() {
        let rich = markup(
            "a <b>bold</b>, <i>slanted</i> &amp; <a href=\"https://example.org/?a=1&amp;b=2\">linked <u>text</u></a><br/>next",
        )
        .unwrap();
        assert_eq!(rich.text, "a bold, slanted & linked text\nnext");
        assert_eq!(rich.links, ["https://example.org/?a=1&b=2"]);
        let styled: Vec<_> = rich
            .spans
            .iter()
            .map(|span| {
                (
                    &rich.text[span.range.clone()],
                    span.weight,
                    span.italic,
                    span.link,
                )
            })
            .collect();
        assert_eq!(
            styled,
            [
                ("bold", Some(700.0), false, None),
                ("slanted", None, true, None),
                ("linked text", None, false, Some(0)),
            ]
        );
        assert!(rich.spans[2].underline);
    }

    #[test]
    fn unsupported_markup_is_dropped_and_malformed_markup_is_literal() {
        assert_eq!(
            markup("<font color='red'>kept</font> <img src='https://x/y.png' alt='[logo]'/>")
                .unwrap(),
            plain("kept [logo]")
        );
        assert_eq!(
            markup("<img src=\"https://tracker/x.png\">").unwrap().text,
            ""
        );
        for source in [
            "<b>mismatched</i>",
            "unterminated <b",
            "</b>stray close",
            "<a>no href</a>",
            "<a href=unquoted>x</a>",
            "<b><b><b><b><b><b><b><b><b><b><b><b><b><b><b><b><b>deep",
        ] {
            assert_eq!(markup(source).unwrap(), plain(source), "{source}");
        }
        assert_eq!(markup("<b>unclosed").unwrap().text, "unclosed");
        assert_eq!(
            markup("a & b &unknown; &#x1F600; &#0; &#65;").unwrap().text,
            "a & b &unknown; 😀 &#0; A"
        );
        let many_links: String = (0..70)
            .map(|index| format!("<a href='{index}'>x</a>"))
            .collect();
        assert_eq!(markup(&many_links).unwrap(), plain(&many_links));
        assert!(markup(&"x".repeat(MAX_MARKUP_BYTES + 1)).is_err());
    }
}
