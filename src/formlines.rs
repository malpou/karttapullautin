//! Form-line selection: which stretches of the half-interval lines are drawn as form lines
//! (ISOM 103), from the local relief of the ground model under them.
//!
//! The selection is a contour operation: it reads the ground model and smoothjoin's
//! [`ContourSet`] and returns a [`FormLineSelection`], which the renderer draws, and the
//! form-line dump, DXF and GeoJSON are written from.
//!
//! It keeps two quirks of the renderer it came out of, which the output depends on: the
//! lines go through sheet pixels (the vertex tests, the closed-ring test and the form-line
//! points are all taken from the pixel positions), and a form line's points start at a
//! line's second vertex.

use std::error::Error;
use std::path::Path;

use crate::geometry::{BinaryDxf, Bounds, Classification, Point2, Polylines};
use crate::io::fs::FileSystem;
use crate::io::heightmap::HeightMap;
use crate::mapframe::MapFrame;
use crate::merge::{ContourSet, FormLineMode};
use crate::vec2d::Vec2D;

/// The debug intermediate of the [`FormLineSelection`]'s lines; `formlines.dxf`, its text
/// DXF, is a DXF-family product.
pub const FORM_LINES_DUMP: &str = "formlines.dxf.bin";

/// Parameters of [`select_form_lines`].
#[derive(Debug, Clone, PartialEq)]
pub struct FormLineParams {
    /// Whether the half-interval lines are selected into form lines (ini `form_lines`).
    pub form_lines: FormLineMode,
    /// The sheet the lines are tested on: the selection works in its pixels (ini
    /// `mapscale`).
    pub frame: MapFrame,
    /// Local relief threshold; greater gives more form lines (ini `formlinesteepness`;
    /// unset, [`default_relief_threshold`] of the contour interval).
    pub relief_threshold: f64,
    /// Vertices added to each end of a selected stretch (ini `formlineaddition`).
    pub addition_vertices: f64,
    /// Gaps between selected stretches shorter than this many vertices are closed (ini
    /// `minimumgap`).
    pub minimum_gap_vertices: u32,
    /// Give the form lines of depressions their own class (ini
    /// `label_formlines_depressions`).
    pub label_depressions: bool,
    /// Drop form lines where the ground is too steep to draw them apart from the contours
    /// (ini `remove_touching_contours`).
    pub remove_touching_contours: bool,
    /// The trace interval in metres (ini `contour_interval` and `form_lines`): the height
    /// differences the touching test allows are 1 and 1.4 trace intervals.
    pub trace_interval_m: f64,
    /// A closed form line shorter than this on its longer side is not drawn: the ISOM
    /// minimum ring length at the map scale
    /// ([`crate::mapframe::GroundMinima::ring_length_m`]).
    pub ring_length_m: f64,
}

/// The default local relief threshold (ini `formlinesteepness`) per contour interval,
/// measured on the regression tile (`docs/research/formline-defaults.md` in the planning
/// workspace): at 2.5 m the contours already show twice the detail, and 0.15 keeps the
/// form lines to about the ground density 0.37 gives at 5 m.
pub const RELIEF_THRESHOLD_DEFAULTS: [(f64, f64); 2] = [(2.5, 0.15), (5.0, 0.37)];

/// The default relief threshold at `contour_interval`: the [`RELIEF_THRESHOLD_DEFAULTS`]
/// entry of the nearest interval (the larger one halfway between).
pub fn default_relief_threshold(contour_interval: f64) -> f64 {
    let [(fine, fine_threshold), (standard, standard_threshold)] = RELIEF_THRESHOLD_DEFAULTS;
    if contour_interval < (fine + standard) / 2.0 {
        fine_threshold
    } else {
        standard_threshold
    }
}

/// What [`select_form_lines`] keeps of one half-interval line.
#[derive(Debug, Clone, PartialEq)]
pub struct LineKeep {
    /// Per vertex, whether the form line runs through it.
    pub vertices: Vec<bool>,
    /// A small closed line kept whole, drawn solid.
    pub small_ring: bool,
}

impl LineKeep {
    /// Whether vertex `i` is drawn: a kept vertex, or any vertex of a small ring.
    pub fn keeps(&self, i: usize) -> bool {
        self.small_ring || self.vertices[i]
    }
}

/// The form lines of a tile.
#[derive(Debug, Clone)]
pub struct FormLineSelection {
    /// Per line of the [`ContourSet`], in its order, what is kept of a half-interval
    /// line; None for every other line.
    pub keep: Vec<Option<LineKeep>>,
    /// The form lines in ground coordinates: each kept stretch of a half-interval line,
    /// classed [`Classification::Formline`] (or [`Classification::FormlineDepression`]).
    pub lines: Polylines<Point2, Classification>,
    /// The extent of the contours they were selected from.
    pub bounds: Bounds,
}

impl FormLineSelection {
    /// The [`FORM_LINES_DUMP`] debug intermediate.
    pub fn to_bindxf(&self) -> BinaryDxf {
        BinaryDxf::new(self.bounds.clone(), vec![self.lines.clone().into()])
    }
}

