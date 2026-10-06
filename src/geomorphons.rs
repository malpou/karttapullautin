//! Geomorphon landform classification of the ground model.
//!
//! Jasiewicz and Stepinski (2013), "Geomorphons — a pattern recognition approach to
//! classification and mapping of landforms", Geomorphology 182, 147-156,
//! <https://doi.org/10.1016/j.geomorph.2012.11.005>. The reference implementation is
//! GRASS GIS `r.geomorphon` (by the first author).
//!
//! For every cell, eight lines of sight (E, NE, N, NW, W, SW, S, SE) run outwards up to
//! the search radius. Along each, the steepest elevation angle up (zenith side) and the
//! steepest angle down (nadir side) seen from the cell decide a ternary value: the
//! terrain in that direction is higher, lower or level within the flatness threshold.
//! The eight values form the cell's [`Pattern`]; the counts of higher and lower
//! directions pick one of ten [`Landform`]s from the paper's lookup table.
//!
//! What is implemented:
//! - The ternary rule is the paper's: with zenith angle φ = 90° − (max elevation angle)
//!   and nadir angle ψ = 90° + (min elevation angle), a direction is higher when
//!   ψ − φ > t, lower when φ − ψ > t, and level otherwise. GRASS's later default
//!   (comparing |zenith| with |nadir|) is not used.
//! - Search and skip radii are ground distances in metres, converted with the ground
//!   model's cell size. Each direction steps cell by cell (diagonals step √2 cells),
//!   so a line of sight takes every cell whose centre lies within the search radius and
//!   beyond the skip radius: the search area is a circle, not a square.
//! - The flatness distance of GRASS (lowering t at long range) is not implemented: the
//!   search radii this crate needs are short, where it makes no difference.
//!
//! Edges and gaps:
//! - Lines of sight stop at the grid edge, so cells near it see shorter lines. A
//!   direction with no cell left in it (a border cell facing out) is level. Border
//!   cells therefore lean towards [`Landform::Flat`] and the classes with few
//!   committed directions; callers classify a padded ground model and use its core.
//! - A NaN cell along a line of sight is skipped; the line goes on past it.
//! - A NaN cell itself gets the all-level pattern, so [`Landform::Flat`].
//!
//! Cost: cells × 8 × (search radius / cell size) height reads, single-threaded and
//! deterministic.

use crate::io::heightmap::HeightMap;
use crate::vec2d::Vec2D;

/// The ten geomorphon landform classes (Jasiewicz and Stepinski 2013, Fig. 4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Landform {
    Flat,
    Peak,
    Ridge,
    Shoulder,
    Spur,
    Slope,
    Hollow,
    Footslope,
    Valley,
    Pit,
}

/// The eight line-of-sight directions as grid steps `(dx, dy)`, in [`Pattern`] bit
/// order. The ground model's `y` grows northwards, so `(0, 1)` is north.
pub const DIRECTIONS: [(isize, isize); 8] = [
    (1, 0),   // E
    (1, 1),   // NE
    (0, 1),   // N
    (-1, 1),  // NW
    (-1, 0),  // W
    (-1, -1), // SW
    (0, -1),  // S
    (1, -1),  // SE
];

/// Parameters of the geomorphon classifier.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GeomorphonParams {
    /// Length of each line of sight, in metres (the paper's L). It sets the scale of the
    /// landforms found: 20 m is 2 mm at 1:10 000, about the size of the spurs and
    /// hollows a form line describes.
    pub search_radius_m: f64,
    /// Cells at or closer than this, in metres, are left out of every line of sight,
    /// so small bumps next to the cell do not decide its form. 0 uses every cell.
    pub skip_radius_m: f64,
    /// Flatness threshold t in degrees: a direction whose ψ − φ is within ±t is level.
    /// 1° is the paper's and GRASS's default.
    pub flat_deg: f64,
}

impl Default for GeomorphonParams {
    fn default() -> Self {
        Self {
            search_radius_m: 20.0,
            skip_radius_m: 0.0,
            flat_deg: 1.0,
        }
    }
}

/// A cell's ternary pattern: for each of the [`DIRECTIONS`], whether the terrain that
/// way is lower, higher or level. Bit `i` of `lower` or `higher` is set for direction
/// `i`; a direction with neither bit set is level. Never both.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct Pattern {
    pub lower: u8,
    pub higher: u8,
}

impl Pattern {
    /// Number of directions in which the terrain is lower.
    pub fn lower_count(self) -> u32 {
        self.lower.count_ones()
    }

    /// Number of directions in which the terrain is higher.
    pub fn higher_count(self) -> u32 {
        self.higher.count_ones()
    }

    /// The landform for this pattern, from the paper's lookup table.
    pub fn landform(self) -> Landform {
        Landform::from_counts(self.lower_count(), self.higher_count())
            .expect("a pattern has at most eight committed directions")
    }
}

