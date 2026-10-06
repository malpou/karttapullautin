//! Polygon ring checks for the combined export's curve smoothing, on the centimetre
//! grid the GeoJSON writer rounds to, so the check sees exactly what is written and
//! every predicate is exact integer arithmetic. No GEOS (ADR 0002).

/// A position on the centimetre grid.
pub(crate) type Cm = (i64, i64);

/// A position in metres, rounded to the centimetre grid.
pub(crate) fn cm([x, y]: [f64; 2]) -> Cm {
    ((x * 100.0).round() as i64, (y * 100.0).round() as i64)
}

/// A position on the centimetre grid, in metres.
pub(crate) fn metres((x, y): Cm) -> [f64; 2] {
    [x as f64 / 100.0, y as f64 / 100.0]
}

fn cross(o: Cm, a: Cm, b: Cm) -> i128 {
    (a.0 - o.0) as i128 * (b.1 - o.1) as i128 - (a.1 - o.1) as i128 * (b.0 - o.0) as i128
}

/// Whether `p`, collinear with `a`-`b`, lies within that segment's box.
fn within(p: Cm, a: Cm, b: Cm) -> bool {
    a.0.min(b.0) <= p.0 && p.0 <= a.0.max(b.0) && a.1.min(b.1) <= p.1 && p.1 <= a.1.max(b.1)
}

/// Whether segments `a`-`b` and `c`-`d` share any point.
fn meet(a: Cm, b: Cm, c: Cm, d: Cm) -> bool {
    let (d1, d2) = (cross(a, b, c), cross(a, b, d));
    let (d3, d4) = (cross(c, d, a), cross(c, d, b));
    if d1.signum() * d2.signum() < 0 && d3.signum() * d4.signum() < 0 {
        return true;
    }
    (d1 == 0 && within(c, a, b))
        || (d2 == 0 && within(d, a, b))
        || (d3 == 0 && within(a, c, d))
        || (d4 == 0 && within(b, c, d))
}

/// Whether the segment `b`-`c` folds back over `a`-`b`, the segment before it.
fn folds_back(a: Cm, b: Cm, c: Cm) -> bool {
    cross(a, b, c) == 0 && ((b.0 - a.0) * (c.0 - b.0) + (b.1 - a.1) * (c.1 - b.1)) < 0
}

/// Every pair of segments whose boxes overlap, `(i, j)` with i < j (sort and sweep on x).
fn candidate_pairs(segs: &[(Cm, Cm)]) -> Vec<(usize, usize)> {
    let mut order: Vec<usize> = (0..segs.len()).collect();
    order.sort_by_key(|&i| segs[i].0.0.min(segs[i].1.0));
    let mut active: Vec<usize> = Vec::new();
    let mut pairs = Vec::new();
    for &i in &order {
        let (a, b) = segs[i];
        let minx = a.0.min(b.0);
        active.retain(|&j| segs[j].0.0.max(segs[j].1.0) >= minx);
        let (ylo, yhi) = (a.1.min(b.1), a.1.max(b.1));
        for &j in &active {
            let (c, d) = segs[j];
            if c.1.min(d.1) <= yhi && c.1.max(d.1) >= ylo {
                pairs.push((i.min(j), i.max(j)));
            }
        }
        active.push(i);
    }
    pairs
}

fn segments(ring: &[Cm]) -> Vec<(Cm, Cm)> {
    ring.windows(2).map(|w| (w[0], w[1])).collect()
}

/// Twice the signed area of a closed ring (positive counter-clockwise).
pub(crate) fn area2(ring: &[Cm]) -> i128 {
    ring.windows(2)
        .map(|w| w[0].0 as i128 * w[1].1 as i128 - w[1].0 as i128 * w[0].1 as i128)
        .sum()
}

/// Whether a closed ring (first position repeated last, no position repeated next to
/// itself) is simple: four or more positions, an area, and no two segments meeting but
/// neighbours at their shared vertex.
pub(crate) fn ring_is_simple(ring: &[Cm]) -> bool {
    if ring.len() < 4 || area2(ring) == 0 {
        return false;
    }
    let segs = segments(ring);
    let n = segs.len();
    let neighbours = |i: usize, j: usize| j == i + 1 || (i == 0 && j == n - 1);
    candidate_pairs(&segs).into_iter().all(|(i, j)| {
        let ((a, b), (c, d)) = (segs[i], segs[j]);
        if neighbours(i, j) {
            // they share b = c (or d = a when wrapping); only folding back is wrong
            if j == i + 1 {
                !folds_back(a, b, d)
            } else {
                !folds_back(c, d, b)
            }
        } else {
            !meet(a, b, c, d)
        }
    })
}

/// Whether two closed rings share any point.
#[cfg(test)]
pub(crate) fn rings_meet(r: &[Cm], s: &[Cm]) -> bool {
    let mut segs = segments(r);
    let split = segs.len();
    segs.extend(segments(s));
    candidate_pairs(&segs).into_iter().any(|(i, j)| {
        i < split && j >= split && {
            let ((a, b), (c, d)) = (segs[i], segs[j]);
            meet(a, b, c, d)
        }
    })
}

/// Whether `p`, off the ring's boundary, is inside the closed ring.
pub(crate) fn inside(p: Cm, ring: &[Cm]) -> bool {
    let mut inside = false;
    for w in ring.windows(2) {
        let (a, b) = (w[0], w[1]);
        if (a.1 > p.1) != (b.1 > p.1) && (cross(a, b, p) > 0) == (b.1 > a.1) {
            inside = !inside;
        }
    }
    inside
}

/// A ring's box: `[minx, miny, maxx, maxy]`.
type Bbox = [i64; 4];

fn bbox(ring: &[Cm]) -> Bbox {
    ring.iter()
        .fold([i64::MAX, i64::MAX, i64::MIN, i64::MIN], |b, p| {
            [b[0].min(p.0), b[1].min(p.1), b[2].max(p.0), b[3].max(p.1)]
        })
}

