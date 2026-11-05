//! Vector icons for the navigation rail and the brand mark.
//!
//! The icons live as real SVG files in `assets/icons/` and are embedded with
//! `include_str!`. A small parser turns the SVG subset we use (rect, line,
//! polyline/polygon, circle) into drawing commands, which are then rendered
//! with epaint vector shapes every frame. No textures, no rasterisation, no
//! image decoder: the icons stay crisp at any window scale / DPI.
//!
//! The only raster is `app-icon.rgba`, used for the Windows taskbar icon,
//! because the OS window-icon API accepts bitmap data only.

use egui::{
    epaint::PathStroke, Color32, Painter, Pos2, Rect, Shape, Stroke, StrokeKind, Vec2,
};

const VIEW: f32 = 64.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IconKind {
    Workbench,
    Devices,
    Chips,
    Logs,
    Settings,
}

#[derive(Clone)]
enum Draw {
    StrokeRect {
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        r: f32,
        width: f32,
    },
    FillRect {
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        r: f32,
    },
    Line {
        points: Vec<(f32, f32)>,
        width: f32,
    },
    StrokeCircle {
        cx: f32,
        cy: f32,
        r: f32,
        width: f32,
    },
    FillCircle {
        cx: f32,
        cy: f32,
        r: f32,
    },
    FillPoly {
        points: Vec<(f32, f32)>,
    },
}

#[derive(Clone)]
pub struct Icon {
    cmds: Vec<Draw>,
}

pub struct IconSet {
    nav: [Icon; 5],
    pub logo: Icon,
}

impl IconSet {
    pub fn load() -> Self {
        Self {
            nav: [
                Icon::from_svg(include_str!("../assets/icons/workbench.svg")),
                Icon::from_svg(include_str!("../assets/icons/devices.svg")),
                Icon::from_svg(include_str!("../assets/icons/chips.svg")),
                Icon::from_svg(include_str!("../assets/icons/logs.svg")),
                Icon::from_svg(include_str!("../assets/icons/settings.svg")),
            ],
            logo: Icon::from_svg(include_str!("../assets/icons/logo.svg")),
        }
    }

    pub fn get(&self, kind: IconKind) -> &Icon {
        &self.nav[kind as usize]
    }
}

// --------------------------------------------------------------------------
// Minimal SVG parsing (the subset emitted by tools/gen_icons.py).
// --------------------------------------------------------------------------

fn attr(tag: &str, key: &str) -> Option<String> {
    let needle = format!("{key}=\"");
    let start = tag.find(&needle)? + needle.len();
    let end = start + tag[start..].find('"')?;
    Some(tag[start..end].to_string())
}

fn attr_f(tag: &str, key: &str, default: f32) -> f32 {
    attr(tag, key)
        .and_then(|v| v.trim().parse::<f32>().ok())
        .unwrap_or(default)
}

fn parse_points(raw: &str) -> Vec<(f32, f32)> {
    let nums: Vec<f32> = raw
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter_map(|s| s.trim().parse::<f32>().ok())
        .collect();
    nums.chunks_exact(2).map(|p| (p[0], p[1])).collect()
}

