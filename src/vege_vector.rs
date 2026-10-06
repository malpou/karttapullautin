//! Vectorize the per-cell vegetation classification grids into ISOM-coded polygons:
//! connected-component labeling, ISOM minimum-area dissolve, cell-edge ring tracing,
//! Douglas-Peucker simplification and Chaikin corner rounding. Output is GeoJSON plus a
//! combined binary/text DXF with the symbol code as DXF layer, for map program import.

use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::num::NonZeroU64;
use std::path::Path;

use image::{GrayImage, Luma};
use imageproc::region_labelling::{Connectivity, connected_components};

use crate::config::Config;
use crate::geojson;
use crate::geojson::geojson_types::VegetationPropertiesIsomCode as Code;
use crate::geometry::{BinaryDxf, Classification, Point2, Polylines, signed_area};
use crate::io::fs::FileSystem;
use crate::validity::{Cm, cm, ring_contacts, ring_is_simple};
use crate::vec2d::Vec2D;
use crate::vegetation::VegetationClasses;

/// ISOM 2017-2 minimum footprint area (m²) of a vegetation area symbol, from the spec's
/// "Minimum area" parameters (footprints at 1:15,000, 1 mm = 15 m).
fn min_area_m2(code: Code) -> f64 {
    match code {
        Code::X403000 => 225.0,  // Rough open land: 1 x 1 mm (15 x 15 m)
        Code::X406000 => 225.0,  // Vegetation, slow running: 1 x 1 mm (15 x 15 m)
        Code::X407000 => 337.5,  // Slow running, good visibility: 1.5 x 1 mm (22.5 x 15 m)
        Code::X408000 => 110.25, // Vegetation, walk: 0.7 x 0.7 mm (10.5 x 10.5 m)
        Code::X410000 => 64.0,   // Vegetation, fight: 0.55 x 0.55 mm (8 x 8 m)
    }
}

/// The DXF classification of a vegetation symbol.
fn classification(code: Code) -> Classification {
    match code {
        Code::X403000 => Classification::Veg403,
        Code::X406000 => Classification::Veg406,
        Code::X407000 => Classification::Veg407,
        Code::X408000 => Classification::Veg408,
        Code::X410000 => Classification::Veg410,
    }
}

/// DXF draw order, light to dark, so map programs stack the areas the way the raster does.
fn draw_order(code: Code) -> usize {
    match code {
        Code::X403000 => 0,
        Code::X406000 => 1,
        Code::X408000 => 2,
        Code::X410000 => 3,
        Code::X407000 => 4,
    }
}

/// One vegetation polygon, traced from one connected component of equal grid values.
#[derive(Debug, PartialEq)]
struct VegPolygon {
    /// The grid value of the component (for the green grid: the greenshade index).
    value: u8,
    code: Code,
    /// First = exterior CCW, rest = holes CW. Open (first point not repeated).
    rings: Vec<Vec<Point2>>,
}

/// A vertex on the integer cell-corner lattice.
type V = (i64, i64);

/// Directed boundary edges keyed by start vertex: (end vertex, label on the other side).
type EdgeMap = HashMap<V, Vec<(V, u32)>>;

/// Vectorize one class grid. `code_of` maps a non-zero cell value to its symbol code,
/// `median_radii` mirrors the raster median filtering (0 = skip pass), `epsilon` is the
/// Douglas-Peucker tolerance in meters (0 disables simplification and smoothing).
fn grid_to_polygons(
    grid: &Vec2D<u8>,
    origin: (f64, f64),
    cell: f64,
    code_of: &dyn Fn(u8) -> Code,
    median_radii: [u32; 2],
    epsilon: f64,
) -> Vec<VegPolygon> {
    trace_polygons(grid, origin, cell, code_of, median_radii, epsilon).0
}

