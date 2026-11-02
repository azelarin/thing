//! mime and action negotiation for external drag and drop, independent of
//! wayland objects.

use thing_core::DropType;
use thing_runtime::{
    DragAction, DragActions, DragData, DropDataError, MAX_DRAG_TEXT_BYTES, MAX_URI_LIST_BYTES,
    decode_drop_text, decode_uri_list, encode_uri_list,
};
use wayland_client::{WEnum, protocol::wl_data_device_manager::DndAction};

use crate::text_data::{TEXT_MIME_TYPES, supports_mime_type};

/// the only format local files are offered and read as.
pub(crate) const URI_LIST_MIME_TYPE: &str = "text/uri-list";

/// the drag and drop actions that a mask allows. `ask` and unknown bits are
/// ignored because thing never offers or chooses them.
pub(crate) fn actions_from_mask(mask: u32) -> DragActions {
    DragActions {
        copy: mask & DndAction::Copy.bits() != 0,
        moves: mask & DndAction::Move.bits() != 0,
    }
}

pub(crate) fn mask(actions: DragActions) -> DndAction {
    let mut mask = DndAction::empty();
    if actions.copy {
        mask |= DndAction::Copy;
    }
    if actions.moves {
        mask |= DndAction::Move;
    }
    mask
}

pub(crate) fn action_mask(action: DragAction) -> DndAction {
    match action {
        DragAction::Copy => DndAction::Copy,
        DragAction::Move => DndAction::Move,
    }
}

/// the single action a compositor selected, or `None` for none, ask, or an
/// invalid combination.
pub(crate) fn selected_action(mask: u32) -> Option<DragAction> {
    if mask == DndAction::Copy.bits() {
        Some(DragAction::Copy)
    } else if mask == DndAction::Move.bits() {
        Some(DragAction::Move)
    } else {
        None
    }
}

pub(crate) fn bits(value: WEnum<DndAction>) -> u32 {
    match value {
        WEnum::Value(value) => value.bits(),
        WEnum::Unknown(value) => value,
    }
}

fn is_uri_list(mime_type: &str) -> bool {
    mime_type.trim().eq_ignore_ascii_case(URI_LIST_MIME_TYPE)
}

/// the data kinds an offer can supply, in a fixed order.
pub(crate) fn offered_types(mime_types: &[String]) -> Vec<DropType> {
    let mut types = Vec::with_capacity(2);
    if mime_types.iter().any(|mime_type| is_uri_list(mime_type)) {
        types.push(DropType::Files);
    }
    if mime_types
        .iter()
        .any(|mime_type| supports_mime_type(mime_type))
    {
        types.push(DropType::Text);
    }
    types
}

/// the advertised mime type to read for `kind`. text uses the existing
/// clipboard preference, so utf-8 types win over plain `text/plain`.
pub(crate) fn mime_type_for(
    kind: DropType,
    mime_types: &[String],
    text: Option<String>,
) -> Option<String> {
    match kind {
        DropType::Text => text,
        DropType::Files => mime_types
            .iter()
            .find(|mime_type| is_uri_list(mime_type))
            .cloned(),
    }
}

/// the most bytes a read for `kind` may return.
pub(crate) fn read_limit(kind: DropType) -> usize {
    match kind {
        DropType::Text => MAX_DRAG_TEXT_BYTES,
        DropType::Files => MAX_URI_LIST_BYTES,
    }
}

pub(crate) fn decode(kind: DropType, bytes: Vec<u8>) -> Result<DragData, DropDataError> {
    match kind {
        DropType::Text => decode_drop_text(bytes).map(DragData::Text),
        DropType::Files => decode_uri_list(&bytes).map(DragData::Files),
    }
}

/// the mime types an outgoing drag offers for its data.
pub(crate) fn source_mime_types(data: &DragData) -> &'static [&'static str] {
    match data {
        DragData::Text(_) => &TEXT_MIME_TYPES,
        DragData::Files(_) => &[URI_LIST_MIME_TYPE],
    }
}

/// whether an outgoing drag of `kind` answers a request for `mime_type`.
pub(crate) fn serves(kind: DropType, mime_type: &str) -> bool {
    match kind {
        DropType::Text => supports_mime_type(mime_type),
        DropType::Files => is_uri_list(mime_type),
    }
}

pub(crate) fn encode(data: &DragData) -> Vec<u8> {
    match data {
        DragData::Text(text) => text.as_bytes().to_vec(),
        DragData::Files(paths) => encode_uri_list(paths).into_bytes(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offers_map_to_kinds_and_preferred_mime_types() {
        let mime_types = vec![
            "TEXT/URI-LIST".to_owned(),
            "text/plain".to_owned(),
            "application/x-other".to_owned(),
        ];
        assert_eq!(
            offered_types(&mime_types),
            [DropType::Files, DropType::Text]
        );
        assert_eq!(
            mime_type_for(DropType::Files, &mime_types, None).as_deref(),
            Some("TEXT/URI-LIST")
        );
        assert!(offered_types(&["image/png".to_owned()]).is_empty());
        assert!(serves(DropType::Files, "text/uri-list"));
        assert!(!serves(DropType::Files, "text/plain"));
        assert!(serves(DropType::Text, "UTF8_STRING"));
    }

    #[test]
    fn only_one_copy_or_move_is_a_selected_action() {
        assert_eq!(selected_action(1), Some(DragAction::Copy));
        assert_eq!(selected_action(2), Some(DragAction::Move));
        for invalid in [0, 3, 4, 5, 8] {
            assert_eq!(selected_action(invalid), None, "{invalid}");
        }
        let actions = actions_from_mask(DndAction::all().bits() | 64);
        assert!(actions.copy && actions.moves);
        assert_eq!(mask(actions), DndAction::Copy | DndAction::Move);
        assert!(actions_from_mask(DndAction::Ask.bits()).is_empty());
    }

    #[test]
    fn payloads_round_trip_and_reject_remote_files() {
        let files = DragData::Files(vec!["/tmp/a b".into()]);
        assert_eq!(decode(DropType::Files, encode(&files)).unwrap(), files);
        assert_eq!(
            decode(DropType::Files, b"https://example.com/a\r\n".to_vec()),
            Err(DropDataError::UnsupportedUri)
        );
        assert_eq!(
            decode(DropType::Text, b"\xff".to_vec()),
            Err(DropDataError::InvalidUtf8)
        );
    }
}
