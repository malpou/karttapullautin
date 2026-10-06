//! Cliff detection on the ground model raster: slope, hysteresis, skeleton, polylines.
//!
//! The pipeline, every step a public function so a caller can stop or look in between:
//! 1. [`slope`]: the gradient magnitude of the ground model per cell, in metres of drop
//!    per metre (Horn's 3×3 operator, the one GIS slope tools use).
//! 2. [`hysteresis`]: cells at or above the high threshold seed a cliff; cells at or above
//!    the low threshold join it when they are 8-connected to a seed (Canny's double
//!    threshold). A weaker stretch that continues a strong one is kept; a weak stretch on
//!    its own is not.
//! 3. [`skeleton`]: Rosenfeld's directional thinning of the cliff cells (topology
//!    preserving; Zhang-Suen erased diagonal bands), followed by a pass that removes
//!    staircase cells, so every centre line is one cell wide in the 8-connected sense.
//! 4. [`trace`]: the skeleton as cell paths: one path between every pair of ends or
//!    junctions (a cluster of touching junction cells is one junction), and one closed
//!    path per ring with neither.
//! 5. [`detect`] ties them together: it converts the paths to map coordinates, measures
//!    each line's drop (the step height above the slope beyond it), drops lines below the
//!    minimum drop (steep slope, not a step) or the minimum length, and classifies the
//!    rest passable or impassable.
//!
//! Units: thresholds are slopes (m/m), lengths and drops are metres; the ground model's
//! cell size converts lengths and probe distances to cells. The slope thresholds still
//! depend on the cell size: a vertical step of height `h` reads as a slope of
//! `h / (2 × cell size)` on the cells either side of it (Horn's operator spans two
//! cells), so the defaults hold for the cell size they were measured at (2 m, see
//! [`RasterCliffParams`]). The low threshold must sit above the steepest slope that is
//! not a cliff: below it, the band around a step spreads over the whole slope and its
//! centre line leaves the step.
//!
//! Edges and gaps:
//! - On the border, the operator uses the cells that exist: the missing column or row is
//!   replaced by the centre one and the difference is divided by the shorter span, so a
//!   cliff reaching the edge keeps its slope.
//! - A cell with a NaN height in its 3×3 window has a NaN slope, which is never a cliff
//!   cell. A drop probe that lands on a NaN height or off the grid is skipped (an outer
//!   one leaves the other side to stand for the slope beyond); a line with no valid probe
//!   has a NaN drop and is dropped. The ground model has no NaN after its fill today.
//!
//! Known limits:
//! - Narrow walls and ridges vanish: the probes either side of a wall or a ridge narrower
//!   than twice the probe distance land on ground of the same height, so the drop is
//!   near 0. Measuring each side's drop separately would keep them (ENG-286 may).
//! - A cliff that a spur splits at a junction becomes two lines, each held to the
//!   minimum length on its own.
//!
//! Deterministic (no randomness, fixed scan orders), no IO, single-threaded. Cost is
//! linear in the cells except thinning, which scans the whole grid once per round, and
//! the rounds are about half the widest cliff band in cells. A worklist of the border
//! cells would make thinning linear too, if a finer cell size makes the bands wide.
//!
//! Not done here (ENG-286 `pr/raster-cliffs`): orienting the lines with tags downhill,
//! merging gaps between lines (and the minimum length again after it), smoothing, the
//! ISOM symbol codes and the wiring.

use crate::geometry::Point2;
use crate::io::heightmap::HeightMap;
use crate::vec2d::Vec2D;

/// Parameters of [`detect`]. Not read from the ini yet. The defaults come from a
/// prototype on the regression tile's 2 m ground model against the cliffs `makecliffs`
/// finds there (ENG-306); ENG-286 tunes them with `pullauta eval`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RasterCliffParams {
    /// Slope at or above which a cell seeds a cliff. Metres of drop per metre.
    pub high_slope: f64,
    /// Slope at or above which a cell joins a cliff it touches. Metres per metre.
    pub low_slope: f64,
    /// Lines shorter than this are dropped (ISOM 201/202: 0.6 mm at 1:15 000, 9 m).
    /// Metres along each line.
    pub min_length_m: f64,
    /// Distance either side of a line at which the ground is sampled for its drop (and
    /// twice that for the slope beyond). Metres; at least one cell is used.
    pub probe_m: f64,
    /// Lines whose drop is below this are dropped: steep slope, not a step. Metres.
    pub min_drop_m: f64,
    /// Drop at or above which a line is impassable (ISOM 201) instead of passable
    /// (ISOM 202). Metres.
    pub impassable_drop_m: f64,
}

impl Default for RasterCliffParams {
    fn default() -> Self {
        Self {
            high_slope: 0.8,
            low_slope: 0.5,
            min_length_m: 9.0,
            probe_m: 4.0,
            min_drop_m: 1.0,
            impassable_drop_m: 2.0,
        }
    }
}

/// Whether a cliff line can be crossed, from its drop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Passability {
    /// Drop below [`RasterCliffParams::impassable_drop_m`] (ISOM 202).
    Passable,
    /// Drop at or above it (ISOM 201).
    Impassable,
}