impl Icon {
    fn from_svg(svg: &str) -> Self {
        let mut cmds = Vec::new();
        let mut rest = svg;
        while let Some(start) = rest.find('<') {
            rest = &rest[start..];
            let Some(end) = rest.find('>') else { break };
            let tag = &rest[..=end];
            rest = &rest[end + 1..];
            if tag.starts_with("<?") || tag.starts_with("</") || tag.starts_with("<svg") {
                continue;
            }
            let name = tag[1..]
                .split(|c: char| c.is_whitespace() || c == '/')
                .next()
                .unwrap_or("");
            let filled = tag.contains("fill=\"#FFFFFF\"") || tag.contains("fill=\"white\"");
            let width = attr_f(tag, "stroke-width", 3.4);
            match name {
                "rect" => {
                    let x = attr_f(tag, "x", 0.0);
                    let y = attr_f(tag, "y", 0.0);
                    let w = attr_f(tag, "width", 0.0);
                    let h = attr_f(tag, "height", 0.0);
                    let r = attr_f(tag, "rx", 0.0);
                    if filled {
                        cmds.push(Draw::FillRect { x, y, w, h, r });
                    } else {
                        cmds.push(Draw::StrokeRect { x, y, w, h, r, width });
                    }
                }
                "line" => cmds.push(Draw::Line {
                    points: vec![
                        (attr_f(tag, "x1", 0.0), attr_f(tag, "y1", 0.0)),
                        (attr_f(tag, "x2", 0.0), attr_f(tag, "y2", 0.0)),
                    ],
                    width,
                }),
                "polyline" | "polygon" => {
                    let points = attr(tag, "points").map(|p| parse_points(&p)).unwrap_or_default();
                    if points.is_empty() {
                        continue;
                    }
                    if filled {
                        cmds.push(Draw::FillPoly { points });
                    } else {
                        cmds.push(Draw::Line { points, width });
                    }
                }
                "circle" => {
                    let cx = attr_f(tag, "cx", 32.0);
                    let cy = attr_f(tag, "cy", 32.0);
                    let r = attr_f(tag, "r", 0.0);
                    if filled {
                        cmds.push(Draw::FillCircle { cx, cy, r });
                    } else {
                        cmds.push(Draw::StrokeCircle { cx, cy, r, width });
                    }
                }
                _ => {}
            }
        }
        Self { cmds }
    }

    /// Draw the icon as resolution-independent vector shapes inside `rect`.
    pub fn paint(&self, painter: &Painter, rect: Rect, color: Color32) {
        let scale = rect.width().min(rect.height()) / VIEW;
        let ox = rect.min.x + (rect.width() - VIEW * scale) / 2.0;
        let oy = rect.min.y + (rect.height() - VIEW * scale) / 2.0;
        let map = |p: &(f32, f32)| Pos2::new(ox + p.0 * scale, oy + p.1 * scale);
        let line_width = |w: f32| (w * scale).max(0.6);

        for cmd in &self.cmds {
            match cmd {
                Draw::StrokeRect { x, y, w, h, r, width } => {
                    let rect = Rect::from_min_size(
                        Pos2::new(ox + x * scale, oy + y * scale),
                        Vec2::new(w * scale, h * scale),
                    );
                    painter.add(Shape::rect_stroke(
                        rect,
                        (*r * scale).max(0.0),
                        Stroke::new(line_width(*width), color),
                        StrokeKind::Middle,
                    ));
                }
                Draw::FillRect { x, y, w, h, r } => {
                    let rect = Rect::from_min_size(
                        Pos2::new(ox + x * scale, oy + y * scale),
                        Vec2::new(w * scale, h * scale),
                    );
                    painter.add(Shape::rect_filled(rect, (*r * scale).max(0.0), color));
                }
                Draw::Line { points, width } => {
                    let pts: Vec<Pos2> = points.iter().map(map).collect();
                    painter.add(Shape::line(pts, PathStroke::new(line_width(*width), color)));
                }
                Draw::StrokeCircle { cx, cy, r, width } => {
                    painter.add(Shape::circle_stroke(
                        Pos2::new(ox + cx * scale, oy + cy * scale),
                        r * scale,
                        Stroke::new(line_width(*width), color),
                    ));
                }
                Draw::FillCircle { cx, cy, r } => {
                    painter.add(Shape::circle_filled(
                        Pos2::new(ox + cx * scale, oy + cy * scale),
                        r * scale,
                        color,
                    ));
                }
                Draw::FillPoly { points } => {
                    let pts: Vec<Pos2> = points.iter().map(map).collect();
                    for tri in triangulate(&pts) {
                        painter.add(Shape::convex_polygon(
                            tri.to_vec(),
                            color,
                            PathStroke::NONE,
                        ));
                    }
                }
            }
        }
    }
}

// --------------------------------------------------------------------------
// Ear-clipping triangulation: epaint only fills convex polygons, while the
// lightning bolt and some gear teeth need concave polygon support.
// --------------------------------------------------------------------------

fn cross(a: Pos2, b: Pos2, c: Pos2) -> f32 {
    (b.x - a.x) * (c.y - a.y) - (b.y - a.y) * (c.x - a.x)
}

fn signed_area(pts: &[Pos2]) -> f32 {
    let mut sum = 0.0;
    for i in 0..pts.len() {
        let a = pts[i];
        let b = pts[(i + 1) % pts.len()];
        sum += a.x * b.y - b.x * a.y;
    }
    sum / 2.0
}