fn boxes_overlap(a: &Bbox, b: &Bbox) -> bool {
    a[0] <= b[2] && b[0] <= a[2] && a[1] <= b[3] && b[1] <= a[3]
}

/// Where two closed rings meet: the points where they touch, or None when they cross
/// or share a stretch.
pub(crate) fn ring_contacts(r: &[Cm], s: &[Cm]) -> Option<Vec<Cm>> {
    let mut segs = segments(r);
    let split = segs.len();
    segs.extend(segments(s));
    let mut touches = Vec::new();
    for (i, j) in candidate_pairs(&segs) {
        if i >= split || j < split {
            continue;
        }
        let ((a, b), (c, d)) = (segs[i], segs[j]);
        if !meet(a, b, c, d) {
            continue;
        }
        let (d1, d2) = (cross(a, b, c), cross(a, b, d));
        if d1 == 0 && d2 == 0 {
            // collinear: a stretch unless they share only an endpoint
            let shared: Vec<Cm> = [c, d]
                .into_iter()
                .filter(|&p| within(p, a, b))
                .chain([a, b].into_iter().filter(|&p| within(p, c, d)))
                .collect();
            if shared.iter().any(|&p| p != shared[0]) {
                return None;
            }
            touches.push(shared[0]);
            continue;
        }
        // one point: an endpoint of one on the other, or a crossing
        let at = [(c, d1 == 0), (d, d2 == 0)]
            .into_iter()
            .chain([(a, cross(c, d, a) == 0), (b, cross(c, d, b) == 0)])
            .find_map(|(p, on)| on.then_some(p));
        touches.push(at?);
    }
    touches.sort_unstable();
    touches.dedup();
    Some(touches)
}

/// The rings of a Polygon (exterior first, each closed) as published: each ring's
/// `candidate` (a reshaped version, such as a smoothed curve) where the polygon stays
/// valid with it, its `original` where not. A candidate is given up when it is not
/// simple, or when it meets another ring (two originals may still touch, as the input
/// had them), a hole leaves the exterior or enters another hole: then the later ring's
/// candidate (a hole's before the exterior's) is given up. The check repeats until no
/// candidate in use conflicts, which ends at the originals at worst; after the first
/// pass only the pairs with a ring given up in the pass before are checked again, since
/// the others are unchanged. Deterministic: which candidates survive depends only on
/// the rings.
///
/// Each pass finds the meeting rings with one sweep over every segment, and the boxes
/// are computed once, so a pass is about O(N log N) in the positions plus a
/// point-in-ring test per hole.
pub(crate) fn keep_valid(original: &[Vec<Cm>], candidate: &[Vec<Cm>]) -> Vec<Vec<Cm>> {
    let n = original.len();
    let mut take: Vec<bool> = (0..n)
        .map(|i| candidate[i] != original[i] && ring_is_simple(&candidate[i]))
        .collect();
    let boxes = |rings: &[Vec<Cm>]| -> Vec<Bbox> { rings.iter().map(|r| bbox(r)).collect() };
    let (original_box, candidate_box) = (boxes(original), boxes(candidate));
    // every ring counts as changed for the first pass
    let mut changed = vec![true; n];
    loop {
        let ring = |i: usize| -> &[Cm] { if take[i] { &candidate[i] } else { &original[i] } };
        let rbox = |i: usize| {
            if take[i] {
                &candidate_box[i]
            } else {
                &original_box[i]
            }
        };
        // a pair to check: a candidate in it, and changed since the last pass
        let check = |i: usize, j: usize| (take[i] || take[j]) && (changed[i] || changed[j]);
        let mut give_up = vec![false; n];
        let mut conflict = |i: usize, j: usize| {
            let (i, j) = (i.min(j), i.max(j));
            // the later ring first: a hole before the exterior
            give_up[if take[j] { j } else { i }] = true;
        };

        // rings that meet, from one sweep over every segment
        let mut segs = Vec::new();
        let mut owner = Vec::new();
        for i in 0..n {
            for s in segments(ring(i)) {
                segs.push(s);
                owner.push(i);
            }
        }
        let mut meeting = std::collections::BTreeSet::new();
        for (a, b) in candidate_pairs(&segs) {
            let (i, j) = (owner[a], owner[b]);
            if i != j && check(i, j) && !meeting.contains(&(i.min(j), i.max(j))) {
                let ((p, q), (r, s)) = (segs[a], segs[b]);
                if meet(p, q, r, s) {
                    meeting.insert((i.min(j), i.max(j)));
                }
            }
        }
        for &(i, j) in &meeting {
            conflict(i, j);
        }

        // rings apart, so any vertex tells where one ring is in the other
        for j in 1..n {
            if check(0, j)
                && !meeting.contains(&(0, j))
                && (!boxes_overlap(rbox(0), rbox(j)) || !inside(ring(j)[0], ring(0)))
            {
                conflict(0, j);
            }
        }
        for i in 1..n {
            for j in i + 1..n {
                if check(i, j)
                    && !meeting.contains(&(i, j))
                    && boxes_overlap(rbox(i), rbox(j))
                    && (inside(ring(j)[0], ring(i)) || inside(ring(i)[0], ring(j)))
                {
                    conflict(i, j);
                }
            }
        }

        if !give_up.contains(&true) {
            return (0..n).map(|i| ring(i).to_vec()).collect();
        }
        for i in 0..n {
            take[i] &= !give_up[i];
        }
        changed = give_up;
    }
}

/// An axis-aligned box on the cm grid: `[minx, miny, maxx, maxy]`.
pub(crate) type CmBox = [i64; 4];

