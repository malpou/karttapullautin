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

/// One contour line and the level it was traced at, so no consumer re-derives the level
/// from the line's position on the heightmap.
#[derive(Debug, Clone, PartialEq)]
pub struct Contour {
    /// The level the line was traced at, in metres: an exact multiple of the interval
    /// it was traced with.
    pub level_m: f64,
    /// The vertices; a closed ring repeats its first vertex at the end. The coordinates
    /// are the producer's: grid cells from the tracer, world metres from a contour file.
    pub line: Vec<Point2>,
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
const BINARY_DXF_VERSION: usize = 2;

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

        // Version 1 (and the crate-version strings "2.X.X" before it) stored the contour
        // kinds as eight separate classifications.
        if object.version == BINARY_DXF_VERSION.to_string() {
            Ok(object)
        } else {
            anyhow::bail!(
                "stale .dxf.bin temp file (version {}, this build reads version {BINARY_DXF_VERSION}): it was written by another build; regenerate it by re-running the job",
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

    /// A smoothed contour line of the second contour generation step (smoothjoin), with
    /// what it is drawn as.
    Contour(ContourKind),

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
    /// vegetation grids into closed rings. `.dxf.bin` stores a classification by its
    /// variant index, so reordering or removing a variant needs a new
    /// [`BINARY_DXF_VERSION`].
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
            Self::ContourSimple => IsomCode::C101_000,
            Self::Contour(kind) => kind.isom_code(),
            Self::Formline | Self::FormlineDepression => IsomCode::C103_000,
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
            Self::ContourSimple => "contour",
            Self::Contour(kind) => kind.symbol_name(),
            Self::Formline => "form line",
            Self::FormlineDepression => "depression form line",
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

    /// The kind of a smoothed contour line; None for every other classification.
    pub fn contour_kind(&self) -> Option<ContourKind> {
        match self {
            Self::Contour(kind) => Some(*kind),
            _ => None,
        }
    }

    /// Whether this is a depression contour or depression half-interval line.
    pub fn is_depression(&self) -> bool {
        self.contour_kind().is_some_and(ContourKind::depression)
    }

    /// Whether this is a half-interval line: a contour kind drawn as a form line where
    /// selected, left out of the merged contours and the vector output.
    pub fn is_half_interval_line(&self) -> bool {
        self.contour_kind().is_some_and(ContourKind::half_interval)
    }
}

/// What a smoothed contour line is drawn as: three independent flags, packed into one
/// byte so [`Classification`] stays one byte.
///
/// - `index`: its level is a multiple of the index contour interval (ISOM 102).
/// - `half_interval`: a half-interval line, halfway between two contours; drawn as a form
///   line (ISOM 103) where the renderer selects it, and left out of vector output.
/// - `depression`: it encloses lower ground.
#[derive(Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct ContourKind(KindBits);

/// The eight flag combinations of a [`ContourKind`] as an enum rather than a `u8`, so the
/// unused values are a niche the other [`Classification`] variants fit in.
#[derive(Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[repr(u8)]
enum KindBits {
    B0,
    B1,
    B2,
    B3,
    B4,
    B5,
    B6,
    B7,
}

/// How smoothjoin's traced lines map to contour kinds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ContourLevels {
    /// Vertical distance between traced lines, in metres (half the contour interval
    /// when the half-interval lines are traced too).
    pub trace_interval: f64,
    /// Levels at multiples of this are index contours, in metres.
    pub index_interval: f64,
    /// Whether every other traced line is a half-interval line: the odd multiples of
    /// `trace_interval`. Without them every traced line is a contour.
    pub half_interval_lines: bool,
}

impl ContourLevels {
    /// The kind of a line traced at `level_m` (not a depression; see
    /// [`ContourKind::with_depression`]).
    pub fn kind_at(&self, level_m: f64) -> ContourKind {
        let step = self.trace_interval;
        // the level snapped to the traced lines, so float noise cannot drop a flag
        let level = (level_m / step + 0.5).floor() * step;
        let is_multiple = |of: f64| (level / of).floor() == level / of;
        ContourKind::from_flags(
            is_multiple(self.index_interval),
            self.half_interval_lines && !is_multiple(2.0 * step),
            false,
        )
    }
}

impl ContourKind {
    const INDEX_BIT: u8 = 1;
    const HALF_INTERVAL_BIT: u8 = 2;
    const DEPRESSION_BIT: u8 = 4;