/// [`grid_to_polygons`], and how many chains were left unsimplified to keep the
/// polygons valid.
fn trace_polygons(
    grid: &Vec2D<u8>,
    origin: (f64, f64),
    cell: f64,
    code_of: &dyn Fn(u8) -> Code,
    median_radii: [u32; 2],
    epsilon: f64,
) -> (Vec<VegPolygon>, usize) {
    let (w, h) = (grid.width() as u32, grid.height() as u32);
    if w == 0 || h == 0 {
        return (Vec::new(), 0);
    }

    // work in image space with pixel (x,y) == grid (x,y); no flips anywhere
    let mut img = GrayImage::from_fn(w, h, |x, y| Luma([grid[(x as usize, y as usize)]]));
    for r in median_radii {
        if r > 0 {
            img = imageproc::filter::median_filter(&img, r, r);
        }
    }

    // dissolve components below the ISOM minimum area into their surroundings.
    // Iterated to a fixpoint: dissolving component B after A dissolved into it orphans
    // A's cells as a new small component, so one pass is not enough on noisy data.
    // Terminates: a pass that changes cells only ever merges components (dissolved cells
    // join a neighbouring class), so the component count strictly decreases each pass.
    let mut labels = connected_components(&img, Connectivity::Four, Luma([0u8]));
    loop {
        let mut comp_cells: HashMap<u32, Vec<(u32, u32)>> = HashMap::new();
        for y in 0..h {
            for x in 0..w {
                let l = labels.get_pixel(x, y)[0];
                if l != 0 {
                    comp_cells.entry(l).or_default().push((x, y));
                }
            }
        }
        let mut changed = false;
        // dissolve in label order: img is mutated as we go, so iteration order affects
        // multi-class outcomes and HashMap order would make them nondeterministic
        let mut comp_cells: Vec<_> = comp_cells.into_iter().collect();
        comp_cells.sort_unstable_by_key(|(label, _)| *label);
        for (_, cells) in &comp_cells {
            let class = img.get_pixel(cells[0].0, cells[0].1)[0];
            let min_cells = (min_area_m2(code_of(class)) / (cell * cell)).ceil() as usize;
            if cells.len() >= min_cells {
                continue;
            }
            // dissolve into the most frequent surrounding class (out-of-grid = background)
            let mut counts: HashMap<u8, usize> = HashMap::new();
            for &(x, y) in cells {
                let mut visit = |nx: i64, ny: i64| {
                    let c = if nx < 0 || ny < 0 || nx >= w as i64 || ny >= h as i64 {
                        0
                    } else {
                        img.get_pixel(nx as u32, ny as u32)[0]
                    };
                    if c != class {
                        *counts.entry(c).or_default() += 1;
                    }
                };
                visit(x as i64 - 1, y as i64);
                visit(x as i64 + 1, y as i64);
                visit(x as i64, y as i64 - 1);
                visit(x as i64, y as i64 + 1);
            }
            // tie-break by class code so equal-count neighbours resolve deterministically
            let new = counts
                .into_iter()
                .max_by_key(|&(c, n)| (n, c))
                .map(|(c, _)| c)
                .unwrap_or(0);
            for &(x, y) in cells {
                img.put_pixel(x, y, Luma([new]));
            }
            changed = true;
        }
        labels = connected_components(&img, Connectivity::Four, Luma([0u8]));
        if !changed {
            break;
        }
    }

    // trace each component's boundary rings
    let lab = |x: i64, y: i64| -> u32 {
        if x < 0 || y < 0 || x >= w as i64 || y >= h as i64 {
            0
        } else {
            labels.get_pixel(x as u32, y as u32)[0]
        }
    };

    // Boundary chains are shared by exactly two components. Simplifying each chain ONCE
    // and reusing it for both sides keeps adjacent polygons perfectly coincident (no
    // hairline slivers). Chains are anchored at junction vertices where 3+ labels meet
    // (or where the same pair pinches diagonally - splitting there keeps the walk
    // decomposition identical on both sides).
    let mut junctions: std::collections::HashSet<(i64, i64)> = Default::default();
    for y in 0..=(h as i64) {
        for x in 0..=(w as i64) {
            let d = [lab(x - 1, y - 1), lab(x, y - 1), lab(x - 1, y), lab(x, y)];
            let mut s = d;
            s.sort_unstable();
            let mut distinct = 1;
            for i in 1..4 {
                if s[i] != s[i - 1] {
                    distinct += 1;
                }
            }
            let checker = d[0] == d[3] && d[1] == d[2] && d[0] != d[1];
            if distinct >= 3 || checker {
                junctions.insert((x, y));
            }
        }
    }

    // directed cell edges, CCW around each cell, with the label on the other side
    let mut comp_edges: HashMap<u32, (u8, EdgeMap)> = HashMap::new();
    for y in 0..h as i64 {
        for x in 0..w as i64 {
            let l = lab(x, y);
            if l == 0 {
                continue;
            }
            let entry = comp_edges
                .entry(l)
                .or_insert_with(|| (img.get_pixel(x as u32, y as u32)[0], HashMap::new()));
            let edges = &mut entry.1;
            let o = lab(x, y - 1);
            if o != l {
                edges.entry((x, y)).or_default().push(((x + 1, y), o));
            }
            let o = lab(x + 1, y);
            if o != l {
                edges
                    .entry((x + 1, y))
                    .or_default()
                    .push(((x + 1, y + 1), o));
            }
            let o = lab(x, y + 1);
            if o != l {
                edges
                    .entry((x + 1, y + 1))
                    .or_default()
                    .push(((x, y + 1), o));
            }
            let o = lab(x - 1, y);
            if o != l {
                edges.entry((x, y + 1)).or_default().push(((x, y), o));
            }
        }
    }

    // deterministic component order: which side of a shared chain simplifies first
    // decides the vertex selection, so HashMap order would make output nondeterministic
    let mut comp_edges: Vec<_> = comp_edges.into_iter().collect();
    comp_edges.sort_unstable_by_key(|(label, _)| *label);
    let traced: Vec<Component> = comp_edges
        .into_iter()
        .map(|(label, (class, edges))| (label, class, chain_rings(edges)))
        .collect();

    // Simplification works chain by chain, so a chord of one ring can cross another
    // ring of the same polygon (a hole and its exterior in a strip one cell wide). The
    // chains of a ring that does are traced again unsimplified, on both sides, until
    // every polygon is valid; the unsimplified rings are, by construction.
    let mut raw: HashSet<ChainKey> = HashSet::new();
    loop {
        let (polygons, offending) =
            assemble(&traced, &junctions, &raw, origin, cell, code_of, epsilon);
        let before = raw.len();
        raw.extend(offending);
        if raw.len() == before {
            return (polygons, raw.len());
        }
        log::debug!("{} chains left unsimplified for valid polygons", raw.len());
    }
}

/// A traced component: its label, its grid value and its rings (lattice vertices, and
/// the label across each edge).
type Component = (u32, u8, Vec<(Vec<V>, Vec<u32>)>);

/// A chain between two components: the components' labels, its end vertices and its
/// least interior vertex (both sides of the chain build the same key).
type ChainKey = (u32, u32, V, V, V);

/// One traced ring: its points, the chains it is made of and its lattice vertices.
struct Traced {
    pts: Vec<Point2>,
    keys: Vec<ChainKey>,
    lattice: Vec<V>,
}

