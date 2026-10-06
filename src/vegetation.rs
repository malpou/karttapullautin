use image::{DynamicImage, GrayAlphaImage, GrayImage, Luma, LumaA};
use imageproc::drawing::{Canvas, draw_filled_circle_mut, draw_line_segment_mut};
use imageproc::filter::median_filter;
use imageproc::rect::Rect;
use log::info;
use std::error::Error;
use std::f32::consts::SQRT_2;
use std::path::Path;

use crate::geometry::Bounds;
use crate::io::fs::FileSystem;
use crate::io::heightmap::HeightMap;
use crate::io::xyz::{LasClass, XyzRecord};
use crate::mapframe::{MapFrame, WorldFile};
use crate::palette::{Palette, PaletteColorEnum, PalettedImage};
use crate::vec2d::Vec2D;

/// Parameters of [`makevege`], which classifies the returns into vegetation, open land
/// and undergrowth and draws the vegetation rasters.
#[derive(Debug, Clone, PartialEq)]
pub struct VegetationParams {
    /// The sheet the undergrowth raster is drawn at (ini `mapscale`).
    pub frame: MapFrame,
    /// Also write the one-channel `*_bit.png` rasters (ini `vege_bitmode`).
    pub vege_bitmode: bool,
    /// LAS class of water returns (ini `waterclass`), drawn blue with `water_blue`.
    pub water_class: u8,
    /// Draw the `water_class` returns blue in `blueblack.png` (ini `water_blue`).
    pub water_blue: bool,

    // green: returns counted per `greendetectsize` cell
    /// Height bands above ground whose returns count towards green; the first matching
    /// stratum counts (ini `stratum1`, `stratum2`, ...).
    pub strata: Vec<Stratum>,
    /// Per band of canopy height (`roof_low..roof_high` m): the green-to-ground ratio at
    /// which the green factor is 1 (ini `threshold1`, ..., each `low|high|ratio`).
    pub thresholds: Vec<(f64, f64, f64)>,
    /// Green factor at which each shade is drawn, lightest first (ini `greenshades`).
    pub greenshades: Vec<f64>,
    /// Red and blue of the lightest green shade, 0-255 (ini `lightgreentone`).
    pub greentone: f64,
    /// Returns less than this many metres above ground count as ground (ini `greenground`).
    pub greenground: f64,
    /// Returns more than this many metres above ground count as high hits (ini `greenhigh`).
    pub greenhigh: f64,
    /// Weight of the high-hit share in the green factor, 0-1 (ini `topweight`).
    pub topweight: f64,
    /// Point density balancing: the factor is `(1 - pointvolumefactor * density /
    /// average density) ^ pointvolumeexponent` (ini `pointvolumefactor`); 0 is off.
    pub pointvolumefactor: f64,
    /// The exponent of that balancing (ini `pointvolumeexponent`).
    pub pointvolumeexponent: f64,
    /// Metres subtracted from every return's height before the green count (ini
    /// `vegezoffset`).
    pub vegezoffset: f64,
    /// Pixels added to each side of a green cell's square (ini `greendotsize`).
    pub addition: i32,
    /// Ground hits a single-return ground point counts as (ini
    /// `firstandlastreturnasground`).
    pub firstandlastreturnasground: u32,
    /// Green weight of a single return below 5 m, usually a boulder (ini
    /// `firstandlastreturnfactor`).
    pub firstandlastfactor: f64,
    /// Green weight of a last return (ini `lastreturnfactor`).
    pub lastfactor: f64,
    /// Use every n-th return only; 0 or 1 uses all (ini `vegethin`).
    pub vegethin: u32,
    /// Side of the green cell in metres (ini `greendetectsize`).
    pub greendetectsize: f64,
    /// Median filter box sizes of the green raster, two rounds; 1 or less is off (ini
    /// `medianboxsize`, `medianboxsize2`).
    pub med: u32,
    pub med2: u32,

    // yellow: returns counted per 3 m cell
    /// Returns less than this many metres above ground count as open (ini `yellowheight`).
    pub yellowheight: f64,
    /// Share of open returns above which a cell is open land (ini `yellowthreshold`).
    pub yellowthreshold: f64,
    /// Non-open hits a single return counts as (ini `yellowfirstlast`).
    pub yellowfirstlast: u32,
    /// Filter the yellow raster with the green's median boxes (ini `yellow_smoothing`).
    pub proceed_yellows: bool,
    /// Median filter box size of the yellow raster without `proceed_yellows`; 1 or less
    /// is off (ini `yellowmedianboxsize`).
    pub medyellow: u32,

    // undergrowth: returns 0.25-1.2 m above ground, per 6 green cells
    /// Undergrowth share above which normal undergrowth is drawn (ini `undergrowth`).
    pub uglimit: f64,
    /// Undergrowth share above which undergrowth walk is drawn (ini `undergrowth2`).
    pub uglimit2: f64,

    // blueblack.png
    /// LAS class drawn black as buildings; 0 is off (ini `buildingsclass`).
    pub buildings: u8,
    /// Ground below this elevation in metres is drawn blue (ini `waterelevation`).
    pub waterele: f64,
}

/// A height band above ground whose returns count towards green (ini `stratum{i}` =
/// `low|high|roof|factor`).
#[derive(Debug, Clone, PartialEq)]
pub struct Stratum {
    /// Lower bound in metres above ground, inclusive.
    pub low: f64,
    /// Upper bound in metres above ground, exclusive.
    pub high: f64,
    /// Counts only where the tallest return in the cell is lower than this, in metres
    /// above ground.
    pub roof: f64,
    /// Green hits a return in this stratum counts as.
    pub factor: f64,
}

/// How the model's classes are drawn, taken from [`VegetationParams`] and carried with
/// the model: the vegetation rasters draw with all of them, the vector export with the
/// median boxes and `uglimit`.
#[derive(Debug, Clone, PartialEq)]
pub struct VegetationDrawing {
    /// Median filter box sizes of the green shades, two rounds; 1 or less is off (ini
    /// `medianboxsize`, `medianboxsize2`).
    pub med: u32,
    pub med2: u32,
    /// Median filter box size of the open land without `proceed_yellows`; 1 or less is
    /// off (ini `yellowmedianboxsize`).
    pub medyellow: u32,
    /// Filter the open land with the green shades' median boxes (ini `yellow_smoothing`).
    pub proceed_yellows: bool,
    /// Undergrowth share above which undergrowth is drawn (ini `undergrowth`).
    pub uglimit: f64,
    /// Undergrowth share above which undergrowth walk is drawn (ini `undergrowth2`).
    pub uglimit2: f64,
}

