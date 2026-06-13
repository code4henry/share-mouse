/// Screen layout management.
///
/// Models the physical arrangement of screens across machines so we know
/// which edge transitions to which peer.

use serde::{Deserialize, Serialize};

/// A screen's position in the virtual layout.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScreenRect {
    /// Top-left X in virtual coordinate space.
    pub x: i32,
    /// Top-left Y in virtual coordinate space.
    pub y: i32,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
}

impl ScreenRect {
    pub fn right(&self) -> i32 {
        self.x + self.width as i32
    }
    pub fn bottom(&self) -> i32 {
        self.y + self.height as i32
    }
}

/// Which edge of a screen the cursor hit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

/// Information about a single screen (local or remote).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScreenInfo {
    pub id: String,
    pub name: String,
    pub rect: ScreenRect,
    /// The peer ID that owns this screen (None = local).
    pub peer_id: Option<String>,
    pub width: u32,
    pub height: u32,
    pub dpi: u32,
}

/// The full screen layout across all machines.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScreenLayout {
    pub screens: Vec<ScreenInfo>,
}

impl ScreenLayout {
    pub fn new() -> Self {
        Self {
            screens: Vec::new(),
        }
    }

    /// Find which screen (if any) is adjacent to the given edge of the given screen.
    pub fn find_neighbor(&self, screen_id: &str, edge: Edge) -> Option<&ScreenInfo> {
        let source = self.screens.iter().find(|s| s.id == screen_id)?;
        let source_rect = &source.rect;

        // Threshold for detecting adjacency (allow small gaps/misalignment).
        const THRESHOLD: i32 = 5;

        self.screens.iter().find(|candidate| {
            if candidate.id == screen_id {
                return false;
            }
            let r = &candidate.rect;

            match edge {
                Edge::Right => {
                    // Candidate's left edge is near source's right edge
                    (r.x - source_rect.right()).abs() < THRESHOLD
                        && ranges_overlap(
                            source_rect.y,
                            source_rect.height,
                            r.y,
                            r.height,
                        )
                }
                Edge::Left => {
                    // Candidate's right edge is near source's left edge
                    (source_rect.x - r.right()).abs() < THRESHOLD
                        && ranges_overlap(
                            source_rect.y,
                            source_rect.height,
                            r.y,
                            r.height,
                        )
                }
                Edge::Bottom => {
                    // Candidate's top edge is near source's bottom edge
                    (r.y - source_rect.bottom()).abs() < THRESHOLD
                        && ranges_overlap(
                            source_rect.x,
                            source_rect.width,
                            r.x,
                            r.width,
                        )
                }
                Edge::Top => {
                    // Candidate's bottom edge is near source's top edge
                    (source_rect.y - r.bottom()).abs() < THRESHOLD
                        && ranges_overlap(
                            source_rect.x,
                            source_rect.width,
                            r.x,
                            r.width,
                        )
                }
            }
        })
    }

    /// Detect which edge (if any) the cursor is at on the local screen.
    /// Returns the edge and the neighbor screen if applicable.
    pub fn detect_edge(&self, local_screen_id: &str, cursor_x: i32, cursor_y: i32) -> Option<(Edge, &ScreenInfo)> {
        let screen = self.screens.iter().find(|s| s.id == local_screen_id)?;
        let rect = &screen.rect;

        const EDGE_ZONE: i32 = 6; // pixels from edge to trigger transition

        // Check each edge
        let edges = [
            (Edge::Right, cursor_x >= rect.right() - EDGE_ZONE),
            (Edge::Left, cursor_x <= rect.x + EDGE_ZONE),
            (Edge::Bottom, cursor_y >= rect.bottom() - EDGE_ZONE),
            (Edge::Top, cursor_y <= rect.y + EDGE_ZONE),
        ];

        for (edge, at_edge) in edges {
            if at_edge {
                if let Some(neighbor) = self.find_neighbor(local_screen_id, edge) {
                    return Some((edge, neighbor));
                }
            }
        }

        None
    }