impl Landform {
    /// The landform for `lower` lower and `higher` higher directions, by the lookup
    /// table of Jasiewicz and Stepinski (2013, Fig. 4) as GRASS `r.geomorphon` encodes
    /// it. `None` when the counts add up to more than eight.
    pub fn from_counts(lower: u32, higher: u32) -> Option<Landform> {
        const FL: Landform = Landform::Flat;
        const PK: Landform = Landform::Peak;
        const RI: Landform = Landform::Ridge;
        const SH: Landform = Landform::Shoulder;
        const SP: Landform = Landform::Spur;
        const SL: Landform = Landform::Slope;
        const HL: Landform = Landform::Hollow;
        const FS: Landform = Landform::Footslope;
        const VL: Landform = Landform::Valley;
        const PT: Landform = Landform::Pit;
        // Rows: lower count; columns: higher count. Cells right of the anti-diagonal
        // (lower + higher > 8) cannot occur and are never read; they hold FL.
        const FORMS: [[Landform; 9]; 9] = [
            /* 0 */ [FL, FL, FL, FS, FS, VL, VL, VL, PT],
            /* 1 */ [FL, FL, FS, FS, FS, VL, VL, VL, FL],
            /* 2 */ [FL, SH, SL, SL, HL, HL, VL, FL, FL],
            /* 3 */ [SH, SH, SL, SL, SL, HL, FL, FL, FL],
            /* 4 */ [SH, SH, SP, SL, SL, FL, FL, FL, FL],
            /* 5 */ [RI, RI, SP, SP, FL, FL, FL, FL, FL],
            /* 6 */ [RI, RI, RI, FL, FL, FL, FL, FL, FL],
            /* 7 */ [RI, RI, FL, FL, FL, FL, FL, FL, FL],
            /* 8 */ [PK, FL, FL, FL, FL, FL, FL, FL, FL],
        ];
        if lower + higher > 8 {
            return None;
        }
        Some(FORMS[lower as usize][higher as usize])
    }
}

/// The ternary pattern of every cell of `ground`. Deterministic, no IO.
pub fn patterns(ground: &HeightMap, params: &GeomorphonParams) -> Vec2D<Pattern> {
    let grid = &ground.grid;
    let (w, h) = (grid.width() as isize, grid.height() as isize);
    let flat = params.flat_deg.to_radians();

    // Per direction: ground length of one step, first and last step of the line.
    let reach: [(f64, isize, isize); 8] = DIRECTIONS.map(|(dx, dy)| {
        let step = ground.scale * ((dx * dx + dy * dy) as f64).sqrt();
        // The epsilon keeps a radius that is an exact multiple of the step inclusive.
        let steps = |m: f64| (m / step + 1e-9).floor().max(0.0) as isize;
        (
            step,
            steps(params.skip_radius_m) + 1,
            steps(params.search_radius_m),
        )
    });

    let mut out = Vec2D::new(grid.width(), grid.height(), Pattern::default());
    for (x, y, pattern) in out.iter_mut() {
        let z0 = grid[(x, y)];
        if z0.is_nan() {
            continue;
        }
        for (i, (&(dx, dy), &(step, first, last))) in DIRECTIONS.iter().zip(&reach).enumerate() {
            // Gradients (rise over run) instead of angles: atan is monotonic, so it is
            // taken once per direction, not once per cell.
            let mut max_grad = f64::NEG_INFINITY;
            let mut min_grad = f64::INFINITY;
            for k in first..=last {
                let (cx, cy) = (x as isize + k * dx, y as isize + k * dy);
                if cx < 0 || cy < 0 || cx >= w || cy >= h {
                    break;
                }
                let z = grid[(cx as usize, cy as usize)];
                if z.is_nan() {
                    continue;
                }
                let grad = (z - z0) / (k as f64 * step);
                max_grad = max_grad.max(grad);
                min_grad = min_grad.min(grad);
            }
            if max_grad == f64::NEG_INFINITY {
                continue; // no cell in this direction: level
            }
            // ψ − φ = (90° + min elevation angle) − (90° − max elevation angle)
            let nadir_minus_zenith = min_grad.atan() + max_grad.atan();
            if nadir_minus_zenith > flat {
                pattern.higher |= 1 << i;
            } else if nadir_minus_zenith < -flat {
                pattern.lower |= 1 << i;
            }
        }
    }
    out
}

