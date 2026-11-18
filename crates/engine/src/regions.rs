//! Regions: named parts of the mix, each exported to a file of its own. They sit in region lanes above the
//! tracks; a lane holds the folder and format for all its regions.

use std::path::PathBuf;

use crate::{Error, ExportSettings, Frames, LaneId, RangeId, Result};

#[derive(Debug, Clone, PartialEq)]
pub struct RegionLane {
    pub id: LaneId,
    /// Where the files go; `None` until the user picks one.
    pub folder: Option<PathBuf>,
    pub settings: ExportSettings,
    /// Sorted by start, not overlapping; with a loop each starts in `0..loop_end` and may run past it.
    pub regions: Vec<Region>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Region {
    pub id: RangeId,
    pub start: Frames,
    pub end: Frames,
    /// The file name without extension; a `/` makes a subfolder.
    pub name: String,
}

impl RegionLane {
    pub fn region(&self, id: RangeId) -> Option<&Region> {
        self.regions.iter().find(|r| r.id == id)
    }

    /// Where a region of this lane is written; `None` while the lane has no folder.
    pub fn file_of(&self, region: &Region) -> Option<PathBuf> {
        let name = format!("{}.{}", region.name, self.settings.format.extension());
        Some(self.folder.as_ref()?.join(name))
    }

    /// "sound 1", "sound 2", …: the first name no region of the lane has.
    pub(crate) fn free_name(&self) -> String {
        (1..)
            .map(|n| format!("sound {n}"))
            .find(|name| !self.regions.iter().any(|r| r.name.eq_ignore_ascii_case(name)))
            .unwrap_or_default()
    }
}

/// A name that works as a file name on every system, trimmed. A `/` separates subfolders.
pub(crate) fn check_name(name: &str) -> Result<String> {
    let name = name.trim();
    let bad = |why: &str| Err(Error::Invalid(format!("the region name \"{name}\" {why}")));
    if name.is_empty() {
        return Err(Error::Invalid("a region needs a name".into()));
    }
    if name.chars().count() > 200 {
        return bad("is too long");
    }
    if name.chars().any(|c| c.is_control() || "\\:*?\"<>|".contains(c)) {
        return bad("has a character that files cannot have: \\ : * ? \" < > |");
    }
    // Windows drops a dot or space at the end of a name, so two names could meet in one file.
    let parts_ok = name
        .split('/')
        .all(|p| !p.is_empty() && p != "." && p != ".." && !p.ends_with(['.', ' ']) && !p.starts_with(' '));
    if !parts_ok {
        return bad("has an empty folder, \".\" or \"..\", or a part ending in a dot or space");
    }
    Ok(name.to_string())
}

/// Sorts the lane and refuses regions that are empty, overlap, or have names that are not distinct files.
pub(crate) fn tidy_regions(lane: &mut RegionLane, loop_end: Option<Frames>) -> Result<()> {
    lane.regions.sort_by_key(|r| r.start);
    if lane.regions.iter().any(|r| r.end <= r.start) {
        return Err(Error::Invalid("a region must end after it starts".into()));
    }
    if lane.regions.iter().any(|r| r.start < 0) {
        return Err(Error::Invalid("a region must start at 0 or later".into()));
    }
    for r in &lane.regions {
        check_name(&r.name)?;
    }
    // Case is ignored: on Windows and macOS "Jump" and "jump" are one file.
    let mut names: Vec<String> = lane.regions.iter().map(|r| r.name.to_lowercase()).collect();
    names.sort();
    if let Some(w) = names.windows(2).find(|w| matches!(w, [a, b] if a == b)) {
        return Err(Error::Invalid(format!(
            "two regions in one lane are named \"{}\"; each needs its own name",
            w.first().map_or("", String::as_str)
        )));
    }
    if let Some(l) = loop_end {
        if lane.regions.iter().any(|r| r.start >= l || r.end - r.start > l) {
            return Err(Error::Invalid(
                "with the loop on, a region must start before the loop end and be at most one loop long".into(),
            ));
        }
        if let (Some(first), Some(last)) = (lane.regions.first(), lane.regions.last())
            && lane.regions.len() > 1
            && last.end - l > first.start
        {
            return Err(Error::Invalid(
                "regions in one lane cannot overlap; use another lane".into(),
            ));
        }
    }
    if lane.regions.windows(2).any(|w| matches!(w, [a, b] if a.end > b.start)) {
        return Err(Error::Invalid(
            "regions in one lane cannot overlap; use another lane".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_that_files_cannot_have_are_refused() {
        assert_eq!(check_name("  ui/click ").unwrap(), "ui/click");
        for bad in ["", "a/../b", "/abs", "a:b", "x.", "a//b", "ends/"] {
            assert!(check_name(bad).is_err(), "{bad}");
        }
    }
}
