use std::{borrow::Cow, collections::BTreeMap};

use parley::{
    Alignment, AlignmentOptions, FontContext, FontFeature, FontSettings, FontStack, LayoutContext,
    OverflowWrap, StyleProperty, WordBreakStrength,
};
use thing_core::{Node, NodeId, TextAlign, TextOverflow};

use crate::{Measurer, Size};

const ELLIPSIS: &str = "…";
const RTL_MARK: char = '\u{200f}';
const MAX_ELLIPSIS_CANDIDATES: usize = 3;
static TABULAR_FEATURES: [FontFeature; 1] = [parley::swash::Setting {
    tag: parley::swash::tag_from_bytes(b"tnum"),
    value: 1,
}];

struct Cached {
    text: String,
    family: String,
    size: f32,
    weight: f32,
    width: Option<f32>,
    max_lines: Option<u32>,
    overflow: TextOverflow,
    align: TextAlign,
    tabular_digits: bool,
    layout: parley::Layout<()>,
}

/// font discovery and shaped text cached by widget.
#[derive(Default)]
pub struct TextSystem {
    fonts: FontContext,
    context: LayoutContext<()>,
    cache: BTreeMap<NodeId, Cached>,
}

impl TextSystem {
    pub fn layout(&mut self, node: &Node, width: Option<f32>) -> &parley::Layout<()> {
        let width = width.map(|value| value.max(0.0));
        let valid = self.cache.get(&node.id).is_some_and(|cached| {
            cached.text == node.text
                && cached.family == node.style.font_family
                && cached.size == node.style.font_size
                && cached.weight == node.style.font_weight
                && cached.width == width
                && cached.max_lines == node.max_lines
                && cached.overflow == node.text_overflow
                && cached.align == node.style.text_align
                && cached.tabular_digits == node.style.tabular_digits
        });
        if !valid {
            let (layout, _) = build_layout(&mut self.fonts, &mut self.context, node, width);
            self.cache.insert(
                node.id,
                Cached {
                    text: node.text.clone(),
                    family: node.style.font_family.clone(),
                    size: node.style.font_size,
                    weight: node.style.font_weight,
                    width,
                    max_lines: node.max_lines,
                    overflow: node.text_overflow,
                    align: node.style.text_align,
                    tabular_digits: node.style.tabular_digits,
                    layout,
                },
            );
        }
        &self.cache[&node.id].layout
    }

    /// discard layouts for removed widgets after a reload or list update.
    pub fn retain(&mut self, root: &Node) {
        self.cache.retain(|id, _| root.find(*id).is_some());
    }
}

fn build_layout(
    fonts: &mut FontContext,
    context: &mut LayoutContext<()>,
    node: &Node,
    width: Option<f32>,
) -> (parley::Layout<()>, Option<String>) {
    let mut layout = shape(fonts, context, node, &node.text, false);
    let truncated = break_lines(&mut layout, width, node.max_lines);
    let too_wide = width.is_some_and(|width| layout.width() > width);
    if node.text_overflow != TextOverflow::Ellipsis || (!truncated && !too_wide) {
        align(&mut layout, width, node.style.text_align);
        return (layout, None);
    }

    let source_is_rtl = layout.is_rtl();
    ellipsize(fonts, context, node, width, source_is_rtl)
}

fn ellipsize(
    fonts: &mut FontContext,
    context: &mut LayoutContext<()>,
    node: &Node,
    width: Option<f32>,
    source_is_rtl: bool,
) -> (parley::Layout<()>, Option<String>) {
    let mut marker = shape_candidate(fonts, context, node, ELLIPSIS, source_is_rtl);
    marker.break_all_lines(None);
    let marker_width = marker.width();
    let available_width = width.unwrap_or(f32::MAX);
    if marker_width > available_width {
        let rendered = String::new();
        let mut layout = shape_candidate(fonts, context, node, &rendered, source_is_rtl);
        layout.break_all_lines(width);
        align(&mut layout, width, node.style.text_align);
        return (layout, Some(rendered));
    }

    let mut source = shape(fonts, context, node, &node.text, true);
    break_lines(
        &mut source,
        Some((available_width - marker_width).max(0.0)),
        Some(1),
    );
    let mut end = source
        .get(0)
        .map(|line| line.text_range().end.min(node.text.len()))
        .unwrap_or(0);
    end = trim_line_end(&source, end);
    source.break_all_lines(None);

    // contextual shaping can alter the final width. keep retries constant and
    // fall back to the marker rather than repeatedly reshaping a long string.
    let mut best = None;
    for _ in 0..MAX_ELLIPSIS_CANDIDATES {
        let mut rendered = String::with_capacity(end + ELLIPSIS.len());
        rendered.push_str(&node.text[..end]);
        rendered.push_str(ELLIPSIS);
        let mut candidate = shape_candidate(fonts, context, node, &rendered, source_is_rtl);
        candidate.break_all_lines(width);
        if candidate.len() == 1 && width.is_none_or(|width| candidate.width() <= width) {
            let Some(next) = next_content_end(&source, end) else {
                align(&mut candidate, width, node.style.text_align);
                return (candidate, Some(rendered));
            };
            best = Some((candidate, rendered));
            end = next;
            continue;
        }
        if let Some((mut layout, rendered)) = best.take() {
            align(&mut layout, width, node.style.text_align);
            return (layout, Some(rendered));
        }
        let Some(previous) = previous_cluster_start(&source, end) else {
            break;
        };
        end = trim_line_end(&source, previous);
    }

    if let Some((mut layout, rendered)) = best {
        align(&mut layout, width, node.style.text_align);
        return (layout, Some(rendered));
    }

    marker.break_all_lines(width);
    align(&mut marker, width, node.style.text_align);
    (marker, Some(ELLIPSIS.into()))
}