/// The part of the closed segment `a`-`b` in the box, as parameters `t0 < t1` along it,
/// when that part runs through the box's interior (not just along or across its edge).
fn segment_in_box(a: Cm, b: Cm, bx: &CmBox) -> Option<(f64, f64)> {
    let (ax, ay) = (a.0 as f64, a.1 as f64);
    let (dx, dy) = ((b.0 - a.0) as f64, (b.1 - a.1) as f64);
    let (mut t0, mut t1) = (0.0f64, 1.0f64);
    for (p, q) in [
        (-dx, ax - bx[0] as f64),
        (dx, bx[2] as f64 - ax),
        (-dy, ay - bx[1] as f64),
        (dy, bx[3] as f64 - ay),
    ] {
        if p == 0.0 {
            if q < 0.0 {
                return None;
            }
        } else if p < 0.0 {
            t0 = t0.max(q / p);
        } else {
            t1 = t1.min(q / p);
        }
    }
    if t0 >= t1 {
        return None;
    }
    let (mx, my) = (ax + (t0 + t1) / 2.0 * dx, ay + (t0 + t1) / 2.0 * dy);
    let interior =
        (bx[0] as f64) < mx && mx < (bx[2] as f64) && (bx[1] as f64) < my && my < (bx[3] as f64);
    interior.then_some((t0, t1))
}

/// The point at `t` along `a`-`b`, on the cm grid: `a` and `b` themselves at 0 and 1.
fn point_at(a: Cm, b: Cm, t: f64, bx: &CmBox) -> Cm {
    if t == 0.0 {
        return a;
    }
    if t == 1.0 {
        return b;
    }
    let x = a.0 as f64 + t * (b.0 - a.0) as f64;
    let y = a.1 as f64 + t * (b.1 - a.1) as f64;
    // on the box's edge, within it
    (
        (x.round() as i64).clamp(bx[0], bx[2]),
        (y.round() as i64).clamp(bx[1], bx[3]),
    )
}

fn strictly_inside(p: Cm, bx: &CmBox) -> bool {
    bx[0] < p.0 && p.0 < bx[2] && bx[1] < p.1 && p.1 < bx[3]
}

/// Where a point on the box's edge is along it, counter-clockwise from the lower left.
fn perimeter_at(p: Cm, bx: &CmBox) -> i64 {
    let (w, h) = (bx[2] - bx[0], bx[3] - bx[1]);
    if p.1 == bx[1] {
        p.0 - bx[0]
    } else if p.0 == bx[2] {
        w + p.1 - bx[1]
    } else if p.1 == bx[3] {
        w + h + bx[2] - p.0
    } else {
        2 * w + h + bx[3] - p.1
    }
}

/// A fraction `num / den` (den not 0).
type Frac = (i128, i128);

fn frac_cmp((a, b): Frac, (c, d): Frac) -> std::cmp::Ordering {
    let sign = (b * d).signum();
    (a * d * sign).cmp(&(c * b * sign))
}

fn sub(a: Cm, b: Cm) -> Cm {
    (a.0 - b.0, a.1 - b.1)
}

/// Where a chain meets the box's edge, before rounding: its place along the edge
/// (counter-clockwise from the lower left, in cm) and the direction of the ring's
/// segment there.
struct End {
    s: f64,
    offset: Frac,
}

impl End {
    fn at(a: Cm, b: Cm, t: f64, rounded: Cm, bx: &CmBox) -> Self {
        let (x, y) = (
            a.0 as f64 + t * (b.0 - a.0) as f64,
            a.1 as f64 + t * (b.1 - a.1) as f64,
        );
        let (w, h) = ((bx[2] - bx[0]) as f64, (bx[3] - bx[1]) as f64);
        // the edge the rounded point is on
        let s = if rounded.1 == bx[1] {
            x - bx[0] as f64
        } else if rounded.0 == bx[2] {
            w + y - bx[1] as f64
        } else if rounded.1 == bx[3] {
            w + h + bx[2] as f64 - x
        } else {
            2.0 * w + h + bx[3] as f64 - y
        };
        Self {
            s,
            offset: edge_offset(rounded, sub(b, a), bx),
        }
    }

    /// How `self` and `other` (two ends that round to one point) are ordered
    /// counter-clockwise along the edge: by their unrounded places, and where those
    /// are one point (a vertex on the edge), by where they meet a box shrunk by a hair.
    fn order(&self, other: &End, perimeter: i64) -> std::cmp::Ordering {
        let p = perimeter as f64;
        // the difference, wrapped to the half lap around it
        let d = (self.s - other.s + p / 2.0).rem_euclid(p) - p / 2.0;
        if d.abs() > 1e-6 {
            d.total_cmp(&0.0)
        } else {
            frac_cmp(self.offset, other.offset)
        }
    }

    /// Whether this entry comes after the `exit` it rounds onto.
    fn after(&self, exit: &End, perimeter: i64) -> bool {
        self.order(exit, perimeter).is_gt()
    }
}

/// Where a chain through `p` on the box's edge, heading `d` there, meets that edge
/// moved a hair inwards, relative to `p`, in units of the hair, counter-clockwise
/// along the edge: (d . t) / (d . n) for the edge's direction t and inward normal n.
/// An exit and an entry at one point are ordered by it, as on a box shrunk by a hair,
/// where they are apart: a polygon touching the edge from inside at a vertex then
/// leaves two lobes that meet there, and a hole touching it there the box around it.
fn edge_offset(p: Cm, d: Cm, bx: &CmBox) -> Frac {
    let (t, n) = if p.1 == bx[1] {
        ((1, 0), (0, 1))
    } else if p.0 == bx[2] {
        ((0, 1), (-1, 0))
    } else if p.1 == bx[3] {
        ((-1, 0), (0, -1))
    } else {
        ((0, -1), (1, 0))
    };
    let along = d.0 as i128 * t.0 as i128 + d.1 as i128 * t.1 as i128;
    let into = d.0 as i128 * n.0 as i128 + d.1 as i128 * n.1 as i128;
    if into == 0 { (0, 1) } else { (along, into) }
}