/// Assemble the polygons from the traced rings, every chain simplified (Douglas-Peucker
/// and Chaikin at `epsilon`) but those in `raw`. Also returns the chains of the rings
/// that make a polygon invalid (see [`invalid_rings`]).
#[allow(clippy::too_many_arguments)]
fn assemble(
    traced: &[Component],
    junctions: &HashSet<V>,
    raw: &HashSet<ChainKey>,
    origin: (f64, f64),
    cell: f64,
    code_of: &dyn Fn(u8) -> Code,
    epsilon: f64,
) -> (Vec<VegPolygon>, Vec<ChainKey>) {
    // simplified shared chains, cached so both sides get the identical polyline; the
    // second requester always traverses the chain in the opposite direction
    let mut chain_cache: HashMap<ChainKey, Vec<Point2>> = HashMap::new();
    let to_world = |v: V| Point2::new(origin.0 + v.0 as f64 * cell, origin.1 + v.1 as f64 * cell);
    let simplify = |key: &ChainKey| epsilon > 0.0 && !raw.contains(key);

    let mut polygons = Vec::new();
    let mut offending = Vec::new();
    for (label, class, rings) in traced {
        let (label, class) = (*label, *class);
        let code = code_of(class);
        let mut exteriors: Vec<Traced> = Vec::new();
        let mut holes: Vec<Traced> = Vec::new();
        for (vs, others) in rings {
            let n = vs.len();
            let junct_pos: Vec<usize> = (0..n).filter(|&i| junctions.contains(&vs[i])).collect();
            let mut ring_pts: Vec<Point2> = Vec::new();
            let mut keys = Vec::new();
            if junct_pos.is_empty() {
                // island: one closed chain between exactly two components
                let other = others[0];
                let vmin = *vs.iter().min().unwrap();
                let key = (label.min(other), label.max(other), vmin, vmin, vmin);
                keys.push(key);
                if let Some(cached) = chain_cache.get(&key) {
                    ring_pts = cached.iter().rev().cloned().collect();
                } else {
                    let mut pts: Vec<Point2> = vs.iter().map(|&v| to_world(v)).collect();
                    if simplify(&key) {
                        pts = simplify_closed(pts, epsilon);
                        pts = chaikin_closed(&pts);
                    }
                    chain_cache.insert(key, pts.clone());
                    ring_pts = pts;
                }
            } else {
                for k in 0..junct_pos.len() {
                    let a = junct_pos[k];
                    let b = junct_pos[(k + 1) % junct_pos.len()];
                    let mut chain = vec![vs[a]];
                    let mut i = (a + 1) % n;
                    loop {
                        chain.push(vs[i]);
                        if i == b {
                            break;
                        }
                        i = (i + 1) % n;
                    }
                    let other = others[a];
                    let (e0, e1) = (
                        *chain.first().unwrap().min(chain.last().unwrap()),
                        *chain.first().unwrap().max(chain.last().unwrap()),
                    );
                    // interior min disambiguates parallel chains between the same pair
                    let vmin = chain[1..chain.len().saturating_sub(1)]
                        .iter()
                        .min()
                        .copied()
                        .unwrap_or(chain[0]);
                    let key = (label.min(other), label.max(other), e0, e1, vmin);
                    keys.push(key);
                    let pts: Vec<Point2> = if let Some(cached) = chain_cache.get(&key) {
                        cached.iter().rev().cloned().collect()
                    } else {
                        let mut pts: Vec<Point2> = chain.iter().map(|&v| to_world(v)).collect();
                        if simplify(&key) {
                            pts = dp(&pts, epsilon);
                            pts = chaikin_open(&pts);
                        }
                        chain_cache.insert(key, pts.clone());
                        pts
                    };
                    let skip = if ring_pts.is_empty() { 0 } else { 1 };
                    ring_pts.extend(pts.into_iter().skip(skip));
                }
                if ring_pts.len() > 1 && ring_pts.first() == ring_pts.last() {
                    ring_pts.pop();
                }
            }
            if ring_pts.len() < 3 {
                continue;
            }
            let ring = Traced {
                pts: ring_pts,
                keys,
                lattice: vs.clone(),
            };
            if signed_area(&ring.pts) > 0.0 {
                exteriors.push(ring);
            } else {
                holes.push(ring);
            }
        }
        if exteriors.is_empty() {
            continue;
        }
        // the walk turns right where the component's cells touch diagonally
        // ([`chain_rings`]), so a 4-connected component has exactly one exterior
        debug_assert_eq!(exteriors.len(), 1, "component {label}");
        // should that ever fail, the holes go with the largest exterior, and the rest are
        // emitted hole-less when at least the ISOM minimum
        exteriors.sort_by(|a, b| {
            signed_area(&b.pts)
                .abs()
                .partial_cmp(&signed_area(&a.pts).abs())
                .unwrap()
        });
        let mut it = exteriors.into_iter();
        let mut rings = vec![it.next().unwrap()];
        rings.extend(holes);
        if epsilon > 0.0 {
            for i in invalid_rings(&rings, to_world) {
                offending.extend(rings[i].keys.iter().copied());
            }
        }
        polygons.push(VegPolygon {
            value: class,
            code,
            rings: rings.into_iter().map(|r| r.pts).collect(),
        });
        for extra in it {
            if signed_area(&extra.pts) >= min_area_m2(code) {
                polygons.push(VegPolygon {
                    value: class,
                    code,
                    rings: vec![extra.pts],
                });
            }
        }
    }
    (polygons, offending)
}

