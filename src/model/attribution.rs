//! Core attribution types.
//!
//! `Attribution` and `LineAttribution` are the public data model for attribution
//! ranges. They are pure value types (character- and line-level authorship
//! ranges) shared by the attribution tracker, working logs, and virtual
//! attribution, so they live in `model` rather than up in `operations`.

pub(crate) fn floor_char_boundary(content: &str, index: usize) -> usize {
    let mut boundary = index.min(content.len());
    while boundary > 0 && !content.is_char_boundary(boundary) {
        boundary -= 1;
    }
    boundary
}

pub(crate) fn ceil_char_boundary(content: &str, index: usize) -> usize {
    let mut boundary = index.min(content.len());
    while boundary < content.len() && !content.is_char_boundary(boundary) {
        boundary += 1;
    }
    boundary
}

/// Represents a single attribution range in the file.
/// Ranges can overlap (multiple authors can be attributed to the same text).
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct Attribution {
    /// Character position where this attribution starts (inclusive)
    pub start: usize,
    /// Character position where this attribution ends (exclusive)
    pub end: usize,
    /// Identifier for the author of this range
    pub author_id: String,
    /// Timestamp of the attribution (in milliseconds since epoch)
    pub ts: u128,
}

/// Represents attribution for a range of lines.
/// Both start_line and end_line are inclusive (1-indexed).
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct LineAttribution {
    /// Line number where this attribution starts (inclusive, 1-indexed)
    pub start_line: u32,
    /// Line number where this attribution ends (inclusive, 1-indexed)
    pub end_line: u32,
    /// Identifier for the author of this range
    pub author_id: String,
    /// Author ID that was overwritten by this attribution (e.g., if Alice wrote this line originally, then Bob edited it, overwrote=Alice because her edit was writen over)
    #[serde(default)]
    pub overrode: Option<String>,
}

impl LineAttribution {
    pub fn new(
        start_line: u32,
        end_line: u32,
        author_id: String,
        overrode: Option<String>,
    ) -> Self {
        LineAttribution {
            start_line,
            end_line,
            author_id,
            overrode,
        }
    }

    /// Returns the number of lines this attribution covers
    #[allow(dead_code)]
    pub fn line_count(&self) -> u32 {
        if self.start_line > self.end_line {
            0
        } else {
            self.end_line - self.start_line + 1
        }
    }

    /// Checks if this line attribution is empty
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.start_line > self.end_line
    }

    /// Checks if this attribution overlaps with a given line range (inclusive)
    #[allow(dead_code)]
    pub fn overlaps(&self, start_line: u32, end_line: u32) -> bool {
        self.start_line <= end_line && self.end_line >= start_line
    }

    /// Returns the overlapping portion of this attribution with a given line range
    #[allow(dead_code)]
    pub fn intersection(&self, start_line: u32, end_line: u32) -> Option<(u32, u32)> {
        let overlap_start = self.start_line.max(start_line);
        let overlap_end = self.end_line.min(end_line);

        if overlap_start <= overlap_end {
            Some((overlap_start, overlap_end))
        } else {
            None
        }
    }
}

impl Attribution {
    pub fn new(start: usize, end: usize, author_id: String, ts: u128) -> Self {
        Attribution {
            start,
            end,
            author_id,
            ts,
        }
    }

    /// Returns the length of this attribution range
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.end - self.start
    }

    /// Checks if this attribution is empty
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.start >= self.end
    }

    /// Checks if this attribution overlaps with a given range
    pub fn overlaps(&self, start: usize, end: usize) -> bool {
        self.start < end && self.end > start
    }

    /// Returns the overlapping portion of this attribution with a given range
    pub fn intersection(&self, start: usize, end: usize) -> Option<(usize, usize)> {
        let overlap_start = self.start.max(start);
        let overlap_end = self.end.min(end);

        if overlap_start < overlap_end {
            Some((overlap_start, overlap_end))
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_char_boundaries_preserve_valid_and_clamp_external_indexes() {
        let content = "aé€😀z";

        for boundary in [0, 1, 3, 6, 10, content.len()] {
            assert_eq!(floor_char_boundary(content, boundary), boundary);
            assert_eq!(ceil_char_boundary(content, boundary), boundary);
        }
        assert_eq!(floor_char_boundary(content, usize::MAX), content.len());
        assert_eq!(ceil_char_boundary(content, usize::MAX), content.len());
        assert_eq!(floor_char_boundary("", usize::MAX), 0);
        assert_eq!(ceil_char_boundary("", usize::MAX), 0);
    }

    #[test]
    fn utf8_char_boundaries_round_outward_across_multibyte_scalars() {
        let content = "aé€😀z";

        for (index, expected) in [(2, (1, 3)), (4, (3, 6)), (5, (3, 6))] {
            assert_eq!(
                (
                    floor_char_boundary(content, index),
                    ceil_char_boundary(content, index)
                ),
                expected
            );
        }
        for index in 7..10 {
            assert_eq!(floor_char_boundary(content, index), 6);
            assert_eq!(ceil_char_boundary(content, index), 10);
        }
    }
}