impl VegetationDrawing {
    fn new(params: &VegetationParams) -> Self {
        Self {
            med: params.med,
            med2: params.med2,
            medyellow: params.medyellow,
            proceed_yellows: params.proceed_yellows,
            uglimit: params.uglimit,
            uglimit2: params.uglimit2,
        }
    }
}

/// What `blueblack.png` draws at one pixel.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum WaterOrBuilding {
    #[default]
    Neither,
    /// A return of the `buildingsclass` class nearby, drawn black.
    Building,
    /// A return of the water class nearby (`water_blue`), or ground below
    /// `waterelevation`, drawn blue.
    Water,
}

/// The tile's vegetation, classified per cell by [`makevege`]: the input of
/// [`rasterise_vegetation`] and the vector export.
#[derive(Debug, Clone)]
pub struct VegetationModel {
    /// Greenshade index (1-based, 0 = none) per `block` cell, from the south-west corner.
    pub green: Vec2D<u8>,
    /// 1 where there is open land, per 3 m cell, from the south-west corner.
    pub open_land: Vec2D<u8>,
    /// Undergrowth share per square of six by six green cells (`block * 6` metres), from
    /// the south-west corner: the non-ground returns 0.25-1.2 m above ground over all
    /// returns, those higher than 1.2 m weighing 0.05; undergrowth is drawn where it is
    /// above [`VegetationDrawing::uglimit`], undergrowth walk above `uglimit2`.
    pub undergrowth: Vec2D<f64>,
    /// Per pixel of the vegetation raster ([`VegetationFrame`]), from the north-west
    /// corner: the buildings and water drawn in `blueblack.png`, each return and each low
    /// ground cell as the 3x3 pixels around it.
    pub water_buildings: Vec2D<WaterOrBuilding>,
    /// The ground model's extent.
    pub bounds: Bounds,
    /// Side of the green cell in metres (`greendetectsize`).
    pub block: f64,
    pub drawing: VegetationDrawing,
}

impl VegetationModel {
    /// The frame of the vegetation raster drawn from this model.
    pub fn frame(&self) -> VegetationFrame {
        VegetationFrame::new(&self.bounds, self.block)
    }

    /// 1 where undergrowth is drawn, per `block * 6` cell (the share above
    /// [`VegetationDrawing::uglimit`]).
    pub fn undergrowth_class(&self) -> Vec2D<u8> {
        let mut class = Vec2D::new(self.undergrowth.width(), self.undergrowth.height(), 0u8);
        for (x, y, share) in self.undergrowth.iter() {
            if share > self.drawing.uglimit {
                class[(x, y)] = 1;
            }
        }
        class
    }
}

/// The frame of the vegetation raster (`vegetation.png`/`.pgw`): one pixel per ground
/// metre, north up, with its origin at the ground model's north-west corner. It covers
/// whole green cells, so it can reach past the ground model's east and south edges.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VegetationFrame {
    pub x_origin: f64,
    pub y_origin: f64,
    /// Width in pixels (metres).
    pub width: u32,
    /// Height in pixels (metres).
    pub height: u32,
}

impl VegetationFrame {
    /// The frame of the ground model extent `bounds` covered by green cells of `block`
    /// metres.
    pub fn new(bounds: &Bounds, block: f64) -> Self {
        let w_block = ((bounds.xmax - bounds.xmin) / block).ceil() as usize;
        let h_block = ((bounds.ymax - bounds.ymin) / block).ceil() as usize;
        Self {
            x_origin: bounds.xmin,
            y_origin: bounds.ymax,
            width: (w_block as f64 * block) as u32,
            height: (h_block as f64 * block) as u32,
        }
    }

    /// The frame [`makevege`] gives the model of `ground`.
    pub fn of_ground(ground: &HeightMap, params: &VegetationParams) -> Self {
        Self::new(&ground_bounds(ground), params.greendetectsize)
    }

    /// The raster's world file.
    pub fn world_file(&self) -> WorldFile {
        WorldFile::north_up(1.0, self.x_origin, self.y_origin)
    }
}

fn ground_bounds(ground: &HeightMap) -> Bounds {
    Bounds::new(ground.minx(), ground.maxx(), ground.miny(), ground.maxy())
}