/// Clip a valid Polygon (closed rings on the cm grid, exterior first) to an
/// axis-aligned box: the Polygons of their intersection, each exterior first, the
/// exterior counter-clockwise and the holes clockwise, valid by construction (up to the
/// cm rounding of the points where a ring crosses the box's edge).
///
/// A Weiler-Atherton clip against a convex window. With every ring oriented so the area
/// is on its left, each ring that crosses the box's interior falls into chains, each
/// from a point on the box's edge through the interior to another; a ring with no
/// interior part is outside (or encloses the box), and one all inside stays whole.
/// From the end of a chain, the area runs counter-clockwise along the box's edge to the
/// next chain's start, so following that walk links the chains, with the box corners
/// passed, into the exteriors: a polygon that leaves the box and comes back becomes
/// separate polygons instead of a ring bridged along the edge, and a hole across the
/// edge becomes a notch in its exterior. The holes left whole go to the exterior that
/// holds them. Without chains the box is the exterior where the polygon covers it, and
/// so it is when only holes cross and none of them makes an exterior lobe (a hole
/// touching the edge at a point closes on itself there).
///
/// An exit and an entry that meet the edge at one point (a vertex on the edge, or two
/// crossings that round to one cm) are ordered as unrounded, and at a vertex as on a
/// box shrunk by a hair ([`edge_offset`]): a polygon touching the edge from inside
/// there leaves two lobes, a hole touching it there a box around it.
/// A walk along the edge that passes another chain's end passes through that point,
/// so the ring is written through it twice and split there: a polygon touching the
/// edge from inside at a vertex, while it crosses elsewhere, becomes two polygons
/// touching at that vertex. Crossing points are rounded to the cm grid; two less than
/// a cm apart can merge or swap, which the writer's checks catch (a merged pair is a
/// repeated position, dropped).
pub(crate) fn clip_polygon(rings: &[Vec<Cm>], bx: &CmBox) -> Vec<Vec<Vec<Cm>>> {
    let mut chains: Vec<Vec<Cm>> = Vec::new();
    // each chain's entry and exit, unrounded (see [`End`])
    let mut ends_of: Vec<(End, End)> = Vec::new();
    let mut exterior_inside: Option<Vec<Cm>> = None;
    let mut holes_inside: Vec<Vec<Cm>> = Vec::new();
    let mut outside: Vec<(bool, Vec<Cm>)> = Vec::new();
    let mut exterior_crosses = false;
    for (k, ring) in node(rings).into_iter().enumerate() {
        let mut ring = ring;
        ring.dedup();
        if ring.len() < 4 || area2(&ring) == 0 {
            if k == 0 {
                return Vec::new();
            }
            continue;
        }
        // the area on the left: exterior counter-clockwise, holes clockwise
        if (area2(&ring) > 0) != (k == 0) {
            ring.reverse();
        }
        let open = &ring[..ring.len() - 1];
        let Some(start) = open.iter().position(|&p| !strictly_inside(p, bx)) else {
            if k == 0 {
                exterior_inside = Some(ring);
            } else {
                holes_inside.push(ring);
            }
            continue;
        };
        let n = open.len();
        let mut chain: Vec<Cm> = Vec::new();
        let mut entered: Option<End> = None;
        let mut any = false;
        for i in 0..n {
            let (a, b) = (open[(start + i) % n], open[(start + i + 1) % n]);
            let Some((t0, t1)) = segment_in_box(a, b, bx) else {
                continue;
            };
            any = true;
            let p0 = point_at(a, b, t0, bx);
            if chain.is_empty() || !strictly_inside(p0, bx) {
                // enters the interior here
                chain = vec![p0];
                entered = Some(End::at(a, b, t0, p0, bx));
            }
            let p1 = point_at(a, b, t1, bx);
            chain.push(p1);
            if !strictly_inside(p1, bx) {
                chains.push(std::mem::take(&mut chain));
                let entry = entered.take().expect("a chain starts on the edge");
                ends_of.push((entry, End::at(a, b, t1, p1, bx)));
            }
        }
        if any {
            exterior_crosses |= k == 0;
        } else {
            outside.push((k == 0, ring));
        }
    }
    if let Some(exterior) = exterior_inside {
        // the polygon is within the box; its holes are too
        let mut polygon = vec![exterior];
        polygon.extend(holes_inside);
        return vec![polygon];
    }

    // whether the rings that stay off the box's interior leave the box in the area:
    // the exterior around it and no such hole around it
    let (cx, cy) = ((bx[0] + bx[2]) as f64 / 2.0, (bx[1] + bx[3]) as f64 / 2.0);
    let holds_centre = |ring: &[Cm]| {
        let mut inside = false;
        for w in ring.windows(2) {
            let ((ax, ay), (bx_, by)) = (
                (w[0].0 as f64, w[0].1 as f64),
                (w[1].0 as f64, w[1].1 as f64),
            );
            if (ay > cy) != (by > cy) && cx < ax + (cy - ay) * (bx_ - ax) / (by - ay) {
                inside = !inside;
            }
        }
        inside
    };
    let covered = !exterior_crosses
        && outside.iter().any(|(exterior, _)| *exterior)
        && outside
            .iter()
            .all(|(exterior, ring)| holds_centre(ring) == *exterior);
    let the_box = vec![
        (bx[0], bx[1]),
        (bx[2], bx[1]),
        (bx[2], bx[3]),
        (bx[0], bx[3]),
        (bx[0], bx[1]),
    ];

    let mut exteriors: Vec<Vec<Cm>> = Vec::new();
    if chains.is_empty() {
        // no ring crosses the interior: the box is in the polygon or apart from it
        if !covered {
            return Vec::new();
        }
        exteriors.push(the_box);
    } else {
        let perimeter = 2 * (bx[2] - bx[0] + bx[3] - bx[1]);
        let corners = [
            (0, (bx[0], bx[1])),
            (bx[2] - bx[0], (bx[2], bx[1])),
            (bx[2] - bx[0] + bx[3] - bx[1], (bx[2], bx[3])),
            (2 * (bx[2] - bx[0]) + bx[3] - bx[1], (bx[0], bx[3])),
        ];
        let ahead = |from: i64, to: i64| (to - from).rem_euclid(perimeter);
        let entry: Vec<i64> = chains.iter().map(|c| perimeter_at(c[0], bx)).collect();

        // every chain end on the edge: a walk along the edge that passes one passes
        // through it, so the ring visits that point again and is split there below
        let ends: Vec<(i64, Cm)> = chains
            .iter()
            .flat_map(|c| [c[0], *c.last().unwrap()])
            .map(|p| (perimeter_at(p, bx), p))
            .collect();
        let mut used = vec![false; chains.len()];
        for first in 0..chains.len() {
            if used[first] {
                continue;
            }
            used[first] = true;
            let mut ring: Vec<Cm> = chains[first].clone();
            let mut cur = first;
            loop {
                let exit = perimeter_at(*chains[cur].last().unwrap(), bx);
                let exit_end = &ends_of[cur].1;
                // how far counter-clockwise along the edge an entry is: where both
                // round to one point, ahead only when it comes after the exit
                // unrounded ([`End::after`]), else a whole lap round
                let gap_to = |c: usize| match ahead(exit, entry[c]) {
                    0 if !ends_of[c].0.after(exit_end, perimeter) => perimeter,
                    d => d,
                };
                // the next chain start counter-clockwise along the edge (the first
                // chain closes the ring)
                let next = (0..chains.len())
                    .filter(|&c| !used[c] || c == first)
                    .min_by(|&a, &b| {
                        gap_to(a)
                            .cmp(&gap_to(b))
                            .then_with(|| ends_of[a].0.order(&ends_of[b].0, perimeter))
                            .then(a.cmp(&b))
                    })
                    .unwrap();
                let gap = gap_to(next);
                let mut passed: Vec<(i64, Cm)> = corners
                    .iter()
                    .chain(&ends)
                    .map(|&(s, p)| (ahead(exit, s), p))
                    .filter(|&(d, _)| 0 < d && d < gap)
                    .collect();
                passed.sort_unstable();
                passed.dedup();
                ring.extend(passed.into_iter().map(|(_, p)| p));
                if next == first {
                    ring.push(ring[0]);
                    break;
                }
                used[next] = true;
                ring.extend(chains[next].iter().copied());
                cur = next;
            }
            ring.dedup();
            // a hole that touched its exterior and became a notch pinches the area
            // at the touch point: the ring passes it twice. Split there: a
            // counter-clockwise loop is an exterior, a clockwise one a hole.
            for lobe in split_pinches(ring) {
                if lobe.len() < 4 || area2(&lobe) == 0 {
                    continue;
                }
                if area2(&lobe) > 0 {
                    exteriors.push(lobe);
                } else {
                    holes_inside.push(lobe);
                }
            }
        }
        // only holes cross, each closing on itself where it touches the edge (or
        // collapsing): the exterior around the box is still the box, which those
        // holes touch at a point
        if exteriors.is_empty() && covered {
            exteriors.push(the_box);
        }
    }

    let mut polygons: Vec<Vec<Vec<Cm>>> = exteriors.into_iter().map(|e| vec![e]).collect();
    for hole in holes_inside {
        // a vertex off the exterior's boundary tells which exterior holds the hole
        let holder = polygons.iter().position(|p| {
            hole.iter()
                .find(|&&v| !on_ring(v, &p[0]))
                .is_some_and(|&v| inside(v, &p[0]))
        });
        if let Some(i) = holder {
            polygons[i].push(hole);
        }
    }
    polygons
}