fn shape_candidate(
    fonts: &mut FontContext,
    context: &mut LayoutContext<()>,
    node: &Node,
    text: &str,
    source_is_rtl: bool,
) -> parley::Layout<()> {
    if !source_is_rtl {
        return shape(fonts, context, node, text, false);
    }
    // a neutral truncated prefix must retain the source paragraph direction.
    let mut directed = String::with_capacity(text.len() + RTL_MARK.len_utf8());
    directed.push_str(text);
    directed.push(RTL_MARK);
    shape(fonts, context, node, &directed, false)
}

fn shape(
    fonts: &mut FontContext,
    context: &mut LayoutContext<()>,
    node: &Node,
    text: &str,
    break_anywhere: bool,
) -> parley::Layout<()> {
    let mut builder = context.ranged_builder(fonts, text, 1.0, false);
    builder.push_default(StyleProperty::FontSize(node.style.font_size));
    builder.push_default(StyleProperty::FontWeight(parley::FontWeight::new(
        node.style.font_weight,
    )));
    builder.push_default(StyleProperty::FontStack(FontStack::Source(Cow::Borrowed(
        &node.style.font_family,
    ))));
    if node.style.tabular_digits {
        builder.push_default(StyleProperty::FontFeatures(FontSettings::List(
            Cow::Borrowed(&TABULAR_FEATURES),
        )));
    }
    if break_anywhere {
        builder.push_default(StyleProperty::OverflowWrap(OverflowWrap::Anywhere));
        builder.push_default(StyleProperty::WordBreak(WordBreakStrength::BreakAll));
    }
    builder.build(text)
}

fn break_lines(
    layout: &mut parley::Layout<()>,
    width: Option<f32>,
    max_lines: Option<u32>,
) -> bool {
    let Some(max_lines) = max_lines else {
        layout.break_all_lines(width);
        return false;
    };
    let mut breaker = layout.break_lines();
    for _ in 0..max_lines {
        if breaker.break_next(width.unwrap_or(f32::MAX)).is_none() {
            break;
        }
    }
    let truncated = !breaker.is_done();
    breaker.finish();
    truncated
}

fn align(layout: &mut parley::Layout<()>, width: Option<f32>, value: TextAlign) {
    let value = match value {
        TextAlign::Start => Alignment::Start,
        TextAlign::Center => Alignment::Center,
        TextAlign::End => Alignment::End,
    };
    layout.align(width, value, AlignmentOptions::default());
}

fn trim_line_end(layout: &parley::Layout<()>, end: usize) -> usize {
    let mut trimmed = 0;
    for line in layout.lines() {
        for run in line.runs() {
            for cluster in run.clusters() {
                let range = cluster.text_range();
                if range.end <= end && !cluster.is_hard_line_break() && !cluster.is_space_or_nbsp()
                {
                    trimmed = trimmed.max(range.end);
                }
            }
        }
    }
    trimmed
}

fn previous_cluster_start(layout: &parley::Layout<()>, end: usize) -> Option<usize> {
    let mut previous = None;
    for line in layout.lines() {
        for run in line.runs() {
            for cluster in run.clusters() {
                let range = cluster.text_range();
                if range.end <= end
                    && previous.is_none_or(|(previous_end, _)| range.end > previous_end)
                {
                    previous = Some((range.end, range.start));
                }
            }
        }
    }
    previous.map(|(_, start)| start)
}

