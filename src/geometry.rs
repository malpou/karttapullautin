//! This mod contains structs for storing and loading different types of geometry, like Polylines
//! and a list of Points.
//!
//! These types also have helpers for exporting them to DXF format.

use crate::isom::IsomCode;

/// A 2D point
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Point2 {
    /// The x coordinate of this point.
    pub x: f64,
    /// The y coordinate of this point.
    pub y: f64,
}

impl Point2 {
    /// Create a new point from the given coordinates.
    pub fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }

    /// Euclidean distance to `other`.
    pub fn distance(self, other: Point2) -> f64 {
        ((self.x - other.x).powi(2) + (self.y - other.y).powi(2)).sqrt()
    }

    /// Shortest distance to the segment from `a` to `b` (to `a` when they coincide).
    pub fn distance_to_segment(self, a: Point2, b: Point2) -> f64 {
        let (dx, dy) = (b.x - a.x, b.y - a.y);
        let l2 = dx * dx + dy * dy;
        let t = if l2 == 0.0 {
            0.0
        } else {
            (((self.x - a.x) * dx + (self.y - a.y) * dy) / l2).clamp(0.0, 1.0)
        };
        self.distance(Point2::new(a.x + t * dx, a.y + t * dy))
    }
}

/// A 3D point (eg. 2D + height)
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Point3 {
    /// The x coordinate of this point.
    pub x: f64,
    /// The y coordinate of this point.
    pub y: f64,
    /// The z coordinate of this point (height).
    pub z: f64,
}

impl Point3 {
    /// Create a new point from the given coordinates.
    pub fn new(x: f64, y: f64, z: f64) -> Self {
        Self { x, y, z }
    }
}

/// A collection of points with associated classification. This classification is also used to put
/// the DXF objects into separate layers.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Points {
    points: Vec<Point2>,
    classification: Vec<Classification>,
}

impl Points {
    pub fn new() -> Self {
        Self {
            points: Vec::new(),
            classification: Vec::new(),
        }
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            points: Vec::with_capacity(capacity),
            classification: Vec::with_capacity(capacity),
        }
    }

    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> usize {
        self.points.len()
    }

    /// Add a point to this collection.
    pub fn push(&mut self, point: Point2, class: Classification) {
        self.points.push(point);
        self.classification.push(class);
    }

    /// Iterate over the points in this collection.
    pub fn iter(&self) -> impl Iterator<Item = (&Point2, &Classification)> {
        self.points.iter().zip(self.classification.iter())
    }
}

impl IntoIterator for Points {
    type Item = (Point2, Classification);

    type IntoIter = std::iter::Zip<std::vec::IntoIter<Point2>, std::vec::IntoIter<Classification>>;

    fn into_iter(self) -> Self::IntoIter {
        self.points.into_iter().zip(self.classification)
    }
}

/// A collection polylines with associated classification.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Polylines<P, C> {
    polylines: Vec<Vec<P>>, // TODO: flatten to single vector?
    classification: Vec<C>,
}

impl<P, C> Polylines<P, C> {
    pub fn new() -> Self {
        Self {
            polylines: Vec::new(),
            classification: Vec::new(),
        }
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            polylines: Vec::with_capacity(capacity),
            classification: Vec::with_capacity(capacity),
        }
    }

    pub fn push(&mut self, polyline: Vec<P>, class: C) {
        self.polylines.push(polyline);
        self.classification.push(class);
    }

    pub fn pop(&mut self) {
        self.polylines.pop();
        self.classification.pop();
    }

    pub fn iter(&self) -> impl Iterator<Item = (&Vec<P>, &C)> {
        self.polylines.iter().zip(self.classification.iter())
    }

    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> usize {
        self.polylines.len()
    }
}

impl<P, C> IntoIterator for Polylines<P, C> {
    type Item = (Vec<P>, C);

    type IntoIter = std::iter::Zip<std::vec::IntoIter<Vec<P>>, std::vec::IntoIter<C>>;