    /// A contour (ISOM 101).
    pub const CONTOUR: Self = Self::from_flags(false, false, false);
    /// An index contour (ISOM 102).
    pub const INDEX: Self = Self::from_flags(true, false, false);
    /// A half-interval line (ISOM 103 where selected).
    pub const HALF_INTERVAL: Self = Self::from_flags(false, true, false);
    /// A half-interval line at an index level; still a half-interval line (103).
    pub const INDEX_HALF_INTERVAL: Self = Self::from_flags(true, true, false);

    const fn from_flags(index: bool, half_interval: bool, depression: bool) -> Self {
        let bits = (index as u8 * Self::INDEX_BIT)
            | (half_interval as u8 * Self::HALF_INTERVAL_BIT)
            | (depression as u8 * Self::DEPRESSION_BIT);
        Self(match bits {
            0 => KindBits::B0,
            1 => KindBits::B1,
            2 => KindBits::B2,
            3 => KindBits::B3,
            4 => KindBits::B4,
            5 => KindBits::B5,
            6 => KindBits::B6,
            _ => KindBits::B7,
        })
    }

    /// This kind, enclosing lower ground where `depression` is set.
    pub const fn with_depression(self, depression: bool) -> Self {
        Self::from_flags(self.index(), self.half_interval(), depression)
    }

    const fn has(self, flag: u8) -> bool {
        self.0 as u8 & flag != 0
    }

    pub const fn index(self) -> bool {
        self.has(Self::INDEX_BIT)
    }

    pub const fn half_interval(self) -> bool {
        self.has(Self::HALF_INTERVAL_BIT)
    }

    pub const fn depression(self) -> bool {
        self.has(Self::DEPRESSION_BIT)
    }

    /// ISOM 2017-2 symbol code: a half-interval line is a form line (103), whether or not
    /// its level is also an index level; otherwise index contour (102) or contour (101).
    /// A depression shares its contour's code.
    pub fn isom_code(self) -> IsomCode {
        if self.half_interval() {
            IsomCode::C103_000
        } else if self.index() {
            IsomCode::C102_000
        } else {
            IsomCode::C101_000
        }
    }

    /// Human-readable symbol name, telling depressions apart from the contours that share
    /// their code.
    pub fn symbol_name(self) -> &'static str {
        match (self.depression(), self.isom_code()) {
            (false, IsomCode::C102_000) => "index contour",
            (false, IsomCode::C103_000) => "form line",
            (false, _) => "contour",
            (true, IsomCode::C102_000) => "depression index contour",
            (true, IsomCode::C103_000) => "depression form line",
            (true, _) => "depression contour",
        }
    }
}

