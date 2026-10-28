use std::{borrow::Cow, num::NonZeroUsize, ops::Range};

use parley::{
    FontContext, FontFeature, FontSettings, FontStack, FontWeight, LayoutContext, PlainEditor,
    StyleProperty,
};
use thing_core::{Node, TextAlign};
use unicode_segmentation::UnicodeSegmentation;

const MAX_VALUE_BYTES: usize = 1024 * 1024;
static TABULAR_FEATURES: [FontFeature; 1] = [parley::swash::Setting {
    tag: parley::swash::tag_from_bytes(b"tnum"),
    value: 1,
}];

#[derive(Clone)]
pub struct TextInputCheckpoint {
    editor: PlainEditor<()>,
    scroll_x: f32,
}

#[derive(Clone, Debug, PartialEq)]
struct EditorStyle {
    family: String,
    size: f32,
    weight: f32,
    align: TextAlign,
    tabular_digits: bool,
}

impl EditorStyle {
    fn from_node(node: &Node) -> Self {
        Self {
            family: node.style.font_family.clone(),
            size: node.style.font_size,
            weight: node.style.font_weight,
            align: node.style.text_align,
            tabular_digits: node.style.tabular_digits,
        }
    }
}

/// transient editing state for the focused single-line field.
pub struct TextInputEditor {
    editor: PlainEditor<()>,
    fonts: FontContext,
    context: LayoutContext<()>,
    style: EditorStyle,
    viewport_width: f32,
    scroll_x: f32,
}

impl TextInputEditor {
    pub fn new(node: &Node, viewport_width: f32) -> Self {
        let style = EditorStyle::from_node(node);
        let mut result = Self {
            editor: PlainEditor::new(style.size),
            fonts: FontContext::default(),
            context: LayoutContext::default(),
            style: style.clone(),
            viewport_width: viewport_width.max(0.0),
            scroll_x: 0.0,
        };
        result.apply_style(&style);
        result.editor.set_width(None);
        result.editor.set_quantize(false);
        result.editor.set_text(&node.text);
        result.refresh();
        result
            .editor
            .driver(&mut result.fonts, &mut result.context)
            .move_to_line_end();
        result.ensure_caret_visible();
        result
    }

    pub fn prepare(&mut self, node: &Node, viewport_width: f32) {
        let style = EditorStyle::from_node(node);
        if self.style != style {
            self.apply_style(&style);
            self.style = style;
        }
        self.viewport_width = viewport_width.max(0.0);
        if self.editor.raw_text() != node.text {
            self.editor.set_text(&node.text);
            self.refresh();
            self.editor
                .driver(&mut self.fonts, &mut self.context)
                .move_to_line_end();
        }
        self.refresh();
        self.ensure_caret_visible();
    }

    pub fn checkpoint(&self) -> TextInputCheckpoint {
        TextInputCheckpoint {
            editor: self.editor.clone(),
            scroll_x: self.scroll_x,
        }
    }

    pub fn restore(&mut self, checkpoint: TextInputCheckpoint) {
        self.editor = checkpoint.editor;
        self.scroll_x = checkpoint.scroll_x;
    }

    pub fn value(&self) -> &str {
        self.editor.raw_text()
    }

    pub fn selected_text(&self) -> Option<&str> {
        self.editor.selected_text()
    }

    pub fn selection_range(&self) -> Range<usize> {
        self.editor.raw_selection().text_range()
    }

    pub fn replace_selection(&mut self, text: &str) -> bool {
        if text
            .chars()
            .any(|character| character.is_control() || matches!(character, '\u{2028}' | '\u{2029}'))
        {
            return false;
        }
        let selected = self.selection_range().len();
        let Some(next_len) = self
            .editor
            .raw_text()
            .len()
            .checked_sub(selected)
            .and_then(|len| len.checked_add(text.len()))
        else {
            return false;
        };
        if next_len > MAX_VALUE_BYTES {
            return false;
        }
        let before = self.editor.raw_text().to_owned();
        self.editor
            .driver(&mut self.fonts, &mut self.context)
            .insert_or_replace_selection(text);
        self.ensure_caret_visible();
        self.editor.raw_text() != before
    }