/// Selects the form lines among the half-interval lines of `contours`, from the local
/// relief of `ground`; None without form lines (`form_lines=none`).
///
/// A vertex is wanted where the local relief under it is below the threshold. A line
/// keeps the runs of mostly wanted vertices, extended by `addition_vertices` both ways,
/// with gaps shorter than `minimum_gap_vertices` closed. A small closed line is kept whole
/// when any of it is kept, unless it is below the ISOM minimum ring length, when none of
/// it is. With `remove_touching_contours`, stretches where the ground is too steep for a
/// form line to stand apart from the contours are dropped.
pub fn select_form_lines(
    contours: &ContourSet,
    ground: &HeightMap,
    params: &FormLineParams,
) -> Option<FormLineSelection> {
    if params.form_lines != FormLineMode::Selective {
        return None;
    }
    let frame = params.frame;
    let relief = local_relief(ground, params.relief_threshold);
    let x0 = ground.minx();
    let y0 = ground.maxy();

    let mut keep = Vec::with_capacity(contours.lines.len());
    let mut lines = Polylines::new();
    for (line, &(layer, _)) in contours.lines.iter() {
        // an index half-interval line is drawn as an index contour, whole
        if !layer
            .contour_kind()
            .is_some_and(|k| k.half_interval() && !k.index())
        {
            keep.push(None);
            continue;
        }
        // the line in sheet pixels
        let x = line
            .iter()
            .map(|p| frame.to_px(p.x - x0))
            .collect::<Vec<_>>();
        let y = line
            .iter()
            .map(|p| frame.to_px(y0 - p.y))
            .collect::<Vec<_>>();
        let line_keep = select_line(&x, &y, ground, &relief, params);

        let class = if layer.is_depression() && params.label_depressions {
            Classification::FormlineDepression
        } else {
            Classification::Formline
        };
        let mut points = Vec::new();
        for i in 1..x.len() {
            if line_keep.keeps(i) {
                points.push(pixel_to_ground(x[i], y[i], x0, y0, &frame));
            } else if !points.is_empty() {
                lines.push(std::mem::take(&mut points), class);
            }
        }
        if !points.is_empty() {
            lines.push(points, class);
        }
        keep.push(Some(line_keep));
    }
    Some(FormLineSelection {
        keep,
        lines,
        bounds: contours.bounds.clone(),
    })
}

/// The local relief the selection tests each vertex against, per ground model cell: the
/// least slope over 4 cells across the cell in four directions, damped by the curvature,
/// or 0.01 where the slope changes between 3 and 6 cells by more than a bound that grows
/// as `threshold` falls. A border of 6 cells (7 on the far sides) stays 0.
fn local_relief(ground: &HeightMap, threshold: f64) -> Vec2D<f64> {
    let xyz = &ground.grid;
    let mut relief = Vec2D::new(xyz.width(), xyz.height(), 0f64);
    let sxmax = xyz.width() - 1;
    let symax = xyz.height() - 1;

    for i in 6..(sxmax - 7) {
        for j in 6..(symax - 7) {
            let mut det: f64 = 0.0;
            let mut high: f64 = f64::MIN;

            let mut temp = (xyz[(i - 4, j)] - xyz[(i, j)]).abs() / 4.0;
            let temp2 = (xyz[(i, j)] - xyz[(i + 4, j)]).abs() / 4.0;
            let det2 = (xyz[(i, j)] - 0.5 * (xyz[(i - 4, j)] + xyz[(i + 4, j)])).abs()
                - 0.05 * (xyz[(i - 4, j)] - xyz[(i + 4, j)]).abs();
            let mut porr = (((xyz[(i - 6, j)] - xyz[(i + 6, j)]) / 12.0).abs()
                - ((xyz[(i - 3, j)] - xyz[(i + 3, j)]) / 6.0).abs())
            .abs();

            if det2 > det {
                det = det2;
            }
            if temp2 < temp {
                temp = temp2;
            }
            if temp > high {
                high = temp;
            }

            let mut temp = (xyz[(i, j - 4)] - xyz[(i, j)]).abs() / 4.0;
            let temp2 = (xyz[(i, j)] - xyz[(i, j - 4)]).abs() / 4.0;
            let det2 = (xyz[(i, j)] - 0.5 * (xyz[(i, j - 4)] + xyz[(i, j + 4)])).abs()
                - 0.05 * (xyz[(i, j - 4)] - xyz[(i, j + 4)]).abs();
            let porr2 = (((xyz[(i, j - 6)] - xyz[(i, j + 6)]) / 12.0).abs()
                - ((xyz[(i, j - 3)] - xyz[(i, j + 3)]) / 6.0).abs())
            .abs();

            if porr2 > porr {
                porr = porr2;
            }
            if det2 > det {
                det = det2;
            }
            if temp2 < temp {
                temp = temp2;
            }
            if temp > high {
                high = temp;
            }

            let mut temp = (xyz[(i - 4, j - 4)] - xyz[(i, j)]).abs() / 5.6;
            let temp2 = (xyz[(i, j)] - xyz[(i + 4, j + 4)]).abs() / 5.6;
            let det2 = (xyz[(i, j)] - 0.5 * (xyz[(i - 4, j - 4)] + xyz[(i + 4, j + 4)])).abs()
                - 0.05 * (xyz[(i - 4, j - 4)] - xyz[(i + 4, j + 4)]).abs();
            let porr2 = (((xyz[(i - 6, j - 6)] - xyz[(i + 6, j + 6)]) / 17.0).abs()
                - ((xyz[(i - 3, j - 3)] - xyz[(i + 3, j + 3)]) / 8.5).abs())
            .abs();

            if porr2 > porr {
                porr = porr2;
            }
            if det2 > det {
                det = det2;
            }
            if temp2 < temp {
                temp = temp2;
            }
            if temp > high {
                high = temp;
            }

            let mut temp = (xyz[(i - 4, j + 4)] - xyz[(i, j)]).abs() / 5.6;
            let temp2 = (xyz[(i, j)] - xyz[(i + 4, j - 4)]).abs() / 5.6;
            let det2 = (xyz[(i, j)] - 0.5 * (xyz[(i + 4, j - 4)] + xyz[(i - 4, j + 4)])).abs()
                - 0.05 * (xyz[(i + 4, j - 4)] - xyz[(i - 4, j + 4)]).abs();
            let porr2 = (((xyz[(i + 6, j - 6)] - xyz[(i - 6, j + 6)]) / 17.0).abs()
                - ((xyz[(i + 3, j - 3)] - xyz[(i - 3, j + 3)]) / 8.5).abs())
            .abs();

            if porr2 > porr {
                porr = porr2;
            }
            if det2 > det {
                det = det2;
            }
            if temp2 < temp {
                temp = temp2;
            }
            if temp > high {
                high = temp;
            }

            let mut val = 12.0 * high / (1.0 + 8.0 * det);
            if porr > 0.25 * 0.67 / (0.3 + threshold) {
                val = 0.01;
            }
            if high > val {
                val = high;
            }
            relief[(i, j)] = val;
        }
    }
    relief
}