/// One cliff centre line in map coordinates.
#[derive(Debug, Clone, PartialEq)]
pub struct CliffLine {
    /// Cell centres along the line. A closed line repeats its first point at the end.
    pub points: Vec<Point2>,
    /// The line is a ring (a pit wall or a knoll's flank).
    pub closed: bool,
    /// Height of the step: the median over the line's points of the drop across it
    /// between the ground [`RasterCliffParams::probe_m`] either side, less the drop the
    /// slope beyond those points accounts for. Metres; never NaN (such lines are
    /// dropped).
    pub drop_m: f64,
    pub passability: Passability,
}

impl CliffLine {
    /// Length along the points. Metres.
    pub fn length_m(&self) -> f64 {
        polyline_length(&self.points)
    }
}

/// Runs the whole detection on `ground`. One line per [`trace`] path, in its order, so
/// lines meeting at a junction share its representative point. A line is kept when its
/// drop is at least the minimum drop (NaN is not) and it is at least the minimum length
/// itself: a cliff that a spur splits at a junction can fall short in pieces, which
/// ENG-286's gap merging is to join before it applies the minimum again.
pub fn detect(ground: &HeightMap, params: &RasterCliffParams) -> Vec<CliffLine> {
    let cell = ground.scale;
    let mask = hysteresis(&slope(ground), params.low_slope, params.high_slope);
    let paths = trace(&skeleton(&mask));
    let probe = (params.probe_m / cell).max(1.0);
    paths
        .iter()
        .filter_map(|path| {
            let drop_m = median_drop(&ground.grid, path, probe);
            if drop_m.is_nan() || drop_m < params.min_drop_m {
                return None;
            }
            let points: Vec<Point2> = path
                .cells
                .iter()
                .map(|&(x, y)| {
                    Point2::new(
                        ground.xoffset + cell * x as f64,
                        ground.yoffset + cell * y as f64,
                    )
                })
                .collect();
            if polyline_length(&points) < params.min_length_m {
                return None;
            }
            let passability = if drop_m >= params.impassable_drop_m {
                Passability::Impassable
            } else {
                Passability::Passable
            };
            Some(CliffLine {
                points,
                closed: path.closed,
                drop_m,
                passability,
            })
        })
        .collect()
}

/// Gradient magnitude of the ground model per cell, metres of drop per metre, by Horn's
/// (1981) weighted 3×3 differences. Border cells use the cells that exist (see the
/// module notes); a NaN in the window gives NaN.
pub fn slope(ground: &HeightMap) -> Vec2D<f64> {
    let grid = &ground.grid;
    let (w, h) = (grid.width(), grid.height());
    let mut out = Vec2D::new(w, h, f64::NAN);
    if w == 0 || h == 0 {
        return out;
    }
    let cell = ground.scale;
    for x in 0..w {
        let (x0, x1) = (x.saturating_sub(1), (x + 1).min(w - 1));
        for y in 0..h {
            let (y0, y1) = (y.saturating_sub(1), (y + 1).min(h - 1));
            let z = |xx: usize, yy: usize| grid[(xx, yy)];
            let dx = if x1 > x0 {
                (z(x1, y0) + 2.0 * z(x1, y) + z(x1, y1) - z(x0, y0) - 2.0 * z(x0, y) - z(x0, y1))
                    / (4.0 * (x1 - x0) as f64 * cell)
            } else {
                0.0
            };
            let dy = if y1 > y0 {
                (z(x0, y1) + 2.0 * z(x, y1) + z(x1, y1) - z(x0, y0) - 2.0 * z(x, y0) - z(x1, y0))
                    / (4.0 * (y1 - y0) as f64 * cell)
            } else {
                0.0
            };
            // a NaN anywhere in the window propagates through dx or dy
            out[(x, y)] = if z(x, y).is_nan() {
                f64::NAN
            } else {
                dx.hypot(dy)
            };
        }
    }
    out
}

/// Double threshold: cells with `values >= high` and every cell with `values >= low`
/// 8-connected to one of them through such cells. NaN is below both.
pub fn hysteresis(values: &Vec2D<f64>, low: f64, high: f64) -> Vec2D<bool> {
    let (w, h) = (values.width(), values.height());
    let mut mask = Vec2D::new(w, h, false);
    let mut stack = Vec::new();
    for x in 0..w {
        for y in 0..h {
            if values[(x, y)] >= high && !mask[(x, y)] {
                mask[(x, y)] = true;
                stack.push((x, y));
                while let Some(cell) = stack.pop() {
                    for n in neighbours(cell, w, h) {
                        if !mask[n] && values[n] >= low {
                            mask[n] = true;
                            stack.push(n);
                        }
                    }
                }
            }
        }
    }
    mask
}

