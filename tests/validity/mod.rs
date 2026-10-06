//! A pure-Rust OGC simple-feature validity check for the GeoJSON the jobs write (ADR
//! 0002 keeps GEOS out). Positions are compared exactly on the centimetre grid the
//! writer rounds to, so every predicate is integer arithmetic.
//!
//! A Polygon is valid when every ring is simple (no two segments meet but neighbours at
//! their shared vertex, none folds back on its neighbour), the rings meet each other only
//! at isolated points, every hole lies inside the exterior and outside the other holes,
//! and those touch points leave the interior connected.

use std::collections::HashMap;

/// A position on the centimetre grid.
pub type P = (i64, i64);

/// A position in metres, on the centimetre grid.
pub fn cm([x, y]: [f64; 2]) -> P {
    ((x * 100.0).round() as i64, (y * 100.0).round() as i64)
}

// Written apart from `src/validity.rs` on purpose, with other formulas (parametric
// intersection, the trapezoid area, the winding number) and another pair search (a
// uniform grid of buckets, not a sweep), so a slip in one does not hide in both.

type V2 = (i128, i128);

fn sub(a: P, b: P) -> V2 {
    ((a.0 - b.0) as i128, (a.1 - b.1) as i128)
}

fn perp(u: V2, v: V2) -> i128 {
    u.0 * v.1 - u.1 * v.0
}

fn dot(u: V2, v: V2) -> i128 {
    u.0 * v.0 + u.1 * v.1
}

/// How two segments meet.
#[derive(Debug, PartialEq)]
enum Meet {
    None,
    /// The interiors cross at one point.
    Cross,
    /// They share a stretch of positive length.
    Overlap,
    /// They meet at one point that is an endpoint of at least one of them.
    Touch(P),
}

/// Segments p + t r and q + u s, t and u in [0, 1].
fn meet(p: P, p2: P, q: P, q2: P) -> Meet {
    let (r, s, qp) = (sub(p2, p), sub(q2, q), sub(q, p));
    let denom = perp(r, s);
    if denom == 0 {
        if perp(qp, r) != 0 {
            return Meet::None; // parallel, apart
        }
        // collinear: overlap of [0, |r|^2] and the projections of q and q2 onto r
        let rr = dot(r, r);
        let (t0, t1) = (dot(qp, r), dot(sub(q2, p), r));
        let (lo, hi) = (t0.min(t1).max(0), t0.max(t1).min(rr));
        return match lo.cmp(&hi) {
            std::cmp::Ordering::Greater => Meet::None,
            std::cmp::Ordering::Less => Meet::Overlap,
            std::cmp::Ordering::Equal => Meet::Touch(if lo == 0 {
                p
            } else if lo == rr {
                p2
            } else if lo == t0 {
                q
            } else {
                q2
            }),
        };
    }
    // t = (qp x s) / denom, u = (qp x r) / denom, both in [0, 1]
    let (mut tn, mut un, mut d) = (perp(qp, s), perp(qp, r), denom);
    if d < 0 {
        (tn, un, d) = (-tn, -un, -d);
    }
    if tn < 0 || tn > d || un < 0 || un > d {
        return Meet::None;
    }
    if 0 < tn && tn < d && 0 < un && un < d {
        return Meet::Cross;
    }
    Meet::Touch(match () {
        _ if tn == 0 => p,
        _ if tn == d => p2,
        _ if un == 0 => q,
        _ => q2,
    })
}

/// Every pair of segments sharing a bucket of a uniform 10 m grid: `(i, j)` with i < j.
fn candidate_pairs(segs: &[(P, P)]) -> Vec<(usize, usize)> {
    const BUCKET: i64 = 1000;
    let mut buckets: HashMap<(i64, i64), Vec<usize>> = HashMap::new();
    for (i, (a, b)) in segs.iter().enumerate() {
        for bx in a.0.min(b.0).div_euclid(BUCKET)..=a.0.max(b.0).div_euclid(BUCKET) {
            for by in a.1.min(b.1).div_euclid(BUCKET)..=a.1.max(b.1).div_euclid(BUCKET) {
                buckets.entry((bx, by)).or_default().push(i);
            }
        }
    }
    let mut pairs = std::collections::BTreeSet::new();
    for members in buckets.values() {
        for (k, &i) in members.iter().enumerate() {
            for &j in &members[k + 1..] {
                pairs.insert((i.min(j), i.max(j)));
            }
        }
    }
    pairs.into_iter().collect()
}

