use std::{borrow::Cow, num::NonZeroUsize, ops::Range};

use parley::{
    FontContext, FontFeature, FontSettings, FontStack, FontWeight, LayoutContext, PlainEditor,
    StyleProperty,
    layout::{Affinity, Cursor, Selection},
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
    revision: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TextInputPreedit {
    pub text: String,
    pub cursor: Option<(usize, usize)>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TextInputImeBatch {
    pub preedit: Option<TextInputPreedit>,
    pub commit: String,
    pub delete_before: usize,
    pub delete_after: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextInputSurrounding {
    pub value: String,
    pub anchor: usize,
    pub cursor: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextInputImeOutcome {
    pub changed: bool,
    pub surrounding: TextInputSurrounding,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextInputImeError {
    InvalidCursor,
    InvalidDelete,
    InvalidText,
    ValueTooLong,
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
    revision: u64,
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
            revision: 0,
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
        if self.editor.text() != node.text.as_str() {
            self.editor.set_text(&node.text);
            self.revision = self.revision.wrapping_add(1);
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
            revision: self.revision,
        }
    }

    pub fn restore(&mut self, checkpoint: TextInputCheckpoint) {
        self.editor = checkpoint.editor;
        self.scroll_x = checkpoint.scroll_x;
        self.revision = checkpoint.revision;
    }

    pub fn value(&self) -> Cow<'_, str> {
        if self.editor.raw_compose().is_some() {
            Cow::Owned(self.editor.text().to_string())
        } else {
            Cow::Borrowed(self.editor.raw_text())
        }
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn display_is_empty(&self) -> bool {
        self.editor.raw_text().is_empty()
    }

    pub fn is_composing(&self) -> bool {
        self.editor.raw_compose().is_some()
    }

    pub fn cancel_composition(&mut self) -> bool {
        if !self.is_composing() {
            return false;
        }
        self.editor
            .driver(&mut self.fonts, &mut self.context)
            .clear_compose();
        self.ensure_caret_visible();
        true
    }

    pub fn selected_text(&self) -> Option<&str> {
        self.editor.selected_text()
    }

    pub fn selection_range(&self) -> Range<usize> {
        self.editor.raw_selection().text_range()
    }

    pub fn display_selection(&self) -> (usize, usize) {
        let selection = self.editor.raw_selection();
        (selection.anchor().index(), selection.focus().index())
    }

    pub fn surrounding(&self) -> TextInputSurrounding {
        let value = self.value().into_owned();
        if let Some(compose) = self.editor.raw_compose() {
            return TextInputSurrounding {
                value,
                anchor: compose.start,
                cursor: compose.start,
            };
        }
        let selection = self.editor.raw_selection();
        TextInputSurrounding {
            value,
            anchor: selection.anchor().index(),
            cursor: selection.focus().index(),
        }
    }

    pub fn apply_ime_batch(
        &mut self,
        batch: TextInputImeBatch,
    ) -> Result<TextInputImeOutcome, TextInputImeError> {
        let checkpoint = self.checkpoint();
        let previous = self.surrounding();
        let result = self.apply_ime_batch_inner(batch, &previous);
        if result.is_err() {
            self.restore(checkpoint);
        }
        result
    }

    fn apply_ime_batch_inner(
        &mut self,
        batch: TextInputImeBatch,
        previous: &TextInputSurrounding,
    ) -> Result<TextInputImeOutcome, TextInputImeError> {
        if invalid_text(&batch.commit) {
            return Err(TextInputImeError::InvalidText);
        }
        if let Some(preedit) = &batch.preedit {
            validate_preedit(preedit)?;
        }

        if self.is_composing() {
            self.editor
                .driver(&mut self.fonts, &mut self.context)
                .clear_compose();
        }
        self.delete_surrounding(batch.delete_before, batch.delete_after)?;
        if !batch.commit.is_empty() {
            self.replace_selection_inner(&batch.commit, false)?;
        }

        let preedit = batch.preedit.filter(|preedit| !preedit.text.is_empty());
        if preedit.is_some() && !self.editor.raw_selection().is_collapsed() {
            self.editor
                .driver(&mut self.fonts, &mut self.context)
                .delete_selection();
        }
        let surrounding = self.surrounding();

        if let Some(preedit) = preedit {
            let selected = self.selection_range().len();
            let next_len = self
                .editor
                .raw_text()
                .len()
                .checked_sub(selected)
                .and_then(|len| len.checked_add(preedit.text.len()))
                .ok_or(TextInputImeError::ValueTooLong)?;
            if next_len > MAX_VALUE_BYTES {
                return Err(TextInputImeError::ValueTooLong);
            }
            self.editor
                .driver(&mut self.fonts, &mut self.context)
                .set_compose(&preedit.text, preedit.cursor);
        }

        let changed = surrounding.value != previous.value;
        if surrounding != *previous {
            self.revision = self.revision.wrapping_add(1);
        }
        self.ensure_caret_visible();
        Ok(TextInputImeOutcome {
            changed,
            surrounding,
        })
    }

    fn delete_surrounding(&mut self, before: usize, after: usize) -> Result<(), TextInputImeError> {
        let selection = *self.editor.raw_selection();
        let range = selection.text_range();
        let start = range
            .start
            .checked_sub(before)
            .ok_or(TextInputImeError::InvalidDelete)?;
        let end = range
            .end
            .checked_add(after)
            .filter(|end| *end <= self.editor.raw_text().len())
            .ok_or(TextInputImeError::InvalidDelete)?;
        if !self.editor.raw_text().is_char_boundary(start)
            || !self.editor.raw_text().is_char_boundary(end)
        {
            return Err(TextInputImeError::InvalidDelete);
        }
        let mut driver = self.editor.driver(&mut self.fonts, &mut self.context);
        if let Some(after) = NonZeroUsize::new(after) {
            driver.delete_bytes_after_selection(after);
        }
        if let Some(before) = NonZeroUsize::new(before) {
            driver.delete_bytes_before_selection(before);
        }
        Ok(())
    }

    pub fn replace_selection(&mut self, text: &str) -> bool {
        self.cancel_composition();
        self.replace_selection_inner(text, true).unwrap_or(false)
    }

    fn replace_selection_inner(
        &mut self,
        text: &str,
        update_revision: bool,
    ) -> Result<bool, TextInputImeError> {
        if invalid_text(text) {
            return Err(TextInputImeError::InvalidText);
        }
        let selected = self.selection_range().len();
        let Some(next_len) = self
            .editor
            .raw_text()
            .len()
            .checked_sub(selected)
            .and_then(|len| len.checked_add(text.len()))
        else {
            return Err(TextInputImeError::ValueTooLong);
        };
        if next_len > MAX_VALUE_BYTES {
            return Err(TextInputImeError::ValueTooLong);
        }
        let before = self.surrounding();
        self.editor
            .driver(&mut self.fonts, &mut self.context)
            .insert_or_replace_selection(text);
        self.ensure_caret_visible();
        let after = self.surrounding();
        let changed = after.value != before.value;
        if update_revision && after != before {
            self.revision = self.revision.wrapping_add(1);
        }
        Ok(changed)
    }

    pub fn backspace(&mut self, word: bool) -> bool {
        self.cancel_composition();
        let before = self.surrounding();
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
        let after = self.surrounding();
        let changed = after.value != before.value;
        if after != before {
            self.revision = self.revision.wrapping_add(1);
        }
        changed
    }

    pub fn delete(&mut self, word: bool) -> bool {
        self.cancel_composition();
        let before = self.surrounding();
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
        let after = self.surrounding();
        let changed = after.value != before.value;
        if after != before {
            self.revision = self.revision.wrapping_add(1);
        }
        changed
    }

    pub fn move_left(&mut self, select: bool, word: bool) {
        self.cancel_composition();
        let before = self.display_selection();
        let mut driver = self.editor.driver(&mut self.fonts, &mut self.context);
        match (select, word) {
            (false, false) => driver.move_left(),
            (false, true) => driver.move_word_left(),
            (true, false) => driver.select_left(),
            (true, true) => driver.select_word_left(),
        }
        if self.display_selection() != before {
            self.revision = self.revision.wrapping_add(1);
        }
        self.ensure_caret_visible();
    }

    pub fn move_right(&mut self, select: bool, word: bool) {
        self.cancel_composition();
        let before = self.display_selection();
        let mut driver = self.editor.driver(&mut self.fonts, &mut self.context);
        match (select, word) {
            (false, false) => driver.move_right(),
            (false, true) => driver.move_word_right(),
            (true, false) => driver.select_right(),
            (true, true) => driver.select_word_right(),
        }
        if self.display_selection() != before {
            self.revision = self.revision.wrapping_add(1);
        }
        self.ensure_caret_visible();
    }

    pub fn move_to_start(&mut self, select: bool) {
        self.cancel_composition();
        let before = self.display_selection();
        let mut driver = self.editor.driver(&mut self.fonts, &mut self.context);
        // this editor is unwrapped and single-line. parley's text-start motion
        // preserves the horizontal position when the layout has only one line.
        if select {
            driver.select_to_line_start();
        } else {
            driver.move_to_line_start();
        }
        if self.display_selection() != before {
            self.revision = self.revision.wrapping_add(1);
        }
        self.ensure_caret_visible();
    }

    pub fn move_to_end(&mut self, select: bool) {
        self.cancel_composition();
        let before = self.display_selection();
        let mut driver = self.editor.driver(&mut self.fonts, &mut self.context);
        if select {
            driver.select_to_line_end();
        } else {
            driver.move_to_line_end();
        }
        if self.display_selection() != before {
            self.revision = self.revision.wrapping_add(1);
        }
        self.ensure_caret_visible();
    }

    pub fn select_all(&mut self) {
        self.cancel_composition();
        let before = self.display_selection();
        self.editor
            .driver(&mut self.fonts, &mut self.context)
            .select_all();
        if self.display_selection() != before {
            self.revision = self.revision.wrapping_add(1);
        }
        self.ensure_caret_visible();
    }

    pub(crate) fn select_byte_range(&mut self, anchor: usize, focus: usize) {
        self.editor
            .driver(&mut self.fonts, &mut self.context)
            .select_byte_range(anchor, focus);
        self.ensure_caret_visible();
    }

    pub fn move_to_point(&mut self, x: f32, y: f32) {
        self.cancel_composition();
        let before = self.display_selection();
        let x = x - self.text_offset_x();
        self.editor
            .driver(&mut self.fonts, &mut self.context)
            .move_to_point(x, y);
        if self.display_selection() != before {
            self.revision = self.revision.wrapping_add(1);
        }
        self.ensure_caret_visible();
    }

    pub fn extend_selection_to_point(&mut self, x: f32, y: f32) {
        self.cancel_composition();
        let before = self.display_selection();
        let x = x - self.text_offset_x();
        self.editor
            .driver(&mut self.fonts, &mut self.context)
            .extend_selection_to_point(x, y);
        if self.display_selection() != before {
            self.revision = self.revision.wrapping_add(1);
        }
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

    pub fn preedit_geometry(&self) -> Vec<(parley::BoundingBox, usize)> {
        let Some(range) = self.editor.raw_compose() else {
            return Vec::new();
        };
        let layout = self.layout();
        Selection::new(
            Cursor::from_byte_index(layout, range.start, Affinity::Downstream),
            Cursor::from_byte_index(layout, range.end, Affinity::Upstream),
        )
        .geometry(layout)
    }

    pub fn ime_cursor_area(&self) -> parley::BoundingBox {
        self.editor.ime_cursor_area()
    }

    pub fn text_offset_x(&self) -> f32 {
        let layout = self.layout();
        let width = layout.full_width();
        let mut offset = if width > self.viewport_width {
            -self.scroll_x
        } else {
            let rtl = layout.is_rtl();
            match (self.style.align, rtl) {
                (TextAlign::Start, true) | (TextAlign::End, false) => self.viewport_width - width,
                (TextAlign::Center, _) => (self.viewport_width - width) / 2.0,
                _ => 0.0,
            }
        };
        let Some(caret) = self.cursor_geometry() else {
            return offset;
        };
        let start = offset + caret.x0 as f32;
        let end = offset + caret.x1 as f32;
        if start < 0.0 {
            offset -= start;
        } else if end > self.viewport_width {
            offset -= end - self.viewport_width;
        }
        offset
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

fn invalid_text(text: &str) -> bool {
    text.chars()
        .any(|character| character.is_control() || matches!(character, '\u{2028}' | '\u{2029}'))
}

fn validate_preedit(preedit: &TextInputPreedit) -> Result<(), TextInputImeError> {
    if invalid_text(&preedit.text) {
        return Err(TextInputImeError::InvalidText);
    }
    let Some((anchor, cursor)) = preedit.cursor else {
        return Ok(());
    };
    if !preedit.text.is_char_boundary(anchor) || !preedit.text.is_char_boundary(cursor) {
        return Err(TextInputImeError::InvalidCursor);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use thing_core::{Kind, NodeId};

    fn editor(value: &str) -> TextInputEditor {
        editor_with_width(value, 160.0)
    }

    fn editor_with_width(value: &str, width: f32) -> TextInputEditor {
        editor_with_width_and_align(value, width, TextAlign::Start)
    }

    fn editor_with_width_and_align(
        value: &str,
        width: f32,
        text_align: TextAlign,
    ) -> TextInputEditor {
        let mut node = Node::new(NodeId(1), Kind::TextInput);
        node.text = value.into();
        node.style.text_align = text_align;
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
    fn exact_fit_keeps_the_end_caret_inside_start_alignment() {
        let mut node = Node::new(NodeId(1), Kind::TextInput);
        node.text = "exact fit".into();
        let mut editor = TextInputEditor::new(&node, 160.0);
        let width = editor.layout().full_width();
        editor.prepare(&node, width);

        assert_caret_visible(&editor, width);
    }

    #[test]
    fn short_end_aligned_ltr_keeps_the_caret_visible() {
        let mut editor = editor_with_width_and_align("short", 160.0, TextAlign::End);
        assert_caret_visible(&editor, 160.0);
        editor.move_to_start(false);
        assert_caret_visible(&editor, 160.0);
        editor.move_to_end(true);
        assert_caret_visible(&editor, 160.0);
    }

    #[test]
    fn short_start_aligned_rtl_keeps_the_caret_visible() {
        let mut editor = editor_with_width("אבג", 160.0);
        assert_caret_visible(&editor, 160.0);
        editor.move_to_start(false);
        assert_caret_visible(&editor, 160.0);
        editor.move_to_end(true);
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

    #[test]
    fn preedit_is_displayed_without_entering_the_controlled_value() {
        let mut editor = editor("abc");
        let outcome = editor
            .apply_ime_batch(TextInputImeBatch {
                preedit: Some(TextInputPreedit {
                    text: "候補".into(),
                    cursor: Some((6, 0)),
                }),
                ..TextInputImeBatch::default()
            })
            .unwrap();

        assert_eq!(outcome.surrounding.value, "abc");
        assert!(!outcome.changed);
        assert_eq!(editor.value(), "abc");
        assert_eq!(editor.display_selection(), (9, 3));
        assert!(editor.is_composing());
        assert!(!editor.display_is_empty());
        assert!(!editor.preedit_geometry().is_empty());
        assert!(editor.ime_cursor_area().width() > 0.0);
    }

    #[test]
    fn preedit_replaces_selection_as_one_committed_change() {
        let mut editor = editor("selected");
        editor.select_all();
        let outcome = editor
            .apply_ime_batch(TextInputImeBatch {
                preedit: Some(TextInputPreedit {
                    text: "編集中".into(),
                    cursor: Some((0, 9)),
                }),
                ..TextInputImeBatch::default()
            })
            .unwrap();

        assert!(outcome.changed);
        assert_eq!(outcome.surrounding.value, "");
        assert_eq!(outcome.surrounding.anchor, 0);
        assert_eq!(outcome.surrounding.cursor, 0);
        assert_eq!(editor.value(), "");

        let cleared = editor
            .apply_ime_batch(TextInputImeBatch::default())
            .unwrap();
        assert!(!cleared.changed);
        assert!(!editor.is_composing());
        assert!(editor.display_is_empty());
    }

    #[test]
    fn commit_replaces_old_preedit_before_installing_the_next_one() {
        let mut editor = editor("abc");
        editor
            .apply_ime_batch(TextInputImeBatch {
                preedit: Some(TextInputPreedit {
                    text: "x".into(),
                    cursor: Some((1, 1)),
                }),
                ..TextInputImeBatch::default()
            })
            .unwrap();
        let outcome = editor
            .apply_ime_batch(TextInputImeBatch {
                preedit: Some(TextInputPreedit {
                    text: "z".into(),
                    cursor: None,
                }),
                commit: "y".into(),
                ..TextInputImeBatch::default()
            })
            .unwrap();

        assert_eq!(outcome.surrounding.value, "abcy");
        assert_eq!(editor.value(), "abcy");
        assert!(editor.is_composing());
        assert!(editor.cursor_geometry().is_none());
    }

    #[test]
    fn delete_surrounding_uses_utf8_bytes_outside_the_selection() {
        let mut field = editor("aéXz");
        field.move_to_start(false);
        field.move_right(false, false);
        field.move_right(false, false);
        field.move_right(true, false);
        assert_eq!(field.selection_range(), 3..4);

        let outcome = field
            .apply_ime_batch(TextInputImeBatch {
                commit: "Q".into(),
                delete_before: "é".len(),
                delete_after: 1,
                ..TextInputImeBatch::default()
            })
            .unwrap();
        assert_eq!(outcome.surrounding.value, "aQ");

        let mut invalid = editor("é");
        let revision = invalid.revision();
        assert_eq!(
            invalid.apply_ime_batch(TextInputImeBatch {
                delete_before: 1,
                ..TextInputImeBatch::default()
            }),
            Err(TextInputImeError::InvalidDelete)
        );
        assert_eq!(invalid.value(), "é");
        assert_eq!(invalid.revision(), revision);
    }

    #[test]
    fn delete_surrounding_preserves_reversed_selection_orientation() {
        let mut editor = editor("aéXz");
        editor.move_to_start(false);
        editor.move_right(false, false);
        editor.move_right(false, false);
        editor.move_right(false, false);
        editor.move_left(true, false);
        assert_eq!(editor.display_selection(), (4, 3));

        let outcome = editor
            .apply_ime_batch(TextInputImeBatch {
                delete_before: "é".len(),
                delete_after: 1,
                ..TextInputImeBatch::default()
            })
            .unwrap();

        assert_eq!(outcome.surrounding.value, "aX");
        assert_eq!(outcome.surrounding.anchor, 2);
        assert_eq!(outcome.surrounding.cursor, 1);
    }

    #[test]
    fn delete_surrounding_counts_from_an_existing_preedit() {
        let mut editor = editor("abcz");
        editor.move_to_start(false);
        editor.move_right(false, false);
        editor.move_right(false, false);
        editor
            .apply_ime_batch(TextInputImeBatch {
                preedit: Some(TextInputPreedit {
                    text: "候".into(),
                    cursor: Some((3, 3)),
                }),
                ..TextInputImeBatch::default()
            })
            .unwrap();

        let outcome = editor
            .apply_ime_batch(TextInputImeBatch {
                commit: "Q".into(),
                delete_before: 1,
                delete_after: 1,
                ..TextInputImeBatch::default()
            })
            .unwrap();
        assert_eq!(outcome.surrounding.value, "aQz");
    }

    #[test]
    fn invalid_ime_batches_restore_text_selection_and_revision() {
        let mut editor = editor("é");
        editor.move_to_start(false);
        let before = editor.checkpoint();
        let revision = editor.revision();
        let invalid = TextInputImeBatch {
            preedit: Some(TextInputPreedit {
                text: "é".into(),
                cursor: Some((1, 2)),
            }),
            ..TextInputImeBatch::default()
        };
        assert_eq!(
            editor.apply_ime_batch(invalid),
            Err(TextInputImeError::InvalidCursor)
        );
        assert_eq!(editor.value(), "é");
        assert_eq!(editor.display_selection(), (0, 0));
        assert_eq!(editor.revision(), revision);

        assert_eq!(
            editor.apply_ime_batch(TextInputImeBatch {
                preedit: Some(TextInputPreedit {
                    text: "x".repeat(MAX_VALUE_BYTES + 1),
                    cursor: None,
                }),
                ..TextInputImeBatch::default()
            }),
            Err(TextInputImeError::ValueTooLong)
        );
        editor.restore(before);
    }

    #[test]
    fn invalid_batches_preserve_an_existing_composition() {
        let mut editor = editor("a long value that scrolls beyond the viewport");
        editor
            .apply_ime_batch(TextInputImeBatch {
                preedit: Some(TextInputPreedit {
                    text: "候補".into(),
                    cursor: Some((6, 0)),
                }),
                ..TextInputImeBatch::default()
            })
            .unwrap();
        let value = editor.value().into_owned();
        let selection = editor.display_selection();
        let scroll_x = editor.scroll_x;
        let revision = editor.revision();

        for invalid in [
            TextInputImeBatch {
                commit: "\n".into(),
                ..TextInputImeBatch::default()
            },
            TextInputImeBatch {
                preedit: Some(TextInputPreedit {
                    text: "\u{2028}".into(),
                    cursor: None,
                }),
                ..TextInputImeBatch::default()
            },
            TextInputImeBatch {
                commit: "x".repeat(MAX_VALUE_BYTES + 1),
                ..TextInputImeBatch::default()
            },
        ] {
            assert!(editor.apply_ime_batch(invalid).is_err());
            assert_eq!(editor.value(), value);
            assert_eq!(editor.display_selection(), selection);
            assert_eq!(editor.scroll_x, scroll_x);
            assert_eq!(editor.revision(), revision);
            assert!(editor.is_composing());
        }
    }

    #[test]
    fn exact_value_limit_is_accepted() {
        let mut editor = editor(&"x".repeat(MAX_VALUE_BYTES - 1));
        let outcome = editor
            .apply_ime_batch(TextInputImeBatch {
                commit: "y".into(),
                ..TextInputImeBatch::default()
            })
            .unwrap();

        assert_eq!(outcome.surrounding.value.len(), MAX_VALUE_BYTES);
        assert!(outcome.changed);
    }

    #[test]
    fn absent_and_empty_preedit_events_clear_composition() {
        let mut editor = editor("value");
        let compose = || TextInputImeBatch {
            preedit: Some(TextInputPreedit {
                text: "x".into(),
                cursor: Some((1, 1)),
            }),
            ..TextInputImeBatch::default()
        };

        editor.apply_ime_batch(compose()).unwrap();
        editor
            .apply_ime_batch(TextInputImeBatch {
                preedit: Some(TextInputPreedit::default()),
                ..TextInputImeBatch::default()
            })
            .unwrap();
        assert!(!editor.is_composing());

        editor.apply_ime_batch(compose()).unwrap();
        editor
            .apply_ime_batch(TextInputImeBatch::default())
            .unwrap();
        assert!(!editor.is_composing());
    }

    #[test]
    fn revision_tracks_only_committed_surrounding_changes() {
        let mut editor = editor("abc");
        let initial = editor.revision();
        editor
            .apply_ime_batch(TextInputImeBatch::default())
            .unwrap();
        assert_eq!(editor.revision(), initial);

        editor.move_to_end(false);
        assert_eq!(editor.revision(), initial);
        editor.move_left(false, false);
        let moved = editor.revision();
        assert_ne!(moved, initial);
        editor.move_right(false, false);
        assert_ne!(editor.revision(), moved);

        editor.select_all();
        let selected = editor.revision();
        assert!(!editor.replace_selection("abc"));
        assert_ne!(editor.revision(), selected);
        let collapsed = editor.revision();
        editor.move_to_end(false);
        assert_eq!(editor.revision(), collapsed);
    }

    #[test]
    fn prepare_preserves_matching_composition_and_replaces_external_values() {
        let mut editor = editor("old");
        editor
            .apply_ime_batch(TextInputImeBatch {
                preedit: Some(TextInputPreedit {
                    text: "x".into(),
                    cursor: Some((1, 1)),
                }),
                ..TextInputImeBatch::default()
            })
            .unwrap();
        let mut node = Node::new(NodeId(1), Kind::TextInput);
        node.text = "old".into();
        editor.prepare(&node, 160.0);
        assert!(editor.is_composing());

        node.text = "new".into();
        editor.prepare(&node, 160.0);
        assert!(!editor.is_composing());
        assert_eq!(editor.value(), "new");
    }
}
