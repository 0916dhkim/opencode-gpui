//! Transparent forwarding element; no new layout node, style, hitbox or ID.
use gpui_kit::*;
use opencode_gpui::latency::{self, Phase, PhaseSpan};

pub fn wrap(child: AnyElement, view_slot: u64) -> AnyElement {
    RootProfile {
        child: Some(child),
        view_slot,
    }
    .into_any_element()
}

struct RootProfile {
    child: Option<AnyElement>,
    view_slot: u64,
}
struct Layout {
    child: AnyElement,
    gap: Option<PhaseSpan>,
}

impl Drop for Layout {
    fn drop(&mut self) {
        if let Some(gap) = self.gap.take() {
            gap.discard();
        }
    }
}

impl IntoElement for RootProfile {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}

impl Element for RootProfile {
    type RequestLayoutState = Layout;
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }
    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Layout) {
        let span = latency::global().phase(Phase::RootLayoutRequest, self.view_slot);
        let mut child = self.child.take().expect("profile root already laid out");
        let id = child.request_layout(window, cx);
        drop(span);
        // Layout solving happens AFTER request_layout returns. Keep this gap
        // distinct from the callback and do not call it exclusive solver time.
        let gap = Some(latency::global().phase(Phase::RootLayoutGap, self.view_slot));
        (id, Layout { child, gap })
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        layout: &mut Layout,
        window: &mut Window,
        cx: &mut App,
    ) {
        drop(layout.gap.take());
        let _span = latency::global().phase(Phase::RootPrepaint, self.view_slot);
        layout.child.prepaint(window, cx);
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        layout: &mut Layout,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        let _span = latency::global().phase(Phase::RootPaint, self.view_slot);
        layout.child.paint(window, cx);
    }
}
