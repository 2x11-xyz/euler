#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Region {
    Transcript,
    Activity,
    Status,
    Input,
}

impl Region {
    pub const ALL: [Self; 4] = [Self::Transcript, Self::Activity, Self::Status, Self::Input];

    const fn index(self) -> usize {
        match self {
            Self::Transcript => 0,
            Self::Activity => 1,
            Self::Status => 2,
            Self::Input => 3,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub enum RedrawLevel {
    #[default]
    Clean,
    Partial,
    Full,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DirtyRegions {
    levels: [RedrawLevel; 4],
}

impl DirtyRegions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn mark(&mut self, region: Region, level: RedrawLevel) {
        let slot = &mut self.levels[region.index()];
        *slot = (*slot).max(level);
    }

    pub fn mark_all(&mut self, level: RedrawLevel) {
        for region in Region::ALL {
            self.mark(region, level);
        }
    }

    pub fn mark_resize(&mut self) {
        self.mark_all(RedrawLevel::Full);
    }

    #[cfg(test)]
    pub fn level(&self, region: Region) -> RedrawLevel {
        self.levels[region.index()]
    }

    pub fn any_stale(&self) -> bool {
        self.levels.iter().any(|level| *level != RedrawLevel::Clean)
    }

    pub fn take(&mut self) -> Self {
        std::mem::take(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_region_escalates_and_never_downgrades() {
        let mut dirty = DirtyRegions::new();

        dirty.mark(Region::Transcript, RedrawLevel::Partial);
        dirty.mark(Region::Transcript, RedrawLevel::Clean);
        assert_eq!(dirty.level(Region::Transcript), RedrawLevel::Partial);

        dirty.mark(Region::Transcript, RedrawLevel::Full);
        dirty.mark(Region::Transcript, RedrawLevel::Partial);
        assert_eq!(dirty.level(Region::Transcript), RedrawLevel::Full);
    }

    #[test]
    fn take_returns_stale_snapshot_and_cleans_state() {
        let mut dirty = DirtyRegions::new();
        dirty.mark(Region::Input, RedrawLevel::Partial);

        let snapshot = dirty.take();

        assert_eq!(snapshot.level(Region::Input), RedrawLevel::Partial);
        assert!(!dirty.any_stale());
    }

    #[test]
    fn resize_marks_all_regions_full_redraw() {
        let mut dirty = DirtyRegions::new();

        dirty.mark_resize();

        for region in Region::ALL {
            assert_eq!(dirty.level(region), RedrawLevel::Full);
        }
    }
}