/// The rings of a polygon (exterior first) that make it invalid as written (on the cm
/// grid): rings that are not simple, and pairs of rings that cross, share a stretch or
/// touch anywhere but at a lattice vertex both pass through unsimplified (the corners
/// where the component's cells touch diagonally).
fn invalid_rings(rings: &[Traced], to_world: impl Fn(V) -> Point2) -> Vec<usize> {
    let on_grid: Vec<Vec<Cm>> = rings
        .iter()
        .map(|r| {
            let mut ring: Vec<Cm> = r
                .pts
                .iter()
                .chain(r.pts.first())
                .map(|p| cm([p.x, p.y]))
                .collect();
            ring.dedup();
            ring
        })
        .collect();
    let bbox = |ring: &[Cm]| {
        ring.iter()
            .fold([i64::MAX, i64::MAX, i64::MIN, i64::MIN], |b, p| {
                [b[0].min(p.0), b[1].min(p.1), b[2].max(p.0), b[3].max(p.1)]
            })
    };
    let boxes: Vec<[i64; 4]> = on_grid.iter().map(|r| bbox(r)).collect();
    let mut bad = vec![false; rings.len()];
    for i in 0..rings.len() {
        if !ring_is_simple(&on_grid[i]) {
            bad[i] = true;
        }
        for j in i + 1..rings.len() {
            let (a, b) = (boxes[i], boxes[j]);
            if a[0] > b[2] || b[0] > a[2] || a[1] > b[3] || b[1] > a[3] {
                continue;
            }
            let allowed = || -> HashSet<Cm> {
                let theirs: HashSet<V> = rings[j].lattice.iter().copied().collect();
                rings[i]
                    .lattice
                    .iter()
                    .filter(|v| theirs.contains(v))
                    .map(|&v| {
                        let p = to_world(v);
                        cm([p.x, p.y])
                    })
                    .collect()
            };
            let valid = ring_contacts(&on_grid[i], &on_grid[j]).is_some_and(|touches| {
                touches.is_empty() || {
                    let allowed = allowed();
                    touches.iter().all(|t| allowed.contains(t))
                }
            });
            if !valid {
                bad[i] = true;
                bad[j] = true;
            }
        }
    }
    (0..rings.len()).filter(|&i| bad[i]).collect()
}

/// Chain directed edges into closed rings, tracking the other-side label per segment.
/// Every vertex has equal in/out degree, so a walk always finds an outgoing edge until
/// it closes on its start.
///
/// A vertex with two outgoing edges is a corner where the component's cells touch
/// diagonally. The walk turns right there, onto the other cell's edge, so the two
/// passes through the corner land in different rings (the exterior and a hole touching
/// it, or two holes) instead of one ring touching itself, which is not a valid polygon
/// ring. The walk starts at the minimum vertex, which has a cell edge to its left and
/// so is never such a corner.
fn chain_rings(mut edges: EdgeMap) -> Vec<(Vec<V>, Vec<u32>)> {
    let mut rings = Vec::new();
    // start each walk at the minimum remaining vertex so ring rotation is deterministic
    while let Some(start) = edges.keys().min().copied() {
        let mut ring = vec![start];
        let mut others = Vec::new();
        let mut prev: Option<V> = None;
        let mut cur = start;
        loop {
            let Some(nexts) = edges.get_mut(&cur) else {
                // inconsistent topology; drop this partial ring rather than panic
                ring.clear();
                break;
            };
            // the right turn: the incoming direction rotated clockwise (edges run
            // counter-clockwise around their cells, x right and y up)
            let pick = prev
                .filter(|_| nexts.len() > 1)
                .and_then(|p| {
                    let right = (cur.1 - p.1, p.0 - cur.0);
                    nexts
                        .iter()
                        .position(|&(n, _)| (n.0 - cur.0, n.1 - cur.1) == right)
                })
                .unwrap_or(nexts.len() - 1);
            let (next, other) = nexts.remove(pick);
            if nexts.is_empty() {
                edges.remove(&cur);
            }
            others.push(other);
            if next == start {
                break;
            }
            ring.push(next);
            prev = Some(cur);
            cur = next;
        }
        if ring.len() >= 3 {
            rings.push((ring, others));
        }
    }
    rings
}

fn perp_dist(p: &Point2, a: &Point2, b: &Point2) -> f64 {
    let (dx, dy) = (b.x - a.x, b.y - a.y);
    let len = (dx * dx + dy * dy).sqrt();
    if len == 0.0 {
        return ((p.x - a.x).powi(2) + (p.y - a.y).powi(2)).sqrt();
    }
    ((p.x - a.x) * dy - (p.y - a.y) * dx).abs() / len
}

/// Douglas-Peucker on an open polyline (endpoints kept).
pub(crate) fn dp(pts: &[Point2], eps: f64) -> Vec<Point2> {
    let n = pts.len();
    if n <= 2 {
        return pts.to_vec();
    }
    let mut keep = vec![false; n];
    keep[0] = true;
    keep[n - 1] = true;
    let mut stack = vec![(0usize, n - 1)];
    while let Some((i, j)) = stack.pop() {
        if j <= i + 1 {
            continue;
        }
        let (mut maxd, mut idx) = (0.0f64, i);
        for k in i + 1..j {
            let d = perp_dist(&pts[k], &pts[i], &pts[j]);
            if d > maxd {
                maxd = d;
                idx = k;
            }
        }
        if maxd > eps {
            keep[idx] = true;
            stack.push((i, idx));
            stack.push((idx, j));
        }
    }
    pts.iter()
        .zip(keep)
        .filter(|&(_, k)| k)
        .map(|(p, _)| *p)
        .collect()
}

/// Douglas-Peucker for a closed ring: split at the vertex farthest from vertex 0,
/// simplify both halves, rejoin. Ring is open (no repeated first point).
pub(crate) fn simplify_closed(ring: Vec<Point2>, eps: f64) -> Vec<Point2> {
    if ring.len() <= 4 {
        return ring;
    }
    let far = (1..ring.len())
        .max_by(|&a, &b| {
            let da = (ring[a].x - ring[0].x).powi(2) + (ring[a].y - ring[0].y).powi(2);
            let db = (ring[b].x - ring[0].x).powi(2) + (ring[b].y - ring[0].y).powi(2);
            da.partial_cmp(&db).unwrap()
        })
        .unwrap();
    let mut first = dp(&ring[..=far], eps);
    let mut second: Vec<Point2> = ring[far..].to_vec();
    second.push(ring[0]);
    let second = dp(&second, eps);
    // first ends at ring[far], second starts there and ends at ring[0] = first[0]
    first.extend_from_slice(&second[1..second.len() - 1]);
    first
}