/// The rings with every point where one touches another's segment between its
/// vertices made a vertex of that segment too, so a touch point is a vertex of both
/// rings and a ring the clip joins through it passes it twice ([`split_pinches`]).
fn node(rings: &[Vec<Cm>]) -> Vec<Vec<Cm>> {
    let mut segs = Vec::new();
    let mut at = Vec::new();
    for (r, ring) in rings.iter().enumerate() {
        for (k, s) in segments(ring).into_iter().enumerate() {
            segs.push(s);
            at.push((r, k));
        }
    }
    // the points to insert into each segment
    let mut extra: std::collections::HashMap<(usize, usize), Vec<Cm>> = Default::default();
    for (i, j) in candidate_pairs(&segs) {
        if at[i].0 == at[j].0 {
            continue;
        }
        for (into, (p, q), from) in [(i, segs[i], segs[j]), (j, segs[j], segs[i])] {
            for v in [from.0, from.1] {
                if v != p && v != q && cross(p, q, v) == 0 && within(v, p, q) {
                    extra.entry(at[into]).or_default().push(v);
                }
            }
        }
    }
    rings
        .iter()
        .enumerate()
        .map(|(r, ring)| {
            let mut out = Vec::with_capacity(ring.len());
            for (k, w) in ring.windows(2).enumerate() {
                out.push(w[0]);
                if let Some(points) = extra.get_mut(&(r, k)) {
                    let d = |p: &Cm| (p.0 - w[0].0).abs() + (p.1 - w[0].1).abs();
                    points.sort_by_key(d);
                    points.dedup();
                    out.extend(points.iter().copied());
                }
            }
            out.extend(ring.last());
            out
        })
        .collect()
}

/// The simple loops of a closed ring that passes some point more than once, split at
/// those points (a ring that passes each point once is its only loop).
fn split_pinches(ring: Vec<Cm>) -> Vec<Vec<Cm>> {
    let mut open = ring;
    open.pop();
    let mut loops = Vec::new();
    let mut seen: std::collections::HashMap<Cm, usize> = std::collections::HashMap::new();
    let mut i = 0;
    while i < open.len() {
        match seen.get(&open[i]) {
            Some(&start) => {
                // open[start..i] is a loop through open[start] = open[i]
                let mut lobe: Vec<Cm> = open.drain(start..i).collect();
                lobe.push(lobe[0]);
                for p in &lobe[1..lobe.len() - 1] {
                    seen.remove(p);
                }
                loops.push(lobe);
                i = start + 1;
            }
            None => {
                seen.insert(open[i], i);
                i += 1;
            }
        }
    }
    if let Some(&first) = open.first() {
        open.push(first);
        loops.push(open);
    }
    loops
}

