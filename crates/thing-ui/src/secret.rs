use std::{
    ops::Range,
    sync::atomic::{Ordering, compiler_fence},
};

use thing_core::Node;
use unicode_segmentation::UnicodeSegmentation;

use crate::TextInputEditor;

pub const MAX_SECRET_BYTES: usize = 1024;
const PROTECTED_PAGE_BYTES: usize = 4096;
const MASK: &str = "•";

#[repr(C, align(4096))]
struct ProtectedPage([u8; PROTECTED_PAGE_BYTES]);

/// secret bytes with a bounded allocation and best-effort swap and dump protection.
///
/// the operating system and authentication modules may make their own copies. the
/// helper process is the hard lifetime boundary for those copies.
pub struct SecretBytes {
    page: Box<ProtectedPage>,
    len: usize,
}

impl SecretBytes {
    fn empty() -> Self {
        let page = Box::new(ProtectedPage([0; PROTECTED_PAGE_BYTES]));
        let mut secret = Self { page, len: 0 };
        secret.protect();
        secret
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn len(&self) -> usize {
        self.len
    }

    /// copy one bounded secret into protected storage.
    pub fn copy_from(bytes: &[u8]) -> Option<Self> {
        if bytes.len() > MAX_SECRET_BYTES || std::str::from_utf8(bytes).is_err() {
            return None;
        }
        let mut secret = Self::empty();
        secret.page.0[..bytes.len()].copy_from_slice(bytes);
        secret.len = bytes.len();
        Some(secret)
    }

    /// expose the bytes only for the duration of a private transport operation.
    pub fn expose<T>(&self, operation: impl FnOnce(&[u8]) -> T) -> T {
        operation(&self.page.0[..self.len])
    }

    fn as_str(&self) -> &str {
        // SAFETY: insertions are checked UTF-8 strings and edits preserve character boundaries.
        unsafe { std::str::from_utf8_unchecked(&self.page.0[..self.len]) }
    }

    fn replace(&mut self, range: Range<usize>, value: &str) -> bool {
        let old_len = self.len;
        let removed = range.len();
        let Some(new_len) = old_len
            .checked_sub(removed)
            .and_then(|length| length.checked_add(value.len()))
        else {
            return false;
        };
        if new_len > MAX_SECRET_BYTES {
            return false;
        }

        let destination = range.start + value.len();
        self.page.0.copy_within(range.end..old_len, destination);
        self.page.0[range.start..destination].copy_from_slice(value.as_bytes());
        if new_len < old_len {
            secure_zero(&mut self.page.0[new_len..old_len]);
        }
        self.len = new_len;
        true
    }

    fn clear(&mut self) {
        secure_zero(&mut self.page.0[..self.len]);
        self.len = 0;
    }

    fn protect(&mut self) {
        let pointer = self.page.0.as_mut_ptr().cast::<libc::c_void>();
        // these protections are best effort because low memlock limits are common.
        // SAFETY: pointer covers the live, page-aligned allocation for the stated length.
        unsafe {
            libc::mlock(pointer, PROTECTED_PAGE_BYTES);
            #[cfg(target_os = "linux")]
            libc::madvise(pointer, PROTECTED_PAGE_BYTES, libc::MADV_DONTDUMP);
        }
    }
}

impl Drop for SecretBytes {
    fn drop(&mut self) {
        self.clear();
        let pointer = self.page.0.as_mut_ptr().cast::<libc::c_void>();
        // SAFETY: the page remains valid until this drop completes.
        unsafe {
            libc::munlock(pointer, PROTECTED_PAGE_BYTES);
        }
    }
}

/// host-owned editing state for one `ui.secret_input`.
///
/// only a mask is copied into the ordinary text renderer. the underlying bytes
/// are neither clonable nor printable and never enter the widget tree.
pub struct SecretInputEditor {
    secret: SecretBytes,
    anchor: usize,
    focus: usize,
    display: TextInputEditor,
}

impl SecretInputEditor {
    pub fn new(node: &Node, viewport_width: f32) -> Self {
        let mut editor = Self {
            secret: SecretBytes::empty(),
            anchor: 0,
            focus: 0,
            display: TextInputEditor::new(node, viewport_width),
        };
        editor.sync_display(node, viewport_width);
        editor
    }

    pub fn prepare(&mut self, node: &Node, viewport_width: f32) {
        self.sync_display(node, viewport_width);
    }

    pub fn is_empty(&self) -> bool {
        self.secret.is_empty()
    }