/// One-cell-wide centre lines of `mask`: Rosenfeld's (1975) directional thinning, then
/// the removal of staircase cells, so a line's inner cells have exactly two neighbours.
///
/// Each round peels the north, south, east and west borders in turn. A border cell is
/// removed when it is simple (Yokoi's 8-connectivity number is 1: taking it away splits
/// or joins nothing) and is not a line end (it has two or more neighbours); the cells of
/// one border go together. Rounds repeat until one removes nothing, so components, holes
/// and ends survive. Zhang and Suen (1984) was tried first and erased a diagonal step's
/// band completely (its known weakness on staircase bands). A staircase cell has two
/// perpendicular 4-neighbours and neighbours that stay 8-connected without it; those are
/// removed one at a time in scan order. Cells off the grid count as background.
pub fn skeleton(mask: &Vec2D<bool>) -> Vec2D<bool> {
    let (w, h) = (mask.width(), mask.height());
    let mut img = Vec2D::new(w, h, false);
    for (x, y, v) in mask.iter() {
        img[(x, y)] = v;
    }

    let mut remove = Vec::new();
    loop {
        let mut changed = false;
        // north, south, east, west in NEIGHBOURS indices
        for border in [0, 4, 2, 6] {
            remove.clear();
            for x in 0..w {
                for y in 0..h {
                    if !img[(x, y)] {
                        continue;
                    }
                    let p = ring(&img, x, y);
                    let b = p.iter().filter(|&&v| v).count();
                    if !p[border] && b >= 2 && connectivity(&p) == 1 {
                        remove.push((x, y));
                    }
                }
            }
            changed |= !remove.is_empty();
            for &c in &remove {
                img[c] = false;
            }
        }
        if !changed {
            break;
        }
    }

    // staircase cells, removed one at a time in scan order so a pair is never both lost
    for x in 0..w {
        for y in 0..h {
            if !img[(x, y)] {
                continue;
            }
            let p = ring(&img, x, y);
            let corner = (0..4).any(|k| p[2 * k] && p[(2 * k + 2) % 8]);
            // one 8-connected run of neighbours: the ring has one run once a diagonal
            // between two set 4-neighbours is counted as set
            let mut q = p;
            for k in 0..4 {
                let (i, j) = (2 * k, (2 * k + 2) % 8);
                if p[i] && p[j] {
                    q[2 * k + 1] = true;
                }
            }
            let runs = (0..8).filter(|&i| !q[i] && q[(i + 1) % 8]).count();
            if corner && runs == 1 {
                img[(x, y)] = false;
            }
        }
    }
    img
}

/// A path of skeleton cells from [`trace`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CellPath {
    /// Cells `(x, y)` in order. A closed path repeats its first cell at the end.
    pub cells: Vec<(usize, usize)>,
    pub closed: bool,
}

/// Splits a skeleton into paths. Junction cells (three or more neighbours) that touch
/// form one junction, 8-connected; a junction is one node, drawn at its representative
/// cell (the member nearest the cluster's centroid, the first in scan order on a tie).
/// Every end (one neighbour) and every junction starts a path along each of its
/// unvisited links that leaves it, which runs through two-neighbour cells to the next end
/// or junction. Links inside a junction are not paths, nor is a detour that leaves a
/// junction and comes back to it through cells that all touch it. The rings left over,
/// made of two-neighbour cells only, become closed paths. A lone cell is a path of one
/// cell. Paths come in scan order (x, then y) of the cell they leave from, links in
/// [`NEIGHBOURS`] order.
pub fn trace(skel: &Vec2D<bool>) -> Vec<CellPath> {
    let (w, h) = (skel.width(), skel.height());
    let link = |c: (usize, usize), k: usize| offset(c, NEIGHBOURS[k], w, h).filter(|&n| skel[n]);
    let degree = |c: (usize, usize)| (0..8).filter(|&k| link(c, k).is_some()).count();

    // junction clusters and their representative cells
    const NONE: usize = usize::MAX;
    let mut cluster = Vec2D::new(w, h, NONE);
    let mut reps = Vec::new();
    let mut stack = Vec::new();
    for x in 0..w {
        for y in 0..h {
            if !skel[(x, y)] || cluster[(x, y)] != NONE || degree((x, y)) < 3 {
                continue;
            }
            let id = reps.len();
            let mut members = vec![(x, y)];
            cluster[(x, y)] = id;
            stack.push((x, y));
            while let Some(c) = stack.pop() {
                for n in neighbours(c, w, h) {
                    if skel[n] && cluster[n] == NONE && degree(n) >= 3 {
                        cluster[n] = id;
                        members.push(n);
                        stack.push(n);
                    }
                }
            }
            members.sort_unstable();
            let k = members.len() as f64;
            let cx = members.iter().map(|c| c.0 as f64).sum::<f64>() / k;
            let cy = members.iter().map(|c| c.1 as f64).sum::<f64>() / k;
            let d2 = |c: &(usize, usize)| (c.0 as f64 - cx).powi(2) + (c.1 as f64 - cy).powi(2);
            let mut rep = members[0];
            for m in &members[1..] {
                if d2(m) < d2(&rep) {
                    rep = *m;
                }
            }
            reps.push(rep);
        }
    }
    let snap = |c: (usize, usize)| match cluster[c] {
        NONE => c,
        id => reps[id],
    };

    // visited links per cell, one bit per NEIGHBOURS index
    let mut visited = Vec2D::new(w, h, 0u8);
    let mut paths = Vec::new();

    let walk = |start: (usize, usize), first: usize, visited: &mut Vec2D<u8>| {
        let mut cells = vec![snap(start)];
        let (mut cur, mut k) = (start, first);
        loop {
            let next = link(cur, k).expect("a link joins two skeleton cells");
            visited[cur] |= 1 << k;
            visited[next] |= 1 << opposite(k);
            cells.push(snap(next));
            if next == start || cluster[next] != NONE || degree(next) != 2 {
                break;
            }
            let follow = (0..8).find(|&j| visited[next] & (1 << j) == 0 && link(next, j).is_some());
            match follow {
                Some(j) => (cur, k) = (next, j),
                None => break,
            }
        }
        let closed = cells.len() > 2 && cells.first() == cells.last();
        CellPath { cells, closed }
    };

    for x in 0..w {
        for y in 0..h {
            let c = (x, y);
            if !skel[c] {
                continue;
            }
            let id = cluster[c];
            if id == NONE && degree(c) == 2 {
                continue;
            }
            if degree(c) == 0 {
                paths.push(CellPath {
                    cells: vec![c],
                    closed: false,
                });
            }
            for k in 0..8 {
                let Some(n) = link(c, k) else { continue };
                if visited[c] & (1 << k) != 0 {
                    continue;
                }
                if id != NONE && cluster[n] == id {
                    visited[c] |= 1 << k;
                    visited[n] |= 1 << opposite(k);
                    continue;
                }
                let path = walk(c, k, &mut visited);
                let inner = &path.cells[1..path.cells.len() - 1];
                let detour = id != NONE
                    && path.closed
                    && inner
                        .iter()
                        .all(|&i| neighbours(i, w, h).any(|n| cluster[n] == id));
                if !detour {
                    paths.push(path);
                }
            }
        }
    }
    for x in 0..w {
        for y in 0..h {
            let c = (x, y);
            if skel[c] && visited[c] == 0 && cluster[c] == NONE && degree(c) == 2 {
                let first = (0..8)
                    .find(|&k| link(c, k).is_some())
                    .expect("a ring cell has two neighbours");
                paths.push(walk(c, first, &mut visited));
            }
        }
    }
    paths
}