/// What is kept of one half-interval line, given in sheet pixels (`x`, `y`).
fn select_line(
    x: &[f64],
    y: &[f64],
    ground: &HeightMap,
    relief: &Vec2D<f64>,
    params: &FormLineParams,
) -> LineKeep {
    let &FormLineParams {
        frame,
        relief_threshold,
        addition_vertices,
        minimum_gap_vertices,
        remove_touching_contours,
        trace_interval_m,
        ring_length_m,
        ..
    } = params;
    let xyz = &ground.grid;
    let (x0, y0) = (ground.minx(), ground.maxy());
    let (xstart, ystart, size) = (ground.xoffset, ground.yoffset, ground.scale);
    // the height differences across a cell, straight and diagonal, below which a form
    // line stands apart from the contours: 2.5 and 3.5 m at a 5 m contour interval
    let touching_straight = trace_interval_m;
    let touching_diagonal = trace_interval_m * 7.0 / 5.0;

    let closed = x.first() == x.last() && y.first() == y.last();
    let mut smallringtest = false;
    // wanted: the local relief is low
    let mut help = vec![false; x.len()];
    // kept
    let mut help2 = vec![true; x.len()];
    // apart from the contours
    let mut help3 = vec![false; x.len()];
    for i in 0..x.len() {
        let world = pixel_to_ground(x[i], y[i], x0, y0, &frame);
        let xx = ((world.x - xstart) / size).floor() as usize;
        let yy = ((world.y - ystart) / size).floor() as usize;

        // make sure indices are within bounds for the grid lookups
        if xx >= xyz.width() - 1 || yy >= xyz.height() - 1 || xx < 1 || yy < 1 {
            continue;
        }

        if relief[(xx, yy)] < relief_threshold
            || relief[(xx, yy + 1)] < relief_threshold
            || relief[(xx + 1, yy)] < relief_threshold
            || relief[(xx + 1, yy + 1)] < relief_threshold
        {
            help[i] = true;
        }
        if (xyz[(xx - 1, yy)] - xyz[(xx + 1, yy)]).abs() < touching_straight
            && (xyz[(xx, yy - 1)] - xyz[(xx, yy + 1)]).abs() < touching_straight
            && (xyz[(xx, yy)] - xyz[(xx + 1, yy + 1)]).abs() < touching_diagonal
            && (xyz[(xx - 1, yy - 1)] - xyz[(xx + 1, yy + 1)]).abs() < touching_diagonal
            && (xyz[(xx + 1, yy - 1)] - xyz[(xx - 1, yy + 1)]).abs() < touching_diagonal
        {
            help3[i] = true;
        }
    }
    // keep a vertex where at least 5 of the 9 around it are wanted
    for i in 5..(x.len() - 6) {
        let mut apu = 0;
        for j in (i - 5)..(i + 4) {
            if help[j] {
                apu += 1;
            }
        }
        if apu < 5 {
            help2[i] = false;
        }
    }
    for i in 0..6 {
        help2[i] = help2[6]
    }
    for i in (x.len() - 6)..x.len() {
        help2[i] = help2[x.len() - 7]
    }
    // extend each kept stretch forwards, then backwards, round a closed line
    let mut on = 0.0;
    for i in 0..x.len() {
        if help2[i] {
            on = addition_vertices
        }
        if on > 0.0 {
            help2[i] = true;
            on -= 1.0;
        }
    }
    if closed && on > 0.0 {
        let mut i = 0;
        while i < x.len() && on > 0.0 {
            help2[i] = true;
            on -= 1.0;
            i += 1;
        }
    }
    let mut on = 0.0;
    for i in 0..x.len() {
        let ii = x.len() - i - 1;
        if help2[ii] {
            on = addition_vertices
        }
        if on > 0.0 {
            help2[ii] = true;
            on -= 1.0;
        }
    }
    if closed && on > 0.0 {
        let mut i = (x.len() - 1) as i32;
        while i > -1 && on > 0.0 {
            help2[i as usize] = true;
            on -= 1.0;
            i -= 1;
        }
    }
    // Let's not break small form line rings
    //
    // ...but only down to the size ISOM allows one to be drawn at. A closed form line is
    // legitimate for a knoll or depression (ISOM 2017-2 symbol 103) and dashing it would
    // not read as a ring, which is why this rule promotes a qualifying small ring to a
    // solid loop. The rule had no minimum size though, so a ring of a handful of vertices
    // was promoted exactly like a real knoll. On flat hummocky ground with form lines at a
    // 1.25 m interval that is most of the rings on the map, and the result is a render
    // covered in closed loops that carry no information and are below the size the symbol
    // may legally be drawn at anyway.
    //
    // Rings under the ISOM minimum are therefore dropped rather than filled in.
    for max_length in [122usize, 60].iter() {
        smallringtest = false;
        if closed && x.len() < *max_length {
            smallringtest = help2.iter().any(|v| *v);
            if smallringtest && closed_ring_below_isom_minimum(x, y, &frame, ring_length_m) {
                smallringtest = false;
                help2.iter_mut().for_each(|h| *h = false);
            }
            if smallringtest {
                help2.iter_mut().for_each(|h| *h = true);
            }
        }
    }

    // Let's draw short gaps together
    if !smallringtest {
        let mut tester = 1;
        for i in 1..x.len() {
            if help2[i] {
                if tester < i && ((i - tester) as u32) < minimum_gap_vertices {
                    for j in tester..(i + 1) {
                        help2[j] = true;
                    }
                }
                tester = i;
            }
        }
    }
    if remove_touching_contours {
        // remove formlines when it is really steep
        let mut touched = false;
        for i in 0..x.len() {
            if !help3[i] {
                for k in 0..5 {
                    if i + k < x.len() {
                        help2[i + k] = false;
                        touched = true;
                    }
                    // was `i - k + 1 > 0 && i - k < x.len()`, which overflowed in a debug
                    // build and wrapped to this in a release one
                    if k <= i {
                        help2[i - k] = false;
                        touched = true;
                    }
                }
            }
        }
        if touched {
            let mut k = 0;
            for i in 0..x.len() {
                if help2[i] {
                    k += 1;
                }
                if k > 0 && !help2[i] && k < 15 {
                    for l in (i - k)..i {
                        help2[l] = false;
                    }
                }
                if !help2[i] {
                    k = 0;
                }
            }
        }
    }
    LineKeep {
        vertices: help2,
        small_ring: smallringtest,
    }
}