    pub fn backspace(&mut self, word: bool) -> bool {
        let before = self.editor.raw_text().to_owned();
        self.refresh();
        let selection = *self.editor.raw_selection();
        let grapheme_len = if selection.is_collapsed() && !word {
            let end = selection.focus().index();
            let start = self.editor.raw_text()[..end]
                .grapheme_indices(true)
                .next_back()
                .map_or(end, |(index, _)| index);
            NonZeroUsize::new(end - start)
        } else {
            None
        };
        let mut driver = self.editor.driver(&mut self.fonts, &mut self.context);
        if !selection.is_collapsed() {
            driver.delete_selection();
        } else if word {
            driver.backdelete_word();
        } else if let Some(len) = grapheme_len {
            driver.delete_bytes_before_selection(len);
        }
        self.ensure_caret_visible();
        self.editor.raw_text() != before
    }

    pub fn delete(&mut self, word: bool) -> bool {
        let before = self.editor.raw_text().to_owned();
        self.refresh();
        let selection = *self.editor.raw_selection();
        let grapheme_len = if selection.is_collapsed() && !word {
            let start = selection.focus().index();
            self.editor.raw_text()[start..]
                .graphemes(true)
                .next()
                .map(str::len)
                .and_then(NonZeroUsize::new)
        } else {
            None
        };
        let mut driver = self.editor.driver(&mut self.fonts, &mut self.context);
        if !selection.is_collapsed() {
            driver.delete_selection();
        } else if word {
            driver.delete_word();
        } else if let Some(len) = grapheme_len {
            driver.delete_bytes_after_selection(len);
        }
        self.ensure_caret_visible();
        self.editor.raw_text() != before
    }

    pub fn move_left(&mut self, select: bool, word: bool) {
        let mut driver = self.editor.driver(&mut self.fonts, &mut self.context);
        match (select, word) {
            (false, false) => driver.move_left(),
            (false, true) => driver.move_word_left(),
            (true, false) => driver.select_left(),
            (true, true) => driver.select_word_left(),
        }
        self.ensure_caret_visible();
    }

    pub fn move_right(&mut self, select: bool, word: bool) {
        let mut driver = self.editor.driver(&mut self.fonts, &mut self.context);
        match (select, word) {
            (false, false) => driver.move_right(),
            (false, true) => driver.move_word_right(),
            (true, false) => driver.select_right(),
            (true, true) => driver.select_word_right(),
        }
        self.ensure_caret_visible();
    }

    pub fn move_to_start(&mut self, select: bool) {
        let mut driver = self.editor.driver(&mut self.fonts, &mut self.context);
        // this editor is unwrapped and single-line. parley's text-start motion
        // preserves the horizontal position when the layout has only one line.
        if select {
            driver.select_to_line_start();
        } else {
            driver.move_to_line_start();
        }
        self.ensure_caret_visible();
    }

    pub fn move_to_end(&mut self, select: bool) {
        let mut driver = self.editor.driver(&mut self.fonts, &mut self.context);
        if select {
            driver.select_to_line_end();
        } else {
            driver.move_to_line_end();
        }
        self.ensure_caret_visible();
    }

    pub fn select_all(&mut self) {
        self.editor
            .driver(&mut self.fonts, &mut self.context)
            .select_all();
        self.ensure_caret_visible();
    }

    pub fn move_to_point(&mut self, x: f32, y: f32) {
        let x = x - self.text_offset_x();
        self.editor
            .driver(&mut self.fonts, &mut self.context)
            .move_to_point(x, y);
        self.ensure_caret_visible();
    }

    pub fn extend_selection_to_point(&mut self, x: f32, y: f32) {
        let x = x - self.text_offset_x();
        self.editor
            .driver(&mut self.fonts, &mut self.context)
            .extend_selection_to_point(x, y);
        self.ensure_caret_visible();
    }

    pub fn layout(&self) -> &parley::Layout<()> {
        self.editor
            .try_layout()
            .expect("text input layout is prepared before painting")
    }

    pub fn selection_geometry(&self) -> Vec<(parley::BoundingBox, usize)> {
        self.editor.selection_geometry()
    }

    pub fn cursor_geometry(&self) -> Option<parley::BoundingBox> {
        self.editor.cursor_geometry(1.0)
    }

