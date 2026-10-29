use std::collections::BTreeMap;

use thing_core::NodeId;
use thing_runtime::UiTarget;
use thing_ui::Layout;

struct PresentedSurface {
    layout: Layout,
    root: NodeId,
    consumer: String,
    targets: BTreeMap<NodeId, UiTarget>,
}

#[derive(Default)]
pub(crate) struct SurfaceLayouts {
    // pointer geometry changes only after the corresponding frame is visible.
    presented: Option<PresentedSurface>,
    // keyboard focus and ime can use the next layout before presentation.
    input: Option<Layout>,
    input_invalidated: bool,
}

#[derive(Clone, Copy)]
pub(crate) enum LayoutSnapshot {
    Presented,
    Input,
}

impl SurfaceLayouts {
    pub(crate) fn presented(&self) -> Option<&Layout> {
        self.presented.as_ref().map(|presented| &presented.layout)
    }

    pub(crate) fn input(&self) -> Option<&Layout> {
        self.input.as_ref().or_else(|| self.presented())
    }

    pub(crate) fn snapshot(&self, snapshot: LayoutSnapshot) -> Option<&Layout> {
        match snapshot {
            LayoutSnapshot::Presented => self.presented(),
            LayoutSnapshot::Input => self.input(),
        }
    }

    pub(crate) fn stage_input(&mut self, layout: Layout) {
        self.input = Some(layout);
        self.input_invalidated = false;
    }

    pub(crate) fn take_input(&mut self) -> Option<Layout> {
        self.input_invalidated = false;
        self.input.take()
    }

    pub(crate) fn presented_target(&self, id: NodeId) -> Option<ScopedUiTarget> {
        let presented = self.presented.as_ref()?;
        Some(ScopedUiTarget {
            consumer: presented.consumer.clone(),
            target: presented.targets.get(&id)?.clone(),
        })
    }

    pub(crate) fn presented_pointer_target(&self, id: NodeId) -> Option<ScopedUiTarget> {
        self.presented()?
            .pointer_target_is_eligible(id)
            .then(|| self.presented_target(id))?
    }

    pub(crate) fn presented_path(&self, id: NodeId) -> Vec<ScopedUiTarget> {
        let Some(presented) = &self.presented else {
            return Vec::new();
        };
        let Some(target) = presented.targets.get(&id) else {
            return Vec::new();
        };
        target
            .interactive_path_from(presented.root)
            .filter_map(|id| {
                Some(ScopedUiTarget {
                    consumer: presented.consumer.clone(),
                    target: presented.targets.get(&id)?.clone(),
                })
            })
            .collect()
    }

    pub(crate) fn publish(
        &mut self,
        layout: Layout,
        root: NodeId,
        consumer: String,
        targets: BTreeMap<NodeId, UiTarget>,
    ) {
        self.presented = Some(PresentedSurface {
            layout,
            root,
            consumer,
            targets,
        });
        self.input = None;
        self.input_invalidated = false;
    }

    pub(crate) fn invalidate_input(&mut self) {
        self.input = None;
        self.input_invalidated = true;
    }

    pub(crate) fn clear_input(&mut self) {
        self.input = None;
        self.input_invalidated = false;
    }

    pub(crate) fn input_needs_refresh(&self) -> bool {
        self.input_invalidated
    }

    #[cfg(test)]
    pub(crate) fn presented_consumer(&self) -> Option<&str> {
        self.presented
            .as_ref()
            .map(|presented| presented.consumer.as_str())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ScopedUiTarget {
    pub(crate) consumer: String,
    pub(crate) target: UiTarget,
}