/// The eight neighbour steps `(dx, dy)`, clockwise from north (`y` grows northwards).
pub const NEIGHBOURS: [(isize, isize); 8] = [
    (0, 1),
    (1, 1),
    (1, 0),
    (1, -1),
    (0, -1),
    (-1, -1),
    (-1, 0),
    (-1, 1),
];

fn opposite(k: usize) -> usize {
    (k + 4) % 8
}

fn offset(c: (usize, usize), d: (isize, isize), w: usize, h: usize) -> Option<(usize, usize)> {
    let x = c.0.checked_add_signed(d.0)?;
    let y = c.1.checked_add_signed(d.1)?;
    (x < w && y < h).then_some((x, y))
}

fn neighbours(c: (usize, usize), w: usize, h: usize) -> impl Iterator<Item = (usize, usize)> {
    NEIGHBOURS.iter().filter_map(move |&d| offset(c, d, w, h))
}

/// Yokoi's connectivity number for 8-connected cells of a cell's ring (in [`NEIGHBOURS`]
/// order, 4-neighbours at even indices): the number of foreground groups the cell
/// touches. A border cell with 1 is simple.
fn connectivity(p: &[bool; 8]) -> u32 {
    let bg = |i: usize| u32::from(!p[i % 8]);
    [0, 2, 4, 6]
        .iter()
        .map(|&k| bg(k) - bg(k) * bg(k + 1) * bg(k + 2))
        .sum()
}

/// The eight neighbours of `(x, y)` in [`NEIGHBOURS`] order; off the grid is false.
fn ring(img: &Vec2D<bool>, x: usize, y: usize) -> [bool; 8] {
    let (w, h) = (img.width(), img.height());
    NEIGHBOURS.map(|d| offset((x, y), d, w, h).is_some_and(|n| img[n]))
}

fn polyline_length(points: &[Point2]) -> f64 {
    points
        .windows(2)
        .map(|s| (s[1].x - s[0].x).hypot(s[1].y - s[0].y))
        .sum()
}

/// Median over the path's cells of the step height across the path. With n the unit
/// normal through the cells two steps either side (around the seam of a closed path)
/// and z(t) the ground at t cells along n, the drop is z(probe) − z(−probe) less the
/// drop the slope beyond the probes accounts for: twice the smaller of the two outer
/// rises, z(2·probe) − z(probe) and z(−probe) − z(−2·probe), counted in the direction of
/// the drop and at least 0 each (twice the one there is when an outer probe is off the
/// grid or on NaN); at least 0. A uniform slope is 0, a step on flat ground its full
/// height, and a step with another step beyond it on one side keeps its own height.
/// A cell whose inner probes are off the grid or on NaN is skipped; NaN when none is
/// left.
fn median_drop(grid: &Vec2D<f64>, path: &CellPath, probe: f64) -> f64 {
    let cells = &path.cells;
    // a closed path repeats its first cell: count it once and wrap around the seam
    let n = if path.closed {
        cells.len() - 1
    } else {
        cells.len()
    };
    let around = |i: usize, d: isize| {
        if path.closed {
            cells[(i as isize + d).rem_euclid(n as isize) as usize]
        } else {
            cells[(i as isize + d).clamp(0, n as isize - 1) as usize]
        }
    };
    let mut drops: Vec<f64> = (0..n)
        .filter_map(|i| {
            let (a, b) = (around(i, -2), around(i, 2));
            let (tx, ty) = (b.0 as f64 - a.0 as f64, b.1 as f64 - a.1 as f64);
            let len = tx.hypot(ty);
            // a single cell has no direction: probe along x
            let (nx, ny) = if len > 0.0 {
                (-ty / len, tx / len)
            } else {
                (1.0, 0.0)
            };
            let (px, py) = (cells[i].0 as f64, cells[i].1 as f64);
            let z = |t: f64| bilinear(grid, px + nx * t, py + ny * t);
            let (up, down) = (z(probe)?, z(-probe)?);
            let inner = up - down;
            let sign = inner.signum();
            let rise_up = z(2.0 * probe).map(|far| (sign * (far - up)).max(0.0));
            let rise_down = z(-2.0 * probe).map(|far| (sign * (down - far)).max(0.0));
            let beyond = match (rise_up, rise_down) {
                (Some(u), Some(d)) => 2.0 * u.min(d),
                (Some(r), None) | (None, Some(r)) => 2.0 * r,
                (None, None) => 0.0,
            };
            Some((inner.abs() - beyond).max(0.0))
        })
        .collect();
    if drops.is_empty() {
        return f64::NAN;
    }
    drops.sort_by(f64::total_cmp);
    let m = drops.len() / 2;
    if drops.len() % 2 == 1 {
        drops[m]
    } else {
        (drops[m - 1] + drops[m]) / 2.0
    }
}