/// Classify the tile's `returns` into green shades, open land, undergrowth and the
/// water and buildings of `blueblack.png`. `returns` are in file order: `vegethin` keeps
/// every n-th; their heights above ground are taken from the `ground` model.
pub fn makevege(
    ground: &HeightMap,
    returns: &[XyzRecord],
    params: &VegetationParams,
) -> VegetationModel {
    info!("Generating vegetation...");

    // in world coordinates
    let size = ground.scale;
    let xyz = &ground.grid;

    let thresholds = &params.thresholds;
    let block = params.greendetectsize;

    let &VegetationParams {
        yellowheight,
        yellowthreshold,
        greenground,
        pointvolumefactor,
        pointvolumeexponent,
        greenhigh,
        topweight,
        vegezoffset: zoffset,
        firstandlastreturnasground,
        firstandlastfactor,
        lastfactor,
        yellowfirstlast,
        vegethin,
        ..
    } = params;
    let greenshades = &params.greenshades;

    let bounds = ground_bounds(ground);
    let Bounds {
        xmin,
        xmax,
        ymin,
        ymax,
    } = bounds;
    // xmax/ymax are always slightly superior to the max x/y values within the ground model
    // this mean (xmax - xmin).ceil() > (x - xmin).floor() is always true
    // for more detail why, check the xyz2heightmap function and the heightmap.rs file

    // here we overlay two other grids on top of the heightmap, but with the same origin
    let w_block = ((xmax - xmin) / block).ceil() as usize;
    let h_block = ((ymax - ymin) / block).ceil() as usize;

    let w_3 = ((xmax - xmin) / 3.0).ceil() as usize;
    let h_3 = ((ymax - ymin) / 3.0).ceil() as usize;

    let mut top = Vec2D::new(w_block, h_block, 0.0); // block
    let mut yhit = Vec2D::new(w_3, h_3, 0_u32); // 3.0
    let mut noyhit = Vec2D::new(w_3, h_3, 0_u32); // 3.0

    for (i, r) in returns.iter().enumerate() {
        if vegethin == 0 || ((i + 1) as u32).is_multiple_of(vegethin) {
            let x: f64 = r.x;
            let y: f64 = r.y;
            let h: f64 = r.z as f64;
            let r3 = r.class();
            let r4 = r.number_of_returns;
            let r5 = r.return_number;

            let xx = ((x - xmin) / block) as usize;
            let yy = ((y - ymin) / block) as usize;
            let t = &mut top[(xx, yy)];
            if h > *t {
                *t = h;
            }
            let xx = ((x - xmin) / 3.0) as usize;
            let yy = ((y - ymin) / 3.0) as usize;

            if r3 == LasClass::Ground
                || h < yellowheight
                    + xyz[(((x - xmin) / size) as usize, ((y - ymin) / size) as usize)]
            {
                yhit[(xx, yy)] += 1;
            } else if r4 == 1 && r5 == 1 {
                noyhit[(xx, yy)] += yellowfirstlast;
            } else {
                noyhit[(xx, yy)] += 1;
            }
        }
    }
    // rebind the variables to be non-mut for the rest of the function
    let (top, yhit, noyhit) = (top, yhit, noyhit);

    let mut firsthit = Vec2D::new(w_block, h_block, 0_u32); // block
    let mut ghit = Vec2D::new(w_block, h_block, 0_u32); // block
    let mut greenhit = Vec2D::new(w_block, h_block, 0_f32); // block
    let mut highit = Vec2D::new(w_block, h_block, 0_u32); // block

    let w_block_step = ((xmax - xmin) / (block * UNDERGROWTH_STEP as f64)).ceil() as usize;
    let h_block_step = ((ymax - ymin) / (block * UNDERGROWTH_STEP as f64)).ceil() as usize;

    #[derive(Default, Clone)]
    struct UggItem {
        ugg: f32,
        ug: u32,
    }
    let mut ug = Vec2D::new(w_block_step, h_block_step, UggItem::default()); // block / step

    for (i, r) in returns.iter().enumerate() {
        if vegethin == 0 || ((i + 1) as u32).is_multiple_of(vegethin) {
            let x: f64 = r.x;
            let y: f64 = r.y;
            let h: f64 = r.z as f64 - zoffset;
            let r3 = r.class();
            let r4 = r.number_of_returns;
            let r5 = r.return_number;

            if r5 == 1 {
                let xx = ((x - xmin) / block) as usize;
                let yy = ((y - ymin) / block) as usize;
                firsthit[(xx, yy)] += 1;
            }

            // linear interpolation of the height at the point based on the surrpoinding cells in the heightmap
            let thelele = {
                let xx = ((x - xmin) / size) as usize;
                let yy = ((y - ymin) / size) as usize;

                let a = xyz[(xx, yy)];

                // if we are on the edge, simply extend the values
                let (b, c, d) = if xx < xyz.width() - 1 && yy < xyz.height() - 1 {
                    // inside, take all values
                    (xyz[(xx + 1, yy)], xyz[(xx, yy + 1)], xyz[(xx + 1, yy + 1)])
                } else if xx < xyz.width() - 1 {
                    // on bottom edge, extend downwards
                    (xyz[(xx + 1, yy)], a, a)
                } else if yy < xyz.height() - 1 {
                    // on right edge, extend to the right
                    (a, xyz[(xx, yy + 1)], a)
                } else {
                    // in corner, use this height for all
                    (a, a, a)
                };

                let distx = (x - xmin) / size - xx as f64;
                let disty = (y - ymin) / size - yy as f64;

                // linear interpolation of the elevation at the point
                let ab = a * (1.0 - distx) + b * distx;
                let cd = c * (1.0 - distx) + d * distx;
                ab * (1.0 - disty) + cd * disty
            };

            let xx = ((x - xmin) / block / (UNDERGROWTH_STEP as f64)) as usize;
            let yy = ((y - ymin) / block / (UNDERGROWTH_STEP as f64)) as usize;
            let hh = h - thelele;
            let ug_entry = &mut ug[(xx, yy)];
            if hh <= 1.2 {
                if r3 == LasClass::Ground {
                    ug_entry.ugg += 1.0;
                } else if hh > 0.25 {
                    ug_entry.ug += 1;
                } else {
                    ug_entry.ugg += 1.0;
                }
            } else {
                ug_entry.ugg += 0.05;
            }

            let xx = ((x - xmin) / block) as usize;
            let yy = ((y - ymin) / block) as usize;
            if r3 == LasClass::Ground || greenground >= hh {
                if r4 == 1 && r5 == 1 {
                    ghit[(xx, yy)] += firstandlastreturnasground;
                } else {
                    ghit[(xx, yy)] += 1;
                }
            } else {
                let mut last = 1.0;
                if r4 == r5 {
                    last = lastfactor;
                    if hh < 5.0 {
                        last = firstandlastfactor;
                    }
                }

                // NOTE: the use of top here means that we cannot combine the two processing loops into one
                let top_val = top[(xx, yy)];
                for &Stratum {
                    low,
                    high,
                    roof,
                    factor,
                } in params.strata.iter()
                {
                    if hh >= low && hh < high && top_val - thelele < roof {
                        greenhit[(xx, yy)] += (factor * last) as f32;
                        break;
                    }
                }

                if greenhigh < hh {
                    highit[(xx, yy)] += 1;
                }
            }
        }
    }
    // rebind the variables to be non-mut for the rest of the function
    let (firsthit, ug, ghit, greenhit, highit) = (firsthit, ug, ghit, greenhit, highit);

    // per 3 m cell: 1 where open land is drawn
    let mut open_land = Vec2D::new(w_3, h_3, 0u8);
    for x in 0..(w_3 - 2) {
        for y in 0..(h_3 - 2) {
            let mut ghit2 = 0;
            let mut highhit2 = 0;

            // sum in a 2x2 area
            for i in x..x + 2 {
                for j in y..y + 2 {
                    ghit2 += yhit[(i, j)];
                    highhit2 += noyhit[(i, j)];
                }
            }
            if ghit2 as f64 / (highhit2 as f64 + ghit2 as f64 + 0.01) > yellowthreshold {
                open_land[(x, y)] = 1;
            }
        }
    }

    // compute global average firsthit
    let aveg = {
        let mut aveg = 0;
        let mut avecount = 0;

        for x in 0..w_block {
            for y in 0..h_block {
                if ghit[(x, y)] > 1 {
                    aveg += firsthit[(x, y)];
                    avecount += 1;
                }
            }
        }
        aveg as f64 / avecount as f64
    };

    // per block cell: the greenshade index (1-based, 0 = none)
    let mut green = Vec2D::new(w_block, h_block, 0u8);
    for x in 0..w_block {
        for y in 0..h_block {
            let roof = top[(x, y)]
                - xyz[(
                    (x as f64 * block / size) as usize,
                    (y as f64 * block / size) as usize,
                )];

            // find lowest firsthit in a 5x5 area
            let mut firsthit2 = firsthit[(x, y)];
            for i in x.saturating_sub(2)..(x + 3).min(w_block) {
                for j in y.saturating_sub(2)..(y + 3).min(h_block) {
                    let value = firsthit[(i, j)];
                    if value < firsthit2 {
                        firsthit2 = value;
                    }
                }
            }

            let greenhit2 = greenhit[(x, y)] as f64;
            let highit2 = highit[(x, y)];
            let ghit2 = ghit[(x, y)];

            let mut greenlimit = 9999.0;
            for &(v0, v1, v2) in thresholds.iter() {
                if roof >= v0 && roof < v1 {
                    greenlimit = v2;
                    break;
                }
            }

            let thevalue = greenhit2 / (ghit2 as f64 + greenhit2 + 1.0)
                * (1.0 - topweight
                    + topweight * highit2 as f64
                        / (ghit2 as f64 + greenhit2 + highit2 as f64 + 1.0))
                * (1.0 - pointvolumefactor * firsthit2 as f64 / (aveg + 0.00001))
                    .powf(pointvolumeexponent);
            if thevalue > 0.0 {
                let mut greenshade = 0;
                for (i, &shade) in greenshades.iter().enumerate() {
                    if thevalue > greenlimit * shade {
                        greenshade = i + 1;
                    }
                }
                if greenshade > 0 {
                    green[(x, y)] = greenshade as u8;
                }
            }
        }
    }

    // per block*step cell: the share of undergrowth returns
    let mut undergrowth = Vec2D::new(w_block_step, h_block_step, 0.0);
    for (x, y, share) in undergrowth.iter_mut() {
        let ug_entry = &ug[(x, y)];
        *share = ug_entry.ug as f64 / (ug_entry.ug as f64 + ug_entry.ugg as f64 + 0.01);
    }

    let frame = VegetationFrame::new(&bounds, block);
    let water_buildings = water_and_buildings(ground, returns, params, frame);

    info!("Done");
    VegetationModel {
        green,
        open_land,
        undergrowth,
        water_buildings,
        bounds,
        block,
        drawing: VegetationDrawing::new(params),
    }
}