/// Twice the signed area of a closed ring (positive counter-clockwise), by trapezoids.
pub fn area2(ring: &[P]) -> i128 {
    -ring
        .windows(2)
        .map(|w| (w[1].0 - w[0].0) as i128 * (w[1].1 + w[0].1) as i128)
        .sum::<i128>()
}

/// Where `p` is with respect to a closed ring.
#[derive(Debug, PartialEq)]
enum Side {
    Inside,
    Outside,
    Boundary,
}

/// By the winding number.
fn side(p: P, ring: &[P]) -> Side {
    let mut winding = 0;
    for w in ring.windows(2) {
        let (a, b) = (w[0], w[1]);
        let turn = perp(sub(b, a), sub(p, a));
        if turn == 0 && dot(sub(p, a), sub(p, b)) <= 0 {
            return Side::Boundary;
        }
        if a.1 <= p.1 && b.1 > p.1 && turn > 0 {
            winding += 1;
        } else if a.1 > p.1 && b.1 <= p.1 && turn < 0 {
            winding -= 1;
        }
    }
    if winding != 0 {
        Side::Inside
    } else {
        Side::Outside
    }
}

/// Why a Polygon (closed rings, exterior first, on the cm grid) is not OGC-valid, or
/// None when it is. Ring shape (closure, size, orientation) is [`ring_shape`]'s.
pub fn invalid_reason(rings: &[Vec<P>]) -> Option<String> {
    // every segment, tagged with its ring and its index in the ring
    let mut segs = Vec::new();
    let mut tags = Vec::new();
    for (r, ring) in rings.iter().enumerate() {
        if ring.len() < 4 || area2(ring) == 0 {
            return Some(format!("ring {r} is degenerate"));
        }
        for (k, w) in ring.windows(2).enumerate() {
            segs.push((w[0], w[1]));
            tags.push((r, k));
        }
    }
    // rings and touch points as one graph: a cycle splits the interior
    let mut touches: HashMap<(usize, usize, P), ()> = HashMap::new();
    for (i, j) in candidate_pairs(&segs) {
        let ((ri, ki), (rj, kj)) = (tags[i], tags[j]);
        let (a, b) = segs[i];
        let (c, d) = segs[j];
        let m = meet(a, b, c, d);
        if ri == rj {
            let n = rings[ri].len() - 1;
            let next = |x: usize, y: usize| (x + 1) % n == y;
            if next(ki, kj) || next(kj, ki) {
                // neighbours share one vertex; anything more folds the ring back
                if m == Meet::Overlap {
                    return Some(format!("ring {ri} folds back at {a:?}"));
                }
                continue;
            }
            if m != Meet::None {
                return Some(format!("ring {ri} self-intersects near {a:?} ({m:?})"));
            }
        } else {
            match m {
                Meet::None => {}
                Meet::Touch(p) => {
                    touches.insert((ri.min(rj), ri.max(rj), p), ());
                }
                _ => return Some(format!("rings {ri} and {rj} cross near {a:?} ({m:?})")),
            }
        }
    }
    // holes inside the exterior and outside each other: every vertex off the other
    // ring's boundary decides (rings that cross were caught above)
    let bbox = |ring: &[P]| {
        ring.iter()
            .fold((i64::MAX, i64::MAX, i64::MIN, i64::MIN), |b, p| {
                (b.0.min(p.0), b.1.min(p.1), b.2.max(p.0), b.3.max(p.1))
            })
    };
    let boxes: Vec<_> = rings.iter().map(|r| bbox(r)).collect();
    for h in 1..rings.len() {
        if rings[h]
            .iter()
            .any(|&p| side(p, &rings[0]) == Side::Outside)
        {
            return Some(format!("hole {h} is outside the exterior"));
        }
        for o in 1..rings.len() {
            let (a, b) = (boxes[h], boxes[o]);
            if o == h || a.2 < b.0 || b.2 < a.0 || a.3 < b.1 || b.3 < a.1 {
                continue;
            }
            if rings[h].iter().any(|&p| side(p, &rings[o]) == Side::Inside) {
                return Some(format!("hole {h} is inside hole {o}"));
            }
        }
    }
    // union-find over rings and touch points
    let mut parent: Vec<usize> = (0..rings.len()).collect();
    let mut point_node: HashMap<P, usize> = HashMap::new();
    fn find(parent: &mut [usize], i: usize) -> usize {
        let mut i = i;
        while parent[i] != i {
            parent[i] = parent[parent[i]];
            i = parent[i];
        }
        i
    }
    let mut touches: Vec<_> = touches.into_keys().collect();
    touches.sort();
    for (ri, rj, p) in touches {
        let node = *point_node.entry(p).or_insert_with(|| {
            parent.push(parent.len());
            parent.len() - 1
        });
        for r in [ri, rj] {
            let (x, y) = (find(&mut parent, r), find(&mut parent, node));
            if x == y {
                return Some(format!("the rings touching at {p:?} split the interior"));
            }
            parent[x] = y;
        }
    }
    None
}