impl std::fmt::Debug for ContourKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContourKind")
            .field("index", &self.index())
            .field("half_interval", &self.half_interval())
            .field("depression", &self.depression())
            .finish()
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
/// dropped lines come back empty so callers keep indexing by input position. A joined line
/// grows from the slot it is returned in, so that slot's class (a contour's level, say)
/// belongs to it.
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
        // every contour kind reaches its code through the classification
        for kind in all_contour_kinds() {
            let c = Contour(kind);
            assert_eq!(c.isom_code(), Some(kind.isom_code()), "{c:?}");
            assert_eq!(c.symbol_name(), Some(kind.symbol_name()), "{c:?}");
        }
    }

    /// The four upland kinds, then the same four as depressions.
    fn all_contour_kinds() -> impl Iterator<Item = ContourKind> {
        use super::ContourKind as K;
        [false, true].into_iter().flat_map(|depression| {
            [
                K::CONTOUR,
                K::INDEX,
                K::HALF_INTERVAL,
                K::INDEX_HALF_INTERVAL,
            ]
            .map(|k| k.with_depression(depression))
        })
    }

    #[test]
    fn contour_kind_constants_carry_their_flags() {
        use super::ContourKind as K;
        let flags = |k: K| (k.index(), k.half_interval(), k.depression());
        assert_eq!(flags(K::CONTOUR), (false, false, false));
        assert_eq!(flags(K::INDEX), (true, false, false));
        assert_eq!(flags(K::HALF_INTERVAL), (false, true, false));
        assert_eq!(flags(K::INDEX_HALF_INTERVAL), (true, true, false));
        assert_eq!(flags(K::INDEX.with_depression(true)), (true, false, true));
        assert_eq!(
            K::INDEX.with_depression(true).with_depression(false),
            K::INDEX
        );
        assert_eq!(
            all_contour_kinds()
                .collect::<std::collections::HashSet<_>>()
                .len(),
            8
        );
    }

    /// All eight flag combinations against the vendored symbol table: a half-interval
    /// line is a form line even at an index level, and a depression shares its code.
    #[test]
    fn contour_kind_isom_code_matches_the_symbol_table() {
        use super::ContourKind as K;
        use crate::isom::IsomTable;
        let expected = [
            (K::CONTOUR, "101.000", "contour"),
            (K::INDEX, "102.000", "index contour"),
            (K::HALF_INTERVAL, "103.000", "form line"),
            (K::INDEX_HALF_INTERVAL, "103.000", "form line"),
            (
                K::CONTOUR.with_depression(true),
                "101.000",
                "depression contour",
            ),
            (
                K::INDEX.with_depression(true),
                "102.000",
                "depression index contour",
            ),
            (
                K::HALF_INTERVAL.with_depression(true),
                "103.000",
                "depression form line",
            ),
            (
                K::INDEX_HALF_INTERVAL.with_depression(true),
                "103.000",
                "depression form line",
            ),
        ];
        assert_eq!(
            expected.map(|(k, ..)| k).to_vec(),
            all_contour_kinds().collect::<Vec<_>>()
        );
        for (kind, code, name) in expected {
            assert_eq!(kind.isom_code(), code.parse().unwrap(), "{kind:?}");
            assert_eq!(kind.isom_code().table(), IsomTable::Contours, "{kind:?}");
            assert_eq!(kind.symbol_name(), name, "{kind:?}");
        }
    }

    /// The kinds smoothjoin gives a traced level: 1.25 m between traced lines (a 2.5 m
    /// contour interval), index contours every 12.5 m.
    #[test]
    fn contour_kind_flags_follow_the_index_and_half_interval_multiples() {
        use super::ContourKind as K;
        let levels = ContourLevels {
            trace_interval: 1.25,
            index_interval: 12.5,
            half_interval_lines: true,
        };
        // contours at the multiples of 2.5 m, half-interval lines halfway between
        assert_eq!(levels.kind_at(10.0), K::CONTOUR);
        assert_eq!(levels.kind_at(11.25), K::HALF_INTERVAL);
        assert_eq!(levels.kind_at(0.0), K::INDEX);
        assert_eq!(levels.kind_at(12.5), K::INDEX);
        assert_eq!(levels.kind_at(25.0), K::INDEX);
        assert_eq!(levels.kind_at(-12.5), K::INDEX);
        assert_eq!(levels.kind_at(-1.25), K::HALF_INTERVAL);
        // float noise around a traced level snaps to it
        assert_eq!(levels.kind_at(12.5 + 1e-9), K::INDEX);
        assert_eq!(levels.kind_at(11.25 - 1e-9), K::HALF_INTERVAL);
        // every traced line is a contour when there are no half-interval lines
        let no_half = ContourLevels {
            half_interval_lines: false,
            ..levels
        };
        assert_eq!(no_half.kind_at(11.25), K::CONTOUR);
        assert_eq!(no_half.kind_at(12.5), K::INDEX);
        // an index level that is also an odd multiple is both (drawn as a form line)
        let odd_index = ContourLevels {
            index_interval: 3.75,
            ..levels
        };
        assert_eq!(odd_index.kind_at(3.75), K::INDEX_HALF_INTERVAL);
        assert_eq!(K::INDEX_HALF_INTERVAL.isom_code(), IsomCode::C103_000);
    }

    /// The `.dxf.bin` encoding of a few classifications, pinned: `.dxf.bin` stores a
    /// classification by its variant index (and a contour's kind after it), so a
    /// reordered, added-in-the-middle or removed variant changes these bytes. Bump
    /// [`BINARY_DXF_VERSION`] when this changes, then update the bytes and the version.
    #[test]
    fn classification_encoding_is_pinned_to_the_binary_dxf_version() {
        use super::Classification::*;
        use super::ContourKind as K;
        let bytes = |c: Classification| {
            let mut out = Vec::new();
            crate::util::write_object(&mut out, &c).unwrap();
            out
        };
        assert_eq!(BINARY_DXF_VERSION, 2);
        assert_eq!(bytes(ContourSimple), [0]);
        assert_eq!(bytes(Contour(K::INDEX)), [1, 1]);
        assert_eq!(
            bytes(Contour(K::HALF_INTERVAL.with_depression(true))),
            [1, 6]
        );
        assert_eq!(bytes(Cliff3), [10]);
        assert_eq!(bytes(Veg410), [18]);
    }

    /// A contour kind survives the `.dxf.bin` round trip.
    #[test]
    fn contour_kinds_round_trip_through_binary_dxf() {
        let mut lines = Polylines::new();
        for kind in all_contour_kinds() {
            lines.push(
                vec![Point3::new(0.0, 0.0, 1.0)],
                (Classification::Contour(kind), 1.0),
            );
        }
        let dxf = BinaryDxf::new(Bounds::new(0.0, 1.0, 0.0, 1.0), vec![lines.into()]);
        let mut bytes = Vec::new();
        dxf.to_writer(&mut bytes).unwrap();
        let back = BinaryDxf::from_reader(&mut bytes.as_slice()).unwrap();
        let Geometry::Polylines3(back) = back.take_geometry().swap_remove(0) else {
            panic!("not 3D polylines");
        };
        let kinds: Vec<_> = back.classification.iter().map(|(c, _)| *c).collect();
        let expected: Vec<_> = all_contour_kinds().map(Classification::Contour).collect();
        assert_eq!(kinds, expected);
    }

    #[test]
    fn ugly_and_depression_line_flags() {
        use super::Classification::*;
        assert!(UglyDotknoll.is_ugly() && UglyUdepression.is_ugly());
        assert!(!Dotknoll.is_ugly() && !Udepression.is_ugly());
        assert!(
            FormlineDepression.is_depression_line()
                && Contour(ContourKind::INDEX_HALF_INTERVAL.with_depression(true))
                    .is_depression_line()
        );
        assert!(
            !Formline.is_depression_line() && !Contour(ContourKind::CONTOUR).is_depression_line()
        );
    }

    #[test]
    fn dxf_layers_are_symbol_codes_and_skip_knoll_detector_artifact() {
        let mut lines = Polylines::new();
        let line = vec![Point2::new(0.0, 0.0), Point2::new(1.0, 1.0)];
        lines.push(line.clone(), Classification::Contour(ContourKind::INDEX));
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
        lines.push(ring, Classification::Contour(ContourKind::CONTOUR));
        let dxf = BinaryDxf::new(Bounds::new(0.0, 1.0, 0.0, 1.0), vec![lines.into()]);
        let mut out = Vec::new();
        dxf.to_dxf(&mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("POLYLINE\r\n 66\r\n1\r\n  8\r\n406.000\r\n 70\r\n1\r\n  0\r\n"));
        assert!(text.contains("POLYLINE\r\n 66\r\n1\r\n  8\r\n101.000\r\n  0\r\n"));
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
