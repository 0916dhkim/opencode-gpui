//! Stable logical hit testing and a constant-extent tab reorder preview.
//! Animated row coordinates must never be used to choose the insertion slot.

#[derive(Clone, Debug)]
pub struct TabDragState {
    ids: Vec<String>,
    source: usize,
    insertion: Option<usize>,
}

impl TabDragState {
    pub fn new(ids: Vec<String>, source: &str) -> Option<Self> {
        let source = ids.iter().position(|id| id == source)?;
        Some(Self {
            ids,
            source,
            insertion: Some(source),
        })
    }

    pub fn source(&self) -> &str {
        &self.ids[self.source]
    }

    pub fn matches(&self, ids: &[String]) -> bool {
        self.ids == ids
    }

    /// `y` is relative to the unanimated content origin, including native scroll.
    /// A small midpoint deadband prevents pointer noise from toggling neighbors.
    /// Call this again on release, with the same deadband, before committing.
    pub fn update(&mut self, y: f32, height: f32, gap: f32, inside: bool) -> bool {
        let previous_hole = self.hole();
        let next = if inside
            && y.is_finite()
            && height.is_finite()
            && height > 0.
            && gap.is_finite()
            && gap >= 0.
        {
            let pitch = height + gap;
            let mut slot = ((y + gap + height / 2.) / pitch)
                .floor()
                .clamp(0., self.ids.len() as f32) as usize;
            if let Some(previous) = self.insertion {
                let deadband = height * (3. / 34.);
                if (slot == previous + 1 && y < previous as f32 * pitch + height / 2. + deadband)
                    || (previous == slot + 1 && y > slot as f32 * pitch + height / 2. - deadband)
                {
                    slot = previous;
                }
            }
            Some(slot)
        } else {
            None
        };
        self.insertion = next;
        // Both sides of the source have the same visual permutation. Retain
        // the raw slot for symmetric hysteresis, without a redundant redraw.
        previous_hole != self.hole()
    }

    pub fn hole(&self) -> Option<usize> {
        self.insertion
            .map(|slot| slot - usize::from(slot > self.source))
    }

    /// A permutation of 0..N: hiding the source leaves exactly one moving hole.
    pub fn projected_index(&self, index: usize) -> usize {
        let Some(hole) = self.hole() else {
            return index;
        };
        if index == self.source {
            return hole;
        }
        let compact = index - usize::from(index > self.source);
        compact + usize::from(compact >= hole)
    }

    pub fn destination(&self) -> Option<(&str, bool)> {
        let slot = self.insertion?;
        if self.hole()? == self.source {
            return None;
        }
        if slot == self.ids.len() {
            Some((self.ids.last()?.as_str(), true))
        } else {
            Some((self.ids[slot].as_str(), false))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(source: usize) -> TabDragState {
        TabDragState::new(
            (0..10).map(|i| i.to_string()).collect(),
            &source.to_string(),
        )
        .unwrap()
    }

    #[test]
    fn every_target_is_a_bounded_permutation_with_one_hole() {
        for source in 0..10 {
            for slot in 0..=10 {
                let mut drag = state(source);
                drag.insertion = Some(slot);
                let mut projected: Vec<_> = (0..10).map(|i| drag.projected_index(i)).collect();
                assert_eq!(projected[source], drag.hole().unwrap());
                projected.sort();
                assert_eq!(projected, (0..10).collect::<Vec<_>>());
            }
        }
    }

    #[test]
    fn exact_two_second_bottom_to_top_gesture_has_no_feedback() {
        let mut drag = state(9);
        for i in 0..=240 {
            let y = 368. - 361. * i as f32 / 240.; // global 461..100, content origin 93
            drag.update(y, 34., 5., true);
        }
        assert_eq!(drag.hole(), Some(0));
        assert_eq!(drag.destination(), Some(("0", false)));
        assert_eq!(drag.projected_index(0), 1);
        assert_eq!(drag.projected_index(9), 0);
    }

    #[test]
    fn release_coordinates_endpoints_and_reversals_are_authoritative() {
        let mut drag = state(0);
        drag.update(500., 34., 5., true);
        assert_eq!(drag.destination(), Some(("9", true)));
        drag.update(200., 34., 5., true);
        drag.update(-10., 34., 5., true);
        assert_eq!(drag.destination(), None);
        assert_eq!(drag.hole(), Some(0));
        drag.update(368., 34., 5., true); // release without an intervening motion
        assert_eq!(drag.destination(), Some(("9", true)));
        drag.update(0., 34., 5., false);
        assert_eq!(drag.hole(), None);
        assert_eq!(drag.destination(), None);
        for i in 0..10 {
            assert_eq!(drag.projected_index(i), i);
        }
    }

    #[test]
    fn midpoint_hysteresis_scales_with_zoom_and_does_not_trap_fast_motion() {
        for zoom in [0.7, 1., 1.75] {
            let mut drag = state(9);
            drag.update(0., 34. * zoom, 5. * zoom, true);
            for y in [16., 18., 17., 19.] {
                drag.update(y * zoom, 34. * zoom, 5. * zoom, true);
                assert_eq!(drag.hole(), Some(0));
            }
            drag.update(21. * zoom, 34. * zoom, 5. * zoom, true);
            assert_eq!(drag.hole(), Some(1));
            drag.update(500. * zoom, 34. * zoom, 5. * zoom, true);
            assert_eq!(drag.hole(), Some(9));
        }
    }

    #[test]
    fn source_adjacent_hysteresis_is_symmetric_without_redundant_notifications() {
        let mut drag = state(0);
        assert!(!drag.update(55., 34., 5., true));
        assert!(!drag.update(57., 34., 5., true));
        assert_eq!(drag.hole(), Some(0));
        assert!(drag.update(60., 34., 5., true));
        assert_eq!(drag.hole(), Some(1));
        assert!(!drag.update(55., 34., 5., true));
        assert!(drag.update(52., 34., 5., true));
        assert_eq!(drag.hole(), Some(0));
    }

    #[test]
    fn scrolling_under_a_stationary_pointer_retargets_in_content_coordinates() {
        let mut drag = state(0);
        let pointer_y = 300.;
        drag.update(pointer_y - 93., 34., 5., true);
        let before = drag.hole();
        // The native content origin moves upward by three pitches; pointer stays.
        assert!(drag.update(pointer_y - (93. - 117.), 34., 5., true));
        assert_eq!(drag.hole().unwrap(), before.unwrap() + 3);
    }

    #[test]
    fn metadata_changes_and_invalid_geometry_cannot_commit_a_stale_drop() {
        assert!(TabDragState::new(vec![], "missing").is_none());
        let mut drag = state(9);
        assert!(!drag.matches(&["other".into()]));
        assert!(!drag.update(368., 34., 5., true));
        drag.update(f32::NAN, 34., 5., true);
        assert_eq!(drag.destination(), None);
        drag.update(0., 0., 5., true);
        assert_eq!(drag.destination(), None);
    }
}