fn next_content_end(layout: &parley::Layout<()>, end: usize) -> Option<usize> {
    let mut next_content = None;
    let mut next_break = None;
    for line in layout.lines() {
        for run in line.runs() {
            for cluster in run.clusters() {
                let range = cluster.text_range();
                if range.start < end {
                    continue;
                }
                if cluster.is_hard_line_break() {
                    next_break =
                        Some(next_break.map_or(range.start, |start: usize| start.min(range.start)));
                } else if !cluster.is_space_or_nbsp()
                    && next_content.is_none_or(|(start, _)| range.start < start)
                {
                    next_content = Some((range.start, range.end));
                }
            }
        }
    }
    let (start, end) = next_content?;
    if next_break.is_some_and(|line_break| line_break <= start) {
        None
    } else {
        Some(end)
    }
}

impl Measurer for TextSystem {
    fn measure(&mut self, node: &Node, max_width: Option<f32>) -> Size {
        let layout = self.layout(node, max_width);
        Size {
            width: layout.width(),
            height: layout.height(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parley::layout::Cluster;
    use thing_core::Kind;

    fn node(text: &str) -> Node {
        let mut node = Node::new(NodeId(1), Kind::Text);
        node.text = text.into();
        node
    }

    fn build(
        text: &mut TextSystem,
        node: &Node,
        width: Option<f32>,
    ) -> (parley::Layout<()>, Option<String>) {
        build_layout(&mut text.fonts, &mut text.context, node, width)
    }

    fn natural_width(text: &mut TextSystem, base: &Node, value: &str) -> f32 {
        let mut probe = base.clone();
        probe.text = value.into();
        probe.max_lines = None;
        probe.text_overflow = TextOverflow::Clip;
        build(text, &probe, None).0.width()
    }

    #[test]
    fn shapes_mixed_scripts_and_remeasures_changed_text() {
        let mut text = TextSystem::default();
        let mut node = Node::new(NodeId(1), Kind::Text);
        node.text = "Hello שלום مرحبا".into();
        let size = text.measure(&node, None);
        // this integration check needs at least one installed system font.
        assert!(size.width > 0.0 && size.height > 0.0);
        node.text = "Hello".into();
        assert!(text.measure(&node, None).width < size.width);
        node.text = "Hello world".into();
        assert!(text.measure(&node, Some(10.0)).height > size.height);
    }

    #[test]
    fn separates_layouts_for_colliding_ids_from_different_roots() {
        let mut text = TextSystem::default();
        let mut first = Node::new(NodeId(1), Kind::Row);
        let mut first_label = Node::new(NodeId(2), Kind::Text);
        first_label.text = "short".into();
        first_label.style.font_size = 12.0;
        first.children.push(first_label);

        let mut second = Node::new(NodeId(1), Kind::Row);
        let mut second_label = Node::new(NodeId(2), Kind::Text);
        second_label.text = "a much longer label for another output".into();
        second_label.style.font_size = 24.0;
        second_label.style.font_weight = 700.0;
        second.children.push(second_label);

        text.retain(&first);
        let first_size = text.measure(&first.children[0], None);
        text.retain(&second);
        let second_size = text.measure(&second.children[0], Some(40.0));
        assert!(second_size.height > first_size.height);

        text.retain(&first);
        assert_eq!(text.measure(&first.children[0], None), first_size);
    }

    #[test]
    fn line_limits_alignment_and_font_features_invalidate_the_cache() {
        let mut text = TextSystem::default();
        let mut node = node("one two three four five six seven");

        let unlimited_height = text.layout(&node, Some(45.0)).height();
        node.max_lines = Some(2);
        let limited = text.layout(&node, Some(45.0));
        assert_eq!(limited.len(), 2);
        assert!(limited.height() < unlimited_height);
        assert_eq!(text.cache[&node.id].max_lines, Some(2));

        node.text = "12".into();
        node.max_lines = Some(1);
        let start = text
            .layout(&node, Some(100.0))
            .get(0)
            .unwrap()
            .metrics()
            .offset;
        node.style.text_align = TextAlign::End;
        let end = text
            .layout(&node, Some(100.0))
            .get(0)
            .unwrap()
            .metrics()
            .offset;
        assert!(end > start);
        assert_eq!(text.cache[&node.id].align, TextAlign::End);

        node.style.tabular_digits = true;
        text.layout(&node, Some(100.0));
        assert!(text.cache[&node.id].tabular_digits);

        node.text = "a label that does not fit".into();
        node.text_overflow = TextOverflow::Ellipsis;
        text.layout(&node, Some(50.0));
        assert_eq!(text.cache[&node.id].overflow, TextOverflow::Ellipsis);
    }

    #[test]
    fn ellipsizes_latin_text_but_leaves_an_exact_fit_unchanged() {
        let mut text = TextSystem::default();
        let mut node = node("a long latin label that should not fit");
        node.max_lines = Some(1);
        node.text_overflow = TextOverflow::Ellipsis;

        let (layout, rendered) = build(&mut text, &node, Some(120.0));
        let rendered = rendered.unwrap();
        assert!(rendered.ends_with(ELLIPSIS));
        assert!(node.text.starts_with(rendered.trim_end_matches(ELLIPSIS)));
        assert_eq!(layout.len(), 1);
        assert!(layout.width() <= 120.0);
        assert_eq!(node.text, "a long latin label that should not fit");

        node.text = "exact fit".into();
        let width = natural_width(&mut text, &node, &node.text);
        let (layout, rendered) = build(&mut text, &node, Some(width));
        assert!(rendered.is_none());
        assert_eq!(layout.len(), 1);
        assert_eq!(layout.width(), width);
    }

    #[test]
    fn ellipsis_preserves_emoji_and_combining_clusters() {
        let mut text = TextSystem::default();
        let mut node = node("");
        node.max_lines = Some(1);
        node.text_overflow = TextOverflow::Ellipsis;
        let marker_width = natural_width(&mut text, &node, ELLIPSIS);

        for cluster in ["👨‍👩‍👧‍👦", "e\u{301}"] {
            node.text = format!("{cluster} trailing text");
            let width = natural_width(&mut text, &node, cluster) + marker_width + 0.5;
            let (_, rendered) = build(&mut text, &node, Some(width));
            assert_eq!(rendered.unwrap(), format!("{cluster}{ELLIPSIS}"));
        }
    }

    #[test]
    fn ellipsis_handles_newlines_rtl_and_a_box_narrower_than_the_marker() {
        let mut text = TextSystem::default();
        let mut node = node("short\nsecond line");
        node.max_lines = Some(1);
        node.text_overflow = TextOverflow::Ellipsis;

        let (_, rendered) = build(&mut text, &node, Some(200.0));
        assert_eq!(rendered.as_deref(), Some("short…"));

        node.text = "هذا عنوان عربي طويل للغاية".into();
        let (layout, rendered) = build(&mut text, &node, Some(120.0));
        let rendered = rendered.unwrap();
        assert!(layout.is_rtl());
        assert!(rendered.ends_with(ELLIPSIS));
        let marker = Cluster::from_byte_index(&layout, rendered.len() - ELLIPSIS.len()).unwrap();
        let first = Cluster::from_byte_index(&layout, 0).unwrap();
        assert!(marker.visual_offset().unwrap() < first.visual_offset().unwrap());

        node.text = "hidden".into();
        let (layout, rendered) = build(&mut text, &node, Some(0.0));
        assert_eq!(rendered.as_deref(), Some(""));
        assert_eq!(layout.len(), 1);
        assert_eq!(layout.width(), 0.0);
        assert!(layout.height() > 0.0);
    }

    #[test]
    fn ellipsis_removes_all_trailing_space_before_a_hard_break() {
        let mut text = TextSystem::default();
        let mut node = node("word     \nremainder");
        node.max_lines = Some(1);
        node.text_overflow = TextOverflow::Ellipsis;

        let (_, rendered) = build(&mut text, &node, Some(200.0));
        assert_eq!(rendered.as_deref(), Some("word…"));

        node.text = format!("word{}\nremainder", " ".repeat(10_000));
        let (_, rendered) = build(&mut text, &node, Some(100_000.0));
        assert_eq!(rendered.as_deref(), Some("word…"));
    }

    #[test]
    fn ellipsis_probes_forward_after_conservative_width_estimation() {
        let mut text = TextSystem::default();
        let mut node = node("hello world after");
        node.max_lines = Some(1);
        node.text_overflow = TextOverflow::Ellipsis;
        let width = natural_width(&mut text, &node, "hello wor…") + 0.5;

        let (layout, rendered) = build(&mut text, &node, Some(width));
        assert_eq!(rendered.as_deref(), Some("hello wor…"));
        assert!(layout.width() <= width);
    }

    #[test]
    fn ellipsis_keeps_rtl_direction_when_the_visible_prefix_is_neutral() {
        let mut text = TextSystem::default();
        let mut node = node("123456789 هذا عنوان عربي");
        node.max_lines = Some(1);
        node.text_overflow = TextOverflow::Ellipsis;
        assert!(build(&mut text, &node, None).0.is_rtl());
        let width = natural_width(&mut text, &node, "123…") + 0.5;

        let (layout, rendered) = build(&mut text, &node, Some(width));
        let rendered = rendered.unwrap();
        assert_eq!(rendered, "123…");
        assert!(layout.is_rtl());
        let marker = Cluster::from_byte_index(&layout, rendered.len() - ELLIPSIS.len()).unwrap();
        let first = Cluster::from_byte_index(&layout, 0).unwrap();
        assert!(marker.visual_offset().unwrap() < first.visual_offset().unwrap());
    }
}