/// What RFC 7946 and the sinks want of a ring's shape: closed, four or more positions,
/// no position repeated next to itself, exterior counter-clockwise and holes clockwise.
pub fn ring_shape(ring: &[P], exterior: bool) -> Option<String> {
    if ring.len() < 4 {
        return Some(format!("{} positions", ring.len()));
    }
    if ring.first() != ring.last() {
        return Some("open".into());
    }
    if let Some(w) = ring.windows(2).find(|w| w[0] == w[1]) {
        return Some(format!("repeats {:?}", w[0]));
    }
    let a = area2(ring);
    match (exterior, a > 0) {
        (true, false) => Some("exterior is clockwise".into()),
        (false, true) => Some("hole is counter-clockwise".into()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ring(pts: &[(i64, i64)]) -> Vec<P> {
        let mut r = pts.to_vec();
        r.push(pts[0]);
        r
    }

    const SQUARE: [(i64, i64); 4] = [(0, 0), (10, 0), (10, 10), (0, 10)];

    #[test]
    fn a_square_with_a_hole_is_valid() {
        let hole = ring(&[(2, 2), (2, 8), (8, 8), (8, 2)]);
        assert_eq!(invalid_reason(&[ring(&SQUARE), hole]), None);
    }

    #[test]
    fn a_bow_tie_crosses_itself() {
        let r = ring(&[(0, 0), (10, 10), (10, 0), (0, 20)]);
        assert!(invalid_reason(&[r]).unwrap().contains("self-intersects"));
    }

    #[test]
    fn a_ring_touching_itself_at_a_vertex_is_invalid() {
        // two squares joined at (10, 10), walked as one ring
        let r = ring(&[
            (0, 0),
            (10, 0),
            (10, 10),
            (20, 10),
            (20, 20),
            (10, 20),
            (10, 10),
            (0, 10),
        ]);
        assert!(invalid_reason(&[r]).is_some());
    }

    #[test]
    fn a_hole_may_touch_the_exterior_at_one_point() {
        let hole = ring(&[(0, 5), (5, 8), (5, 2)]);
        assert_eq!(invalid_reason(&[ring(&SQUARE), hole]), None);
        // ... but not at two: the interior falls apart
        let split = ring(&[(0, 5), (10, 5), (5, 2)]);
        assert!(invalid_reason(&[ring(&SQUARE), split]).is_some());
    }

    #[test]
    fn holes_outside_or_crossing_or_nested_are_invalid() {
        let outside = ring(&[(20, 20), (20, 30), (30, 30), (30, 20)]);
        assert!(invalid_reason(&[ring(&SQUARE), outside]).is_some());
        let crossing = ring(&[(5, 5), (5, 15), (8, 15), (8, 5)]);
        assert!(invalid_reason(&[ring(&SQUARE), crossing]).is_some());
        let big = ring(&[(1, 1), (1, 9), (9, 9), (9, 1)]);
        let small = ring(&[(3, 3), (3, 6), (6, 6), (6, 3)]);
        assert!(invalid_reason(&[ring(&SQUARE), big, small]).is_some());
    }

    #[test]
    fn a_spike_folds_back() {
        let r = ring(&[(0, 0), (10, 0), (15, 0), (10, 0), (10, 10), (0, 10)]);
        assert!(invalid_reason(&[r]).is_some());
    }

    #[test]
    fn ring_shape_wants_closed_counter_clockwise_exteriors() {
        assert_eq!(ring_shape(&ring(&SQUARE), true), None);
        let mut cw = ring(&SQUARE);
        cw.reverse();
        assert!(ring_shape(&cw, true).is_some());
        assert_eq!(ring_shape(&cw, false), None);
        let mut dup = ring(&SQUARE);
        dup.insert(1, (0, 0));
        assert!(ring_shape(&dup, true).unwrap().contains("repeats"));
    }
}
