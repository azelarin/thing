use std::{borrow::Cow, collections::BTreeMap};

use parley::{FontContext, FontStack, LayoutContext, StyleProperty};
use thing_core::{Node, NodeId};

use crate::{Measurer, Size};

struct Cached {
    text: String,
    family: String,
    size: f32,
    weight: f32,
    width: Option<f32>,
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
        });
        if !valid {
            let mut builder = self
                .context
                .ranged_builder(&mut self.fonts, &node.text, 1.0, false);
            builder.push_default(StyleProperty::FontSize(node.style.font_size));
            builder.push_default(StyleProperty::FontWeight(parley::FontWeight::new(
                node.style.font_weight,
            )));
            builder.push_default(StyleProperty::FontStack(FontStack::Source(Cow::Borrowed(
                &node.style.font_family,
            ))));
            let mut layout = builder.build(&node.text);
            layout.break_all_lines(width);
            self.cache.insert(
                node.id,
                Cached {
                    text: node.text.clone(),
                    family: node.style.font_family.clone(),
                    size: node.style.font_size,
                    weight: node.style.font_weight,
                    width,
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
    use thing_core::Kind;

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
}