    pub fn len(&self) -> usize {
        self.secret.len()
    }

    pub fn replace_selection(&mut self, value: &str, node: &Node, viewport_width: f32) -> bool {
        if invalid_secret_text(value) {
            return false;
        }
        let range = self.selection();
        if !self.secret.replace(range.clone(), value) {
            return false;
        }
        self.anchor = range.start + value.len();
        self.focus = self.anchor;
        self.sync_display(node, viewport_width);
        true
    }

    pub fn backspace(&mut self, word: bool, node: &Node, viewport_width: f32) -> bool {
        let mut range = self.selection();
        if range.is_empty() {
            range.start = if word {
                previous_word_start(self.secret.as_str(), range.start)
            } else {
                previous_grapheme(self.secret.as_str(), range.start)
            };
        }
        self.delete_range(range, node, viewport_width)
    }

    pub fn delete(&mut self, word: bool, node: &Node, viewport_width: f32) -> bool {
        let mut range = self.selection();
        if range.is_empty() {
            range.end = if word {
                next_word_end(self.secret.as_str(), range.end)
            } else {
                next_grapheme(self.secret.as_str(), range.end)
            };
        }
        self.delete_range(range, node, viewport_width)
    }

    pub fn move_left(&mut self, select: bool, word: bool, node: &Node, viewport_width: f32) {
        let target = if !select && self.anchor != self.focus {
            self.anchor.min(self.focus)
        } else if word {
            previous_word_start(self.secret.as_str(), self.focus)
        } else {
            previous_grapheme(self.secret.as_str(), self.focus)
        };
        self.move_focus(target, select, node, viewport_width);
    }

    pub fn move_right(&mut self, select: bool, word: bool, node: &Node, viewport_width: f32) {
        let target = if !select && self.anchor != self.focus {
            self.anchor.max(self.focus)
        } else if word {
            next_word_end(self.secret.as_str(), self.focus)
        } else {
            next_grapheme(self.secret.as_str(), self.focus)
        };
        self.move_focus(target, select, node, viewport_width);
    }

    pub fn move_to_start(&mut self, select: bool, node: &Node, viewport_width: f32) {
        self.move_focus(0, select, node, viewport_width);
    }

    pub fn move_to_end(&mut self, select: bool, node: &Node, viewport_width: f32) {
        self.move_focus(self.secret.len(), select, node, viewport_width);
    }

    pub fn select_all(&mut self, node: &Node, viewport_width: f32) {
        self.anchor = 0;
        self.focus = self.secret.len();
        self.sync_display(node, viewport_width);
    }

    pub fn move_to_point(&mut self, x: f32, y: f32, node: &Node, viewport_width: f32) {
        self.display.move_to_point(x, y);
        self.focus = self.secret_byte_for_mask(self.display.display_selection().1);
        self.anchor = self.focus;
        self.sync_display(node, viewport_width);
    }

    pub fn extend_selection_to_point(&mut self, x: f32, y: f32, node: &Node, viewport_width: f32) {
        self.display.extend_selection_to_point(x, y);
        self.focus = self.secret_byte_for_mask(self.display.display_selection().1);
        self.sync_display(node, viewport_width);
    }

    pub fn display(&mut self) -> &mut TextInputEditor {
        &mut self.display
    }

    pub fn take(&mut self, node: &Node, viewport_width: f32) -> SecretBytes {
        let secret = std::mem::replace(&mut self.secret, SecretBytes::empty());
        self.anchor = 0;
        self.focus = 0;
        self.sync_display(node, viewport_width);
        secret
    }

    pub fn clear(&mut self, node: &Node, viewport_width: f32) {
        self.secret.clear();
        self.anchor = 0;
        self.focus = 0;
        self.sync_display(node, viewport_width);
    }

    fn delete_range(&mut self, range: Range<usize>, node: &Node, viewport_width: f32) -> bool {
        if range.is_empty() || !self.secret.replace(range.clone(), "") {
            return false;
        }
        self.anchor = range.start;
        self.focus = range.start;
        self.sync_display(node, viewport_width);
        true
    }

    fn move_focus(&mut self, target: usize, select: bool, node: &Node, viewport_width: f32) {
        self.focus = target;
        if !select {
            self.anchor = target;
        }
        self.sync_display(node, viewport_width);
    }

    fn selection(&self) -> Range<usize> {
        self.anchor.min(self.focus)..self.anchor.max(self.focus)
    }