fn in_triangle(p: Pos2, a: Pos2, b: Pos2, c: Pos2) -> bool {
    let d1 = cross(a, b, p);
    let d2 = cross(b, c, p);
    let d3 = cross(c, a, p);
    let all_pos = d1 >= -1e-4 && d2 >= -1e-4 && d3 >= -1e-4;
    let all_neg = d1 <= 1e-4 && d2 <= 1e-4 && d3 <= 1e-4;
    all_pos || all_neg
}

fn triangulate(pts: &[Pos2]) -> Vec<[Pos2; 3]> {
    let n = pts.len();
    if n < 3 {
        return Vec::new();
    }
    let mut idx: Vec<usize> = (0..n).collect();
    if signed_area(pts) < 0.0 {
        idx.reverse();
    }
    let mut tris = Vec::new();
    let mut guard = 0;
    while idx.len() > 3 && guard < n * n {
        guard += 1;
        let len = idx.len();
        let mut ear = None;
        for i in 0..len {
            let a = pts[idx[(i + len - 1) % len]];
            let b = pts[idx[i]];
            let c = pts[idx[(i + 1) % len]];
            if cross(a, b, c) <= 1e-6 {
                continue;
            }
            let blocked = idx.iter().any(|&j| {
                let p = pts[j];
                (p - a).length_sq() > 1e-6
                    && (p - b).length_sq() > 1e-6
                    && (p - c).length_sq() > 1e-6
                    && in_triangle(p, a, b, c)
            });
            if !blocked {
                ear = Some(i);
                break;
            }
        }
        match ear {
            Some(i) => {
                let len = idx.len();
                let a = pts[idx[(i + len - 1) % len]];
                let b = pts[idx[i]];
                let c = pts[idx[(i + 1) % len]];
                tris.push([a, b, c]);
                idx.remove(i);
            }
            None => break,
        }
    }
    if idx.len() == 3 {
        tris.push([pts[idx[0]], pts[idx[1]], pts[idx[2]]]);
    } else if idx.len() > 3 {
        // Degenerate fallback: triangle fan (should not happen for our icons).
        for k in 1..idx.len() - 1 {
            tris.push([pts[idx[0]], pts[idx[k]], pts[idx[k + 1]]]);
        }
    }
    tris
}

/// Raw RGBA for the Windows taskbar / title-bar icon (bitmap API only).
pub fn window_icon() -> egui::IconData {
    const RGBA: &[u8] = include_bytes!("../assets/icons/app-icon.rgba");
    egui::IconData {
        rgba: RGBA.to_vec(),
        width: 64,
        height: 64,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_svg_parses_into_commands() {
        let set = IconSet::load();
        // primitives per icon, as emitted by tools/gen_icons.py
        let expected = [3, 5, 14, 5, 10];
        for (i, count) in expected.iter().enumerate() {
            assert_eq!(set.nav[i].cmds.len(), *count, "nav icon {i} command count");
        }
        assert_eq!(set.logo.cmds.len(), 10, "logo command count");
    }

    #[test]
    fn concave_polygons_are_triangulated() {
        // The lightning bolt is a 7-vertex concave polygon; ear clipping must
        // produce n-2 triangles and cover every original vertex.
        let bolt: Vec<Pos2> = [
            (35.0, 19.0),
            (26.0, 34.0),
            (31.0, 34.0),
            (28.0, 45.0),
            (39.0, 29.0),
            (33.0, 29.0),
            (37.0, 19.0),
        ]
        .iter()
        .map(|&(x, y)| Pos2::new(x, y))
        .collect();
        let tris = triangulate(&bolt);
        assert_eq!(tris.len(), bolt.len() - 2, "triangle count");
        for p in &bolt {
            assert!(
                tris.iter().any(|t| t.iter().any(|q| (q.x - p.x).abs() < 1e-3
                    && (q.y - p.y).abs() < 1e-3)),
                "vertex {p:?} missing"
            );
        }
    }

    #[test]
    fn window_icon_is_valid_rgba() {
        let icon = window_icon();
        assert_eq!(icon.width, 64);
        assert_eq!(icon.height, 64);
        assert_eq!(icon.rgba.len(), 64 * 64 * 4);
    }
}