/// Undergrowth cells are this many green cells wide.
const UNDERGROWTH_STEP: f32 = 6.0;

/// The pixels of `frame` around each building or water return (with `water_blue`), and
/// then around each ground cell below `waterele`: 3x3 pixels each, clipped to the frame,
/// a later one drawn over an earlier one.
fn water_and_buildings(
    ground: &HeightMap,
    returns: &[XyzRecord],
    params: &VegetationParams,
    frame: VegetationFrame,
) -> Vec2D<WaterOrBuilding> {
    let (xmin, ymax) = (frame.x_origin, frame.y_origin);
    let mut grid = Vec2D::new(
        frame.width as usize,
        frame.height as usize,
        WaterOrBuilding::Neither,
    );
    let mut draw = |x: f64, y: f64, what: WaterOrBuilding| {
        // the 3x3 square around the pixel (x - xmin, ymax - y), truncated
        let (px, py) = ((x - xmin) as i32, (ymax - y) as i32);
        for i in px.saturating_sub(1)..=px.saturating_add(1) {
            for j in py.saturating_sub(1)..=py.saturating_add(1) {
                if i >= 0 && j >= 0 && (i as u32) < frame.width && (j as u32) < frame.height {
                    grid[(i as usize, j as usize)] = what;
                }
            }
        }
    };

    let buildings = params.buildings;
    let water = params.water_blue.then_some(params.water_class);
    if buildings > 0 || water.is_some() {
        for r in returns {
            let c: u8 = r.classification;
            if buildings > 0 && c == buildings {
                draw(r.x, r.y, WaterOrBuilding::Building);
            }
            if Some(c) == water {
                draw(r.x, r.y, WaterOrBuilding::Water);
            }
        }
    }
    for (x, y, hh) in ground.iter() {
        if hh < params.waterele {
            draw(x, y, WaterOrBuilding::Water);
        }
    }
    grid
}

/// The rasters [`rasterise_vegetation`] draws from a [`VegetationModel`].
pub struct VegetationRasters {
    /// The green shades (`greens.png`, debug).
    pub greens: PalettedImage,
    /// The open land (`yellow.png`, debug).
    pub open_land: PalettedImage,
    /// The open land drawn over the green shades (`vegetation.png`).
    pub vegetation: PalettedImage,
    /// The undergrowth at the sheet's pixels (`undergrowth.png`).
    pub undergrowth: PalettedImage,
    /// One-channel undergrowth, 1 normal, 2 walk (`undergrowth_bit.png`, a product with
    /// `vege_bitmode`).
    pub undergrowth_bit: GrayImage,
    /// The water and buildings (`blueblack.png`, debug; the map draws it from
    /// [`MapRasters::layers`]).
    pub blueblack: PalettedImage,
    /// With `vege_bitmode`: the one-channel green shades (`greens_bit.png`, debug), open
    /// land (`yellow_bit.png`, debug) and both (`vegetation_bit.png`).
    pub bits: Option<VegetationBits>,
    /// The frame of `greens`, `open_land`, `vegetation` and `blueblack`
    /// (`vegetation.pgw`).
    pub frame: VegetationFrame,
    /// The frame of `undergrowth` (`undergrowth.pgw`).
    pub undergrowth_world: WorldFile,
    palette: Palette,
}

impl VegetationRasters {
    /// The rasters the map is drawn on, dropping the rest.
    pub fn into_map_rasters(self) -> MapRasters {
        MapRasters {
            vegetation: self.vegetation,
            undergrowth: self.undergrowth,
            blueblack: self.blueblack,
            frame: self.frame,
            palette: self.palette,
        }
    }
}

/// The vegetation rasters the map is drawn on, paletted, as [`VegetationRasters`] drew
/// them.
pub struct MapRasters {
    vegetation: PalettedImage,
    undergrowth: PalettedImage,
    blueblack: PalettedImage,
    frame: VegetationFrame,
    palette: Palette,
}

impl MapRasters {
    /// The layers the map is drawn on: `vegetation`, `undergrowth` and `blueblack` in the
    /// colours their PNGs decode to.
    pub fn layers(&self) -> crate::render::VegetationLayers {
        crate::render::VegetationLayers {
            vegetation: self.vegetation.to_rgba(&self.palette),
            undergrowth: self.undergrowth.to_rgba(&self.palette),
            water_buildings: Some(self.blueblack.to_rgba(&self.palette)),
            world: self.frame.world_file(),
        }
    }
}

