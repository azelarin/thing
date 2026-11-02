//! logical-pixel layout and input, independent of the window system and renderer.

mod brush;
mod composite;
mod editor;
mod frame;
mod image_effects;
mod images;
mod layout;
pub mod png;
mod secret;
mod shadow;
mod skia;
mod text;
mod visual;
pub use editor::{
    TextInputCheckpoint, TextInputEditor, TextInputImeBatch, TextInputImeError,
    TextInputImeOutcome, TextInputPreedit, TextInputSurrounding,
};
pub use frame::{InspectorHighlight, Interaction, PaintFrame};
pub use layout::{Layout, Measurer, Placement, ScrollState, ScrollStep, SemanticPlacement, Size};
pub use secret::{MAX_SECRET_BYTES, SecretBytes, SecretInputEditor};
pub use skia::Painter;
pub use text::{TextBrush, TextSystem, link_regions};
pub use visual::Affine;

pub use image_effects::validate_image_effects;
pub use shadow::validate_shadows;

pub use composite::validate_compositing;