    /// Map cursor position from one screen's edge to the neighbor's corresponding position.
    /// Returns normalized (0.0–1.0) coordinates on the target screen.
    /// Normalization uses the SOURCE screen's dimensions (cursor coords are in source space).
    pub fn map_cursor_to_neighbor(
        &self,
        source_id: &str,
        edge: Edge,
        cursor_x: i32,
        cursor_y: i32,
        _neighbor: &ScreenInfo,
    ) -> (f32, f32) {
        let source = match self.screens.iter().find(|s| s.id == source_id) {
            Some(s) => s,
            None => return (0.5, 0.5), // fallback
        };
        let sr = &source.rect;

        match edge {
            Edge::Right => {
                // Enter neighbor from its left edge; Y tracks source Y.
                let y_norm = ((cursor_y - sr.y) as f32 / sr.height as f32).clamp(0.0, 1.0);
                (0.0, y_norm)
            }
            Edge::Left => {
                let y_norm = ((cursor_y - sr.y) as f32 / sr.height as f32).clamp(0.0, 1.0);
                (1.0, y_norm)
            }
            Edge::Bottom => {
                let x_norm = ((cursor_x - sr.x) as f32 / sr.width as f32).clamp(0.0, 1.0);
                (x_norm, 0.0)
            }
            Edge::Top => {
                let x_norm = ((cursor_x - sr.x) as f32 / sr.width as f32).clamp(0.0, 1.0);
                (x_norm, 1.0)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local_layout() -> ScreenLayout {
        ScreenLayout {
            screens: vec![
                ScreenInfo {
                    id: "local".to_string(),
                    name: "Local".to_string(),
                    rect: ScreenRect { x: 0, y: 0, width: 2560, height: 1440 },
                    peer_id: None,
                    width: 2560,
                    height: 1440,
                    dpi: 72,
                },
                ScreenInfo {
                    id: "remote".to_string(),
                    name: "Remote".to_string(),
                    rect: ScreenRect { x: 2560, y: 0, width: 1920, height: 1080 },
                    peer_id: Some("abc".to_string()),
                    width: 1920,
                    height: 1080,
                    dpi: 72,
                },
            ],
        }
    }

    #[test]
    fn detect_right_edge_finds_neighbor() {
        let l = local_layout();
        assert!(l.detect_edge("local", 2555, 720).is_some()); // within 6px zone
        assert!(l.detect_edge("local", 100, 720).is_none()); // mid-screen
    }

    #[test]
    fn detect_left_edge_no_neighbor() {
        let l = local_layout();
        assert!(l.detect_edge("local", 1, 720).is_none()); // left edge, no neighbor
    }

    #[test]
    fn find_neighbor_right_only() {
        let l = local_layout();
        assert!(l.find_neighbor("local", Edge::Right).is_some());
        assert!(l.find_neighbor("local", Edge::Left).is_none());
    }

    #[test]
    fn map_right_edge_same_res() {
        let mut l = local_layout();
        // make remote same size as local
        l.screens[1].rect = ScreenRect { x: 2560, y: 0, width: 2560, height: 1440 };
        let n = l.find_neighbor("local", Edge::Right).unwrap().clone();
        let (x, y) = l.map_cursor_to_neighbor("local", Edge::Right, 2559, 720, &n);
        assert_eq!(x, 0.0);
        assert!((y - 0.5).abs() < 0.01);
    }

    #[test]
    fn map_right_edge_diff_res() {
        // local 2560x1440, remote 1920x1080 — cursor_y is in SOURCE space.
        let l = local_layout();
        let n = l.find_neighbor("local", Edge::Right).unwrap().clone();
        let (x, y) = l.map_cursor_to_neighbor("local", Edge::Right, 2559, 720, &n);
        assert_eq!(x, 0.0);
        assert!((y - 0.5).abs() < 0.01, "mid-height should be 0.5, got {}", y);
        let (_, y0) = l.map_cursor_to_neighbor("local", Edge::Right, 2559, 0, &n);
        assert!(y0.abs() < 0.01);
        let (_, yb) = l.map_cursor_to_neighbor("local", Edge::Right, 2559, 1439, &n);
        assert!((yb - 1.0).abs() < 0.01);
    }
}


/// Check if two ranges overlap (for screen adjacency detection).
fn ranges_overlap(start1: i32, len1: u32, start2: i32, len2: u32) -> bool {
    let end1 = start1 + len1 as i32;
    let end2 = start2 + len2 as i32;
    start1 < end2 && start2 < end1
}