    pub fn text_offset_x(&self) -> f32 {
        let layout = self.layout();
        let width = layout.full_width();
        if width > self.viewport_width {
            return -self.scroll_x;
        }
        let rtl = layout.is_rtl();
        match (self.style.align, rtl) {
            (TextAlign::Start, true) | (TextAlign::End, false) => self.viewport_width - width,
            (TextAlign::Center, _) => (self.viewport_width - width) / 2.0,
            _ => 0.0,
        }
    }

    fn apply_style(&mut self, style: &EditorStyle) {
        let styles = self.editor.edit_styles();
        styles.insert(StyleProperty::FontSize(style.size));
        styles.insert(StyleProperty::FontWeight(FontWeight::new(style.weight)));
        styles.insert(StyleProperty::FontStack(FontStack::Source(Cow::Owned(
            style.family.clone(),
        ))));
        if style.tabular_digits {
            styles.insert(StyleProperty::FontFeatures(FontSettings::List(Cow::Owned(
                TABULAR_FEATURES.to_vec(),
            ))));
        } else {
            let marker = StyleProperty::FontFeatures(FontSettings::List(Cow::Borrowed(&[])));
            styles.remove(std::mem::discriminant(&marker));
        }
    }

    fn refresh(&mut self) {
        self.editor
            .refresh_layout(&mut self.fonts, &mut self.context);
    }