/// The geomorphon landform of every cell of `ground`. Deterministic, no IO.
pub fn classify(ground: &HeightMap, params: &GeomorphonParams) -> Vec2D<Landform> {
    let patterns = patterns(ground, params);
    let mut out = Vec2D::new(patterns.width(), patterns.height(), Landform::Flat);
    for (x, y, landform) in out.iter_mut() {
        *landform = patterns[(x, y)].landform();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const N: usize = 161;
    const C: usize = N / 2;

    /// An `N`×`N` ground model with 1 m cells; `z` takes the offset from the centre
    /// cell in metres (x east, y north).
    fn ground(z: impl Fn(f64, f64) -> f64) -> HeightMap {
        let mut grid = Vec2D::new(N, N, 0.0);
        for (x, y, v) in grid.iter_mut() {
            *v = z(x as f64 - C as f64, y as f64 - C as f64);
        }
        HeightMap {
            xoffset: 0.0,
            yoffset: 0.0,
            scale: 1.0,
            grid,
        }
    }

    fn params(radius_m: f64) -> GeomorphonParams {
        GeomorphonParams {
            search_radius_m: radius_m,
            ..GeomorphonParams::default()
        }
    }

    fn at(map: &Vec2D<Landform>, dx: isize, dy: isize) -> Landform {
        map[((C as isize + dx) as usize, (C as isize + dy) as usize)]
    }

    #[test]
    fn flat_plane_is_flat() {
        let map = classify(&ground(|_, _| 100.0), &params(5.0));
        assert!(map.iter().all(|(_, _, l)| l == Landform::Flat));
    }

    #[test]
    fn tilted_plane_is_slope() {
        let map = classify(&ground(|x, y| 0.3 * x + 0.1 * y), &params(5.0));
        for (dx, dy) in [(0, 0), (-8, 3), (7, -6)] {
            assert_eq!(at(&map, dx, dy), Landform::Slope, "at ({dx}, {dy})");
        }
    }

    #[test]
    fn cone_is_peak_at_apex_and_slope_on_flanks() {
        // Flank cells 60 m out with a 4 m search radius: the cone's plan curvature
        // across the line of sight stays under the 1° threshold (a sharper cone or a
        // longer radius reads the flank as a spur, which it also is).
        let map = classify(&ground(|x, y| -0.25 * x.hypot(y)), &params(4.0));
        assert_eq!(at(&map, 0, 0), Landform::Peak);
        for (dx, dy) in [(60, 0), (0, -60), (-60, 0)] {
            assert_eq!(at(&map, dx, dy), Landform::Slope, "at ({dx}, {dy})");
        }
    }

    #[test]
    fn inverted_cone_is_pit_at_bottom() {
        let map = classify(&ground(|x, y| 0.25 * x.hypot(y)), &params(4.0));
        assert_eq!(at(&map, 0, 0), Landform::Pit);
        assert_eq!(at(&map, 60, 0), Landform::Slope);
    }

    #[test]
    fn prism_is_ridge_along_crest_and_slope_on_flanks() {
        let map = classify(&ground(|x, _| -0.5 * x.abs()), &params(5.0));
        for dy in [-10, 0, 10] {
            assert_eq!(at(&map, 0, dy), Landform::Ridge, "crest at y {dy}");
            assert_eq!(at(&map, 10, dy), Landform::Slope, "east flank at y {dy}");
            assert_eq!(at(&map, -10, dy), Landform::Slope, "west flank at y {dy}");
        }
    }

    #[test]
    fn trough_is_valley_along_floor() {
        let map = classify(&ground(|_, y| 0.5 * y.abs()), &params(5.0));
        for dx in [-10, 0, 10] {
            assert_eq!(at(&map, dx, 0), Landform::Valley, "floor at x {dx}");
            assert_eq!(at(&map, dx, 10), Landform::Slope, "north flank at x {dx}");
        }
    }

    #[test]
    fn step_is_shoulder_above_and_footslope_below() {
        // Level at 10 m west of x = -5, a 1:1 slope down to level 0 east of x = 5.
        let map = classify(&ground(|x, _| 5.0 - x.clamp(-5.0, 5.0)), &params(4.0));
        assert_eq!(at(&map, -5, 0), Landform::Shoulder);
        assert_eq!(at(&map, 0, 0), Landform::Slope);
        assert_eq!(at(&map, 5, 0), Landform::Footslope);
        assert_eq!(at(&map, -15, 0), Landform::Flat);
        assert_eq!(at(&map, 15, 0), Landform::Flat);
    }

    #[test]
    fn pattern_bits_follow_directions() {
        // East is uphill: E, NE and SE are higher; W, NW and SW lower; N and S level.
        let p = patterns(&ground(|x, _| x), &params(3.0));
        let p = p[(C, C)];
        assert_eq!(p.higher, 0b1000_0011); // SE, NE, E
        assert_eq!(p.lower, 0b0011_1000); // SW, W, NW
    }

    #[test]
    fn radii_scale_with_cell_size() {
        // The radius is ground distance, not a cell count: on 2 m cells, 10 m is five
        // steps and finds the ridge.
        let mut g = ground(|x, _| -0.5 * x.abs());
        g.scale = 2.0;
        let map = classify(&g, &params(10.0));
        assert_eq!(at(&map, 0, 0), Landform::Ridge);
        // A 1 m radius is shorter than one 2 m step: no direction has a cell.
        let map = classify(&g, &params(1.0));
        assert_eq!(at(&map, 0, 0), Landform::Flat);
    }

    #[test]
    fn skip_radius_ignores_a_small_bump() {
        // A 2 cm bump on the cell itself reads as a peak from every cell out to 5 m;
        // skipping the first 2 m, the drop is under the 1° threshold.
        let g = ground(|x, y| if x.hypot(y) < 0.5 { 0.02 } else { 0.0 });
        assert_eq!(at(&classify(&g, &params(5.0)), 0, 0), Landform::Peak);
        let skip = GeomorphonParams {
            skip_radius_m: 2.0,
            ..params(5.0)
        };
        assert_eq!(at(&classify(&g, &skip), 0, 0), Landform::Flat);
    }

    #[test]
    fn edges_shorten_lines_of_sight_and_nan_is_flat() {
        let mut g = ground(|x, _| -0.5 * x.abs());
        // Crest cell on the south border: S, SE and SW have no cell and read level,
        // leaving four lower directions, so the ridge reads as a shoulder there.
        let map = classify(&g, &params(5.0));
        assert_eq!(map[(C, 0)], Landform::Shoulder);
        // A NaN next to the crest is skipped; a NaN cell is flat.
        g.grid[(C + 1, C)] = f64::NAN;
        let map = classify(&g, &params(5.0));
        assert_eq!(at(&map, 0, 0), Landform::Ridge);
        assert_eq!(at(&map, 1, 0), Landform::Flat);
    }

    #[test]
    fn lookup_table_matches_the_paper() {
        use Landform::*;
        // (lower, higher) -> landform, from Jasiewicz and Stepinski 2013 Fig. 4.
        let cases = [
            ((0, 0), Flat),
            ((1, 1), Flat),
            ((2, 0), Flat),
            ((8, 0), Peak),
            ((0, 8), Pit),
            ((7, 1), Ridge),
            ((5, 0), Ridge),
            ((6, 2), Ridge),
            ((1, 7), Valley),
            ((0, 5), Valley),
            ((3, 0), Shoulder),
            ((2, 1), Shoulder),
            ((0, 3), Footslope),
            ((1, 2), Footslope),
            ((4, 2), Spur),
            ((5, 3), Spur),
            ((2, 4), Hollow),
            ((3, 5), Hollow),
            ((3, 3), Slope),
            ((4, 4), Slope),
            ((2, 2), Slope),
        ];
        for ((lower, higher), expected) in cases {
            assert_eq!(
                Landform::from_counts(lower, higher),
                Some(expected),
                "({lower}, {higher})"
            );
        }
        assert_eq!(Landform::from_counts(5, 4), None);
    }

    #[test]
    fn lookup_table_is_symmetric() {
        // Turning the ground upside down swaps lower and higher, and each landform
        // with its mirror.
        let mirror = |l: Landform| match l {
            Landform::Peak => Landform::Pit,
            Landform::Pit => Landform::Peak,
            Landform::Ridge => Landform::Valley,
            Landform::Valley => Landform::Ridge,
            Landform::Shoulder => Landform::Footslope,
            Landform::Footslope => Landform::Shoulder,
            Landform::Spur => Landform::Hollow,
            Landform::Hollow => Landform::Spur,
            l => l,
        };
        for lower in 0..=8 {
            for higher in 0..=8 - lower {
                let l = Landform::from_counts(lower, higher).unwrap();
                assert_eq!(Landform::from_counts(higher, lower), Some(mirror(l)));
            }
        }
    }

    #[test]
    #[ignore = "timing note, run with --release --ignored --nocapture"]
    fn runtime_on_a_1000_by_1000_ground_model() {
        let mut grid = Vec2D::new(1000, 1000, 0.0);
        for (x, y, v) in grid.iter_mut() {
            let (x, y) = (x as f64 * 2.0, y as f64 * 2.0);
            *v = 10.0 * (x / 37.0).sin() * (y / 53.0).cos() + 0.05 * x;
        }
        let g = HeightMap {
            xoffset: 0.0,
            yoffset: 0.0,
            scale: 2.0,
            grid,
        };
        let start = std::time::Instant::now();
        let map = classify(&g, &GeomorphonParams::default());
        println!(
            "classify 1000x1000, 2 m cells, 20 m radius: {:?}",
            start.elapsed()
        );
        assert!(map.iter().any(|(_, _, l)| l == Landform::Ridge));
    }
}