    fn into_iter(self) -> Self::IntoIter {
        self.polylines.into_iter().zip(self.classification)
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum Geometry {
    Points(Points),

    /// Polylines2 is used for 2D polylines with a classification.
    Polylines2(Polylines<Point2, Classification>),

    /// Polylines3 is used for 2D polylines with a height (z coordinate).
    Polylines3(Polylines<Point3, (Classification, f64)>), // Classification + height
}

impl From<Points> for Geometry {
    fn from(points: Points) -> Self {
        Geometry::Points(points)
    }
}
impl From<Polylines<Point2, Classification>> for Geometry {
    fn from(polylines: Polylines<Point2, Classification>) -> Self {
        Geometry::Polylines2(polylines)
    }
}
impl From<Polylines<Point3, (Classification, f64)>> for Geometry {
    fn from(polylines: Polylines<Point3, (Classification, f64)>) -> Self {
        Geometry::Polylines3(polylines)
    }
}

/// The version of the BinaryDxf file format. If any content of the [`BinaryDxf`] struct changes,
/// including any sub-fields (basically anything in this mod) we need to increase this version.
const BINARY_DXF_VERSION: usize = 1;

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct BinaryDxf {
    /// the version of the program that created this file, used to detect stale temp files
    version: String,
    bounds: Bounds,
    data: Vec<Geometry>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Bounds {
    pub xmin: f64,
    pub xmax: f64,
    pub ymin: f64,
    pub ymax: f64,
}

impl Bounds {
    pub fn new(xmin: f64, xmax: f64, ymin: f64, ymax: f64) -> Self {
        Self {
            xmin,
            xmax,
            ymin,
            ymax,
        }
    }

    /// The smallest box holding every point; `None` when there are none.
    pub fn around(points: impl IntoIterator<Item = Point2>) -> Option<Self> {
        let mut points = points.into_iter();
        let first = points.next()?;
        Some(
            points.fold(Self::new(first.x, first.x, first.y, first.y), |b, p| {
                Self::new(
                    b.xmin.min(p.x),
                    b.xmax.max(p.x),
                    b.ymin.min(p.y),
                    b.ymax.max(p.y),
                )
            }),
        )
    }
}

impl BinaryDxf {
    pub fn new(bounds: Bounds, data: Vec<Geometry>) -> Self {
        Self {
            version: BINARY_DXF_VERSION.to_string(),
            bounds,
            data,
        }
    }

    pub fn bounds(&self) -> &Bounds {
        &self.bounds
    }

    /// Get the points in this geometry, or [`None`] if does not contain [`Polylines`] data.
    pub fn take_geometry(self) -> Vec<Geometry> {
        self.data
    }

    /// Serialize this object to a writer.
    pub fn to_writer<W: std::io::Write>(&self, writer: &mut W) -> anyhow::Result<()> {
        crate::util::write_object(writer, self)
    }
    /// Read this object from a reader. Returns an error if the version does not match.
    pub fn from_reader<R: std::io::Read>(reader: &mut R) -> anyhow::Result<Self> {
        let object: Self = crate::util::read_object(reader)?;

        // Prevously we were using the crate version, which is a string starting
        // with 2.X.X, but now we use a simple integer version. Version 1 supports all those
        // "2.X.X" versions as well.
        if (object.version == BINARY_DXF_VERSION.to_string())
            || (BINARY_DXF_VERSION == 1 && object.version.starts_with("2."))
        {
            Ok(object)
        } else {
            anyhow::bail!(
                "This DXF.BIN file version is not supported by this executable. Please re-run this command with an executable that supports this version (dxf.bin file version: {})",
                object.version,
            );
        }
    }

    /// Write this geometry to a DXF file, one DXF layer per symbol code. Records with no
    /// symbol code (the knoll-detector artifact) are left out.
    pub fn to_dxf<W: std::io::Write>(&self, writer: &mut W) -> anyhow::Result<()> {
        write!(
            writer,
            "  0\r\nSECTION\r\n  2\r\nHEADER\r\n  9\r\n$EXTMIN\r\n 10\r\n{}\r\n 20\r\n{}\r\n  9\r\n$EXTMAX\r\n 10\r\n{}\r\n 20\r\n{}\r\n  0\r\nENDSEC\r\n  0\r\nSECTION\r\n  2\r\nENTITIES\r\n  0\r\n",
            self.bounds.xmin, self.bounds.ymin, self.bounds.xmax, self.bounds.ymax
        )?;

        for geom in &self.data {
            match geom {
                Geometry::Points(points) => {
                    for (point, class) in points.points.iter().zip(&points.classification) {
                        let Some(layer) = class.isom_code() else {
                            continue;
                        };

                        write!(
                            writer,
                            "POINT\r\n  8\r\n{layer}\r\n 10\r\n{}\r\n 20\r\n{}\r\n 50\r\n0\r\n  0\r\n",
                            point.x, point.y
                        )?;
                    }
                }
                Geometry::Polylines2(polylines) => {
                    for (polyline, class) in
                        polylines.polylines.iter().zip(&polylines.classification)
                    {
                        let Some(layer) = class.isom_code() else {
                            continue;
                        };
                        // closed flag (70=1) on area rings so importers read them as areas
                        let closed = if class.is_area() { " 70\r\n1\r\n" } else { "" };
                        write!(
                            writer,
                            "POLYLINE\r\n 66\r\n1\r\n  8\r\n{layer}\r\n{closed}  0\r\n"
                        )?;

                        for p in polyline {
                            write!(
                                writer,
                                "VERTEX\r\n  8\r\n{layer}\r\n 10\r\n{}\r\n 20\r\n{}\r\n  0\r\n",
                                p.x, p.y,
                            )?;
                        }
                        write!(writer, "SEQEND\r\n  0\r\n")?;
                    }
                }
                Geometry::Polylines3(polylines) => {
                    for (polyline, (class, height)) in
                        polylines.polylines.iter().zip(&polylines.classification)
                    {
                        let Some(layer) = class.isom_code() else {
                            continue;
                        };

                        write!(
                            writer,
                            "POLYLINE\r\n 66\r\n1\r\n  8\r\n{layer}\r\n 38\r\n{height}\r\n  0\r\n"
                        )?;

                        for p in polyline {
                            write!(
                                writer,
                                "VERTEX\r\n  8\r\n{}\r\n 10\r\n{}\r\n 20\r\n{}\r\n 30\r\n{}\r\n  0\r\n",
                                layer, p.x, p.y, height
                            )?;
                        }
                        write!(writer, "SEQEND\r\n  0\r\n")?;
                    }
                }
            }
        }

        writer.write_all("ENDSEC\r\n  0\r\nEOF\r\n".as_bytes())?;
        Ok(())
    }
}

/// Classification used for contour generation
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Classification {
    /// Used in first contour generation step
    ContourSimple,

    /// Used in second contour generation step (smoothjoin)
    Contour,
    ContourIndex,
    ContourIntermed,
    ContourIndexIntermed,

    Depression,
    DepressionIndex,
    DepressionIntermed,
    DepressionIndexIntermed,

    /// Use for formlines (generated in render)
    Formline,
    FormlineDepression,

    /// Used in dotknoll detections
    Dotknoll,
    Udepression,
    UglyDotknoll,
    UglyUdepression,

    /// Comes from knolldetector
    Knoll1010,

    /// Used for cliff generations
    Cliff2,
    Cliff3,
    Cliff4,

    /// The tick drawn inside a depression contour so it reads as a depression rather
    /// than a knoll. ISOM 2017-2 makes it part of symbol 101 ("a depression has to have
    /// at least one slope line"), not a symbol of its own, so it carries contour weight
    /// and maps to 101 downstream. Generated in merge, alongside the ring it belongs to.
    SlopeLine,
    SmallDepression,

    /// Vegetation and open-land areas, one per ISOM 2017-2 area symbol, traced from the
    /// vegetation grids into closed rings. Kept at the end so the bincode variant indices
    /// of the older variants (and thus existing `.dxf.bin` files) stay stable.
    Veg403,
    Veg406,
    Veg407,
    Veg408,
    Veg410,
}

impl Classification {
    /// ISOM 2017-2 symbol code this classification is drawn with; DXF output also uses
    /// it as the DXF layer name. None for [`Self::Knoll1010`], a knoll-detector artifact
    /// with no map symbol, which vector output skips.
    pub fn isom_code(&self) -> Option<IsomCode> {
        Some(match self {
            Self::ContourSimple | Self::Contour | Self::Depression => IsomCode::C101_000,
            Self::ContourIndex | Self::DepressionIndex => IsomCode::C102_000,
            // intermediate (half-interval) contours are represented as form lines in ISOM
            Self::ContourIntermed
            | Self::ContourIndexIntermed
            | Self::DepressionIntermed
            | Self::DepressionIndexIntermed
            | Self::Formline
            | Self::FormlineDepression => IsomCode::C103_000,
            Self::Dotknoll | Self::UglyDotknoll => IsomCode::C109_000,
            Self::Udepression | Self::UglyUdepression | Self::SmallDepression => IsomCode::C111_000,
            Self::Cliff2 => IsomCode::C202_000,
            Self::Cliff3 | Self::Cliff4 => IsomCode::C201_000,
            // a tick that belongs to symbol 101, drawn as its slope-line variant
            Self::SlopeLine => IsomCode::C101_001,
            Self::Veg403 => IsomCode::C403_000,
            Self::Veg406 => IsomCode::C406_000,
            Self::Veg407 => IsomCode::C407_000,
            Self::Veg408 => IsomCode::C408_000,
            Self::Veg410 => IsomCode::C410_000,
            Self::Knoll1010 => return None,
        })
    }

    /// Human-readable name of the symbol, telling depression lines apart from the
    /// contours that share their symbol code. None where [`Self::isom_code`] is None.
    pub fn symbol_name(&self) -> Option<&'static str> {
        Some(match self {
            Self::ContourSimple | Self::Contour => "contour",
            Self::ContourIndex => "index contour",
            Self::ContourIntermed | Self::ContourIndexIntermed | Self::Formline => "form line",
            Self::Depression => "depression contour",
            Self::DepressionIndex => "depression index contour",
            Self::DepressionIntermed | Self::DepressionIndexIntermed | Self::FormlineDepression => {
                "depression form line"
            }
            Self::Dotknoll | Self::UglyDotknoll => "knoll",
            Self::Udepression | Self::UglyUdepression | Self::SmallDepression => "small depression",
            Self::Cliff2 => "cliff",
            Self::Cliff3 | Self::Cliff4 => "impassable cliff",
            Self::SlopeLine => "slope line",
            Self::Veg403 => "rough open land",
            Self::Veg406 => "vegetation: slow running",
            Self::Veg407 => "vegetation: slow running, good visibility",
            Self::Veg408 => "vegetation: walk",
            Self::Veg410 => "vegetation: fight",
            Self::Knoll1010 => return None,
        })
    }

    /// Whether this classification is an area symbol, whose polylines are closed rings.
    pub fn is_area(&self) -> bool {
        matches!(
            self,
            Self::Veg403 | Self::Veg406 | Self::Veg407 | Self::Veg408 | Self::Veg410
        )
    }

    /// Whether the knoll detector was unsure of this knoll or small depression. Definite
    /// and uncertain points share a symbol code, so this is the only way to rank them.
    pub fn is_ugly(&self) -> bool {
        matches!(self, Self::UglyDotknoll | Self::UglyUdepression)
    }

    /// Whether this line encloses lower ground: the depression contours and the
    /// depression form line. Unlike [`Self::is_depression`], includes the form line.
    pub fn is_depression_line(&self) -> bool {
        self.is_depression() || *self == Self::FormlineDepression
    }

    pub fn is_contour(&self) -> bool {
        matches!(
            self,
            Self::Contour | Self::ContourIndex | Self::ContourIntermed | Self::ContourIndexIntermed
        )
    }

    pub fn is_depression(&self) -> bool {
        matches!(
            self,
            Self::Depression
                | Self::DepressionIndex
                | Self::DepressionIntermed
                | Self::DepressionIndexIntermed
        )
    }

    pub fn is_index(&self) -> bool {
        matches!(
            self,
            Self::ContourIndex
                | Self::ContourIndexIntermed
                | Self::DepressionIndex
                | Self::DepressionIndexIntermed
        )
    }

    pub fn is_intermed(&self) -> bool {
        matches!(
            self,
            Self::ContourIntermed
                | Self::ContourIndexIntermed
                | Self::DepressionIntermed
                | Self::DepressionIndexIntermed
        )
    }
}

/// A closed contour line, first vertex repeated last.
#[derive(Debug, Clone)]
pub struct Ring {
    points: Vec<Point2>,
}

impl Ring {
    /// Build from parallel x/y slices. Closes the ring (pushes a copy of the first vertex)
    /// only when the last vertex differs from the first, so an already closed input gets no
    /// extra edge. Panics if the slices differ in length.
    pub fn from_xy(x: &[f64], y: &[f64]) -> Self {
        assert_eq!(x.len(), y.len(), "x and y must have the same length");
        let mut points: Vec<Point2> = x
            .iter()
            .zip(y.iter())
            .map(|(&x, &y)| Point2::new(x, y))
            .collect();
        if let (Some(first), Some(last)) = (points.first(), points.last())
            && (first.x != last.x || first.y != last.y)
        {
            let first = *first;
            points.push(first);
        }
        Self { points }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.points.len()
    }

    /// Even-odd ray cast towards +x over consecutive edges (prev -> cur). Exact for concave
    /// shapes. Every edge is tested, the first and the closing one included. An edge counts
    /// when `y0 <= p.y < y1` (either direction) and `p.x` lies strictly left of the crossing,
    /// so points on edges follow that half-open convention. An empty ring contains nothing.
    pub fn contains(&self, p: Point2) -> bool {
        let mut hit = 0;
        for w in self.points.windows(2) {
            let (x0, y0) = (w[0].x, w[0].y);
            let (x1, y1) = (w[1].x, w[1].y);
            if ((y0 <= p.y && p.y < y1) || (y1 <= p.y && p.y < y0))
                && (p.x < (x1 - x0) * (p.y - y0) / (y1 - y0) + x0)
            {
                hit += 1;
            }
        }
        hit % 2 == 1
    }

    /// Shortest distance from `p` to any edge of the ring.
    ///
    /// Legacy behaviour kept on purpose: an empty ring yields `f64::MAX.sqrt()`.
    pub fn distance_to_point(&self, p: Point2) -> f64 {
        let n = self.points.len();
        (0..n)
            .map(|i| p.distance_to_segment(self.points[i], self.points[(i + 1) % n]))
            .fold(f64::MAX.sqrt(), f64::min)
    }
}

/// Shoelace formula; positive = counter-clockwise. The ring may be open or closed.
pub fn signed_area(ring: &[Point2]) -> f64 {
    let n = ring.len();
    let mut s = 0.0;
    for i in 0..n {
        let p = &ring[i];
        let q = &ring[(i + 1) % n];
        s += p.x * q.y - q.x * p.y;
    }
    s / 2.0
}

/// Join polylines whose quantized endpoints (1 mm) meet, end to end.
///
/// Lines with `len() >= max_vertices` are emptied and never joined (`usize::MAX` = no limit).
/// Returns one `Vec<Point2>` per input slot, same length and order: absorbed donors and
/// dropped lines come back empty so callers keep indexing by input position.
///
/// Each quantized endpoint registers up to two lines: the first to reach it in `heads1`, the
/// last in `heads2`. A missing entry means "no line", so every slot, 0 included, can be a
/// join partner.
pub fn join_polylines<C>(lines: &Polylines<Point2, C>, max_vertices: usize) -> Vec<Vec<Point2>> {
    use rustc_hash::FxHashMap;
    use std::collections::hash_map::Entry;

    // Internal type used to index into the hashmaps and vectors.
    // Since using f64 coordinates directly has problems with rounding (and do not impl Eq and
    // Hash), we can use an integer representation of the coordinates to index into the HashMaps.
    // By multiplying by 1000, we can keep a precision of 3 decimal places, which is sufficient for
    // what we need.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    struct Key {
        x: i64,
        y: i64,
    }
    impl Key {
        fn new(x: f64, y: f64) -> Self {
            Key {
                x: (x * 1000.0) as i64,
                y: (y * 1000.0) as i64,
            }
        }
    }

    /// The first candidate `usable` accepts, looked up in this order: the head in the first
    /// and second tables, then the tail in the second and first.
    fn join_partner(
        heads1: &FxHashMap<Key, usize>,
        heads2: &FxHashMap<Key, usize>,
        head: Key,
        tail: Key,
        usable: impl Fn(usize) -> bool,
    ) -> Option<usize> {
        [
            heads1.get(&head),
            heads2.get(&head),
            heads2.get(&tail),
            heads1.get(&tail),
        ]
        .into_iter()
        .flatten()
        .copied()
        .find(|&j| usable(j))
    }

    let mut heads1: FxHashMap<Key, usize> = FxHashMap::default();
    let mut heads2: FxHashMap<Key, usize> = FxHashMap::default();
    // (head, tail) of each slot's current line; None for a dropped line.
    let mut ends = Vec::<Option<(Key, Key)>>::with_capacity(lines.len());
    let mut out = Vec::<Vec<Point2>>::with_capacity(lines.len());

    for (j, (line, _c)) in lines.iter().enumerate() {
        if line.len() < max_vertices {
            let first = line.first().unwrap();
            let last = line.last().unwrap();

            let head = Key::new(first.x, first.y);
            let tail = Key::new(last.x, last.y);

            ends.push(Some((head, tail)));
            out.push(line.clone());

            for key in [head, tail] {
                if let Entry::Vacant(e) = heads1.entry(key) {
                    e.insert(j);
                } else {
                    heads2.insert(key, j);
                }
            }
        } else {
            ends.push(None);
            out.push(vec![]);
        }
    }

    for l in 0..lines.len() {
        let Some((mut head, mut tail)) = ends[l] else {
            continue;
        };
        if out[l].is_empty() {
            continue;
        }
        while let Some(to_join) = join_partner(&heads1, &heads2, head, tail, |j| {
            j != l && !out[j].is_empty()
        }) {
            // only kept lines are registered, so a partner always has ends
            let (join_head, join_tail) = ends[to_join].expect("a join partner is a kept line");
            if tail == join_head {
                heads1.remove(&tail);
                heads2.remove(&tail);
                let mut donor = out[to_join].clone();
                out[l].append(&mut donor);
                tail = join_tail;
                out[to_join].clear();
            } else if tail == join_tail {
                heads1.remove(&tail);
                heads2.remove(&tail);
                let mut donor = out[to_join].clone();
                donor.reverse();
                out[l].append(&mut donor);
                tail = join_head;
                out[to_join].clear();
            } else if head == join_tail {
                heads1.remove(&head);
                heads2.remove(&head);
                let donor = out[to_join].clone();
                out[l].splice(0..0, donor);
                head = join_head;
                out[to_join].clear();
            } else if head == join_head {
                heads1.remove(&head);
                heads2.remove(&head);
                let mut donor = out[to_join].clone();
                donor.reverse();
                out[l].splice(0..0, donor);
                head = join_tail;
                out[to_join].clear();
            }
        }
        ends[l] = Some((head, tail));
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit_square_closed() -> Ring {
        Ring::from_xy(&[0.0, 1.0, 1.0, 0.0, 0.0], &[0.0, 0.0, 1.0, 1.0, 0.0])
    }

    fn pl(lines: &[&[(f64, f64)]]) -> Polylines<Point2, ()> {
        let mut out = Polylines::new();
        for line in lines {
            out.push(line.iter().map(|&(x, y)| Point2::new(x, y)).collect(), ());
        }
        out
    }

    fn xy(line: &[Point2]) -> Vec<(f64, f64)> {
        line.iter().map(|p| (p.x, p.y)).collect()
    }

    #[test]
    fn ring_contains_unit_square() {
        let ring = unit_square_closed();
        assert!(ring.contains(Point2::new(0.5, 0.5)));
        assert!(!ring.contains(Point2::new(2.0, 0.5)));
        assert!(!ring.contains(Point2::new(-1.0, 0.5)));
    }

    #[test]
    fn ring_contains_concave_l_shape() {
        let ring = Ring::from_xy(
            &[0.0, 2.0, 2.0, 1.0, 1.0, 0.0, 0.0],
            &[0.0, 0.0, 1.0, 1.0, 2.0, 2.0, 0.0],
        );
        assert!(ring.contains(Point2::new(0.5, 1.5)));
        assert!(ring.contains(Point2::new(1.5, 0.5)));
        // the notch
        assert!(!ring.contains(Point2::new(1.5, 1.5)));
    }

    /// Pins the legacy half-open convention. (0.5, 0) on the bottom edge: that edge has
    /// y0 == y1 and never matches, the right edge (1,0)->(1,1) does (0 <= 0 < 1 and 0.5 < 1),
    /// the left edge (0,1)->(0,0) does not (0.5 < 0 is false): one hit, inside. (1, 0.5) on
    /// the right edge: the right edge fails 1.0 < 1, the left edge fails 1.0 < 0: outside.
    #[test]
    fn ring_contains_point_on_edge_legacy_convention() {
        let ring = unit_square_closed();
        assert!(ring.contains(Point2::new(0.5, 0.0)));
        assert!(!ring.contains(Point2::new(1.0, 0.5)));
    }

    /// The unit square started at (1, 1), so its right edge is the closing edge: the only
    /// edge the ray from (0.5, 0.5) crosses. The knoll detector's elevation pass once skipped
    /// the closing edge and called this point outside.
    #[test]
    fn ring_contains_counts_the_closing_edge() {
        let ring = Ring::from_xy(&[1.0, 0.0, 0.0, 1.0, 1.0], &[1.0, 1.0, 0.0, 0.0, 1.0]);
        assert!(ring.contains(Point2::new(0.5, 0.5)));
    }

    /// The unit square started at (1, 0), so its right edge is the first edge: the only edge
    /// the ray from (0.5, 0.5) crosses. The knoll lift once skipped the first edge and called
    /// this point outside.
    #[test]
    fn ring_contains_counts_the_first_edge() {
        let ring = Ring::from_xy(&[1.0, 1.0, 0.0, 0.0, 1.0], &[0.0, 1.0, 1.0, 0.0, 0.0]);
        assert!(ring.contains(Point2::new(0.5, 0.5)));
    }

    #[test]
    fn point_distances_and_bounds() {
        let p = Point2::new;
        assert_eq!(p(0.0, 0.0).distance(p(3.0, 4.0)), 5.0);
        // beyond the end, beside the middle, and a zero-length segment
        assert_eq!(
            p(13.0, 4.0).distance_to_segment(p(0.0, 0.0), p(10.0, 0.0)),
            5.0
        );
        assert_eq!(
            p(5.0, -2.0).distance_to_segment(p(0.0, 0.0), p(10.0, 0.0)),
            2.0
        );
        assert_eq!(
            p(3.0, 4.0).distance_to_segment(p(0.0, 0.0), p(0.0, 0.0)),
            5.0
        );
        assert!(Bounds::around([]).is_none());
        let b = Bounds::around([p(1.0, 5.0), p(-2.0, 3.0), p(4.0, 0.0)]).unwrap();
        assert_eq!((b.xmin, b.xmax, b.ymin, b.ymax), (-2.0, 4.0, 0.0, 5.0));
        assert_eq!(signed_area(&[p(0.0, 0.0), p(2.0, 0.0), p(2.0, 2.0)]), 2.0);
    }

    #[test]
    fn ring_contains_empty_is_false() {
        let ring = Ring::from_xy(&[], &[]);
        assert!(!ring.contains(Point2::new(0.0, 0.0)));
    }

    #[test]
    fn ring_from_xy_closes_open_input_only() {
        let open = Ring::from_xy(&[0.0, 1.0, 1.0, 0.0], &[0.0, 0.0, 1.0, 1.0]);
        let closed = unit_square_closed();
        assert_eq!(open.len(), 5);
        assert_eq!(closed.len(), 5);
        for p in [(0.5, 0.5), (2.0, 0.5), (-1.0, 0.5), (0.5, 0.0), (1.0, 0.5)] {
            assert_eq!(
                open.contains(Point2::new(p.0, p.1)),
                closed.contains(Point2::new(p.0, p.1))
            );
        }
    }

    #[test]
    fn ring_distance_to_point_unit_square() {
        let ring = unit_square_closed();
        assert_eq!(ring.distance_to_point(Point2::new(0.5, 2.0)), 1.0);
        assert_eq!(ring.distance_to_point(Point2::new(0.5, 0.25)), 0.25);
    }

    #[test]
    fn join_polylines_head_to_tail() {
        let lines = pl(&[&[(0.0, 0.0), (1.0, 0.0)], &[(1.0, 0.0), (2.0, 0.0)]]);
        let joined = join_polylines(&lines, usize::MAX);
        assert_eq!(joined.len(), 2);
        assert_eq!(
            xy(&joined[0]),
            vec![(0.0, 0.0), (1.0, 0.0), (1.0, 0.0), (2.0, 0.0)]
        );
        assert!(joined[1].is_empty());
    }

    #[test]
    fn join_polylines_tail_to_tail_reverses_donor() {
        let lines = pl(&[&[(0.0, 0.0), (1.0, 0.0)], &[(2.0, 0.0), (1.0, 0.0)]]);
        let joined = join_polylines(&lines, usize::MAX);
        assert_eq!(
            xy(&joined[0]),
            vec![(0.0, 0.0), (1.0, 0.0), (1.0, 0.0), (2.0, 0.0)]
        );
        assert!(joined[1].is_empty());
    }

    #[test]
    fn join_polylines_drops_lines_at_max_vertices() {
        let lines = pl(&[
            &[(0.0, 0.0), (1.0, 0.0), (2.0, 0.0)],
            &[(2.0, 0.0), (3.0, 0.0)],
        ]);
        let joined = join_polylines(&lines, 3);
        assert!(joined[0].is_empty());
        assert_eq!(xy(&joined[1]), vec![(2.0, 0.0), (3.0, 0.0)]);
    }

    #[test]
    fn join_polylines_empty_input() {
        let lines: Polylines<Point2, ()> = Polylines::new();
        assert!(join_polylines(&lines, usize::MAX).is_empty());
    }

    /// Slot 0 registers like any other: line 0 takes K in the first table, line 1 lands in the
    /// second and line 2 overwrites it there. At l = 0 the first-table hit is line 0 itself
    /// (skipped) and the second-table hit is line 2; the heads meet, so line 2 is reversed and
    /// spliced in front of line 0 and K is retired, leaving line 1 untouched.
    #[test]
    fn join_polylines_slot_zero_is_a_join_partner() {
        let lines = pl(&[
            &[(0.0, 0.0), (-1.0, 0.0)],
            &[(0.0, 0.0), (0.0, 1.0)],
            &[(0.0, 0.0), (1.0, 1.0)],
        ]);
        let joined = join_polylines(&lines, usize::MAX);
        assert_eq!(
            xy(&joined[0]),
            vec![(1.0, 1.0), (0.0, 0.0), (0.0, 0.0), (-1.0, 0.0)]
        );
        assert_eq!(xy(&joined[1]), vec![(0.0, 0.0), (0.0, 1.0)]);
        assert!(joined[2].is_empty());
    }

    #[test]
    fn isom_code_is_isom_2017_2_for_every_classification() {
        use super::Classification::*;
        let expected = [
            (ContourSimple, Some(IsomCode::C101_000), Some("contour")),
            (Contour, Some(IsomCode::C101_000), Some("contour")),
            (
                ContourIndex,
                Some(IsomCode::C102_000),
                Some("index contour"),
            ),
            (ContourIntermed, Some(IsomCode::C103_000), Some("form line")),
            (
                ContourIndexIntermed,
                Some(IsomCode::C103_000),
                Some("form line"),
            ),
            (
                Depression,
                Some(IsomCode::C101_000),
                Some("depression contour"),
            ),
            (
                DepressionIndex,
                Some(IsomCode::C102_000),
                Some("depression index contour"),
            ),
            (
                DepressionIntermed,
                Some(IsomCode::C103_000),
                Some("depression form line"),
            ),
            (
                DepressionIndexIntermed,
                Some(IsomCode::C103_000),
                Some("depression form line"),
            ),
            (Formline, Some(IsomCode::C103_000), Some("form line")),
            (
                FormlineDepression,
                Some(IsomCode::C103_000),
                Some("depression form line"),
            ),
            (Dotknoll, Some(IsomCode::C109_000), Some("knoll")),
            (
                Udepression,
                Some(IsomCode::C111_000),
                Some("small depression"),
            ),
            (UglyDotknoll, Some(IsomCode::C109_000), Some("knoll")),
            (
                UglyUdepression,
                Some(IsomCode::C111_000),
                Some("small depression"),
            ),
            (Knoll1010, None, None),
            (Cliff2, Some(IsomCode::C202_000), Some("cliff")),
            (Cliff3, Some(IsomCode::C201_000), Some("impassable cliff")),
            (Cliff4, Some(IsomCode::C201_000), Some("impassable cliff")),
            (SlopeLine, Some(IsomCode::C101_001), Some("slope line")),
            (
                SmallDepression,
                Some(IsomCode::C111_000),
                Some("small depression"),
            ),
        ];
        for (c, code, name) in expected {
            assert_eq!(c.isom_code(), code, "{c:?}");
            assert_eq!(c.symbol_name(), name, "{c:?}");
        }
    }

    #[test]
    fn ugly_and_depression_line_flags() {
        use super::Classification::*;
        assert!(UglyDotknoll.is_ugly() && UglyUdepression.is_ugly());
        assert!(!Dotknoll.is_ugly() && !Udepression.is_ugly());
        assert!(
            FormlineDepression.is_depression_line() && DepressionIndexIntermed.is_depression_line()
        );
        assert!(!Formline.is_depression_line() && !Contour.is_depression_line());
    }

    #[test]
    fn dxf_layers_are_symbol_codes_and_skip_knoll_detector_artifact() {
        let mut lines = Polylines::new();
        let line = vec![Point2::new(0.0, 0.0), Point2::new(1.0, 1.0)];
        lines.push(line.clone(), Classification::ContourIndex);
        lines.push(line, Classification::Knoll1010);
        let dxf = BinaryDxf::new(Bounds::new(0.0, 1.0, 0.0, 1.0), vec![lines.into()]);
        let mut out = Vec::new();
        dxf.to_dxf(&mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert_eq!(text.matches("POLYLINE").count(), 1);
        assert!(text.contains("  8\r\n102.000\r\n"));
        assert!(!text.contains("1010"));
    }

    #[test]
    fn dxf_area_rings_are_closed_polylines_on_their_symbol_layer() {
        let mut lines = Polylines::new();
        let ring = vec![
            Point2::new(0.0, 0.0),
            Point2::new(1.0, 0.0),
            Point2::new(0.0, 1.0),
            Point2::new(0.0, 0.0),
        ];
        lines.push(ring.clone(), Classification::Veg406);
        lines.push(ring, Classification::Contour);
        let dxf = BinaryDxf::new(Bounds::new(0.0, 1.0, 0.0, 1.0), vec![lines.into()]);
        let mut out = Vec::new();
        dxf.to_dxf(&mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("POLYLINE\r\n 66\r\n1\r\n  8\r\n406.000\r\n 70\r\n1\r\n  0\r\n"));
        assert!(text.contains("POLYLINE\r\n 66\r\n1\r\n  8\r\n101.000\r\n  0\r\n"));
    }

    /// `.dxf.bin` stores a classification as its variant index: new variants go at the
    /// end so existing indices (and files) keep their meaning.
    #[test]
    fn classification_variant_indices_are_stable() {
        assert_eq!(Classification::ContourSimple as u8, 0);
        assert_eq!(Classification::SmallDepression as u8, 20);
        assert_eq!(Classification::Veg403 as u8, 21);
        assert_eq!(Classification::Veg410 as u8, 25);
    }

    #[test]
    fn test_classification_size_is_single_byte() {
        assert_eq!(
            std::mem::size_of::<super::Classification>(),
            1,
            "Classification should be a single byte"
        );
    }
}
