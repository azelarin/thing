use std::fs;

use tempfile::TempDir;
use thing_core::{Color, Node, TextOverflow};
use thing_runtime::{Event, Runtime};

fn load(source: &str) -> mlua::Result<(TempDir, Runtime)> {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("panel.luau");
    fs::write(&path, source).unwrap();
    Runtime::load(path).map(|runtime| (directory, runtime))
}

fn label(runtime: &Runtime) -> &Node {
    &runtime.root().unwrap().children[0]
}

#[test]
fn spans_build_text_styles_and_shared_links() {
    let (_directory, runtime) = load(
        r##"
        return ui.panel { children = { ui.text {
            spans = {
                'read ',
                { text = 'the guide', bold = true, link = 'https://example.org/guide' },
                ' or ',
                { text = 'its index', italic = true, color = '#ff0000', link = 'https://example.org/guide', underline = false },
                { text = '.', weight = 300 },
            },
            on_link = function() end,
            max_lines = 3,
            overflow = 'ellipsis',
            style = { letter_spacing = 0.5 },
        } } }
        "##,
    )
    .unwrap();
    let node = label(&runtime);
    assert_eq!(node.text, "read the guide or its index.");
    assert_eq!(&*node.links, ["https://example.org/guide"]);
    let spans: Vec<_> = node
        .spans
        .iter()
        .map(|span| {
            (
                &node.text[span.range.clone()],
                span.weight,
                span.italic,
                span.underline,
                span.color,
                span.link,
            )
        })
        .collect();
    assert_eq!(
        spans,
        [
            ("the guide", Some(700.0), false, true, None, Some(0)),
            (
                "its index",
                None,
                true,
                false,
                Some(Color([255, 0, 0, 255])),
                Some(0)
            ),
            (".", Some(300.0), false, false, None, None),
        ]
    );
    assert!(node.link_handler && node.is_interactive());
    assert_eq!(node.text_overflow, TextOverflow::Ellipsis);
    assert_eq!(node.max_lines, Some(3));
    assert_eq!(node.style.letter_spacing, 0.5);
}

#[test]
fn markup_is_safe_and_malformed_markup_stays_literal() {
    let (_directory, runtime) = load(
        r#"
        return ui.panel { children = {
            ui.text { markup = 'new <b>message</b> from <a href="mailto:a@example.org">ada</a> &lt;3' },
            ui.text { markup = '<b>broken</i>' },
        } }
        "#,
    )
    .unwrap();
    let node = label(&runtime);
    assert_eq!(node.text, "new message from ada <3");
    assert_eq!(&*node.links, ["mailto:a@example.org"]);
    // without on_link, links are styled but take no input.
    assert!(!node.is_interactive());
    let broken = &runtime.root().unwrap().children[1];
    assert_eq!(broken.text, "<b>broken</i>");
    assert!(broken.spans.is_empty());
}

#[test]
fn links_activate_only_through_on_link_with_current_indices() {
    let (_directory, mut runtime) = load(
        r#"
        local opened = ui.state('')
        local second = ui.state(true)
        return ui.panel { children = {
            ui.text {
                spans = function()
                    local spans = { { text = 'first', link = 'one' }, ' ' }
                    if second:get() then table.insert(spans, { text = 'second', link = 'two' }) end
                    return spans
                end,
                on_link = function(target, index)
                    opened:set(`{target} {index}`)
                    second:set(false)
                end,
            },
            ui.text { text = function() return opened:get() end },
        } }
        "#,
    )
    .unwrap();
    let id = label(&runtime).id;
    assert!(runtime.dispatch(id, Event::Link(1)).unwrap());
    assert_eq!(runtime.root().unwrap().children[1].text, "two 2");
    // the second link is gone, so its old index does nothing.
    assert_eq!(label(&runtime).links.len(), 1);
    assert!(!runtime.dispatch(id, Event::Link(1)).unwrap());
    assert_eq!(runtime.root().unwrap().children[1].text, "two 2");
    assert!(runtime.dispatch(id, Event::Link(0)).unwrap());
    assert_eq!(runtime.root().unwrap().children[1].text, "one 1");
}

#[test]
fn invalid_styled_text_is_rejected_before_publication() {
    let many_spans = (0..300)
        .map(|index| format!("{{ text = 'x', weight = {} }}", 100 + index % 2 * 100))
        .collect::<Vec<_>>()
        .join(", ");
    let many_links = (0..70)
        .map(|index| format!("{{ text = 'x', link = 'l{index}' }}"))
        .collect::<Vec<_>>()
        .join(", ");
    for (properties, message) in [
        ("text = 'a', spans = { 'b' }", "mutually exclusive"),
        ("spans = { 'a' }, markup = 'b'", "mutually exclusive"),
        ("spans = 'plain'", "list"),
        ("spans = { 42 }", "string or a span table"),
        (
            "spans = { { text = 'a', size = 3 } }",
            "unknown span property",
        ),
        ("spans = { { bold = true } }", "span.text"),
        (
            "spans = { { text = 'a', weight = 5000 } }",
            "between 1 and 1000",
        ),
        ("spans = { { text = 'a', link = 42 } }", "span.link"),
        ("spans = { { text = 'a', link = '' } }", "links must be"),
        (&format!("spans = {{ {many_spans} }}"), "at most 256 spans"),
        (&format!("spans = {{ {many_links} }}"), "at most 64 links"),
        (
            "text = 'a', style = { letter_spacing = 500 }",
            "letter_spacing",
        ),
        (
            "text = 'a', overflow = 'ellipsis'",
            "ellipsis requires max_lines",
        ),
    ] {
        let source = format!("return ui.panel {{ children = {{ ui.text {{ {properties} }} }} }}");
        let error = match load(&source) {
            Ok(_) => panic!("accepted {properties}"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains(message), "{properties}: {error}");
    }
    let error = load("return ui.panel { children = { ui.button { on_link = function() end } } }")
        .err()
        .unwrap()
        .to_string();
    assert!(error.contains("on_link is not supported"), "{error}");

    // a rejected reactive update keeps the published spans.
    let (_directory, mut runtime) = load(
        r#"
        local bad = ui.state(false)
        return ui.panel { children = {
            ui.text { spans = function()
                return { { text = 'ok', weight = if bad:get() then 0 else 700 } }
            end },
            ui.button { on_click = function() bad:set(true) end },
        } }
        "#,
    )
    .unwrap();
    let before = label(&runtime).clone();
    let button = runtime.root().unwrap().children[1].id;
    assert!(runtime.dispatch(button, Event::Click).is_err());
    assert_eq!(label(&runtime), &before);
}