/// The one-channel vegetation rasters of `vege_bitmode`.
pub struct VegetationBits {
    /// 0 none, 2 for the first green shade, 3 for the second, ...
    pub greens: GrayImage,
    /// 1 where there is open land, otherwise transparent.
    pub open_land: GrayAlphaImage,
    /// `open_land` over `greens`.
    pub vegetation: DynamicImage,
}

/// Draw the vegetation rasters of `model`; `params` gives the green shades' colours and
/// squares, `vege_bitmode` and the sheet the undergrowth is drawn at.
pub fn rasterise_vegetation(
    model: &VegetationModel,
    params: &VegetationParams,
) -> VegetationRasters {
    let palette = Palette::new(params);
    let block = model.block;
    let addition = params.addition;
    let VegetationDrawing {
        med,
        med2,
        medyellow,
        proceed_yellows,
        uglimit,
        uglimit2,
    } = model.drawing;
    let frame = model.frame();
    let (img_width, img_height) = (frame.width, frame.height);

    // render yellow as multiple small squares
    let mut imgye2 = PalettedImage::new(
        img_width,
        img_height,
        PaletteColorEnum::BackgroundWhite.to_color(),
    );
    let h_3 = model.open_land.height();
    for (x, y, open) in model.open_land.iter() {
        if open == 1 {
            imgye2.draw_filled_rect(
                Rect::at(x as i32 * 3 + 2, (h_3 as i32 - y as i32) * 3 - 3).of_size(3, 3),
                PaletteColorEnum::Yellow2.to_color(),
            );
        }
    }

    let mut imggr1 = PalettedImage::new(
        img_width,
        img_height,
        PaletteColorEnum::BackgroundWhite.to_color(),
    );
    let (w_block, h_block) = (model.green.width(), model.green.height());
    // column by column: a square can overlap its neighbours
    for (x, y, greenshade) in model.green.iter() {
        if greenshade > 0 {
            imggr1.draw_filled_rect(
                Rect::at(
                    ((x as f64 - 0.5) * block) as i32 - addition,
                    (((h_block as f64 - y as f64) - 0.5) * block) as i32 - addition,
                )
                .of_size(
                    (block as i32 + addition) as u32,
                    (block as i32 + addition) as u32,
                ),
                PaletteColorEnum::GreenShade(greenshade - 1).to_color(),
            );
        }
    }

    if med > 1 {
        imggr1 = imggr1.median_filter(med / 2, med / 2);
    }
    if med2 > 1 {
        imggr1 = imggr1.median_filter(med2 / 2, med2 / 2);
    }
    if proceed_yellows {
        if med > 1 {
            imgye2 = imgye2.median_filter(med / 2, med / 2);
        }
        if med2 > 1 {
            imgye2 = imgye2.median_filter(med2 / 2, med2 / 2);
        }
    } else if medyellow > 1 {
        imgye2 = imgye2.median_filter(medyellow / 2, medyellow / 2);
    }

    let bits = params.vege_bitmode.then(|| {
        // create a new Luma image initialized with Zeros
        let mut g_img = GrayImage::new(imggr1.width(), imggr1.height());

        // modify pixels of the new image based on the green shades
        for (pixel, out) in imggr1.pixels().zip(g_img.pixels_mut()) {
            // if pixel is background, just skip (already initialized to 0)
            if *pixel == PaletteColorEnum::BackgroundWhite.to_color() {
                continue;
            }

            // else, find the corresponding green shade and set the output pixel
            for idx in 0..params.greenshades.len() {
                if *pixel == PaletteColorEnum::GreenShade(idx as u8).to_color() {
                    // index starts at 2 for the first green tone
                    *out = Luma([idx as u8 + 2]);
                }
            }
        }

        let mut y_img = GrayAlphaImage::new(imgye2.width(), imgye2.height());
        for (pixel, out) in imgye2.pixels().zip(y_img.pixels_mut()) {
            if *pixel == PaletteColorEnum::Yellow2.to_color() {
                *out = LumaA([1, 255]);
            }
        }

        // overlay the two bit images (yellow on top of green)
        let mut img_bit = DynamicImage::ImageLuma8(g_img.clone());
        let img_bit2 = DynamicImage::ImageLumaA8(y_img.clone());
        image::imageops::overlay(&mut img_bit, &img_bit2, 0, 0);
        VegetationBits {
            greens: g_img,
            open_land: y_img,
            vegetation: img_bit,
        }
    });

    // overlay yellow on top of green as the total vegetation image
    let mut vegetation = imggr1.clone();
    vegetation.overlay(&imgye2, 0, 0);

    let mut blueblack = PalettedImage::new(
        img_width,
        img_height,
        PaletteColorEnum::BackgroundWhite.to_color(),
    );
    for (x, y, what) in model.water_buildings.iter() {
        let color = match what {
            WaterOrBuilding::Neither => continue,
            WaterOrBuilding::Building => PaletteColorEnum::Black,
            WaterOrBuilding::Water => PaletteColorEnum::Blue,
        };
        blueblack.draw_pixel(x as u32, y as u32, color.to_color());
    }

    let sheet = params.frame;

    // factor to convert from coordinates to pixels
    let tmpfactor = sheet.px_per_metre() as f32;

    let step = UNDERGROWTH_STEP;
    let bf32 = block as f32;
    let hf32 = h_block as f32;
    let ww = w_block as f32 * bf32;
    let hh = hf32 * bf32;
    let mut x = 0.0_f32;

    let mut imgug = PalettedImage::new(
        sheet.to_px(w_block as f64 * block) as u32,
        sheet.to_px(h_block as f64 * block) as u32,
        PaletteColorEnum::Transparent.to_color(),
    );
    let mut img_ug_bit = GrayImage::from_pixel(
        sheet.to_px(w_block as f64 * block) as u32,
        sheet.to_px(h_block as f64 * block) as u32,
        Luma([0x00]),
    );
    loop {
        if x >= ww {
            break;
        }
        let mut y = 0.0_f32;
        loop {
            if y >= hh {
                break;
            }
            let xx = (x / bf32 / step) as usize;
            let yy = (y / bf32 / step) as usize;

            let value = model.undergrowth[(xx, yy)];
            if value > uglimit {
                draw_line_segment_mut(
                    &mut imgug,
                    (
                        tmpfactor * (x + bf32 * 3.0),
                        tmpfactor * (hf32 * bf32 - y - bf32 * 3.0),
                    ),
                    (
                        tmpfactor * (x + bf32 * 3.0),
                        tmpfactor * (hf32 * bf32 - y + bf32 * 3.0),
                    ),
                    PaletteColorEnum::Undergrowth.to_color(),
                );
                draw_line_segment_mut(
                    &mut imgug,
                    (
                        tmpfactor * (x + bf32 * 3.0) + 1.0,
                        tmpfactor * (hf32 * bf32 - y - bf32 * 3.0),
                    ),
                    (
                        tmpfactor * (x + bf32 * 3.0) + 1.0,
                        tmpfactor * (hf32 * bf32 - y + bf32 * 3.0),
                    ),
                    PaletteColorEnum::Undergrowth.to_color(),
                );
                draw_line_segment_mut(
                    &mut imgug,
                    (
                        tmpfactor * (x - bf32 * 3.0),
                        tmpfactor * (hf32 * bf32 - y - bf32 * 3.0),
                    ),
                    (
                        tmpfactor * (x - bf32 * 3.0),
                        tmpfactor * (hf32 * bf32 - y + bf32 * 3.0),
                    ),
                    PaletteColorEnum::Undergrowth.to_color(),
                );
                draw_line_segment_mut(
                    &mut imgug,
                    (
                        tmpfactor * (x - bf32 * 3.0) + 1.0,
                        tmpfactor * (hf32 * bf32 - y - bf32 * 3.0),
                    ),
                    (
                        tmpfactor * (x - bf32 * 3.0) + 1.0,
                        tmpfactor * (hf32 * bf32 - y + bf32 * 3.0),
                    ),
                    PaletteColorEnum::Undergrowth.to_color(),
                );

                if params.vege_bitmode {
                    draw_filled_circle_mut(
                        &mut img_ug_bit,
                        (
                            (tmpfactor * (x)) as i32,
                            (tmpfactor * (hf32 * bf32 - y)) as i32,
                        ),
                        (bf32 * 9.0 * SQRT_2) as i32,
                        Luma([0x01]),
                    )
                }
            }
            if value > uglimit2 {
                draw_line_segment_mut(
                    &mut imgug,
                    (tmpfactor * x, tmpfactor * (hf32 * bf32 - y - bf32 * 3.0)),
                    (tmpfactor * x, tmpfactor * (hf32 * bf32 - y + bf32 * 3.0)),
                    PaletteColorEnum::Undergrowth.to_color(),
                );
                draw_line_segment_mut(
                    &mut imgug,
                    (
                        tmpfactor * x + 1.0,
                        tmpfactor * (hf32 * bf32 - y - bf32 * 3.0),
                    ),
                    (
                        tmpfactor * x + 1.0,
                        tmpfactor * (hf32 * bf32 - y + bf32 * 3.0),
                    ),
                    PaletteColorEnum::Undergrowth.to_color(),
                );

                if params.vege_bitmode {
                    draw_filled_circle_mut(
                        &mut img_ug_bit,
                        (
                            (tmpfactor * (x)) as i32,
                            (tmpfactor * (hf32 * bf32 - y)) as i32,
                        ),
                        (bf32 * 9.0 * SQRT_2) as i32,
                        Luma([0x02]),
                    )
                }
            }

            y += bf32 * step;
        }
        x += bf32 * step;
    }

    let undergrowth_bit = median_filter(&img_ug_bit, (bf32 * step) as u32, (bf32 * step) as u32);

    VegetationRasters {
        greens: imggr1,
        open_land: imgye2,
        vegetation,
        undergrowth: imgug,
        undergrowth_bit,
        blueblack,
        bits,
        frame,
        // the pixel pitch the raster is drawn with: the reciprocal of the f32 factor
        undergrowth_world: WorldFile::north_up(
            1.0 / f64::from(tmpfactor),
            model.bounds.xmin,
            model.bounds.ymax,
        ),
        palette,
    }
}