/// One iteration of Chaikin corner cutting on an open polyline, endpoints preserved.
pub(crate) fn chaikin_open(pts: &[Point2]) -> Vec<Point2> {
    if pts.len() < 3 {
        return pts.to_vec();
    }
    let mut out = vec![pts[0]];
    for w in pts.windows(2) {
        out.push(Point2::new(
            0.75 * w[0].x + 0.25 * w[1].x,
            0.75 * w[0].y + 0.25 * w[1].y,
        ));
        out.push(Point2::new(
            0.25 * w[0].x + 0.75 * w[1].x,
            0.25 * w[0].y + 0.75 * w[1].y,
        ));
    }
    out.push(pts[pts.len() - 1]);
    out
}

/// One iteration of Chaikin corner cutting on a closed ring (open representation).
pub(crate) fn chaikin_closed(ring: &[Point2]) -> Vec<Point2> {
    let n = ring.len();
    let mut out = Vec::with_capacity(2 * n);
    for i in 0..n {
        let p = &ring[i];
        let q = &ring[(i + 1) % n];
        out.push(Point2::new(
            0.75 * p.x + 0.25 * q.x,
            0.75 * p.y + 0.25 * q.y,
        ));
        out.push(Point2::new(
            0.25 * p.x + 0.75 * q.x,
            0.25 * p.y + 0.75 * q.y,
        ));
    }
    out
}

/// Vegetation polygons as `VegetationProperties` Polygon features. With `shade`, each
/// feature also carries its polygon's grid value as `shade`.
fn area_features(
    polygons: &[VegPolygon],
    shade: bool,
) -> impl Iterator<Item = geojson::geojson_types::Feature> + '_ {
    polygons.iter().map(move |p| {
        let shade = shade.then(|| {
            NonZeroU64::new(p.value.into()).expect("traced components are never background")
        });
        geojson::vegetation_area(p.code, shade, &p.rings)
    })
}