/// Whether a closed line, `x`/`y` in sheet pixels of `frame`, is below the ISOM minimum
/// closed form line (knoll or depression): 1.1 mm on the 1:15 000 original, `ring_length_m`
/// on the ground (16.5 m at 1:15 000 and, symbols enlarged, at 1:10 000). Measured on the
/// ring's longer bounding-box side, so an elongated ring is judged by its length: this
/// drops specks, not real knolls.
fn closed_ring_below_isom_minimum(
    x: &[f64],
    y: &[f64],
    frame: &MapFrame,
    ring_length_m: f64,
) -> bool {
    let (mut xmin, mut xmax) = (f64::MAX, f64::MIN);
    let (mut ymin, mut ymax) = (f64::MAX, f64::MIN);
    for (&px, &py) in x.iter().zip(y.iter()) {
        xmin = xmin.min(px);
        xmax = xmax.max(px);
        ymin = ymin.min(py);
        ymax = ymax.max(py);
    }
    (xmax - xmin).max(ymax - ymin) * frame.metres_per_px() < ring_length_m
}

/// Inverse of the draw transform `frame.to_px((x - x0), (y0 - y))`: a map-pixel position
/// back to ground coordinates, one operator order for both axes.
pub(crate) fn pixel_to_ground(x: f64, y: f64, x0: f64, y0: f64, frame: &MapFrame) -> Point2 {
    Point2::new(frame.to_metres(x) + x0, frame.to_metres(-y) + y0)
}