/// Bilinear height at cell coordinates; None off the grid or next to a NaN.
fn bilinear(grid: &Vec2D<f64>, x: f64, y: f64) -> Option<f64> {
    let (w, h) = (grid.width(), grid.height());
    if !(x >= 0.0 && y >= 0.0 && x <= (w - 1) as f64 && y <= (h - 1) as f64) {
        return None;
    }
    let (x0, y0) = (x.floor() as usize, y.floor() as usize);
    let (x1, y1) = ((x0 + 1).min(w - 1), (y0 + 1).min(h - 1));
    let (fx, fy) = (x - x0 as f64, y - y0 as f64);
    let z = (grid[(x0, y0)] * (1.0 - fx) + grid[(x1, y0)] * fx) * (1.0 - fy)
        + (grid[(x0, y1)] * (1.0 - fx) + grid[(x1, y1)] * fx) * fy;
    (!z.is_nan()).then_some(z)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `w × h` ground model at `cell` metres with heights `z(x, y)` in metres, cell
    /// centres at `(cell · i, cell · j)`.
    fn ground(w: usize, h: usize, cell: f64, z: impl Fn(f64, f64) -> f64) -> HeightMap {
        let mut grid = Vec2D::new(w, h, 0.0);
        for (x, y, v) in grid.iter_mut() {
            *v = z(cell * x as f64, cell * y as f64);
        }
        HeightMap {
            xoffset: 0.0,
            yoffset: 0.0,
            scale: cell,
            grid,
        }
    }

    /// Thresholds that make a vertical step a cliff from 2 m up on 1 m cells (slope
    /// h / 2 of the cells either side), and keep it going down to 1 m.
    fn params() -> RasterCliffParams {
        RasterCliffParams {
            high_slope: 1.0,
            low_slope: 0.5,
            ..RasterCliffParams::default()
        }
    }

    fn total_length(lines: &[CliffLine]) -> f64 {
        lines.iter().map(CliffLine::length_m).sum()
    }

    fn skeleton_cells(skel: &Vec2D<bool>) -> Vec<(usize, usize)> {
        skel.iter().filter(|c| c.2).map(|c| (c.0, c.1)).collect()
    }

    #[test]
    fn straight_step_gives_one_line_along_it() {
        // 3 m step at x = 50.5 m across a 101 m square
        let g = ground(101, 101, 1.0, |x, _| if x < 50.5 { 3.0 } else { 0.0 });
        let lines = detect(&g, &params());
        assert_eq!(lines.len(), 1, "{lines:?}");
        let line = &lines[0];
        assert!(!line.closed);
        assert!(line.length_m() >= 98.0, "length {}", line.length_m());
        for p in &line.points {
            assert!((p.x - 50.5).abs() <= 0.5, "off the step: {p:?}");
        }
        assert!((line.drop_m - 3.0).abs() < 1e-9, "drop {}", line.drop_m);
        assert_eq!(line.passability, Passability::Impassable);
    }

    #[test]
    fn skeleton_of_a_step_is_one_cell_wide() {
        let g = ground(41, 41, 1.0, |x, _| if x < 20.5 { 3.0 } else { 0.0 });
        let mask = hysteresis(&slope(&g), 0.5, 1.0);
        assert_eq!(mask.iter().filter(|c| c.2).count(), 2 * 41, "band of two");
        let skel = skeleton(&mask);
        for y in 0..41 {
            let row = (0..41).filter(|&x| skel[(x, y)]).count();
            assert!(row <= 1, "row {y} has {row} cells");
        }
        assert!(skeleton_cells(&skel).len() >= 39);
    }

    #[test]
    fn diagonal_step_survives_thinning() {
        let g = ground(61, 61, 1.0, |x, y| if x + y < 60.5 { 3.0 } else { 0.0 });
        let lines = detect(&g, &params());
        assert_eq!(lines.len(), 1, "{lines:?}");
        // the diagonal is 60√2 = 84.9 m long
        let len = lines[0].length_m();
        assert!(len > 80.0 && len < 88.0, "length {len}");
        for p in &lines[0].points {
            assert!((p.x + p.y - 60.5).abs() <= 1.5, "off the step: {p:?}");
        }
    }

    #[test]
    fn l_shaped_step_gives_one_connected_line_with_a_corner() {
        // a raised quadrant: steps along x = 50.5 (y < 50.5) and y = 50.5 (x < 50.5)
        let g = ground(
            101,
            101,
            1.0,
            |x, y| {
                if x < 50.5 && y < 50.5 { 3.0 } else { 0.0 }
            },
        );
        let lines = detect(&g, &params());
        assert_eq!(lines.len(), 1, "{lines:?}");
        let line = &lines[0];
        let len = line.length_m();
        assert!(len > 96.0 && len < 104.0, "length {len}");
        let near_corner = line
            .points
            .iter()
            .any(|p| (p.x - 50.5).abs() <= 1.5 && (p.y - 50.5).abs() <= 1.5);
        assert!(near_corner, "no point at the corner");
        for p in &line.points {
            let on_x = (p.x - 50.5).abs() <= 1.5 && p.y <= 52.0;
            let on_y = (p.y - 50.5).abs() <= 1.5 && p.x <= 52.0;
            assert!(on_x || on_y, "off the steps: {p:?}");
        }
    }

    #[test]
    fn gentle_slope_has_no_cliff() {
        // 0.4 m per metre (22°), under the low threshold
        let g = ground(101, 101, 1.0, |x, y| 0.4 * x + 0.1 * y);
        assert!(detect(&g, &params()).is_empty());
        assert!(
            slope(&g)
                .iter()
                .all(|c| (c.2 - 0.4f64.hypot(0.1)).abs() < 1e-9)
        );
    }

    #[test]
    fn steep_slope_without_a_step_is_dropped() {
        // 0.9 m per metre: every cell is a cliff cell, but the drop above the slope is 0
        let g = ground(61, 61, 1.0, |x, _| 0.9 * x);
        assert!(hysteresis(&slope(&g), 0.5, 1.0).iter().all(|c| !c.2));
        let p = RasterCliffParams {
            high_slope: 0.8,
            ..params()
        };
        assert!(
            hysteresis(&slope(&g), p.low_slope, p.high_slope)
                .iter()
                .all(|c| c.2)
        );
        assert!(detect(&g, &p).is_empty());
        // a 3 m step on that slope (2.4 m/m either side of it) is found once the low
        // threshold sits above the slope; below it the band swallows the whole slope
        let g = ground(61, 61, 1.0, |x, _| {
            0.9 * x + if x < 30.5 { 0.0 } else { 3.0 }
        });
        let p = RasterCliffParams {
            high_slope: 2.0,
            low_slope: 1.2,
            ..p
        };
        let lines = detect(&g, &p);
        assert_eq!(lines.len(), 1, "{lines:?}");
        for line in &lines {
            assert!((line.drop_m - 3.0).abs() < 0.5, "drop {}", line.drop_m);
        }
    }

    #[test]
    fn step_shorter_than_the_minimum_is_dropped() {
        // a 3 m step 7 cells long: about 6 m of centre line, under 9 m
        let g = ground(41, 7, 1.0, |x, _| if x < 20.5 { 3.0 } else { 0.0 });
        assert!(detect(&g, &params()).is_empty());
        // the same step 12 cells long is kept
        let g = ground(41, 12, 1.0, |x, _| if x < 20.5 { 3.0 } else { 0.0 });
        assert_eq!(detect(&g, &params()).len(), 1);
    }

    #[test]
    fn hysteresis_keeps_a_weak_continuation_of_a_strong_step() {
        // 3 m drop for y < 50, 1.4 m (slope 0.7: below high, above low) beyond
        let step = |x: f64, y: f64| {
            let h = if y < 50.0 { 3.0 } else { 1.4 };
            if x < 50.5 { h } else { 0.0 }
        };
        let lines = detect(&ground(101, 101, 1.0, step), &params());
        assert!(total_length(&lines) >= 98.0, "{lines:?}");

        // the weak step alone is not a cliff
        let weak = ground(101, 101, 1.0, |x, _| if x < 50.5 { 1.4 } else { 0.0 });
        assert!(detect(&weak, &params()).is_empty());
    }

    #[test]
    fn pit_wall_is_one_closed_line() {
        // a 3 m deep round pit of radius 15 m
        let g = ground(81, 81, 1.0, |x, y| {
            if (x - 40.0).hypot(y - 40.0) < 15.0 {
                -3.0
            } else {
                0.0
            }
        });
        let lines = detect(&g, &params());
        assert_eq!(lines.len(), 1, "{lines:?}");
        let line = &lines[0];
        assert!(line.closed);
        assert_eq!(line.points.first(), line.points.last());
        // the wall runs at r = 15, the skeleton within a cell of it
        let len = line.length_m();
        let expected = 2.0 * std::f64::consts::PI * 15.0;
        assert!((len - expected).abs() < 0.12 * expected, "length {len}");
        for p in &line.points {
            let r = (p.x - 40.0).hypot(p.y - 40.0);
            assert!((r - 15.0).abs() <= 1.5, "r = {r}");
        }
        assert_eq!(line.passability, Passability::Impassable);
        assert!((line.drop_m - 3.0).abs() < 0.5, "drop {}", line.drop_m);
    }

    #[test]
    fn drop_decides_passability() {
        let step = |h: f64| ground(41, 41, 1.0, move |x, _| if x < 20.5 { h } else { 0.0 });
        let p = params();
        let low = detect(
            &step(1.5),
            &RasterCliffParams {
                high_slope: 0.7,
                ..p
            },
        );
        assert_eq!(low.len(), 1);
        assert!((low[0].drop_m - 1.5).abs() < 1e-9);
        assert_eq!(low[0].passability, Passability::Passable);
        let high = detect(&step(2.5), &p);
        assert_eq!(high[0].passability, Passability::Impassable);
    }

    #[test]
    fn thresholds_and_lengths_scale_with_cell_size() {
        // the same 3 m step on 2 m cells reads as slope 0.75: under a 1.0 seed
        let g = ground(51, 51, 2.0, |x, _| if x < 50.5 { 3.0 } else { 0.0 });
        assert!(
            slope(&g)
                .iter()
                .all(|c| c.2 == 0.0 || (c.2 - 0.75).abs() < 1e-9)
        );
        assert!(detect(&g, &params()).is_empty());
        let lines = detect(
            &g,
            &RasterCliffParams {
                high_slope: 0.7,
                low_slope: 0.3,
                ..params()
            },
        );
        assert_eq!(lines.len(), 1);
        assert!(lines[0].length_m() >= 96.0, "{}", lines[0].length_m());
        // 4 rows of 2 m cells: 6 m of line, under 9 m
        let short = ground(51, 4, 2.0, |x, _| if x < 50.5 { 3.0 } else { 0.0 });
        let p = RasterCliffParams {
            high_slope: 0.7,
            ..params()
        };
        assert!(detect(&short, &p).is_empty());
    }

    #[test]
    fn nan_cells_are_never_cliffs() {
        let mut g = ground(41, 41, 1.0, |x, _| if x < 20.5 { 3.0 } else { 0.0 });
        for y in 0..41 {
            g.grid[(20, y)] = f64::NAN;
        }
        let s = slope(&g);
        for y in 0..41 {
            for x in 19..=21 {
                assert!(s[(x, y)].is_nan());
            }
        }
        assert!(detect(&g, &params()).is_empty());
        // a NaN cell elsewhere leaves the step alone
        let mut g = ground(41, 41, 1.0, |x, _| if x < 20.5 { 3.0 } else { 0.0 });
        g.grid[(5, 5)] = f64::NAN;
        assert_eq!(detect(&g, &params()).len(), 1);
    }

    #[test]
    fn border_cells_keep_their_slope() {
        let g = ground(10, 10, 1.0, |x, _| 0.5 * x);
        assert!(slope(&g).iter().all(|c| (c.2 - 0.5).abs() < 1e-9));
    }

    #[test]
    fn trace_splits_at_junctions_and_finds_rings() {
        let mut skel = Vec2D::new(9, 9, false);
        // a T: a row y = 4 from x = 0 to 8 and a column x = 4 from y = 5 to 8
        for x in 0..9 {
            skel[(x, 4)] = true;
        }
        for y in 5..9 {
            skel[(4, y)] = true;
        }
        // thinning keeps the arms and drops the staircase cell at the crossing, so the
        // junction is one cell
        let skel = skeleton(&skel);
        let paths = trace(&skel);
        assert_eq!(paths.len(), 3, "{paths:?}");
        assert!(paths.iter().all(|p| !p.closed));
        let mut covered: Vec<_> = paths.iter().flat_map(|p| p.cells.clone()).collect();
        covered.sort();
        covered.dedup();
        assert_eq!(covered, skeleton_cells(&skel));

        // a square ring has no ends: thinning cuts its corners (staircase cells), and
        // the octagon left is one closed path
        let mut ring = Vec2D::new(6, 6, false);
        for i in 1..5 {
            ring[(i, 1)] = true;
            ring[(i, 4)] = true;
            ring[(1, i)] = true;
            ring[(4, i)] = true;
        }
        let ring = skeleton(&ring);
        assert_eq!(skeleton_cells(&ring).len(), 8);
        let paths = trace(&ring);
        assert_eq!(paths.len(), 1, "{paths:?}");
        assert!(paths[0].closed);
        assert_eq!(paths[0].cells.len(), 8 + 1);
    }

    #[test]
    fn plus_is_four_arms_from_one_junction() {
        // a one-cell-wide "+" with arms of two cells: the centre and the four inner arm
        // cells all have three or more neighbours and form one junction
        let mut skel = Vec2D::new(11, 11, false);
        for i in 3..=7 {
            skel[(i, 5)] = true;
            skel[(5, i)] = true;
        }
        let paths = trace(&skel);
        assert_eq!(paths.len(), 4, "{paths:?}");
        let mut tips: Vec<(usize, usize)> = Vec::new();
        for p in &paths {
            // each arm runs between the junction, drawn at the centre, and its tip
            assert_eq!(p.cells.len(), 2, "{p:?}");
            assert!(p.cells.contains(&(5, 5)), "{p:?}");
            assert!(!p.closed);
            tips.extend(p.cells.iter().filter(|&&c| c != (5, 5)));
        }
        tips.sort();
        assert_eq!(tips, vec![(3, 5), (5, 3), (5, 7), (7, 5)]);
    }

    #[test]
    fn junction_cluster_is_one_node() {
        // eight arms of five cells from (10, 10): the centre, the first cell of every arm
        // and the second of the four straight arms all have three or more neighbours
        // and make one 13-cell junction
        let mut skel = Vec2D::new(21, 21, false);
        skel[(10, 10)] = true;
        for d in NEIGHBOURS {
            for r in 1..=5 {
                skel[offset((10, 10), (d.0 * r, d.1 * r), 21, 21).unwrap()] = true;
            }
        }
        let paths = trace(&skel);
        assert_eq!(paths.len(), 8, "{paths:?}");
        let ring = |c: (usize, usize)| (c.0 as isize - 10).abs().max((c.1 as isize - 10).abs());
        for p in &paths {
            let ends = [p.cells[0], *p.cells.last().unwrap()];
            let tip = if ends[0] == (10, 10) {
                ends[1]
            } else {
                ends[0]
            };
            let straight = tip.0 == 10 || tip.1 == 10;
            // the centre, then cells 3-5 of a straight arm or 2-5 of a diagonal one
            assert_eq!(p.cells.len(), if straight { 4 } else { 5 }, "{p:?}");
            assert!(ends.contains(&(10, 10)), "snapped to the centre: {p:?}");
            assert_eq!(ring(tip), 5, "{p:?}");
        }
    }

    #[test]
    fn crossing_steps_meet_at_one_point() {
        // raised opposite quadrants: two 3 m steps cross at (50.5, 50.5)
        let g = ground(101, 101, 1.0, |x, y| {
            if (x < 50.5) != (y < 50.5) { 3.0 } else { 0.0 }
        });
        let p = params();
        let lines = detect(&g, &p);
        assert_eq!(lines.len(), 4, "{lines:?}");
        // the one end all four lines share
        let ends: Vec<Point2> = lines
            .iter()
            .flat_map(|l| [l.points[0], *l.points.last().unwrap()])
            .collect();
        let centre = *ends
            .iter()
            .find(|e| ends.iter().filter(|o| o == e).count() == 4)
            .expect("a shared junction point");
        for line in &lines {
            assert!(line.length_m() >= 45.0, "length {}", line.length_m());
            let ends = [line.points[0], *line.points.last().unwrap()];
            assert!(ends.contains(&centre), "{line:?}");
            assert!((line.drop_m - 3.0).abs() < 0.5, "drop {}", line.drop_m);
        }
        assert!((centre.x - 50.5).abs() <= 1.5 && (centre.y - 50.5).abs() <= 1.5);
    }

    #[test]
    fn two_tier_steps_keep_their_own_heights() {
        // two 4 m steps down to the east, d apart: the other step is beyond the probes
        // on one side only, so it is not taken for slope
        for d in [6.0, 8.0, 10.0] {
            let g = ground(101, 101, 1.0, |x, _| {
                let upper = if x < 50.5 - d { 4.0 } else { 0.0 };
                let lower = if x < 50.5 { 4.0 } else { 0.0 };
                upper + lower
            });
            let lines = detect(&g, &params());
            assert_eq!(lines.len(), 2, "d = {d}: {lines:?}");
            for line in &lines {
                assert!(line.length_m() >= 98.0, "d = {d}: {}", line.length_m());
                assert!((line.drop_m - 4.0).abs() < 1e-9, "d = {d}: {}", line.drop_m);
            }
        }
    }

    #[test]
    fn uniform_slope_at_the_edge_has_no_drop() {
        // a line two cells from the west edge of a 0.9 m/m slope: the western outer probe
        // is off the grid, so the eastern one stands for the slope beyond
        let g = ground(30, 30, 1.0, |x, _| 0.9 * x);
        let path = CellPath {
            cells: (0..30).map(|y| (2, y)).collect(),
            closed: false,
        };
        let drop = median_drop(&g.grid, &path, 2.0);
        assert!(drop.abs() < 1e-9, "drop {drop}");
        // and a step at the edge on flat ground keeps its height
        let g = ground(30, 30, 1.0, |x, _| if x < 2.5 { 3.0 } else { 0.0 });
        assert!((median_drop(&g.grid, &path, 2.0) - 3.0).abs() < 1e-9);
    }

    #[test]
    fn line_without_a_measurable_drop_is_dropped() {
        // a 3 m step on a grid 4 cells wide: probes 4 m either side are off the grid
        let g = ground(4, 40, 1.0, |x, _| if x < 1.5 { 3.0 } else { 0.0 });
        assert!(hysteresis(&slope(&g), 0.5, 1.0).iter().any(|c| c.2));
        assert!(detect(&g, &params()).is_empty());
    }

    #[test]
    fn output_is_deterministic() {
        let g = ground(81, 81, 1.0, |x, y| {
            let pit = if (x - 40.0).hypot(y - 40.0) < 15.0 {
                -3.0
            } else {
                0.0
            };
            pit + if x < 10.5 { 4.0 } else { 0.0 }
        });
        assert_eq!(detect(&g, &params()), detect(&g, &params()));
    }

    #[test]
    #[ignore = "timing note, run with --release --ignored --nocapture"]
    fn runtime_on_a_1000_by_1000_ground_model() {
        // rolling terrain with a terrace of steps and a few pits, 2 m cells
        let g = ground(1000, 1000, 2.0, |x, y| {
            let base = 20.0 * (x / 300.0).sin() + 15.0 * (y / 230.0).cos();
            let terrace = 3.0 * ((x + 0.3 * y) / 120.0).floor();
            let pit = if ((x % 400.0) - 200.0).hypot((y % 400.0) - 200.0) < 40.0 {
                -4.0
            } else {
                0.0
            };
            base + terrace + pit
        });
        let p = RasterCliffParams::default();
        let start = std::time::Instant::now();
        let lines = detect(&g, &p);
        let elapsed = start.elapsed();
        println!(
            "1000 x 1000: {} lines, {:.0} m, {:.2?}",
            lines.len(),
            total_length(&lines),
            elapsed
        );
        assert!(!lines.is_empty());
    }
}