    fn sync_display(&mut self, node: &Node, viewport_width: f32) {
        let mut masked = node.clone();
        masked.text = MASK.repeat(self.secret.as_str().graphemes(true).count());
        self.display.prepare(&masked, viewport_width);
        let anchor = self.mask_byte_for_secret(self.anchor);
        let focus = self.mask_byte_for_secret(self.focus);
        self.display.select_byte_range(anchor, focus);
    }

    fn mask_byte_for_secret(&self, secret_byte: usize) -> usize {
        self.secret.as_str()[..secret_byte].graphemes(true).count() * MASK.len()
    }

    fn secret_byte_for_mask(&self, mask_byte: usize) -> usize {
        let grapheme = mask_byte / MASK.len();
        self.secret
            .as_str()
            .grapheme_indices(true)
            .nth(grapheme)
            .map_or(self.secret.len(), |(index, _)| index)
    }
}

fn previous_grapheme(value: &str, position: usize) -> usize {
    value[..position]
        .grapheme_indices(true)
        .next_back()
        .map_or(position, |(index, _)| index)
}

fn next_grapheme(value: &str, position: usize) -> usize {
    value[position..]
        .graphemes(true)
        .next()
        .map_or(position, |grapheme| position + grapheme.len())
}

fn previous_word_start(value: &str, position: usize) -> usize {
    value[..position]
        .unicode_word_indices()
        .next_back()
        .map_or(0, |(index, _)| index)
}

fn next_word_end(value: &str, position: usize) -> usize {
    value[position..]
        .unicode_word_indices()
        .next()
        .map_or(value.len(), |(index, word)| position + index + word.len())
}

fn invalid_secret_text(value: &str) -> bool {
    value.is_empty()
        || value
            .chars()
            .any(|character| character.is_control() || matches!(character, '\u{2028}' | '\u{2029}'))
}

fn secure_zero(bytes: &mut [u8]) {
    for byte in bytes {
        // volatile writes prevent the compiler from eliding the clear before free.
        // SAFETY: byte is a valid unique reference for this volatile write.
        unsafe { std::ptr::write_volatile(byte, 0) };
    }
    compiler_fence(Ordering::SeqCst);
}

#[cfg(test)]
mod tests {
    use thing_core::{Kind, NodeId};

    use super::*;

    fn node() -> Node {
        Node::new(NodeId(1), Kind::SecretInput)
    }

    #[test]
    fn display_contains_only_one_mask_per_grapheme() {
        let node = node();
        let mut editor = SecretInputEditor::new(&node, 200.0);
        assert!(editor.replace_selection("päss👨\u{200d}👩", &node, 200.0));

        assert_eq!(editor.len(), "päss👨\u{200d}👩".len());
        assert_eq!(editor.display.value(), "•••••");
    }

    #[test]
    fn edits_preserve_unicode_boundaries_and_clear_on_take() {
        let node = node();
        let mut editor = SecretInputEditor::new(&node, 200.0);
        assert!(editor.replace_selection("abc👨\u{200d}👩", &node, 200.0));
        assert!(editor.backspace(false, &node, 200.0));
        assert_eq!(editor.len(), 3);
        editor.move_to_start(false, &node, 200.0);
        editor.move_right(true, false, &node, 200.0);
        assert!(editor.replace_selection("z", &node, 200.0));

        let secret = editor.take(&node, 200.0);
        assert!(editor.is_empty());
        assert_eq!(editor.display.value(), "");
        secret.expose(|bytes| assert_eq!(bytes, b"zbc"));
    }

    #[test]
    fn rejects_controls_and_values_over_the_fixed_capacity() {
        let node = node();
        let mut editor = SecretInputEditor::new(&node, 200.0);
        assert!(!editor.replace_selection("line\nbreak", &node, 200.0));
        assert!(!editor.replace_selection(&"x".repeat(MAX_SECRET_BYTES + 1), &node, 200.0));
        assert!(editor.is_empty());
    }

    #[test]
    fn selection_and_deletion_never_copy_the_secret_into_the_node() {
        let node = node();
        let mut editor = SecretInputEditor::new(&node, 200.0);
        assert!(editor.replace_selection("private", &node, 200.0));
        editor.select_all(&node, 200.0);
        assert!(editor.delete(false, &node, 200.0));

        assert!(node.text.is_empty());
        assert!(editor.is_empty());
        assert_eq!(editor.display.value(), "");
    }
}