/// Vectorize and write all vegetation vector outputs from the classes `makevege` drew
/// (with a vector family in `outputs`): the `vegetation_areas` table when
/// [`Config::vector_tables`], `vegetation.dxf` with the dxf family, and always
/// `vegetation.dxf.bin`, the batch crop's input; with `vector_shade=1` the green areas
/// also carry their greenshade index. The open land grid's origin is shifted +1.5 m
/// (makevege's 2x2 sum window).
pub fn export_all(
    fs: &impl FileSystem,
    config: &Config,
    tmpfolder: &Path,
    classes: &VegetationClasses,
) -> Result<(), Box<dyn Error>> {
    let VegetationClasses {
        green,
        open_land,
        undergrowth,
        bounds,
        block,
    } = classes;
    let block = *block;
    log::info!("Vectorizing vegetation...");
    let eps = config.vector_simplify;

    // mirror the raster median filtering, radius converted from meters to cells
    let radius = |m: u32, cell: f64| -> u32 {
        if m > 1 {
            (((m / 2) as f64 / cell).round() as u32).max(1)
        } else {
            0
        }
    };
    let vege = &config.vegetation;
    let green_med = [radius(vege.med, block), radius(vege.med2, block)];
    let open_land_med = if vege.proceed_yellows {
        [radius(vege.med, 3.0), radius(vege.med2, 3.0)]
    } else {
        [radius(vege.medyellow, 3.0), 0]
    };

    let map = &config.vector_greenshade_isom;
    let green_code = |c: u8| -> Code {
        *map.get((c as usize).saturating_sub(1))
            .or(map.last())
            .expect("vector_greenshade_isom is validated non-empty at config load")
    };

    let green_polys = grid_to_polygons(
        green,
        (bounds.xmin, bounds.ymin),
        block,
        &green_code,
        green_med,
        eps,
    );
    let open_land_polys = grid_to_polygons(
        open_land,
        (bounds.xmin + 1.5, bounds.ymin + 1.5),
        3.0,
        &|_| Code::X403000,
        open_land_med,
        eps,
    );
    let undergrowth_polys = grid_to_polygons(
        undergrowth,
        (bounds.xmin, bounds.ymin),
        block * 6.0,
        &|_| Code::X407000,
        [0, 0],
        eps,
    );

    // only the green areas have shades; open land and undergrowth are 0/1 grids
    if config.vector_tables() {
        let features = area_features(&green_polys, config.vector_shade)
            .chain(area_features(&open_land_polys, false))
            .chain(area_features(&undergrowth_polys, false))
            .collect();
        geojson::write_tables(
            fs,
            tmpfolder,
            geojson::Source::Vegetation,
            features,
            config.epsg,
        )?;
    }

    // combined DXF in draw order (stable sort keeps the traced order within a symbol)
    let mut all: Vec<&VegPolygon> = green_polys
        .iter()
        .chain(&open_land_polys)
        .chain(&undergrowth_polys)
        .collect();
    all.sort_by_key(|p| draw_order(p.code));

    let mut lines: Polylines<Point2, Classification> = Polylines::new();
    for p in all {
        let class = classification(p.code);
        for ring in &p.rings {
            let mut closed = ring.clone();
            closed.push(ring[0]);
            lines.push(closed, class);
        }
    }
    let dxf = BinaryDxf::new(bounds.clone(), vec![lines.into()]);
    dxf.to_writer(&mut fs.create(tmpfolder.join("vegetation.dxf.bin"))?)?;
    if config.outputs.dxf {
        dxf.to_dxf(&mut fs.create(tmpfolder.join("vegetation.dxf"))?)?;
    }
    log::info!("Done");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 8x8 grid: one class-1 region with a hole, plus a checkerboard corner where two
    /// class-1 cells touch diagonally. Also a tiny isolated patch that must dissolve.
    #[test]
    fn trace_hole_and_checkerboard() {
        let mut grid = Vec2D::new(8, 8, 0u8);
        // 5x5 block of class 1 at (1..6, 1..6) with a hole at (3,3)
        for x in 1..6 {
            for y in 1..6 {
                grid[(x, y)] = 1;
            }
        }
        grid[(3, 3)] = 0;
        // checkerboard corner: diagonal touch at (6,6)
        grid[(6, 6)] = 1;
        // no isolated patch here; dissolve tested separately

        // min area tiny so nothing dissolves (cell=10 -> 100 m² per cell)
        let polys = grid_to_polygons(&grid, (0.0, 0.0), 10.0, &|_| Code::X410000, [0, 0], 0.0);

        // the diagonal cell is its own component -> 2 polygons
        assert_eq!(polys.len(), 2, "expected big region + diagonal cell");
        let big = polys
            .iter()
            .find(|p| p.rings[0].len() > 4)
            .expect("big region present");
        assert_eq!(big.rings.len(), 2, "big region should have exterior + hole");
        assert!(signed_area(&big.rings[0]) > 0.0, "exterior must be CCW");
        assert!(signed_area(&big.rings[1]) < 0.0, "hole must be CW");
        // the exterior ring encloses all 25 cells; the hole is a separate ring of 1 cell
        assert_eq!(signed_area(&big.rings[0]), 25.0 * 100.0);
        assert_eq!(signed_area(&big.rings[1]), -100.0);
    }

    /// A C of cells whose tips touch diagonally, in all four rotations: its boundary is an
    /// exterior and a hole touching at the tips' shared corner, each ring through that
    /// corner once (a ring touching itself there is OGC-invalid).
    #[test]
    fn diagonal_touch_traces_an_exterior_and_a_hole() {
        // the C in a 3x3 block: every cell but the centre (the hole) and one corner
        let c = [(0, 0), (1, 0), (2, 0), (0, 1), (2, 1), (0, 2), (1, 2)];
        for rotation in 0..4 {
            let mut grid = Vec2D::new(5, 5, 0u8);
            for &(x, y) in &c {
                let (mut x, mut y) = (x as i64 - 1, y as i64 - 1);
                for _ in 0..rotation {
                    (x, y) = (-y, x);
                }
                grid[((x + 2) as usize, (y + 2) as usize)] = 1;
            }
            let polys = grid_to_polygons(&grid, (0.0, 0.0), 10.0, &|_| Code::X410000, [0, 0], 0.0);
            assert_eq!(polys.len(), 1, "rotation {rotation}");
            let rings = &polys[0].rings;
            assert_eq!(rings.len(), 2, "rotation {rotation}: {rings:?}");
            assert_eq!(signed_area(&rings[0]), 800.0, "rotation {rotation}");
            assert_eq!(signed_area(&rings[1]), -100.0, "rotation {rotation}");
            for ring in rings {
                let mut seen = std::collections::HashSet::new();
                for p in ring {
                    assert!(
                        seen.insert((p.x as i64, p.y as i64)),
                        "rotation {rotation}: {p:?} twice in {ring:?}"
                    );
                }
            }
        }
    }

    /// Two classes around a pinch: the C of class 1 from
    /// [`diagonal_touch_traces_an_exterior_and_a_hole`] with its gap cell class 2. With
    /// simplification on, the C's hole is point for point the gap's exterior.
    #[test]
    fn two_classes_share_the_edge_around_a_pinch() {
        let mut grid = Vec2D::new(5, 5, 0u8);
        for (x, y) in [(1, 1), (2, 1), (3, 1), (1, 2), (3, 2), (1, 3), (2, 3)] {
            grid[(x, y)] = 1;
        }
        grid[(2, 2)] = 2;
        let code_of = |c: u8| if c == 1 { Code::X410000 } else { Code::X406000 };
        let polys = grid_to_polygons(&grid, (0.0, 0.0), 20.0, &code_of, [0, 0], 2.0);
        let c = polys.iter().find(|p| p.value == 1).unwrap();
        let gap = polys.iter().find(|p| p.value == 2).unwrap();
        assert_eq!(c.rings.len(), 2, "{c:?}");
        let points = |ring: &[Point2]| -> std::collections::BTreeSet<(i64, i64)> {
            ring.iter().map(|p| cm([p.x, p.y])).collect()
        };
        assert_eq!(points(&c.rings[1]), points(&gap.rings[0]));
    }

    fn traced(pts: &[(f64, f64)], lattice: &[V]) -> Traced {
        Traced {
            pts: pts.iter().map(|&(x, y)| Point2::new(x, y)).collect(),
            keys: Vec::new(),
            lattice: lattice.to_vec(),
        }
    }

    #[test]
    fn invalid_rings_allows_touching_only_at_a_shared_lattice_vertex() {
        let to_world = |v: V| Point2::new(v.0 as f64, v.1 as f64);
        let exterior = traced(
            &[(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 10.0)],
            &[(0, 0), (10, 0), (10, 10), (0, 10)],
        );
        // a hole touching the exterior at the lattice vertex (0, 5) both pass through
        let touching = traced(&[(0.0, 5.0), (5.0, 2.0), (5.0, 8.0)], &[(0, 5), (5, 2)]);
        let mut exterior_through = traced(
            &[
                (0.0, 0.0),
                (10.0, 0.0),
                (10.0, 10.0),
                (0.0, 10.0),
                (0.0, 5.0),
            ],
            &[(0, 0), (10, 0), (10, 10), (0, 10), (0, 5)],
        );
        assert!(invalid_rings(&[exterior_through, touching], to_world).is_empty());
        // the same touch where the unsimplified rings do not meet
        exterior_through = traced(
            &[
                (0.0, 0.0),
                (10.0, 0.0),
                (10.0, 10.0),
                (0.0, 10.0),
                (0.0, 5.0),
            ],
            &[(0, 0), (10, 0), (10, 10), (0, 10)],
        );
        let touching = traced(&[(0.0, 5.0), (5.0, 2.0), (5.0, 8.0)], &[(1, 5), (5, 2)]);
        assert_eq!(
            invalid_rings(&[exterior_through, touching], to_world),
            [0, 1]
        );
        // a hole crossing the exterior, and a ring crossing itself
        let crossing = traced(&[(5.0, 5.0), (15.0, 5.0), (15.0, 8.0)], &[]);
        assert_eq!(invalid_rings(&[exterior, crossing], to_world), [0, 1]);
        let bow_tie = traced(&[(0.0, 0.0), (10.0, 10.0), (10.0, 0.0), (0.0, 20.0)], &[]);
        assert_eq!(invalid_rings(&[bow_tie], to_world), [0]);
    }

    /// Simplified coarsely, noisy grids give polygons whose chords cross or touch; the
    /// fallback to unsimplified chains leaves every polygon valid, its rings meeting
    /// only at lattice vertices.
    #[test]
    fn simplification_leaves_every_polygon_valid() {
        let mut fell_back = 0;
        for seed in 0..20usize {
            let mut grid = Vec2D::new(30, 30, 0u8);
            for x in 0..30usize {
                for y in 0..30usize {
                    grid[(x, y)] = ((x * 7 + y * 13 + x * y * (seed + 3)) % 5 % 3) as u8;
                }
            }
            let code_of = |c: u8| if c == 1 { Code::X410000 } else { Code::X408000 };
            let (polys, raw) = trace_polygons(&grid, (0.0, 0.0), 3.0, &code_of, [0, 0], 6.0);
            fell_back += raw;
            for p in &polys {
                let rings: Vec<Vec<Cm>> = p
                    .rings
                    .iter()
                    .map(|r| r.iter().chain(r.first()).map(|q| cm([q.x, q.y])).collect())
                    .collect();
                for (i, r) in rings.iter().enumerate() {
                    assert!(ring_is_simple(r), "seed {seed}: {r:?}");
                    for s in &rings[i + 1..] {
                        let touches = ring_contacts(r, s).expect("rings cross");
                        // the lattice is every 3 m
                        assert!(
                            touches.iter().all(|t| t.0 % 300 == 0 && t.1 % 300 == 0),
                            "seed {seed}: {touches:?}"
                        );
                    }
                }
            }
        }
        assert!(fell_back > 0, "no grid needed the fallback");
    }

    #[test]
    fn dissolve_small_patch() {
        let mut grid = Vec2D::new(10, 10, 0u8);
        // big class-1 region
        for x in 0..10 {
            for y in 0..5 {
                grid[(x, y)] = 1;
            }
        }
        // single-cell class-2 island inside it: 9 m² << min area, must dissolve into 1
        grid[(4, 2)] = 2;
        let code_of = |c: u8| -> Code {
            match c {
                1 => Code::X410000,
                _ => Code::X406000,
            }
        };
        let polys = grid_to_polygons(&grid, (0.0, 0.0), 3.0, &code_of, [0, 0], 0.0);
        assert_eq!(polys.len(), 1);
        assert_eq!(polys[0].code, Code::X410000);
        assert_eq!(polys[0].rings.len(), 1, "island dissolved, no hole");
    }

    /// Speckled multi-class grid: after the dissolve fixpoint no emitted polygon may be
    /// below its ISOM minimum area (the exact bug seen on real data with one pass).
    #[test]
    fn dissolve_reaches_fixpoint() {
        let mut grid = Vec2D::new(20, 20, 0u8);
        // deterministic speckle of classes 1..=3 over a solid class-1 base
        for x in 0..20 {
            for y in 0..12 {
                grid[(x, y)] = 1;
            }
        }
        for x in 0..20usize {
            for y in 12..20usize {
                grid[(x, y)] = ((x * 7 + y * 13) % 4) as u8; // 0..=3 speckle
            }
        }
        let code_of = |c: u8| -> Code {
            match c {
                1 => Code::X406000,
                2 => Code::X408000,
                _ => Code::X410000,
            }
        };
        // cell 3 m => 9 m² per cell, minimums are 225/110.25/64 m²
        let polys = grid_to_polygons(&grid, (0.0, 0.0), 3.0, &code_of, [0, 0], 0.0);
        assert!(!polys.is_empty());
        for p in &polys {
            let a = signed_area(&p.rings[0]);
            assert!(
                a >= min_area_m2(p.code),
                "polygon of code {} below minimum: {a} m²",
                p.code
            );
        }
    }

    /// Two adjacent classes with a jagged seam: after simplification + smoothing both
    /// polygons must contain the exact same vertices along the shared boundary (the
    /// white-sliver regression).
    #[test]
    fn adjacent_polygons_share_simplified_boundary() {
        let mut grid = Vec2D::new(12, 12, 0u8);
        for y in 0..12usize {
            let split = 6 + (y % 2); // jagged seam
            for x in 0..split {
                grid[(x, y)] = 1;
            }
            for x in split..12 {
                grid[(x, y)] = 2;
            }
        }
        let code_of = |c: u8| -> Code {
            match c {
                1 => Code::X406000,
                _ => Code::X410000,
            }
        };
        let polys = grid_to_polygons(&grid, (0.0, 0.0), 5.0, &code_of, [0, 0], 2.0);
        assert_eq!(polys.len(), 2);
        let seam = |rings: &Vec<Vec<Point2>>| -> std::collections::BTreeSet<(i64, i64)> {
            rings[0]
                .iter()
                .filter(|p| p.x > 29.9 && p.x < 35.1)
                .map(|p| ((p.x * 1000.0).round() as i64, (p.y * 1000.0).round() as i64))
                .collect()
        };
        let (a, b) = (seam(&polys[0].rings), seam(&polys[1].rings));
        assert!(a.len() >= 2, "seam vertices expected");
        assert_eq!(
            a, b,
            "shared boundary must be point-identical on both sides"
        );
    }

    #[test]
    fn simplify_keeps_shape() {
        // staircase square ~ 10x10 with unit steps
        let mut ring = Vec::new();
        for i in 0..10 {
            ring.push(Point2::new(i as f64, 0.0));
        }
        for i in 0..10 {
            ring.push(Point2::new(10.0, i as f64));
        }
        for i in 0..10 {
            ring.push(Point2::new(10.0 - i as f64, 10.0));
        }
        for i in 0..10 {
            ring.push(Point2::new(0.0, 10.0 - i as f64));
        }
        let simplified = simplify_closed(ring, 0.5);
        assert!(simplified.len() <= 8, "collinear points removed");
        let area = signed_area(&simplified).abs();
        assert!((area - 100.0).abs() < 1.0);
    }

    /// 6x6-cell blocks of three classes and background, sprinkled with single cells that
    /// must dissolve: the case where HashMap order used to change the output.
    fn speckle() -> Vec2D<u8> {
        let mut grid = Vec2D::new(42, 42, 0u8);
        for x in 0..42usize {
            for y in 0..42usize {
                let block = ((x / 6) * 5 + (y / 6) * 3) % 4;
                let noise = (x * 7 + y * 13 + x * y) % 11 == 0;
                grid[(x, y)] = (if noise { block + 1 } else { block } % 4) as u8;
            }
        }
        grid
    }

    fn speckle_code(c: u8) -> Code {
        match c {
            1 => Code::X406000,
            2 => Code::X408000,
            _ => Code::X410000,
        }
    }

    /// Same input, same output: every run builds fresh HashMaps with fresh random seeds,
    /// so repeated runs would disagree if iteration order leaked into the result.
    #[test]
    fn grid_to_polygons_is_deterministic() {
        let run = || grid_to_polygons(&speckle(), (0.0, 0.0), 3.0, &speckle_code, [0, 0], 2.0);
        let first = run();
        assert!(first.len() > 1);
        for _ in 0..5 {
            assert_eq!(run(), first);
        }
    }

    #[test]
    fn area_features_are_vegetation_polygons() {
        use crate::geojson::geojson_types::{
            FeatureGeometryType, FeatureProperties, GeoJsonOutput,
        };

        let polys = grid_to_polygons(&speckle(), (0.0, 0.0), 3.0, &speckle_code, [0, 0], 2.0);
        let fs = crate::io::fs::memory::MemoryFileSystem::new();
        let features = area_features(&polys, false).collect();
        geojson::write_tables(
            &fs,
            Path::new(""),
            geojson::Source::Vegetation,
            features,
            None,
        )
        .unwrap();

        let path = geojson::file_name(crate::isom::IsomTable::VegetationAreas);
        let value: serde_json::Value = serde_json::from_reader(fs.open(path).unwrap()).unwrap();
        let out: GeoJsonOutput = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(out.features.len(), polys.len());
        for (feature, p) in out.features.iter().zip(&polys) {
            assert_eq!(feature.geometry.type_, FeatureGeometryType::Polygon);
            let FeatureProperties::VegetationProperties(props) = &feature.properties else {
                panic!("not vegetation properties: {:?}", feature.properties);
            };
            assert_eq!(props.isom_code, p.code);
            assert!(props.shade.is_none());
            // GeoJSON rings are closed: one more position than the open ring
            assert_eq!(feature.geometry.coordinates.len(), p.rings.len());
            for (coords, ring) in feature.geometry.coordinates.iter().zip(&p.rings) {
                let coords = coords.as_array().unwrap();
                assert_eq!(coords.len(), ring.len() + 1);
                assert_eq!(coords.first(), coords.last());
            }
        }
        // only `isom_code` in the properties
        let props = &value["features"][0]["properties"];
        assert_eq!(props.as_object().unwrap().len(), 1, "{props}");
    }

    #[test]
    fn area_features_with_shade_add_the_greenshade_index() {
        use crate::geojson::geojson_types::FeatureProperties;

        let polys = grid_to_polygons(&speckle(), (0.0, 0.0), 3.0, &speckle_code, [0, 0], 2.0);
        let features: Vec<_> = area_features(&polys, true).collect();
        let value = serde_json::json!({ "features": features });
        assert_eq!(features.len(), polys.len());
        let mut shades = std::collections::BTreeSet::new();
        for (i, (feature, p)) in features.iter().zip(&polys).enumerate() {
            let FeatureProperties::VegetationProperties(props) = &feature.properties else {
                panic!("not vegetation properties: {:?}", feature.properties);
            };
            let shade = props.shade.expect("every green area has a shade").get();
            assert_eq!(shade, u64::from(p.value));
            assert_eq!(props.isom_code, speckle_code(p.value), "shade {shade}");
            // an integer in the JSON, not a string or a float
            assert!(value["features"][i]["properties"]["shade"].is_u64());
            shades.insert(shade);
        }
        assert_eq!(shades.into_iter().collect::<Vec<_>>(), [1, 2, 3]);
    }
}
