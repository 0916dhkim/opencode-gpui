use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    sync::Arc,
};

use gpui::{
    AnyElement, AnyView, App, AvailableSpace, Bounds, ElementId, Global, InteractiveElement,
    IntoElement, Pixels, SharedString, StatefulInteractiveElement, Styled, Window, div, size,
};

type Builder = dyn Fn(&SharedString, &mut Window, &mut App) -> AnyView + Send + Sync;

/// Optional application-wide tooltip presentation for rendered Markdown/HTML links.
/// Native text shaping, wrapping, selection and link activation remain unchanged.
#[derive(Clone)]
pub struct LinkTooltip(Arc<Builder>);

impl Global for LinkTooltip {}

impl LinkTooltip {
    pub fn install(
        builder: impl Fn(&SharedString, &mut Window, &mut App) -> AnyView + Send + Sync + 'static,
        cx: &mut App,
    ) {
        cx.set_global(Self(Arc::new(builder)));
    }
}

/// A transparent native tooltip trigger over an already-shaped link fragment.
/// This is an out-of-flow element: it neither replaces nor remeasures text.
/// Normal hitboxes do not occlude the original text's selection/click listeners.
pub(super) fn prepaint(
    id: ElementId,
    url: SharedString,
    bounds: Bounds<Pixels>,
    window: &mut Window,
    cx: &mut App,
) -> Option<AnyElement> {
    let builder = cx.try_global::<LinkTooltip>()?.0.clone();
    let bounds = bounds.intersect(&window.content_mask().bounds);
    if bounds.size.width <= Pixels::ZERO
        || bounds.size.height <= Pixels::ZERO
        || cx.has_active_drag()
    {
        return None;
    }
    // Position and destination are part of the identity: a reflow, scroll or
    // source replacement must not keep a tooltip built for the previous URL
    // or trigger geometry. Hash rather than retaining URLs in debug IDs.
    let mut hash = DefaultHasher::new();
    id.hash(&mut hash);
    url.hash(&mut hash);
    for value in [
        bounds.origin.x,
        bounds.origin.y,
        bounds.size.width,
        bounds.size.height,
    ] {
        f32::from(value).to_bits().hash(&mut hash);
    }
    let mut trigger = div()
        .id(ElementId::NamedInteger(
            "text-link-tooltip".into(),
            hash.finish(),
        ))
        .w(bounds.size.width)
        .h(bounds.size.height)
        .tooltip(move |window, cx| builder(&url, window, cx))
        .into_any_element();
    trigger.prepaint_as_root(
        bounds.origin,
        size(
            AvailableSpace::Definite(bounds.size.width),
            AvailableSpace::Definite(bounds.size.height),
        ),
        window,
        cx,
    );
    Some(trigger)
}