/// Whether `p` lies on the closed ring's boundary.
fn on_ring(p: Cm, ring: &[Cm]) -> bool {
    ring.windows(2)
        .any(|w| cross(w[0], w[1], p) == 0 && within(p, w[0], w[1]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ring(pts: &[(i64, i64)]) -> Vec<Cm> {
        let mut r = pts.to_vec();
        r.push(pts[0]);
        r
    }

    const SQUARE: [(i64, i64); 4] = [(0, 0), (100, 0), (100, 100), (0, 100)];

    #[test]
    fn simple_rings() {
        assert!(ring_is_simple(&ring(&SQUARE)));
        // a bow tie crosses itself
        assert!(!ring_is_simple(&ring(&[
            (0, 0),
            (100, 100),
            (100, 0),
            (0, 200)
        ])));
        // two squares walked as one ring touch at (100, 100)
        assert!(!ring_is_simple(&ring(&[
            (0, 0),
            (100, 0),
            (100, 100),
            (200, 100),
            (200, 200),
            (100, 200),
            (100, 100),
            (0, 100),
        ])));
        // a spike folds back on itself
        assert!(!ring_is_simple(&ring(&[
            (0, 0),
            (100, 0),
            (150, 0),
            (100, 0),
            (100, 100),
            (0, 100)
        ])));
        // collinear vertices are fine
        assert!(ring_is_simple(&ring(&[
            (0, 0),
            (50, 0),
            (100, 0),
            (100, 100),
            (0, 100)
        ])));
        assert!(!ring_is_simple(&ring(&[(0, 0), (100, 0), (50, 0)])));
    }

    #[test]
    fn inside_and_meeting() {
        let square = ring(&SQUARE);
        assert!(inside((50, 50), &square));
        assert!(!inside((150, 50), &square));
        let touching = ring(&[(100, 50), (150, 0), (150, 100)]);
        assert!(rings_meet(&square, &touching));
        let apart = ring(&[(101, 50), (150, 0), (150, 100)]);
        assert!(!rings_meet(&square, &apart));
    }

    #[test]
    fn keep_valid_takes_candidates_that_keep_the_polygon_valid() {
        let shell = ring(&SQUARE);
        let hole = ring(&[(20, 20), (20, 80), (80, 80), (80, 20)]);
        // a bigger, still valid exterior and a smaller hole are taken
        let grown = ring(&[(-10, -10), (110, -10), (110, 110), (-10, 110)]);
        let shrunk = ring(&[(30, 30), (30, 70), (70, 70), (70, 30)]);
        assert_eq!(
            keep_valid(
                &[shell.clone(), hole.clone()],
                &[grown.clone(), shrunk.clone()]
            ),
            [grown.clone(), shrunk]
        );
        // a hole that would cross the exterior is given up, and the exterior kept
        let crossing = ring(&[(20, 20), (20, 120), (80, 120), (80, 20)]);
        assert_eq!(
            keep_valid(&[shell.clone(), hole.clone()], &[grown.clone(), crossing]),
            [grown, hole.clone()]
        );
        // a self-crossing exterior is given up
        let bow_tie = ring(&[(0, 0), (100, 100), (100, 0), (0, 200)]);
        assert_eq!(
            keep_valid(std::slice::from_ref(&shell), &[bow_tie]),
            std::slice::from_ref(&shell)
        );
        // a hole that would leave a shrunk exterior gives both up
        let small = ring(&[(0, 0), (10, 0), (10, 10), (0, 10)]);
        assert_eq!(
            keep_valid(&[shell.clone(), hole.clone()], &[small, hole.clone()]),
            [shell, hole]
        );
    }

    #[test]
    fn keep_valid_lets_original_rings_touch() {
        // the input's hole touches its exterior at (0, 50): allowed, as given
        let shell = ring(&SQUARE);
        let hole = ring(&[(0, 50), (50, 80), (50, 20)]);
        let moved = ring(&[(10, 50), (50, 80), (50, 20)]);
        assert_eq!(
            keep_valid(
                &[shell.clone(), hole.clone()],
                &[shell.clone(), moved.clone()]
            ),
            [shell.clone(), moved]
        );
        // a smoothed exterior must not touch the original hole
        let through = ring(&[(0, 0), (100, 0), (100, 100), (0, 100), (0, 50)]);
        assert_eq!(
            keep_valid(&[shell.clone(), hole.clone()], &[through, hole.clone()]),
            [shell, hole]
        );
    }

    const BOX: CmBox = [0, 0, 100, 100];

    /// Check the clipped polygons are valid within `bx`: simple rings, exterior
    /// counter-clockwise, holes clockwise and inside it, two rings meeting at one point
    /// at most (OGC lets a hole touch the exterior or another hole at a point); return
    /// each one's area x 2 and ring count.
    fn valid_in(polygons: &[Vec<Vec<Cm>>], bx: &CmBox) -> Vec<(i128, usize)> {
        for p in polygons {
            for (k, r) in p.iter().enumerate() {
                assert!(ring_is_simple(r), "{p:?}");
                assert_eq!(area2(r) > 0, k == 0, "{p:?}");
                assert!(
                    r.iter().all(|&(x, y)| (bx[0]..=bx[2]).contains(&x)
                        && (bx[1]..=bx[3]).contains(&y)),
                    "{p:?}"
                );
                for s in &p[k + 1..] {
                    let touches = ring_contacts(r, s).unwrap_or_else(|| panic!("{p:?}"));
                    assert!(touches.len() <= 1, "{p:?}");
                }
                if k > 0 {
                    let v = r.iter().find(|&&v| !on_ring(v, &p[0])).unwrap();
                    assert!(inside(*v, &p[0]), "{p:?}");
                }
            }
        }
        polygons
            .iter()
            .map(|p| (p.iter().map(|r| area2(r)).sum(), p.len()))
            .collect()
    }

    fn checked(polygons: &[Vec<Vec<Cm>>]) -> Vec<(i128, usize)> {
        valid_in(polygons, &BOX)
    }

    #[test]
    fn clip_splits_where_a_vertex_touches_the_edge_from_inside() {
        // the ring dips to (50, 0) on the box's lower edge between two arms that
        // cross it: two triangles touching at (50, 0)
        let w = ring(&[(10, -50), (90, -50), (90, 80), (50, 0), (10, 80)]);
        let polygons = clip_polygon(&[w], &BOX);
        assert_eq!(checked(&polygons), [(3200, 1), (3200, 1)], "{polygons:?}");
    }

    #[test]
    fn clip_keeps_the_box_around_holes_touching_its_edge() {
        let around = ring(&[(-10, -10), (110, -10), (110, 110), (-10, 110)]);
        // a hole with a vertex on the box's edge: the box less the hole, touching it
        let touching = ring(&[(50, 0), (40, 20), (60, 20)]);
        let polygons = clip_polygon(&[around.clone(), touching], &BOX);
        assert_eq!(checked(&polygons), [(20000 - 400, 2)], "{polygons:?}");
        // a sliver of a hole whose two crossings round to one point: about the box
        let sliver = ring(&[(49, -100), (51, -100), (50, 5)]);
        let polygons = clip_polygon(&[around, sliver], &BOX);
        let parts = checked(&polygons);
        assert_eq!(parts.len(), 1, "{polygons:?}");
        assert!(parts[0].0 >= 20000 - 20, "{polygons:?}");
    }

    /// A small deterministic generator (an LCG), so the fuzz test is the same each run.
    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self, n: i64) -> i64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((self.0 >> 33) % n as u64) as i64
        }
    }

    /// A star-shaped ring on a lattice of `step` around `centre`: vertices at sorted
    /// angles, so counter-clockwise; None when the snapping leaves it not simple.
    fn star(rng: &mut Lcg, centre: Cm, radius: i64, step: i64) -> Option<Vec<Cm>> {
        let k = 3 + rng.next(7) as usize;
        let mut angles: Vec<i64> = (0..k).map(|_| rng.next(3600)).collect();
        angles.sort_unstable();
        let mut r: Vec<Cm> = angles
            .iter()
            .map(|&a| {
                let (t, d) = (
                    a as f64 / 3600.0 * std::f64::consts::TAU,
                    (radius / 4 + rng.next(radius)) as f64,
                );
                let snap = |v: f64| (v / step as f64).round() as i64 * step;
                (
                    snap(centre.0 as f64 + d * t.cos()),
                    snap(centre.1 as f64 + d * t.sin()),
                )
            })
            .collect();
        r.dedup();
        r.push(r[0]);
        r.dedup();
        (r.len() >= 4 && area2(&r) > 0 && ring_is_simple(&r)).then_some(r)
    }

    /// Twice the area of a ring's part in the box, by Sutherland-Hodgman (right for the
    /// area even where it bridges along an edge), in floating point.
    fn clipped_area2(ring: &[Cm], bx: &CmBox) -> f64 {
        let mut pts: Vec<[f64; 2]> = ring[..ring.len() - 1]
            .iter()
            .map(|p| [p.0 as f64, p.1 as f64])
            .collect();
        for edge in 0..4 {
            let keep = |p: &[f64; 2]| match edge {
                0 => p[0] >= bx[0] as f64,
                1 => p[0] <= bx[2] as f64,
                2 => p[1] >= bx[1] as f64,
                _ => p[1] <= bx[3] as f64,
            };
            let cut = |a: &[f64; 2], b: &[f64; 2]| -> [f64; 2] {
                let (axis, v) = match edge {
                    0 => (0, bx[0]),
                    1 => (0, bx[2]),
                    2 => (1, bx[1]),
                    _ => (1, bx[3]),
                };
                let t = (v as f64 - a[axis]) / (b[axis] - a[axis]);
                [a[0] + t * (b[0] - a[0]), a[1] + t * (b[1] - a[1])]
            };
            let input = std::mem::take(&mut pts);
            for i in 0..input.len() {
                let (prev, cur) = (input[(i + input.len() - 1) % input.len()], input[i]);
                match (keep(&prev), keep(&cur)) {
                    (true, true) => pts.push(cur),
                    (false, true) => pts.extend([cut(&prev, &cur), cur]),
                    (true, false) => pts.push(cut(&prev, &cur)),
                    (false, false) => {}
                }
            }
        }
        (0..pts.len())
            .map(|i| {
                let (a, b) = (pts[i], pts[(i + 1) % pts.len()]);
                a[0] * b[1] - b[0] * a[1]
            })
            .sum::<f64>()
            .abs()
    }

    /// Lattice polygons with holes, clipped to boxes on the lattice (vertices and edges
    /// on the box's edge, as vegetation cell lines on a tile edge) and off it by half a
    /// step (crossings in general position): every result is valid, holds the area the
    /// clip should keep, and is not empty when the polygon covers the box's centre. The
    /// lattice is 5-10 m, so no two crossings fall within the cm the clip rounds to.
    #[test]
    fn clip_fuzz_lattice_polygons() {
        let mut rng = Lcg(517);
        let boxes: [CmBox; 4] = [
            [0, 0, 10000, 10000],
            [0, 0, 6000, 10000],
            [250, 250, 9750, 9750],
            [-250, 750, 10250, 9750],
        ];
        let mut cases = 0;
        while cases < 3000 {
            let step = [500, 1000][rng.next(2) as usize];
            let centre = (rng.next(19) * 1000 - 4000, rng.next(19) * 1000 - 4000);
            let Some(exterior) = star(&mut rng, centre, 12000, step) else {
                continue;
            };
            let mut rings = vec![exterior];
            for _ in 0..rng.next(3) {
                let at = (
                    centre.0 + rng.next(9) * 1000 - 4000,
                    centre.1 + rng.next(9) * 1000 - 4000,
                );
                let Some(mut hole) = star(&mut rng, at, 4000, step) else {
                    continue;
                };
                let fits = ring_contacts(&rings[0], &hole).is_some_and(|t| t.len() <= 1)
                    && hole
                        .iter()
                        .find(|&&v| !on_ring(v, &rings[0]))
                        .is_some_and(|&v| inside(v, &rings[0]))
                    && rings[1..].iter().all(|h| {
                        ring_contacts(h, &hole) == Some(vec![])
                            && !inside(hole[0], h)
                            && !inside(h[0], &hole)
                    });
                if fits {
                    hole.reverse();
                    rings.push(hole);
                }
            }
            cases += 1;
            for bx in &boxes {
                let polygons = clip_polygon(&rings, bx);
                let got: i128 = valid_in(&polygons, bx).iter().map(|(a, _)| a).sum();
                let want = clipped_area2(&rings[0], bx)
                    - rings[1..].iter().map(|h| clipped_area2(h, bx)).sum::<f64>();
                // each crossing rounds by up to half a cm along an edge at most a
                // box's width (a lost or extra lobe is far larger)
                let slack =
                    (bx[2] - bx[0]) as f64 * rings.iter().map(Vec::len).sum::<usize>() as f64;
                assert!(
                    (got as f64 - want).abs() <= slack,
                    "area {got} for {want}: {rings:?} in {bx:?}: {polygons:?}"
                );
                let centre = ((bx[0] + bx[2]) / 2, (bx[1] + bx[3]) / 2);
                let covers = !on_ring(centre, &rings[0])
                    && inside(centre, &rings[0])
                    && rings[1..]
                        .iter()
                        .all(|h| !on_ring(centre, h) && !inside(centre, h));
                if covers {
                    assert!(!polygons.is_empty(), "{rings:?} in {bx:?}");
                }
            }
        }
    }

    #[test]
    fn clip_splits_a_u_whose_base_leaves_the_box() {
        // the base is below the box, the arms come up into it: two polygons, not one
        // ring bridged along the box's lower edge
        let u = ring(&[
            (10, -50),
            (90, -50),
            (90, 90),
            (70, 90),
            (70, -20),
            (30, -20),
            (30, 90),
            (10, 90),
        ]);
        assert_eq!(
            checked(&clip_polygon(&[u], &BOX)),
            [(2 * 1800, 1), (2 * 1800, 1)]
        );
    }

    #[test]
    fn clip_makes_a_hole_across_the_edge_a_notch() {
        let exterior = ring(&[(10, 10), (90, 10), (90, 150), (10, 150)]);
        let hole = ring(&[(40, 80), (40, 120), (60, 120), (60, 80)]);
        assert_eq!(
            checked(&clip_polygon(&[exterior, hole], &BOX)),
            [(2 * (80 * 90 - 20 * 20), 1)]
        );
    }

    #[test]
    fn clip_splits_where_a_notch_meets_the_exterior() {
        // the hole touches the exterior's left side at (10, 50) and crosses the top:
        // as a notch it cuts the area in two, meeting at (10, 50)
        let exterior = ring(&[(10, 10), (90, 10), (90, 150), (10, 150)]);
        let hole = ring(&[(10, 50), (60, 130), (60, 40)]);
        let polygons = clip_polygon(&[exterior, hole], &BOX);
        let parts = checked(&polygons);
        assert_eq!(parts.len(), 2, "{polygons:?}");
        assert!(parts.iter().all(|&(_, rings)| rings == 1));
    }

    #[test]
    fn split_pinches_parts_a_figure_eight() {
        let eight = ring(&[
            (0, 0),
            (10, 0),
            (10, 10),
            (20, 10),
            (20, 20),
            (10, 20),
            (10, 10),
            (0, 10),
        ]);
        let loops = split_pinches(eight);
        assert_eq!(loops.len(), 2);
        assert!(loops.iter().all(|l| ring_is_simple(l) && area2(l) == 200));
    }

    #[test]
    fn clip_drops_a_hole_outside_and_keeps_one_inside() {
        let exterior = ring(&[(10, 10), (90, 10), (90, 150), (10, 150)]);
        let outside = ring(&[(40, 110), (40, 130), (60, 130), (60, 110)]);
        let within = ring(&[(40, 40), (40, 60), (60, 60), (60, 40)]);
        assert_eq!(
            checked(&clip_polygon(&[exterior, outside, within], &BOX)),
            [(2 * (80 * 90 - 20 * 20), 2)]
        );
    }

    #[test]
    fn clip_keeps_a_polygon_inside_whole() {
        // given clockwise: oriented, otherwise unchanged
        let mut exterior = ring(&[(10, 10), (90, 10), (90, 90), (10, 90)]);
        exterior.reverse();
        let hole = ring(&[(40, 40), (60, 40), (60, 60), (40, 60)]);
        let clipped = clip_polygon(&[exterior.clone(), hole.clone()], &BOX);
        assert_eq!(checked(&clipped), [(2 * (6400 - 400), 2)]);
        exterior.reverse();
        let mut hole_cw = hole;
        hole_cw.reverse();
        assert_eq!(clipped, [vec![exterior, hole_cw]]);
    }

    #[test]
    fn clip_of_a_polygon_covering_the_box_is_the_box() {
        let exterior = ring(&[(-10, -10), (110, -10), (110, 110), (-10, 110)]);
        let hole = ring(&[(40, 40), (40, 60), (60, 60), (60, 40)]);
        assert_eq!(
            checked(&clip_polygon(&[exterior.clone(), hole], &BOX)),
            [(2 * (10000 - 400), 2)]
        );
        // a hole around the box leaves nothing, and so does a polygon apart from it
        let around = ring(&[(-5, -5), (-5, 105), (105, 105), (105, -5)]);
        assert!(clip_polygon(&[exterior, around], &BOX).is_empty());
        let apart = ring(&[(200, 200), (300, 200), (300, 300), (200, 300)]);
        assert!(clip_polygon(&[apart], &BOX).is_empty());
    }

    #[test]
    fn clip_follows_edges_along_the_box() {
        // the left half of the box, sharing three of its edges
        let half = ring(&[(0, 0), (50, 0), (50, 100), (0, 100)]);
        assert_eq!(checked(&clip_polygon(&[half], &BOX)), [(2 * 5000, 1)]);
        // the whole box
        assert_eq!(
            checked(&clip_polygon(&[ring(&SQUARE)], &BOX)),
            [(2 * 10000, 1)]
        );
        // a polygon touching the box's edge from inside at one vertex
        let touch = ring(&[(20, 20), (50, 0), (80, 20), (50, 60)]);
        assert_eq!(checked(&clip_polygon(&[touch], &BOX)).len(), 1);
    }
}