/// Write `rasters` to `tmpfolder`: `vegetation.png`, `undergrowth.png`, their world
/// files and, with `vege_bitmode`, `vegetation_bit.png` and `undergrowth_bit.png`; with
/// `debug` also `greens.png`, `yellow.png`, `blueblack.png` (a re-render draws it),
/// `greens_bit.png` and `yellow_bit.png` (with `vege_bitmode`) and `undergrowth_bit.png`.
pub fn write_vegetation(
    fs: &impl FileSystem,
    tmpfolder: &Path,
    rasters: &VegetationRasters,
    debug: bool,
) -> Result<(), Box<dyn Error>> {
    let palette = &rasters.palette;
    let paletted = |name: &str, image: &PalettedImage| -> Result<(), Box<dyn Error>> {
        image.write_to(&mut fs.create(tmpfolder.join(name))?, palette)?;
        Ok(())
    };
    let png = |name: &str| fs.create(tmpfolder.join(name));
    let world = |name: &str, world: &WorldFile| -> Result<(), Box<dyn Error>> {
        world.write(&mut fs.create(tmpfolder.join(name))?)?;
        Ok(())
    };

    if debug {
        paletted("yellow.png", &rasters.open_land)?;
        paletted("greens.png", &rasters.greens)?;
    }
    if let Some(bits) = &rasters.bits {
        if debug {
            bits.greens
                .write_to(&mut png("greens_bit.png")?, image::ImageFormat::Png)?;
            bits.open_land
                .write_to(&mut png("yellow_bit.png")?, image::ImageFormat::Png)?;
        }
        bits.vegetation
            .write_to(&mut png("vegetation_bit.png")?, image::ImageFormat::Png)?;
    }
    paletted("vegetation.png", &rasters.vegetation)?;
    if debug {
        paletted("blueblack.png", &rasters.blueblack)?;
    }
    paletted("undergrowth.png", &rasters.undergrowth)?;
    if debug || rasters.bits.is_some() {
        rasters
            .undergrowth_bit
            .write_to(&mut png("undergrowth_bit.png")?, image::ImageFormat::Png)?;
    }
    world("undergrowth.pgw", &rasters.undergrowth_world)?;
    world("vegetation.pgw", &rasters.frame.world_file())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::fs::memory::MemoryFileSystem;

    /// The template's vegetation parameters.
    fn params() -> VegetationParams {
        crate::config::Config::from_file(Path::new("pullauta.default.ini"))
            .unwrap()
            .vegetation
    }

    /// A flat ground model at 0 m, 30 x 30 m in 1 m cells, its corner at (1000, 2000).
    fn flat_ground() -> HeightMap {
        HeightMap {
            xoffset: 1000.0,
            yoffset: 2000.0,
            scale: 1.0,
            grid: Vec2D::new(30, 30, 0.0),
        }
    }

    fn record(x: f64, y: f64, z: f32, class: u8, of: u8, number: u8) -> XyzRecord {
        XyzRecord {
            x,
            y,
            z,
            classification: class,
            number_of_returns: of,
            return_number: number,
            flags: 0,
        }
    }

    /// One pulse per square metre of `ground`, at the cell centres.
    fn pulses(
        ground: &HeightMap,
        mut pulse: impl FnMut(f64, f64) -> Vec<XyzRecord>,
    ) -> Vec<XyzRecord> {
        let mut returns = Vec::new();
        for i in 0..ground.grid.width() {
            for j in 0..ground.grid.height() {
                let (x, y) = (
                    ground.minx() + i as f64 + 0.5,
                    ground.miny() + j as f64 + 0.5,
                );
                returns.extend(pulse(x, y));
            }
        }
        returns
    }

    /// Ground returns only: open land wherever its 2x2 window fits, no green shade, no
    /// undergrowth.
    #[test]
    fn a_flat_open_field_is_open_land() {
        let ground = flat_ground();
        let returns = pulses(&ground, |x, y| vec![record(x, y, 0.0, 2, 1, 1)]);
        let model = makevege(&ground, &returns, &params());

        assert_eq!(
            (model.open_land.width(), model.open_land.height()),
            (10, 10)
        );
        for (x, y, open) in model.open_land.iter() {
            // the 2x2 sum leaves the last two columns and rows out
            assert_eq!(open, u8::from(x < 8 && y < 8), "open land at ({x}, {y})");
        }
        assert!(model.green.iter().all(|(_, _, shade)| shade == 0));
        assert!(model.undergrowth.iter().all(|(_, _, share)| share == 0.0));
        assert!(model.undergrowth_class().iter().all(|(_, _, c)| c == 0));
    }

    /// Every pulse a first return at 2 m (stratum1, 1-2.65 m) and a last return on the
    /// ground: 9 green and 9 ground hits per 3 m cell, so the green factor is
    /// 9/19 * (1 - topweight) * (1 - pointvolumefactor) = 0.0853 against the 0.1
    /// threshold of a 2 m canopy top (threshold1, 0.2-3 m): above 0.07 (the 4th of
    /// greenshades) and below 0.13 (the 5th), so every cell is green shade 4. No open
    /// land: half the returns are above `yellowheight`.
    #[test]
    fn a_dense_low_stratum_is_green() {
        let ground = flat_ground();
        let returns = pulses(&ground, |x, y| {
            vec![record(x, y, 2.0, 1, 2, 1), record(x, y, 0.0, 2, 2, 2)]
        });
        let model = makevege(&ground, &returns, &params());

        assert_eq!((model.green.width(), model.green.height()), (10, 10));
        assert!(model.green.iter().all(|(_, _, shade)| shade == 4));
        assert!(model.open_land.iter().all(|(_, _, open)| open == 0));
    }

    /// The water and building pixels are the 3x3 squares `draw_filled_rect_mut` draws,
    /// clipped to the frame, a later one over an earlier one; ground below
    /// `waterelevation` comes last.
    #[test]
    fn water_and_buildings_are_the_squares_drawn_clipped() {
        let ground = HeightMap {
            xoffset: 1000.0,
            yoffset: 2000.0,
            scale: 1.0,
            grid: Vec2D::new(7, 5, 10.0),
        };
        let mut ground = ground;
        ground.grid[(6, 0)] = -5.0; // below waterelevation, at the south-east corner
        let params = VegetationParams {
            buildings: 6,
            water_blue: true,
            waterele: 0.0,
            greendetectsize: 1.0,
            ..params()
        };
        let frame = VegetationFrame::of_ground(&ground, &params);
        assert_eq!((frame.width, frame.height), (7, 5));
        let returns = [
            record(1000.2, 2004.9, 0.0, 6, 1, 1), // north-west corner pixel
            record(1003.5, 2002.5, 0.0, 6, 1, 1), // inside
            record(1004.5, 2002.5, 0.0, 9, 1, 1), // water over the building's east edge
            record(1006.9, 2000.1, 0.0, 6, 1, 1), // south-east corner pixel
            record(1003.5, 2010.0, 0.0, 6, 1, 1), // north of the frame
        ];
        let grid = water_and_buildings(&ground, &returns, &params, frame);

        let mut expected = PalettedImage::new(7, 5, PaletteColorEnum::BackgroundWhite.to_color());
        let mut draw = |x: f64, y: f64, color: PaletteColorEnum| {
            imageproc::drawing::draw_filled_rect_mut(
                &mut expected,
                Rect::at((x - 1000.0) as i32 - 1, (2005.0 - y) as i32 - 1).of_size(3, 3),
                color.to_color(),
            );
        };
        for r in &returns {
            if r.classification == 6 {
                draw(r.x, r.y, PaletteColorEnum::Black);
            } else {
                draw(r.x, r.y, PaletteColorEnum::Blue);
            }
        }
        draw(1006.0, 2000.0, PaletteColorEnum::Blue);

        for (x, y, what) in grid.iter() {
            let color = match what {
                WaterOrBuilding::Neither => PaletteColorEnum::BackgroundWhite,
                WaterOrBuilding::Building => PaletteColorEnum::Black,
                WaterOrBuilding::Water => PaletteColorEnum::Blue,
            };
            assert!(
                expected.get_pixel(x as u32, y as u32) == color.to_color(),
                "pixel ({x}, {y}) is {what:?}"
            );
        }
        assert_eq!(grid[(0, 0)], WaterOrBuilding::Building);
        assert_eq!(grid[(4, 2)], WaterOrBuilding::Water);
        assert_eq!(grid[(6, 4)], WaterOrBuilding::Water);
    }

    /// A model of 10 x 10 green cells of 3 m with one green shade 1 cell, one open land
    /// cell, one cell above each undergrowth limit and one building pixel.
    fn hand_built_model() -> VegetationModel {
        let mut green = Vec2D::new(10, 10, 0u8);
        green[(2, 3)] = 1;
        let mut open_land = Vec2D::new(10, 10, 0u8);
        open_land[(6, 6)] = 1;
        let mut undergrowth = Vec2D::new(2, 2, 0.0);
        undergrowth[(0, 0)] = 0.4; // above uglimit (0.35) only
        undergrowth[(1, 1)] = 0.6; // above uglimit2 (0.56) too
        let mut water_buildings = Vec2D::new(30, 30, WaterOrBuilding::Neither);
        water_buildings[(20, 5)] = WaterOrBuilding::Building;
        VegetationModel {
            green,
            open_land,
            undergrowth,
            water_buildings,
            bounds: Bounds::new(1000.0, 1030.0, 2000.0, 2030.0),
            block: 3.0,
            drawing: VegetationDrawing {
                med: 1,
                med2: 1,
                medyellow: 1,
                proceed_yellows: false,
                uglimit: 0.35,
                uglimit2: 0.56,
            },
        }
    }

    #[test]
    fn rasterise_draws_each_class_at_its_cell() {
        let model = hand_built_model();
        let rasters = rasterise_vegetation(&model, &params());
        let at = |image: &PalettedImage, x, y| image.get_pixel(x, y);

        assert_eq!(
            (rasters.vegetation.width(), rasters.vegetation.height()),
            (30, 30)
        );
        // green cell (2, 3): the 3x3 square at ((2 - 0.5) * 3, (10 - 3 - 0.5) * 3)
        let shade1 = PaletteColorEnum::GreenShade(0).to_color();
        assert!(at(&rasters.vegetation, 4, 19) == shade1);
        assert!(at(&rasters.vegetation, 6, 21) == shade1);
        assert!(at(&rasters.vegetation, 7, 21) == PaletteColorEnum::BackgroundWhite.to_color());
        // open land cell (6, 6): the 3x3 square at (6 * 3 + 2, (10 - 6) * 3 - 3)
        assert!(at(&rasters.vegetation, 20, 9) == PaletteColorEnum::Yellow2.to_color());
        assert!(at(&rasters.open_land, 22, 11) == PaletteColorEnum::Yellow2.to_color());
        assert!(at(&rasters.greens, 20, 9) == PaletteColorEnum::BackgroundWhite.to_color());
        assert!(at(&rasters.blueblack, 20, 5) == PaletteColorEnum::Black.to_color());
        assert!(at(&rasters.blueblack, 21, 5) == PaletteColorEnum::BackgroundWhite.to_color());

        // the undergrowth is drawn at the sheet's pixels, the frame one pixel per metre
        let px = params().frame.px_per_metre() as f32;
        assert_eq!(
            rasters.undergrowth.width(),
            params().frame.to_px(30.0) as u32
        );
        assert_eq!(rasters.undergrowth_world.pixel_size_x, 1.0 / f64::from(px));
        assert_eq!(rasters.frame.world_file().pixel_size_x, 1.0);
        let undergrowth = PaletteColorEnum::Undergrowth.to_color();
        let drawn = |x: f32, y: f32| at(&rasters.undergrowth, (px * x) as u32, (px * y) as u32);
        // cell (0, 0), from (0, 30) m north-up: the normal lines at x = +-9 m only
        assert!(drawn(9.0, 25.0) == undergrowth);
        assert!(drawn(0.0, 25.0) != undergrowth);
        // cell (1, 1), from (18, 12) m north-up: the walk line at x = 18 m too
        assert!(drawn(18.0, 10.0) == undergrowth);
        assert!(rasters.bits.is_none());
        assert_eq!(model.undergrowth_class()[(0, 0)], 1);
        assert_eq!(model.undergrowth_class()[(1, 1)], 1);
        assert_eq!(model.undergrowth_class()[(1, 0)], 0);
    }

    /// The products, and the helper rasters only with `debug`; the world file and the
    /// size of `vegetation.png` are the model's frame.
    #[test]
    fn write_vegetation_writes_the_products_and_the_frame() {
        let model = hand_built_model();
        let tmp = Path::new("temp");
        let names = |fs: &MemoryFileSystem| {
            let mut names: Vec<String> = fs
                .list(tmp)
                .unwrap()
                .iter()
                .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
                .collect();
            names.sort();
            names
        };

        let fs = MemoryFileSystem::new();
        fs.create_dir_all(tmp).unwrap();
        write_vegetation(&fs, tmp, &rasterise_vegetation(&model, &params()), false).unwrap();
        assert_eq!(
            names(&fs),
            [
                "undergrowth.pgw",
                "undergrowth.png",
                "vegetation.pgw",
                "vegetation.png"
            ]
        );
        let world = WorldFile::read(&fs, tmp.join("vegetation.pgw")).unwrap();
        let mut png = image::ImageReader::new(fs.open(tmp.join("vegetation.png")).unwrap());
        png.set_format(image::ImageFormat::Png);
        let (width, height) = png.into_dimensions().unwrap();
        let read = VegetationFrame {
            x_origin: world.x_origin,
            y_origin: world.y_origin,
            width,
            height,
        };
        assert_eq!(read, model.frame());

        let fs = MemoryFileSystem::new();
        fs.create_dir_all(tmp).unwrap();
        let bitmode = VegetationParams {
            vege_bitmode: true,
            ..params()
        };
        write_vegetation(&fs, tmp, &rasterise_vegetation(&model, &bitmode), true).unwrap();
        assert_eq!(
            names(&fs),
            [
                "blueblack.png",
                "greens.png",
                "greens_bit.png",
                "undergrowth.pgw",
                "undergrowth.png",
                "undergrowth_bit.png",
                "vegetation.pgw",
                "vegetation.png",
                "vegetation_bit.png",
                "yellow.png",
                "yellow_bit.png"
            ]
        );
    }

    /// The map layers are the written rasters as a re-render decodes them.
    #[test]
    fn map_layers_are_the_written_rasters_decoded() {
        let rasters = rasterise_vegetation(&hand_built_model(), &params());
        let tmp = Path::new("temp");
        let fs = MemoryFileSystem::new();
        fs.create_dir_all(tmp).unwrap();
        write_vegetation(&fs, tmp, &rasters, true).unwrap();
        let decoded = |name: &str| fs.read_image_png(tmp.join(name)).unwrap().to_rgba8();

        let frame = rasters.frame;
        let layers = rasters.into_map_rasters().layers();
        assert!(decoded("vegetation.png") == layers.vegetation);
        assert!(decoded("undergrowth.png") == layers.undergrowth);
        assert!(Some(decoded("blueblack.png")) == layers.water_buildings);
        assert_eq!(
            WorldFile::read(&fs, tmp.join("vegetation.pgw")).unwrap(),
            layers.world
        );
        assert_eq!(layers.frame(), frame);
    }
}