/// Writes `selection` to `tmpfolder`: with `debug` the debug intermediate
/// [`FORM_LINES_DUMP`], with `output_dxf` the text DXF `formlines.dxf`.
pub fn write_form_lines(
    fs: &impl FileSystem,
    tmpfolder: &Path,
    selection: &FormLineSelection,
    debug: bool,
    output_dxf: bool,
) -> Result<(), Box<dyn Error>> {
    crate::contours::write_dxf_files(
        fs,
        tmpfolder,
        FORM_LINES_DUMP,
        &selection.to_bindxf(),
        debug,
        output_dxf,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::{ContourKind, Geometry, Point3};

    fn below(x: &[f64], y: &[f64], frame: &MapFrame) -> bool {
        closed_ring_below_isom_minimum(x, y, frame, frame.ground_minima().ring_length_m)
    }

    /// The template's selection at a 5 m contour interval and 1:10 000.
    fn params() -> FormLineParams {
        let frame = MapFrame::default();
        FormLineParams {
            form_lines: FormLineMode::Selective,
            frame,
            relief_threshold: 0.37,
            addition_vertices: 17.0,
            minimum_gap_vertices: 30,
            label_depressions: false,
            remove_touching_contours: false,
            trace_interval_m: 2.5,
            ring_length_m: frame.ground_minima().ring_length_m,
        }
    }

    /// A 120 m square ground model of 2 m cells with heights `f(x, y)` (metres from its
    /// south-west corner), and one half-interval line through `line`, at the height of its
    /// first vertex.
    fn scene(
        f: impl Fn(f64, f64) -> f64,
        line: &[(f64, f64)],
        kind: ContourKind,
    ) -> (HeightMap, ContourSet) {
        let (w, h) = (60, 60);
        let mut grid = Vec2D::new(w, h, 0.0);
        for x in 0..w {
            for y in 0..h {
                grid[(x, y)] = f(2.0 * x as f64, 2.0 * y as f64);
            }
        }
        let ground = HeightMap {
            xoffset: 1000.0,
            yoffset: 2000.0,
            scale: 2.0,
            grid,
        };
        let level = f(line[0].0, line[0].1);
        let points = line
            .iter()
            .map(|&(x, y)| Point3::new(1000.0 + x, 2000.0 + y, level))
            .collect();
        let mut lines = Polylines::new();
        lines.push(points, (Classification::Contour(kind), level));
        let bounds = Bounds::new(1000.0, 1120.0, 2000.0, 2120.0);
        (ground, ContourSet { lines, bounds })
    }

    /// 90 vertices 1 m apart, south to north, 60 m from the west edge: along the level of
    /// ground rising eastwards.
    fn north_south() -> Vec<(f64, f64)> {
        (0..90).map(|k| (60.0, 15.0 + k as f64)).collect()
    }

    /// A 120 m square ground model of 2 m cells with heights `f(x, y)` (metres from its
    /// south-west corner), and the given lines (vertices in metres from that corner).
    fn pin_scene(
        f: &dyn Fn(f64, f64) -> f64,
        lines: Vec<(Vec<(f64, f64)>, Classification)>,
    ) -> (HeightMap, ContourSet) {
        let (w, h) = (60, 60);
        let mut grid = Vec2D::new(w, h, 0.0);
        for x in 0..w {
            for y in 0..h {
                grid[(x, y)] = f(2.0 * x as f64, 2.0 * y as f64);
            }
        }
        let ground = HeightMap {
            xoffset: 1000.0,
            yoffset: 2000.0,
            scale: 2.0,
            grid,
        };
        let mut set = Polylines::new();
        for (line, class) in lines {
            let level = f(line[0].0, line[0].1);
            let points = line
                .iter()
                .map(|&(x, y)| Point3::new(1000.0 + x, 2000.0 + y, level))
                .collect();
            set.push(points, (class, level));
        }
        let bounds = Bounds::new(1000.0, 1120.0, 2000.0, 2120.0);
        (ground, ContourSet { lines: set, bounds })
    }

    /// `n` vertices 1 m apart, south to north from `y0`, `x` m from the west edge.
    fn pin_north_south(x: f64, y0: f64, n: usize) -> Vec<(f64, f64)> {
        (0..n).map(|k| (x, y0 + k as f64)).collect()
    }

    /// A closed ring of `n` segments, radius `r` m around (`cx`, `cy`), first vertex
    /// repeated last.
    fn pin_ring(cx: f64, cy: f64, r: f64, n: usize) -> Vec<(f64, f64)> {
        (0..=n)
            .map(|k| {
                let a = std::f64::consts::TAU * (k % n) as f64 / n as f64;
                (cx + r * a.cos(), cy + r * a.sin())
            })
            .collect()
    }

    /// The pinned scenes: name, ground, contours, remove_touching_contours,
    /// label_depressions.
    fn pin_scenes() -> Vec<(&'static str, HeightMap, ContourSet, bool, bool)> {
        use ContourKind as K;
        let half = Classification::Contour(K::HALF_INTERVAL);
        let gentle = |x: f64, _: f64| 100.0 + 0.01 * x;
        let steep = |x: f64, _: f64| 100.0 + 0.1 * x;
        let saddle = |x: f64, y: f64| 100.0 + 0.0005 * ((x - 60.0).powi(2) - (y - 60.0).powi(2));
        let bend =
            |x: f64, y: f64| 100.0 + 0.01 * x + if y > 60.0 { 0.2 * (x - 60.0) } else { 0.0 };
        let bowl = |x: f64, y: f64| 100.0 + 0.0002 * ((x - 60.0).powi(2) + (y - 60.0).powi(2));
        // gentle, with two bands too steep for a form line to stand apart from the
        // contours: one at the line's south end, one across its middle
        let banded = |x: f64, y: f64| {
            let band = y < 8.0 || (50.0..56.0).contains(&y);
            100.0 + 0.01 * x + if band { 0.8 * (x - 60.0) } else { 0.0 }
        };
        let diagonal: Vec<(f64, f64)> = (0..80)
            .map(|k| (20.0 + k as f64, 20.0 + k as f64))
            .collect();
        let one =
            |f: &dyn Fn(f64, f64) -> f64, line: Vec<(f64, f64)>| pin_scene(f, vec![(line, half)]);
        let mut scenes = Vec::new();
        for (name, (g, c)) in [
            ("gentle", one(&gentle, pin_north_south(60.0, 15.0, 90))),
            // no vertex is wanted: an explicit no-form-line case
            ("steep", one(&steep, pin_north_south(60.0, 15.0, 90))),
            ("saddle", one(&saddle, diagonal)),
            ("bend", one(&bend, pin_north_south(60.0, 15.0, 90))),
        ] {
            scenes.push((name, g, c, false, false));
        }
        // rings: under 60 vertices kept whole (30 m) or dropped (12 m); 60-121 vertices
        // filled by the 122 pass only; 130 vertices, no small ring, extended round
        let (g, c) = pin_scene(
            &bowl,
            vec![
                (pin_ring(60.0, 60.0, 15.0, 40), half),
                (pin_ring(60.0, 60.0, 6.0, 40), half),
                (pin_ring(60.0, 60.0, 20.0, 90), half),
                (pin_ring(60.0, 60.0, 25.0, 130), half),
            ],
        );
        scenes.push(("rings", g, c, false, false));
        // every kind of line, a depression ring with its slope line, depressions labelled
        let depression = Classification::Contour(K::HALF_INTERVAL.with_depression(true));
        let (g, c) = pin_scene(
            &bend,
            vec![
                (
                    pin_north_south(40.0, 15.0, 90),
                    Classification::Contour(K::CONTOUR),
                ),
                (pin_north_south(60.0, 15.0, 90), half),
                (
                    pin_north_south(50.0, 15.0, 90),
                    Classification::Contour(K::INDEX_HALF_INTERVAL),
                ),
                (pin_ring(80.0, 30.0, 12.0, 50), depression),
                (vec![(92.0, 30.0), (88.0, 30.0)], Classification::SlopeLine),
                (pin_north_south(70.0, 15.0, 90), depression),
                (
                    pin_north_south(30.0, 15.0, 90),
                    Classification::Contour(K::INDEX),
                ),
            ],
        );
        scenes.push(("mixed", g, c, false, true));
        // remove_touching_contours: the south band starts at vertex 0, so the window
        // reaches back past the line's start
        let (g, c) = pin_scene(&banded, vec![(pin_north_south(60.0, 3.0, 95), half)]);
        scenes.push(("touching", g, c, true, false));
        scenes
    }

    /// A hash of the form lines (lengths, classes, coordinate bits) and of the keep masks.
    fn pin_digest(
        lines: &Polylines<Point2, Classification>,
        masks: &[(usize, Vec<bool>, bool)],
    ) -> (Vec<usize>, u64, u64) {
        let mut hash: u64 = 0;
        let mut lengths = Vec::new();
        for (points, class) in lines.iter() {
            lengths.push(points.len());
            hash = hash
                .wrapping_mul(31)
                .wrapping_add((*class == Classification::FormlineDepression) as u64);
            for p in points {
                hash = hash.wrapping_mul(31).wrapping_add(p.x.to_bits());
                hash = hash.wrapping_mul(31).wrapping_add(p.y.to_bits());
            }
        }
        let mut mask_hash: u64 = 0;
        for (line, vertices, small_ring) in masks {
            mask_hash = mask_hash.wrapping_mul(31).wrapping_add(*line as u64);
            mask_hash = mask_hash.wrapping_mul(31).wrapping_add(*small_ring as u64);
            for v in vertices {
                mask_hash = mask_hash.wrapping_mul(3).wrapping_add(*v as u64);
            }
        }
        (lengths, hash, mask_hash)
    }

    /// The form lines and keep masks of seven synthetic scenes, as the renderer selected
    /// them before the selection moved out of it: the digests were taken from
    /// `draw_curves` at e12c8ad (release build, where the touching window's old
    /// `i - k + 1 > 0` wrapped instead of overflowing), with its `help2` and
    /// `smallringtest` recorded per half-interval line.
    #[test]
    fn the_selection_is_the_renderers() {
        // name, (form-line lengths, form-line hash, mask hash), the lines with a mask
        type Pinned = (&'static str, (Vec<usize>, u64, u64), Vec<usize>);
        let expected: [Pinned; 7] = [
            (
                "gentle",
                (vec![89], 17005073223462682624, 7028912417603043956),
                vec![0],
            ),
            ("steep", (vec![], 0, 0), vec![0]),
            (
                "saddle",
                (vec![70], 15009030809514409984, 6880484551466865220),
                vec![0],
            ),
            (
                "bend",
                (vec![63], 11491622743514808320, 7028911146670129792),
                vec![0],
            ),
            (
                "rings",
                (vec![40, 90, 130], 3236957272030243973, 11276270056951265514),
                vec![0, 1, 2, 3],
            ),
            (
                "mixed",
                (vec![63, 50, 63], 828462006844168352, 7606171900857696023),
                vec![1, 3, 5],
            ),
            (
                "touching",
                (vec![34, 38], 15046431797044838400, 14650247113313629232),
                vec![0],
            ),
        ];
        let scenes = pin_scenes();
        assert_eq!(scenes.len(), expected.len());
        for ((name, ground, contours, touching, label), (want_name, want, want_lines)) in
            scenes.into_iter().zip(expected)
        {
            assert_eq!(name, want_name);
            let params = FormLineParams {
                remove_touching_contours: touching,
                label_depressions: label,
                ..params()
            };
            let selection = select_form_lines(&contours, &ground, &params).unwrap();
            let masks: Vec<(usize, Vec<bool>, bool)> = selection
                .keep
                .iter()
                .enumerate()
                .filter_map(|(i, k)| k.as_ref().map(|k| (i, k.vertices.clone(), k.small_ring)))
                .collect();
            let lines: Vec<usize> = masks.iter().map(|m| m.0).collect();
            assert_eq!(lines, want_lines, "{name}");
            assert_eq!(pin_digest(&selection.lines, &masks), want, "{name}");
            if name == "steep" {
                // the explicit no-form-line case: a candidate, nothing of it kept
                assert!(masks[0].1.len() == 90 && masks[0].1.iter().all(|v| !v));
                assert!(!masks[0].2);
            }
        }
    }

    /// On a uniform 10 % slope the contours (50 m apart at 5 m) show the ground and no
    /// form line is drawn; across a gentle saddle, which the contours miss, one is.
    #[test]
    fn a_saddle_needs_a_form_line_and_a_uniform_slope_does_not() {
        let (ground, contours) = scene(
            |x, _| 100.0 + 0.1 * x,
            &north_south(),
            ContourKind::HALF_INTERVAL,
        );
        let selection = select_form_lines(&contours, &ground, &params()).unwrap();
        assert!(selection.lines.iter().next().is_none());
        let keep = selection.keep[0].as_ref().unwrap();
        assert!(!keep.small_ring && keep.vertices.iter().all(|v| !v));

        let diagonal: Vec<(f64, f64)> = (0..80)
            .map(|k| (20.0 + k as f64, 20.0 + k as f64))
            .collect();
        let (ground, contours) = scene(
            |x, y| 100.0 + 0.0005 * ((x - 60.0).powi(2) - (y - 60.0).powi(2)),
            &diagonal,
            ContourKind::HALF_INTERVAL,
        );
        let selection = select_form_lines(&contours, &ground, &params()).unwrap();
        let keep = selection.keep[0].as_ref().unwrap();
        // the middle of the line, over the saddle point, is kept
        assert!(keep.vertices[30..50].iter().all(|v| *v));
        // the form line's points are the kept vertices from the second one on, in ground
        // coordinates
        let kept = (1..80).filter(|&i| keep.keeps(i)).count();
        assert_eq!(
            selection.lines.iter().map(|(p, _)| p.len()).sum::<usize>(),
            kept
        );
        let (first, _) = selection.lines.iter().next().unwrap();
        assert!((first[0].x - first[0].y - (1000.0 - 2000.0)).abs() < 1e-9);
    }

    /// Without form lines nothing is selected, and only half-interval lines are
    /// candidates: a contour and an index half-interval line (drawn as an index contour)
    /// get no selection.
    #[test]
    fn only_half_interval_lines_are_selected() {
        let gentle = |x: f64, _: f64| 100.0 + 0.01 * x;
        let none = FormLineParams {
            form_lines: FormLineMode::None,
            ..params()
        };
        let (ground, contours) = scene(gentle, &north_south(), ContourKind::HALF_INTERVAL);
        assert!(select_form_lines(&contours, &ground, &none).is_none());
        for kind in [ContourKind::CONTOUR, ContourKind::INDEX_HALF_INTERVAL] {
            let (ground, contours) = scene(gentle, &north_south(), kind);
            let selection = select_form_lines(&contours, &ground, &params()).unwrap();
            assert_eq!(selection.keep, vec![None]);
            assert!(selection.lines.iter().next().is_none());
        }
    }

    /// The touching test allows height differences of 1 trace interval between the cells
    /// either side of a vertex (1.4 diagonally): on a 50 % slope they differ by 2 m (at
    /// most 2 m diagonally), under the 2.5 m of a 5 m contour interval and over the
    /// 1.25 m of a 2.5 m one, so a form line stands apart from the contours at 5 m, not at
    /// 2.5 m.
    #[test]
    fn the_touching_test_scales_with_the_trace_interval() {
        let (ground, contours) = scene(
            |x, _| 100.0 + 0.5 * x,
            &north_south(),
            ContourKind::HALF_INTERVAL,
        );
        let at = |trace_interval_m: f64| {
            let params = FormLineParams {
                // every vertex wanted, whatever the relief
                relief_threshold: 1e9,
                remove_touching_contours: true,
                trace_interval_m,
                ..params()
            };
            select_form_lines(&contours, &ground, &params).unwrap()
        };
        assert_eq!(
            at(2.5).lines.iter().map(|(p, _)| p.len()).sum::<usize>(),
            89
        );
        assert!(at(1.25).lines.iter().next().is_none());
    }

    /// A closed half-interval line kept anywhere is kept whole when it is small, and
    /// dropped whole below the ISOM minimum ring length.
    #[test]
    fn small_rings_are_kept_whole_or_dropped() {
        // a bowl, flat enough everywhere to want form lines
        let bowl = |x: f64, y: f64| 100.0 + 0.0002 * ((x - 60.0).powi(2) + (y - 60.0).powi(2));
        let ring = |r: f64| -> Vec<(f64, f64)> {
            (0..=40)
                .map(|k| {
                    let a = std::f64::consts::TAU * (k % 40) as f64 / 40.0;
                    (60.0 + r * a.cos(), 60.0 + r * a.sin())
                })
                .collect()
        };
        // 30 m across: kept whole, solid
        let (ground, contours) = scene(bowl, &ring(15.0), ContourKind::HALF_INTERVAL);
        let selection = select_form_lines(&contours, &ground, &params()).unwrap();
        let keep = selection.keep[0].as_ref().unwrap();
        assert!(keep.small_ring && keep.vertices.iter().all(|v| *v));
        assert_eq!(selection.lines.iter().next().unwrap().0.len(), 40);
        // 12 m across, under the 16.5 m minimum: dropped
        let (ground, contours) = scene(bowl, &ring(6.0), ContourKind::HALF_INTERVAL);
        let selection = select_form_lines(&contours, &ground, &params()).unwrap();
        let keep = selection.keep[0].as_ref().unwrap();
        assert!(!keep.small_ring && keep.vertices.iter().all(|v| !v));
        assert!(selection.lines.iter().next().is_none());
    }

    /// A gap shorter than `minimum_gap_vertices` between two kept stretches is closed.
    #[test]
    fn short_gaps_are_closed() {
        // gentle, with a steep band 20 m wide across the middle of the line
        let banded = |x: f64, y: f64| {
            100.0
                + 0.01 * x
                + if (50.0..70.0).contains(&y) {
                    0.3 * (x - 60.0)
                } else {
                    0.0
                }
        };
        let run = |minimum_gap_vertices| {
            let params = FormLineParams {
                addition_vertices: 0.0,
                minimum_gap_vertices,
                ..params()
            };
            let (ground, contours) = scene(banded, &north_south(), ContourKind::HALF_INTERVAL);
            select_form_lines(&contours, &ground, &params).unwrap()
        };
        assert_eq!(run(0).lines.iter().count(), 2);
        let closed = run(60);
        assert_eq!(closed.lines.iter().count(), 1);
        assert!(
            closed.keep[0].as_ref().unwrap().vertices[1..]
                .iter()
                .all(|v| *v)
        );
    }

    #[test]
    fn the_dump_round_trips_the_lines() {
        let (ground, contours) = scene(
            |x, _| 100.0 + 0.01 * x,
            &north_south(),
            ContourKind::HALF_INTERVAL.with_depression(true),
        );
        let labelled = FormLineParams {
            label_depressions: true,
            ..params()
        };
        let selection = select_form_lines(&contours, &ground, &labelled).unwrap();
        let fs = crate::io::fs::memory::MemoryFileSystem::new();
        write_form_lines(&fs, Path::new(""), &selection, true, true).unwrap();
        assert!(fs.exists("formlines.dxf"));
        let back = BinaryDxf::from_reader(&mut fs.open(FORM_LINES_DUMP).unwrap()).unwrap();
        let Geometry::Polylines2(lines) = back.take_geometry().swap_remove(0) else {
            panic!("no 2D lines");
        };
        assert!(lines.iter().eq(selection.lines.iter()));
        assert!(
            lines
                .iter()
                .all(|(_, c)| *c == Classification::FormlineDepression)
        );
        // without debug and dxf nothing is written
        let fs = crate::io::fs::memory::MemoryFileSystem::new();
        write_form_lines(&fs, Path::new(""), &selection, false, false).unwrap();
        assert!(!fs.exists(FORM_LINES_DUMP));
    }

    /// A square ring of the given ground size, as the selection sees it at `frame`.
    fn ring(metres: f64, frame: &MapFrame) -> (Vec<f64>, Vec<f64>) {
        let s = metres * frame.px_per_metre();
        (vec![0.0, s, s, 0.0, 0.0], vec![0.0, 0.0, s, s, 0.0])
    }

    #[test]
    fn rings_below_the_isom_minimum_are_rejected() {
        // ISOM 2017-2 symbol 103: minimum closed form line 1.1 mm at 1:15,000 = 16.5 m.
        let frame = MapFrame::default();
        let (x, y) = ring(10.0, &frame);
        assert!(below(&x, &y, &frame));
        let (x, y) = ring(20.0, &frame);
        assert!(!below(&x, &y, &frame));
    }

    #[test]
    fn an_elongated_ring_is_judged_by_its_longer_side() {
        // 5 m across but 40 m long: a real feature, not a speck.
        let frame = MapFrame::default();
        let s = frame.px_per_metre();
        let x = vec![0.0, 40.0 * s, 40.0 * s, 0.0, 0.0];
        let y = vec![0.0, 0.0, 5.0 * s, 5.0 * s, 0.0];
        assert!(!below(&x, &y, &frame));
    }

    #[test]
    fn the_bound_is_ground_distance_not_pixels() {
        // Same 20 m ring at 1:15 000 => 2/3 of the pixels, and the verdict must not change;
        // the bound is the same 16.5 m there.
        for scale in [10_000.0, 15_000.0] {
            let frame = MapFrame::at_scale(scale);
            let (x, y) = ring(20.0, &frame);
            assert!(!below(&x, &y, &frame), "{scale}");
            let (x, y) = ring(16.0, &frame);
            assert!(below(&x, &y, &frame), "{scale}");
        }
        // 1:5 000 draws symbols at 100 %: the minimum is 5.5 m.
        let frame = MapFrame::at_scale(5_000.0);
        let (x, y) = ring(6.0, &frame);
        assert!(!below(&x, &y, &frame));
    }

    #[test]
    fn pixel_to_ground_inverts_the_draw_transform_on_both_axes() {
        let (x0, y0) = (500000.0, 6700000.0);
        for scale in [5_000.0, 7_000.0, 10_000.0, 13_000.0, 20_000.0] {
            let frame = MapFrame::at_scale(scale);
            for (gx, gy) in [
                (500123.4, 6699876.6),
                (500001.1, 6699999.3),
                (500777.7, 6699321.9),
            ] {
                let px = frame.to_px(gx - x0);
                let py = frame.to_px(y0 - gy);
                let p = pixel_to_ground(px, py, x0, y0, &frame);
                assert!((p.x - gx).abs() < 1e-6 && (p.y - gy).abs() < 1e-6, "{p:?}");
                // one operator order: the same pixel offset gives the same ground offset
                let q = pixel_to_ground(px, px, 0.0, 0.0, &frame);
                assert_eq!(q.y.to_bits(), (-q.x).to_bits());
            }
        }
    }
}