    fn ensure_caret_visible(&mut self) {
        self.refresh();
        let text_width = self.layout().full_width();
        let Some(caret) = self.editor.cursor_geometry(1.0) else {
            let max_scroll = (text_width - self.viewport_width).max(0.0);
            self.scroll_x = self.scroll_x.clamp(0.0, max_scroll);
            return;
        };
        let x0 = caret.x0 as f32;
        let x1 = caret.x1 as f32;
        let max_scroll = (text_width.max(x1) - self.viewport_width).max(0.0);
        if x0 < self.scroll_x {
            self.scroll_x = x0;
        } else if x1 > self.scroll_x + self.viewport_width {
            self.scroll_x = x1 - self.viewport_width;
        }
        self.scroll_x = self.scroll_x.clamp(0.0, max_scroll);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use thing_core::{Kind, NodeId};

    fn editor(value: &str) -> TextInputEditor {
        editor_with_width(value, 160.0)
    }

    fn editor_with_width(value: &str, width: f32) -> TextInputEditor {
        let mut node = Node::new(NodeId(1), Kind::TextInput);
        node.text = value.into();
        TextInputEditor::new(&node, width)
    }

    fn assert_caret_visible(editor: &TextInputEditor, width: f32) {
        let caret = editor.cursor_geometry().unwrap();
        let offset = editor.text_offset_x();
        let start = offset + caret.x0 as f32;
        let end = offset + caret.x1 as f32;
        assert!(start >= 0.0, "caret starts at {start} before the viewport");
        assert!(end <= width, "caret ends at {end} after viewport {width}");
    }

    #[test]
    fn backspace_removes_extended_graphemes() {
        let mut editor = editor("a e\u{301} 👨‍👩‍👧");
        assert!(editor.backspace(false));
        assert_eq!(editor.value(), "a e\u{301} ");
        assert!(editor.backspace(false));
        assert_eq!(editor.value(), "a e\u{301}");
        assert!(editor.backspace(false));
        assert_eq!(editor.value(), "a ");
    }

    #[test]
    fn editing_sequence_uses_single_line_boundaries() {
        let mut editor = editor("naïve אבג 👨‍👩‍👧");
        editor.select_all();
        assert!(editor.replace_selection("x👨‍👩‍👧y"));
        editor.move_left(false, false);
        assert!(editor.backspace(false));
        editor.move_to_start(false);
        assert!(editor.delete(false));
        editor.move_to_end(false);
        for character in ["a", "b", "c"] {
            assert!(editor.replace_selection(character));
        }
        editor.move_left(false, false);
        editor.move_left(false, false);
        editor.move_right(true, false);
        assert!(editor.replace_selection("z"));

        assert_eq!(editor.value(), "yazc");
    }

    #[test]
    fn boundary_selection_extends_from_the_existing_caret() {
        let mut editor = editor("abc");
        editor.move_to_start(true);
        assert_eq!(editor.selection_range(), 0..3);
        editor.move_to_start(false);
        assert_eq!(editor.selection_range(), 0..0);
        editor.move_to_end(true);
        assert_eq!(editor.selection_range(), 0..3);
        editor.move_to_end(false);
        assert_eq!(editor.selection_range(), 3..3);
    }

    #[test]
    fn right_to_left_boundaries_use_logical_text_edges() {
        let mut editor = editor("אבג");
        assert_eq!(
            editor.selection_range(),
            editor.value().len()..editor.value().len()
        );
        editor.move_to_start(false);
        assert_eq!(editor.selection_range(), 0..0);
        editor.move_to_end(false);
        assert_eq!(
            editor.selection_range(),
            editor.value().len()..editor.value().len()
        );
        editor.move_to_start(true);
        assert_eq!(editor.selection_range(), 0..editor.value().len());
    }

    #[test]
    fn horizontal_scroll_keeps_ltr_caret_visible_at_both_ends() {
        let mut editor = editor_with_width("a long value that needs horizontal scrolling", 40.0);
        assert_caret_visible(&editor, 40.0);
        editor.move_to_start(false);
        assert_caret_visible(&editor, 40.0);
        editor.move_to_end(false);
        assert_caret_visible(&editor, 40.0);
    }

    #[test]
    fn horizontal_scroll_keeps_rtl_caret_visible_at_both_ends() {
        let mut editor = editor_with_width("אבגדה וזחטי כלמנס עפצקר שת", 40.0);
        assert!(editor.layout().full_width() > 40.0);
        assert_caret_visible(&editor, 40.0);
        editor.move_to_start(false);
        assert_caret_visible(&editor, 40.0);
        editor.move_to_end(false);
        assert_caret_visible(&editor, 40.0);
    }

    #[test]
    fn short_values_keep_their_alignment_while_the_caret_moves() {
        let mut editor = editor_with_width("short", 160.0);
        assert!(editor.layout().full_width() < 160.0);
        assert_eq!(editor.text_offset_x(), 0.0);
        assert_caret_visible(&editor, 160.0);
        editor.move_to_start(false);
        assert_eq!(editor.text_offset_x(), 0.0);
        assert_caret_visible(&editor, 160.0);
        editor.move_to_end(false);
        assert_eq!(editor.text_offset_x(), 0.0);
        assert_caret_visible(&editor, 160.0);
    }

    #[test]
    fn point_selection_and_bidi_movement_stay_on_utf8_boundaries() {
        let mut mixed = editor("abc אבג");
        mixed.move_to_point(0.0, 0.0);
        mixed.move_right(true, false);
        let range = mixed.selection_range();
        assert!(mixed.value().is_char_boundary(range.start));
        assert!(mixed.value().is_char_boundary(range.end));
        assert!(!range.is_empty());

        mixed.move_to_point(10_000.0, 0.0);
        let range = mixed.selection_range();
        assert!(range.is_empty());
        assert!(mixed.value().is_char_boundary(range.end));

        let mut ltr = editor("abc");
        ltr.move_to_point(10_000.0, 0.0);
        assert_eq!(ltr.selection_range().end, ltr.value().len());
    }

    #[test]
    fn checkpoints_restore_text_selection_and_scroll() {
        let mut editor = editor("a long value that needs horizontal scrolling");
        let checkpoint = editor.checkpoint();
        editor.select_all();
        assert!(editor.replace_selection("short"));
        assert_eq!(editor.value(), "short");
        editor.restore(checkpoint);
        assert_eq!(
            editor.value(),
            "a long value that needs horizontal scrolling"
        );
        assert_eq!(
            editor.selection_range(),
            editor.value().len()..editor.value().len()
        );
    }

    #[test]
    fn replacement_rejects_controls_and_unicode_line_separators() {
        let mut editor = editor("safe");
        for invalid in ['\u{000b}', '\u{000c}', '\u{0085}', '\u{2028}', '\u{2029}'] {
            editor.select_all();
            assert!(!editor.replace_selection(&invalid.to_string()));
            assert_eq!(editor.value(), "safe");
        }
    }
}
