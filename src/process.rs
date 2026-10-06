use anyhow::Context;
use image::buffer::ConvertBuffer;
use image::{Luma, Rgb, RgbImage, Rgba};
use itertools::izip;
use las::{PointData, PointDataBuilder, Reader};
use log::debug;
use log::info;

use rand::prelude::*;
use rustc_hash::FxHashMap as HashMap;
use std::collections::hash_map::Entry;
use std::error::Error;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;

use crate::blocks;
use crate::cliffs;
use crate::config::{Config, Outputs};
use crate::contours;
use crate::crop;
use crate::formlines::{self, FormLineSelection};
use crate::geojson;
use crate::geometry::BinaryDxf;
use crate::io::fs::FileSystem;
use crate::io::heightmap::HeightMap;
use crate::io::xyz::LasClass;
use crate::io::xyz::XyzInternalWriter;
use crate::io::xyz::XyzRecord;
use crate::isom::IsomTable;
use crate::knolls;
use crate::knolls::DotKnollSet;
use crate::mapframe::WorldFile;
use crate::merge;
use crate::merge::ContourSet;
use crate::plan::InputFileIndex;
use crate::plan::Operation;
use crate::plan::Plan;
use crate::plan::Rect;
use crate::render;
use crate::util::Consumer;
use crate::util::Timing;
use crate::util::read_lines_no_alloc;
use crate::util::thinning_rng;
use crate::vege_vector;
use crate::vegetation;

// compute the number of elements we can buffer for 50MB of memory usage during LAZ -> XyzRecord conversion
const LAZ_BUFFER_SIZE: usize =
    50 * 1024 * 1024 / (size_of::<las::Point>() + size_of::<XyzRecord>());

/// The [`XyzRecord::flags`] of each point in `pd`, in point order.
fn las_flags(pd: &PointData) -> impl Iterator<Item = u8> + '_ {
    let extended = pd.format().is_extended;
    pd.raw_bytes()
        .chunks_exact(pd.record_len())
        .map(move |rec| record_flags(extended, rec))
}

/// The withheld, synthetic and overlap flags of one raw LAS point record. Formats 0-5 keep
/// the class in the low 5 bits of byte 15, synthetic and withheld in bits 5 and 7, and mark
/// overlap as class 12. Formats 6-10 (`extended`) keep synthetic, withheld and overlap in
/// bits 0, 2 and 3 of byte 15.
fn record_flags(extended: bool, rec: &[u8]) -> u8 {
    let b = rec[15];
    if extended {
        XyzRecord::pack_flags(b & 0b0100 != 0, b & 0b0001 != 0, b & 0b1000 != 0)
    } else {
        XyzRecord::pack_flags(b & 0x80 != 0, b & 0x20 != 0, b & 0x1f == 12)
    }
}

pub use crate::plan::batch_tiles;

/// Launches threads and coordinates the logic for processing multiple files in parallell.
/// When it returns, all files have been processed and output files have been generated according to
/// the Config.
pub fn launch_threads<F: FileSystem + Send + Clone + 'static>(
    fs: F,
    config: Arc<Config>,
    zip_files: &[String],
) -> anyhow::Result<()> {
    // first unzip all zip files (if any) to a temporary folder, so that the threads can access the
    // shapefiles without having to worry about unzipping them in parallel
    let shapefiletmpdir = PathBuf::from("temp_shapefiles".to_string());
    fs.create_dir_all(&shapefiletmpdir)
        .context("Could not create temporary folder for shapefiles")?;
    if !zip_files.is_empty() {
        crate::shapefile::unzip_shapefiles(&fs, zip_files).unwrap();
    }

    // folder where we store temporary extracted files to process later
    let staging_folder = Path::new("temp_staging");
    fs.create_dir_all(staging_folder)
        .context("Could not create staging folder")?;

    let timing = Timing::start_now("create_plan");
    let plan = crate::plan::Plan::new_from_input_files(
        fs.clone(),
        &config.lazfolder,
        &config.batchoutfolder,
        staging_folder,
        config.batchbuffer,
    )
    .context("creating plan")?;
    drop(timing);

    if plan.files_to_process().is_empty() {
        info!("No files to process, exiting");
        return Ok(());
    }

    let plan = Arc::new(plan);

    // make sure the output directory exists
    fs.create_dir_all(&config.batchoutfolder)
        .expect("Could not create output folder");

    // we only need to launch maximum as many threads as there are files to process
    let num_threads = config.processes.min(plan.files_to_process().len() as u64) as usize;

    // Create a queue where we send the files that are ready to process to the worker threads.
    // Bound it based on the number of threads so that we cannot have too many files converted
    // waiting for processing at a time.
    // TODO: make it configurable?
    let (tx, rx) = crate::util::make_bounded_queue::<InputFileIndex>((num_threads / 2).max(1));

    // do the processing
    let mut handles: Vec<thread::JoinHandle<()>> = Vec::with_capacity(num_threads);
    for i in 0..num_threads {
        let config = config.clone();
        let fs = fs.clone();
        let rx = rx.clone();
        let has_zip = !zip_files.is_empty();
        let arc_plan = plan.clone();

        let handle = thread::Builder::new()
            .name(format!("worker_{i}"))
            .spawn(move || {
                info!("Starting thread");
                batch_process(&config, &fs, &format!("{}", i + 1), has_zip, arc_plan, rx);
            })
            .expect("Could not spawn thread");
        handles.push(handle);
    }

    let mut planner = plan.extract_once_planner();

    let mut writers: HashMap<InputFileIndex, XyzInternalWriter<_>> = HashMap::default();

    let options = las::ReaderOptions::default().with_laz_parallelism(if config.laz_parallel {
        las::LazParallelism::Yes
    } else {
        las::LazParallelism::No
    });
    log::debug!("Using LAZ parallelism: {:?}", config.laz_parallel);

    // prepare buffers needed for extraction of LAZ files based on the planned operations
    let mut records = Vec::with_capacity(LAZ_BUFFER_SIZE);

    let &Config {
        zoff, thinfactor, ..
    } = &*config;

    let randdist = rand::distr::Bernoulli::new(thinfactor).unwrap();

    while let Some(ops) = planner.next_operation() {
        for op in ops {
            match op {
                Operation::Extract { from, to } => {
                    log::trace!("Extracting from {from:?} to {to:?}");

                    let laz_p = plan.get_input_file(from);

                    let mut reader = Reader::with_options(
                        fs.open(&laz_p.path).expect("Could not open file"),
                        options,
                    )
                    .expect("Could not create reader");

                    let mut pd = PointDataBuilder::new().for_header(reader.header()).build();

                    // one generator per target tile, seeded from the tile names: a tile's own
                    // points are thinned as in a single job, and the thinning of a tile does
                    // not depend on the order of the operations
                    let tile = |i| {
                        plan.get_input_file(i)
                            .path
                            .file_stem()
                            .unwrap()
                            .to_string_lossy()
                    };
                    let mut rngs: Vec<_> = to
                        .iter()
                        .map(|&to_i| thinning_rng(&tile(to_i), &tile(from)))
                        .collect();

                    loop {
                        let n = reader
                            .fill_points(LAZ_BUFFER_SIZE as u64, &mut pd)
                            .expect("could not read LAZ points");
                        if n == 0 {
                            break;
                        }

                        // for each dependency, we need to check all points against their boundary
                        // to know if they should be included in the output.
                        for (&to_i, rng) in to.iter().zip(&mut rngs) {
                            let to_file = plan.get_input_file(to_i);
                            let padded_bounds = to_file.header.bounds.expand(config.batchbuffer);

                            let to_self = to_i == from;

                            // convert all read points to records
                            records.clear();
                            for (
                                pt_x,
                                pt_y,
                                pt_z,
                                pt_classification,
                                pt_number_of_returns,
                                pt_return_number,
                                pt_flags,
                            ) in izip!(
                                pd.x(),
                                pd.y(),
                                pd.z(),
                                pd.classification(),
                                pd.number_of_returns(),
                                pd.return_number(),
                                las_flags(&pd)
                            ) {
                                if (to_self || padded_bounds.contains(pt_x, pt_y))
                                    && (thinfactor == 1.0 || rng.sample(randdist))
                                {
                                    records.push(crate::io::xyz::XyzRecord {
                                        x: pt_x,
                                        y: pt_y,
                                        z: (pt_z + zoff) as f32,
                                        classification: pt_classification,
                                        number_of_returns: pt_number_of_returns,
                                        return_number: pt_return_number,
                                        flags: pt_flags,
                                    });
                                }
                            }

                            // get or create the writer for this file
                            let writer = match writers.entry(to_i) {
                                Entry::Occupied(e) => e.into_mut(),
                                Entry::Vacant(e) => {
                                    let outfile = &to_file.staging_path;
                                    let writer = XyzInternalWriter::new(
                                        fs.create(outfile)
                                            .context("Could not create output file")?,
                                    );
                                    e.insert(writer)
                                }
                            };

                            // write all at once
                            writer
                                .write_records(&records)
                                .expect("Could not write records");
                        }
                    }
                }
                Operation::Process { tile } => {
                    log::trace!("Processing tile {tile:?}");

                    // make sure we close the file for this tile
                    if let Some(mut w) = writers.remove(&tile) {
                        w.finish().context("Could not finish writer")?;
                    } else {
                        anyhow::bail!("Internal error: missing writer for tile {tile:?}");
                    };

                    // finally post output file for processing!
                    // This will block the main thread if the queue is full to provide backpressure.
                    tx.push(tile);
                }
            }
        }
    }

    // we are done, close the producer and wait for all threads to exit
    drop(tx);
    for handle in handles {
        handle.join().unwrap();
    }

    // cleanup extracted shapefiles
    fs.remove_dir_all(&shapefiletmpdir)
        .context("Could not remove temporary shapefile folder")?;
    Ok(())
}

/// Renders the map of `inputs` with the depressions (`nodepressions` false) or without
/// and writes it as `pullautus_depr{thread}` or `pullautus{thread}`.
pub fn render_map(
    fs: &impl FileSystem,
    config: &Config,
    thread: &str,
    inputs: &render::MapInputs,
    nodepressions: bool,
) -> Result<(), Box<dyn Error>> {
    let map = render::render(&config.render, inputs, nodepressions);
    render::write_map(
        fs,
        &render::map_stem(thread, nodepressions),
        &map,
        config.epsg,
    )
}

/// Selects the form lines of `contours` on `ground` and writes them to `tmpfolder`
/// ([`write_form_line_products`]). None without form lines.
pub fn make_form_lines(
    fs: &impl FileSystem,
    config: &Config,
    tmpfolder: &Path,
    ground: &HeightMap,
    contours: &ContourSet,
    debug: bool,
) -> Result<Option<FormLineSelection>, Box<dyn Error>> {
    let selection = formlines::select_form_lines(contours, ground, &config.form_lines);
    if let Some(selection) = &selection {
        write_form_line_products(fs, config, tmpfolder, selection, debug)?;
    }
    Ok(selection)
}

/// Writes the form line `selection` to `tmpfolder`: the dump with `debug`, the DXF and
/// the GeoJSON with their families.
fn write_form_line_products(
    fs: &impl FileSystem,
    config: &Config,
    tmpfolder: &Path,
    selection: &FormLineSelection,
    debug: bool,
) -> Result<(), Box<dyn Error>> {
    formlines::write_form_lines(fs, tmpfolder, selection, debug, config.outputs.dxf)?;
    // As for contours: 103.000 in the contours table is this selected set, not the
    // half-interval lines.
    if config.vector_tables() {
        geojson::write_form_line_tables(fs, tmpfolder, selection, config.epsg)?;
    }
    Ok(())
}

/// The shape files' layers of the map of the tile in `tmpfolder`, drawn in `frame`: the
/// shape files in `filenames`, unzipped into `tmpfolder` first, or without `filenames`
/// (a batch) the ones already unzipped. With `debug` the layers are also written to
/// `tmpfolder`.
fn shape_layers(
    fs: &impl FileSystem,
    config: &Config,
    tmpfolder: &Path,
    frame: vegetation::VegetationFrame,
    filenames: &[String],
    debug: bool,
) -> Option<render::ShapeLayers> {
    #[cfg(feature = "shapefile")]
    {
        if !filenames.is_empty() {
            info!("Rendering shape files");
            crate::shapefile::unzip_and_render(fs, config, tmpfolder, filenames, Some(frame), debug)
                .unwrap()
        } else {
            crate::shapefile::render(fs, config, tmpfolder, Some(frame), true, debug).unwrap()
        }
    }
    #[cfg(not(feature = "shapefile"))]
    {
        let _ = (fs, config, tmpfolder, frame, filenames, debug);
        None
    }
}

/// Renders the shape files in `filenames` and the map of the tile in `tmpfolder` from
/// `inputs` with them. With `debug` the shape files' layers are also written to
/// `tmpfolder`.
pub fn process_zip(
    fs: &impl FileSystem,
    config: &Config,
    thread: &str,
    tmpfolder: &Path,
    inputs: &render::MapInputs,
    filenames: &[String],
    debug: bool,
) -> Result<(), Box<dyn Error>> {
    render::check_raster(config)?;
    let mut timing = Timing::start_now("process_zip");
    timing.start_section("unzip and render shape files");
    let frame = inputs.vegetation.frame();
    let shapes = shape_layers(fs, config, tmpfolder, frame, filenames, debug);
    let inputs = &render::MapInputs {
        shapes: shapes.as_ref(),
        ..*inputs
    };

    info!("Rendering png map with depressions");
    timing.start_section("Rendering png map with depressions");
    render_map(fs, config, thread, inputs, false)?;

    info!("Rendering png map without depressions");
    timing.start_section("Rendering png map without depressions");
    render_map(fs, config, thread, inputs, true)?;

    Ok(())
}

/// The terrain of a tile: what the contour chain makes of the ground model.
pub struct Terrain {
    /// smoothjoin's contours.
    pub contours: ContourSet,
    /// The dot knolls.
    pub dot_knolls: DotKnollSet,
    /// The form lines, None without them.
    pub form_lines: Option<FormLineSelection>,
}

/// The vegetation of a tile: what is drawn from the vegetation model, which is dropped
/// once they are.
pub struct TileVegetation {
    /// The rasters, with the raster family or debug_intermediates=1.
    pub rasters: Option<vegetation::VegetationRasters>,
    /// The vegetation areas, with a vector family.
    pub areas: Option<vege_vector::VegetationAreas>,
}

/// What the stages make of a tile's returns ([`run_stages`]). A stage that does not run
/// leaves its value None.
pub struct TileValues {
    /// The ground model.
    pub ground: HeightMap,
    /// The base map contours (`basemap.dxf.bin`), with basemapinterval unless vegeonly or
    /// cliffsonly.
    pub basemap: Option<BinaryDxf>,
    /// The terrain, unless vegeonly or cliffsonly.
    pub terrain: Option<Terrain>,
    /// The vegetation, unless contoursonly or cliffsonly.
    pub vegetation: Option<TileVegetation>,
    /// The cliffs, unless vegeonly or contoursonly.
    pub cliffs: Option<cliffs::CliffSet>,
    /// The blocks, with detectbuildings unless vegeonly, contoursonly or cliffsonly.
    pub blocks: Option<blocks::Blocks>,
    /// The values only the debug intermediates hold, with debug_intermediates=1.
    pub dumps: Option<StageDumps>,
}

/// The stages' values a tile keeps only for its debug intermediates.
#[derive(Default)]
pub struct StageDumps {
    /// The knoll candidate contours (`contours03.dxf.bin`).
    pub knoll_candidates: Option<BinaryDxf>,
    /// knolldetector's knoll rings and pins (`detected.dxf.bin`, `pins.bin`), unless
    /// skipknolldetection.
    pub detected: Option<(knolls::DetectedKnolls, Vec<knolls::Pin>)>,
    /// The lifted ground model (`xyz_knolls.hmap`).
    pub lifted: Option<HeightMap>,
    /// The contours smoothjoin starts from (`out.dxf.bin`).
    pub traced: Option<BinaryDxf>,
    /// smoothjoin's dot knoll candidates (`dotknolls.bin`).
    pub dot_knoll_candidates: Vec<knolls::DotKnollCandidate>,
    /// The pixels the first cliff pass marked with a passable dash (`c2.png`).
    pub passable_raster: Option<RgbImage>,
}

impl TileValues {
    /// The vegetation rasters, when drawn.
    pub fn rasters(&self) -> Option<&vegetation::VegetationRasters> {
        self.vegetation.as_ref()?.rasters.as_ref()
    }

    /// The values the map is drawn from, on `vegetation` (the rasters' layers); None
    /// unless the terrain and the cliffs were made.
    pub fn map_inputs<'a>(
        &'a self,
        vegetation: &'a render::VegetationLayers,
    ) -> Option<render::MapInputs<'a>> {
        let terrain = self.terrain.as_ref()?;
        Some(render::MapInputs {
            ground: &self.ground,
            contours: &terrain.contours,
            dot_knolls: &terrain.dot_knolls,
            cliffs: self.cliffs.as_ref()?,
            form_lines: terrain.form_lines.as_ref(),
            vegetation,
            blocks: self.blocks.as_ref().map(|b| &b.map),
            shapes: None,
        })
    }
}

/// Runs the stages `config` asks for on a tile's `returns`, without the file system: the
/// ground model, then the contour chain (knolls, smoothjoin, dot knolls, form lines), the
/// vegetation, the cliffs and the blocks, each from the ground model and the returns.
/// `tile` (the tile name) seeds the cliffthin sampling.
pub fn run_stages(config: &Config, returns: &[XyzRecord], tile: &str) -> TileValues {
    let mut timing = Timing::start_now("run_stages");
    let &Config {
        vegeonly,
        cliffsonly,
        contoursonly,
        ..
    } = config;
    let mut dumps = config.debug_intermediates.then(StageDumps::default);

    info!("Knoll detection part 1");
    timing.start_section("ground model");
    // every stage takes the ground model from here
    let ground = contours::xyz2heightmap(returns, &config.ground, config.water_class);

    let mut basemap = None;
    let mut terrain = None;
    if !vegeonly && !cliffsonly {
        if let Some(interval) = config.basemapcontours {
            info!("Basemap contours");
            let traced = contours::trace(&ground, interval);
            basemap = Some(contours::contours_to_bindxf(&traced, &ground));
        }
        terrain = Some(contour_chain(config, &ground, &mut dumps, &mut timing));
    }

    let vegetation = (!cliffsonly && !contoursonly).then(|| {
        info!("Vegetation generation");
        timing.start_section("vegetation generation");
        let model = vegetation::makevege(&ground, returns, &config.vegetation);
        TileVegetation {
            rasters: (config.outputs.raster || config.debug_intermediates)
                .then(|| vegetation::rasterise_vegetation(&model, &config.vegetation)),
            areas: config.outputs.vectorizes_vegetation().then(|| {
                vege_vector::vectorise_vegetation(
                    &model,
                    &config.vector_greenshade_isom,
                    config.vector_simplify,
                )
            }),
        }
    });

    let cliffs = (!vegeonly && !contoursonly).then(|| {
        info!("Cliff generation");
        timing.start_section("cliff generation");
        let (cliffs, passable_raster) = cliffs::makecliffs(&ground, returns, tile, &config.cliff);
        if let Some(dumps) = &mut dumps {
            dumps.passable_raster = Some(passable_raster);
        }
        cliffs
    });

    let blocks = (!vegeonly && !contoursonly && !cliffsonly && config.detectbuildings).then(|| {
        info!("Detecting buildings");
        timing.start_section("detecting buildings");
        blocks::blocks(config.water_class, &ground, returns)
    });

    TileValues {
        ground,
        basemap,
        terrain,
        vegetation,
        cliffs,
        blocks,
        dumps,
    }
}

/// The contour chain on `ground`: the knoll candidates, knoll detection (unless
/// skipknolldetection), the knoll lift, the contours traced and smoothjoined, the dot
/// knolls and the form lines. With `dumps` the values only the debug intermediates hold
/// go there.
fn contour_chain(
    config: &Config,
    ground: &HeightMap,
    dumps: &mut Option<StageDumps>,
    timing: &mut Timing,
) -> Terrain {
    let skipknolldetection = config.skipknolldetection;
    timing.start_section("knoll detection part 1");
    // the fine contours the knoll candidates come from; with skipknolldetection traced
    // only for their debug dump
    let candidates = if !skipknolldetection || dumps.is_some() {
        contours::trace(ground, config.knoll.candidate_interval_m)
    } else {
        Vec::new()
    };
    if let Some(dumps) = dumps {
        dumps.knoll_candidates = Some(contours::contours_to_bindxf(&candidates, ground));
    }
    let pins = if skipknolldetection {
        Vec::new()
    } else {
        info!("Knoll detection part 2");
        timing.start_section("knoll detection part 2");
        let (detected, pins) = knolls::knolldetector(ground, &candidates, &config.knoll);
        if let Some(dumps) = dumps {
            dumps.detected = Some((detected, pins.clone()));
        }
        pins
    };
    drop(candidates);

    info!("Contour generation part 1");
    timing.start_section("contour generation part 1");
    // the lifted ground model; with skipknolldetection only flattened
    let lifted = knolls::xyzknolls(ground, &pins, &config.knoll);

    info!("Contour generation part 2");
    timing.start_section("contour generation part 2");
    // the contours smoothjoin starts from are traced at the levels it classes them at;
    // with skipknolldetection on the unlifted ground model, while smoothjoin and
    // dotknolls read the flattened one
    let traced_on = if skipknolldetection { ground } else { &lifted };
    let traced = contours::trace(traced_on, config.smoothjoin.levels().trace_interval);
    if let Some(dumps) = dumps {
        dumps.traced = Some(contours::contours_to_bindxf(&traced, traced_on));
    }

    info!("Contour generation part 3");
    timing.start_section("contour generation part 3");
    let (contours, candidates) = merge::smoothjoin(&traced, &lifted, &config.smoothjoin);

    info!("Contour generation part 4");
    timing.start_section("contour generation part 4");
    let dot_knolls = knolls::dotknolls(&contours, &candidates, &lifted, &config.knoll);

    // the form lines, selected once for both renders and the vector output
    info!("Selecting formlines");
    timing.start_section("selecting formlines");
    let form_lines = formlines::select_form_lines(&contours, ground, &config.form_lines);

    if let Some(dumps) = dumps {
        dumps.lifted = Some(lifted);
        dumps.dot_knoll_candidates = candidates;
    }
    Terrain {
        contours,
        dot_knolls,
        form_lines,
    }
}

/// Writes a tile's `values` to `tmpfolder`: the products of the families in `outputs`
/// and, with debug_intermediates=1, the debug intermediates. The GeoJSON tables are
/// written in this order: contours, knolls, vegetation, cliffs, form lines.
pub fn write_tile_products(
    fs: &impl FileSystem,
    config: &Config,
    tmpfolder: &Path,
    values: &TileValues,
) -> Result<(), Box<dyn Error>> {
    let TileValues {
        ground,
        basemap,
        terrain,
        vegetation,
        cliffs,
        blocks,
        dumps,
    } = values;
    let debug = config.debug_intermediates;
    let dxf = config.outputs.dxf;

    if debug {
        // the same bytes under the ground model's two dump names
        ground.to_file(fs, tmpfolder.join(knolls::KNOLL_GROUND_DUMP))?;
        fs.copy(
            tmpfolder.join(knolls::KNOLL_GROUND_DUMP),
            tmpfolder.join(render::GROUND_DUMP),
        )?;
    }
    if let Some(basemap) = basemap {
        contours::write_bindxf(fs, tmpfolder, "basemap.dxf.bin", basemap, dxf)?;
    }
    if let Some(terrain) = terrain {
        if let Some(dumps) = dumps {
            if let Some(candidates) = &dumps.knoll_candidates {
                contours::write_bindxf(fs, tmpfolder, knolls::CANDIDATES_DUMP, candidates, dxf)?;
            }
            if let Some((detected, pins)) = &dumps.detected {
                knolls::write_detected(fs, tmpfolder, detected, pins, dxf)?;
            }
            if let Some(lifted) = &dumps.lifted {
                lifted.to_file(fs, tmpfolder.join(knolls::LIFTED_GROUND_DUMP))?;
            }
            if let Some(traced) = &dumps.traced {
                contours::write_bindxf(fs, tmpfolder, merge::TRACED_DUMP, traced, dxf)?;
            }
        }
        let candidates = dumps.as_ref().map_or(&[][..], |d| &d.dot_knoll_candidates);
        merge::write_contours(fs, tmpfolder, &terrain.contours, candidates, debug, dxf)?;
        knolls::write_dot_knolls(fs, tmpfolder, &terrain.dot_knolls, debug, dxf)?;
        // The terrain reaches vector output as GeoJSON written from the values, contours
        // first
        if config.vector_tables() {
            geojson::write_contour_tables(fs, tmpfolder, &terrain.contours, config.epsg)?;
            geojson::write_knoll_tables(fs, tmpfolder, &terrain.dot_knolls, config.epsg)?;
        }
    }
    if let Some(vegetation) = vegetation {
        if let Some(rasters) = &vegetation.rasters {
            vegetation::write_vegetation(fs, tmpfolder, rasters, debug)?;
        }
        if let Some(areas) = &vegetation.areas {
            vege_vector::write_vegetation_areas(fs, config, tmpfolder, areas)?;
        }
    }
    if let Some(cliffs) = cliffs {
        let passable_raster = dumps.as_ref().and_then(|d| d.passable_raster.as_ref());
        cliffs::write_cliffs(fs, tmpfolder, cliffs, passable_raster, debug, dxf)?;
        if config.vector_tables() {
            geojson::write_cliff_tables(fs, tmpfolder, cliffs, config.epsg)?;
        }
    }
    if debug && let Some(blocks) = blocks {
        blocks::write_blocks(fs, tmpfolder, blocks)?;
    }
    if let Some(Terrain {
        form_lines: Some(selection),
        ..
    }) = terrain
    {
        write_form_line_products(fs, config, tmpfolder, selection, debug)?;
    }
    Ok(())
}

/// Runs every stage on the returns of `input_file` ([`run_stages`]), writes the tile's
/// products to `tmpfolder` ([`write_tile_products`]) and, unless `skip_rendering`, renders
/// the map.
pub fn process_tile(
    fs: &impl FileSystem,
    config: &Config,
    thread: &str,
    tmpfolder: &Path,
    input_file: &Path,
    tile: &str,
    skip_rendering: bool,
) -> Result<TileValues, Box<dyn Error>> {
    let mut timing = Timing::start_now("process_tile");
    fs.create_dir_all(tmpfolder)
        .expect("Could not create tmp folder");

    timing.start_section("preparing input file");
    info!("Preparing input file");
    let returns = read_returns(fs, config, input_file, tile)?;
    if config.debug_intermediates {
        let target_file = tmpfolder.join("xyztemp.xyz.bin");
        debug!("Writing records to {:?}", target_file);
        crate::io::xyz::write_all(fs.create(&target_file)?, &returns)?;
    }
    info!("Done");

    timing.start_section("stages");
    let values = run_stages(config, &returns, tile);
    // the products and the map are the stages' values, not the returns
    drop(returns);

    timing.start_section("writing the products");
    write_tile_products(fs, config, tmpfolder, &values).map_err(|e| {
        format!(
            "writing the tile's products in {}: {e}",
            tmpfolder.display()
        )
    })?;

    let layers = values
        .rasters()
        .filter(|_| !skip_rendering && config.outputs.raster)
        .map(|rasters| rasters.layers());
    if let Some(layers) = &layers
        && let Some(inputs) = values.map_inputs(layers)
    {
        info!("Rendering png map with depressions");
        timing.start_section("rendering png map with depressions");
        render_map(fs, config, thread, &inputs, false)?;

        info!("Rendering png map without depressions");
        timing.start_section("rendering png map without depressions");
        render_map(fs, config, thread, &inputs, true)?;
    } else if config.outputs.raster && !skip_rendering {
        info!("Skipped rendering");
    }
    info!("All done!");
    Ok(values)
}

/// The returns of `input_file` (`.xyz`, `.las`, `.laz` or `.xyz.bin`), in file order: the
/// order the `vegethin` counter and the `cliffthin` draws follow. LAS/LAZ points are
/// scaled by `xfactor`, `yfactor` and `zfactor`, lifted by `zoff` and thinned by
/// `thinfactor` with a generator seeded by `tile`.
fn read_returns(
    fs: &impl FileSystem,
    config: &Config,
    input_file: &Path,
    tile: &str,
) -> Result<Vec<XyzRecord>, Box<dyn Error>> {
    let filename = input_file
        .file_name()
        .ok_or_else(|| format!("No extension for input file {}", input_file.display()))?
        .to_string_lossy()
        .to_lowercase();

    let mut returns = if filename.ends_with(".xyz") {
        read_xyz_text(fs, input_file)
    } else if filename.ends_with(".laz") || filename.ends_with(".las") {
        read_las(fs, config, input_file, tile)
    } else if filename.ends_with(".xyz.bin") {
        info!("Reading points from .xyz.bin");
        crate::io::xyz::read_all(fs.open(input_file)?)?
    } else {
        return Err(format!("Unsupported input file: {}", input_file.display()).into());
    };
    if returns.is_empty() {
        return Err(format!("no returns in {}", input_file.display()).into());
    }
    // the returns are held through every stage: no growth slack
    returns.shrink_to_fit();
    Ok(returns)
}

/// The returns of an `.xyz` text file: `x y z` and optionally the classification
/// (default ground), the number of returns and the return number.
fn read_xyz_text(fs: &impl FileSystem, input_file: &Path) -> Vec<XyzRecord> {
    // if we are here we don't know if the file has at least 6 columns, but we assume that it is in the format
    // x y z classification number_of_returns return_number
    info!("Reading points from .xyz");
    let mut returns = Vec::new();
    read_lines_no_alloc(fs, input_file, |line| {
        let mut parts = line.split(' ');
        let x = parts.next().unwrap().parse::<f64>().unwrap();
        let y = parts.next().unwrap().parse::<f64>().unwrap();
        let z = parts.next().unwrap().parse::<f32>().unwrap();

        let classification = parts
            .next()
            .map_or(LasClass::Ground.into(), |c| c.parse::<u8>().unwrap());
        let number_of_returns = parts.next().unwrap_or("0").parse::<u8>().unwrap();
        let return_number = parts.next().unwrap_or("0").parse::<u8>().unwrap();

        returns.push(XyzRecord {
            x,
            y,
            z,
            classification,
            number_of_returns,
            return_number,
            ..Default::default()
        });
    })
    .expect("Could not read file");
    returns
}

/// The returns of a LAS/LAZ file, scaled, lifted and thinned as [`read_returns`] says.
fn read_las(
    fs: &impl FileSystem,
    config: &Config,
    input_file: &Path,
    tile: &str,
) -> Vec<XyzRecord> {
    info!("Reading points from .las/.laz");
    let &Config {
        thinfactor,
        xfactor,
        yfactor,
        zfactor,
        zoff,
        ..
    } = config;

    if thinfactor != 1.0 {
        info!("Using thinning factor {thinfactor}");
    }

    let mut rng = thinning_rng(tile, tile);
    let randdist = rand::distr::Bernoulli::new(thinfactor).unwrap();

    let options = las::ReaderOptions::default().with_laz_parallelism(if config.laz_parallel {
        las::LazParallelism::Yes
    } else {
        las::LazParallelism::No
    });
    let mut reader =
        Reader::with_options(fs.open(input_file).expect("Could not open file"), options)
            .expect("Could not create reader");

    let mut returns = Vec::new();
    if thinfactor == 1.0 {
        returns.reserve_exact(reader.header().number_of_points() as usize);
    }
    let mut pd = PointDataBuilder::new().for_header(reader.header()).build();
    loop {
        let n = reader.fill_points(LAZ_BUFFER_SIZE as u64, &mut pd).unwrap();

        if n == 0 {
            break;
        }

        for (
            pt_x,
            pt_y,
            pt_z,
            pt_classification,
            pt_number_of_returns,
            pt_return_number,
            pt_flags,
        ) in izip!(
            pd.x(),
            pd.y(),
            pd.z(),
            pd.classification(),
            pd.number_of_returns(),
            pd.return_number(),
            las_flags(&pd)
        ) {
            if thinfactor == 1.0 || rng.sample(randdist) {
                returns.push(XyzRecord {
                    x: pt_x * xfactor,
                    y: pt_y * yfactor,
                    z: (pt_z * zfactor + zoff) as f32,
                    classification: pt_classification,
                    number_of_returns: pt_number_of_returns,
                    return_number: pt_return_number,
                    flags: pt_flags,
                });
            }
        }
    }
    returns
}

/// `top` on a `width` x `height` sheet of `background`, its top left corner at (`x`,
/// `y`): a raster cropped (or padded) to a batch tile.
fn placed<I>(
    top: &I,
    width: u32,
    height: u32,
    background: I::Pixel,
    x: i64,
    y: i64,
) -> image::ImageBuffer<I::Pixel, Vec<<I::Pixel as image::Pixel>::Subpixel>>
where
    I: image::GenericImageView,
    I::Pixel: 'static,
{
    let mut sheet = image::ImageBuffer::from_pixel(width, height, background);
    image::imageops::overlay(&mut sheet, top, x, y);
    sheet
}

/// Writes `image` as the PNG `path` with `fs`.
fn write_png<P>(
    fs: &impl FileSystem,
    path: impl AsRef<Path>,
    image: &image::ImageBuffer<P, Vec<P::Subpixel>>,
) where
    P: image::PixelWithColorType,
    [P::Subpixel]: image::EncodableLayout,
{
    image
        .write_to(
            &mut fs.create(path).expect("could not save output png"),
            image::ImageFormat::Png,
        )
        .expect("could not save output png");
}

pub fn batch_process(
    conf: &Config,
    fs: &impl FileSystem,
    thread: &String,
    has_zip: bool,
    plan: Arc<Plan>,
    rx: Consumer<InputFileIndex>,
) {
    let &Config {
        debug_intermediates,
        outputs,
        map_frame: frame,
        vege_bitmode,
        ..
    } = conf;

    let Config { batchoutfolder, .. } = conf;

    // take input files from queue until there are no more
    while let Some(laz_path) = rx.pop() {
        let file_to_process = plan.get_input_file(laz_path);

        let infile = file_to_process.path.as_path();
        let laz = infile.file_stem().unwrap().to_str().unwrap();
        let outfile = file_to_process.output_path.as_path();

        info!("{} -> {}", infile.display(), outfile.display());

        let Rect {
            minx,
            miny,
            maxx,
            maxy,
        } = file_to_process.header.bounds;

        // every tile starts from an empty folder: finish_tile_folder removes it, and
        // one left by a failed tile or an older build is cleared here
        let tmpfolder = PathBuf::from(format!("temp{thread}"));
        clear_tile_folder(fs, &tmpfolder).expect("Could not clear the tile's temp folder");

        // Process the tile; the map is rendered here, with the shape files and cropped
        // to the tile
        // the tile's returns, buffered from its neighbours, are staged by launch_threads
        let staged = &file_to_process.staging_path;
        let mut values = process_tile(fs, conf, thread, &tmpfolder, staged, laz, true)
            .unwrap_or_else(|e| panic!("processing tile {laz} failed: {e}"));
        // debug_intermediates=1 keeps them in the tile folder as xyztemp.xyz.bin
        fs.remove_file(staged)
            .expect("Could not remove the staged point file");

        // the .dxf.bin crops: the batch merge's input for the merged DXF, and with the
        // dxf family each tile's DXF crop. The .dxf.bin crops are removed after the
        // batch unless debug_intermediates=1 (remove_batch_intermediates); contours03,
        // the knoll candidates, and detected, the knoll rings, are debug only.
        let dxf_crops = outputs.dxf || debug_intermediates;
        let write = |name: &str, crop: anyhow::Result<BinaryDxf>| {
            let output = format!("{batchoutfolder}/{laz}_{name}.dxf.bin");
            crop::write_crop(fs, &crop.unwrap(), Path::new(&output), outputs.dxf).unwrap();
        };
        let lines = |name: &str, dxf: BinaryDxf| {
            write(name, crop::crop_polylines(dxf, minx, miny, maxx, maxy));
        };
        // what the map does not draw is cropped (or dropped) before the maps are
        // rendered
        let dumps = values.dumps.take();
        let areas = values.vegetation.as_mut().and_then(|v| v.areas.take());
        let basemap = values.basemap.take();
        if dxf_crops {
            if let Some(dumps) = dumps {
                if let Some(candidates) = dumps.knoll_candidates {
                    lines("contours03", candidates);
                }
                if let Some((detected, _)) = dumps.detected {
                    lines("detected", detected.to_bindxf());
                }
            }
            if let Some(areas) = areas {
                lines("vegetation", areas.to_bindxf());
            }
            if let Some(basemap) = basemap {
                lines("basemap", basemap);
            }
        }

        // the vegetation layers the map is drawn on, and cropped below
        let layers = values
            .rasters()
            .filter(|_| outputs.raster)
            .map(|rasters| rasters.layers());
        if let Some(layers) = &layers
            && let Some(inputs) = values.map_inputs(layers)
        {
            let shapes = has_zip
                .then(|| {
                    let frame = layers.frame();
                    shape_layers(fs, conf, &tmpfolder, frame, &[], debug_intermediates)
                })
                .flatten();
            let inputs = &render::MapInputs {
                shapes: shapes.as_ref(),
                ..inputs
            };
            // the map without the depressions first: its world file is both maps' crop
            let mut cropped_world = None;
            for nodepressions in [true, false] {
                info!("Rendering png map");
                let map = render::render(&conf.render, inputs, nodepressions);
                let tfw = cropped_world.get_or_insert_with(|| {
                    let tfw = &map.world;
                    (
                        minx - tfw.x_origin,
                        -maxy + tfw.y_origin,
                        WorldFile {
                            x_origin: minx + tfw.pixel_size_x / 2.0,
                            y_origin: maxy - tfw.pixel_size_x / 2.0,
                            ..tfw.clone()
                        },
                    )
                });
                let (dx, dy, world) = &*tfw;
                let rgb: RgbImage = map.image.convert();
                drop(map);
                let img = placed(
                    &rgb,
                    (frame.to_px(maxx - minx) + 2.0) as u32,
                    (frame.to_px(maxy - miny) + 2.0) as u32,
                    Rgb([255, 255, 255]),
                    frame.to_px(-dx) as i64,
                    frame.to_px(-dy) as i64,
                );
                drop(rgb);
                // the working copies (kept with debug_intermediates=1), then the tile's
                // map in the output folder
                let stem = render::map_stem(thread, nodepressions);
                write_png(fs, format!("{stem}.png"), &img);
                world
                    .write(
                        &mut fs
                            .create(format!("{stem}.pgw"))
                            .expect("Unable to create file"),
                    )
                    .expect("Unable to write to file");
                crate::crs::write_raster_crs(fs, format!("{stem}.png"), conf.epsg)
                    .expect("Could not write raster CRS sidecar");
                let (png, pgw) = if nodepressions {
                    (outfile.to_path_buf(), format!("{batchoutfolder}/{laz}.pgw"))
                } else {
                    (
                        PathBuf::from(format!("{batchoutfolder}/{laz}_depr.png")),
                        format!("{batchoutfolder}/{laz}_depr.pgw"),
                    )
                };
                fs.copy(format!("{stem}.png"), &png)
                    .expect("Could not copy file to output folder");
                fs.copy(format!("{stem}.pgw"), pgw)
                    .expect("Could not copy file to output folder");
                crate::crs::write_raster_crs(fs, png, conf.epsg)
                    .expect("Could not write raster CRS sidecar");
            }
        } else if has_zip
            && values.terrain.is_some()
            && values.cliffs.is_some()
            && conf.vector_tables()
            && !conf.vectorconf.is_empty()
        {
            // the vector mapping's tables, without drawing the shapes
            #[cfg(feature = "shapefile")]
            crate::shapefile::vector_tables(
                fs,
                conf,
                &tmpfolder,
                vegetation::VegetationFrame::of_ground(&values.ground, &conf.vegetation),
            )
            .unwrap();
        }

        // the vegetation rasters, cropped to the tile like the map
        if let Some(layers) = layers
            && let Some(rasters) = values.rasters()
        {
            let tfw = &rasters.undergrowth_world;

            let dx = minx - tfw.x_origin;
            let dy = -maxy + tfw.y_origin;

            let mut pgw_file_out = fs
                .create(PathBuf::from(&format!(
                    "{batchoutfolder}/{laz}_undergrowth.pgw"
                )))
                .expect("Unable to create file");
            WorldFile {
                x_origin: minx + tfw.pixel_size_x / 2.0,
                y_origin: maxy - tfw.pixel_size_x / 2.0,
                ..tfw.clone()
            }
            .write(&mut pgw_file_out)
            .expect("Unable to write to file");
            drop(pgw_file_out);

            let img = placed(
                &layers.undergrowth,
                (frame.to_px(maxx - minx) + 2.0) as u32,
                (frame.to_px(maxy - miny) + 2.0) as u32,
                Rgba([255, 255, 255, 0]),
                frame.to_px(-dx) as i64,
                frame.to_px(-dy) as i64,
            );
            write_png(fs, format!("{batchoutfolder}/{laz}_undergrowth.png"), &img);

            // one pixel per metre from here
            let width = ((maxx - minx) + 1.0) as u32;
            let height = ((maxy - miny) + 1.0) as u32;
            let vegetation: RgbImage = layers.vegetation.convert();
            let img = placed(
                &vegetation,
                width,
                height,
                Rgb([255, 255, 255]),
                -dx as i64,
                -dy as i64,
            );
            write_png(fs, format!("{batchoutfolder}/{laz}_vege.png"), &img);

            let mut pgw_file_out = fs
                .create(format!("{batchoutfolder}/{laz}_vege.pgw"))
                .expect("Unable to create file");
            WorldFile::north_up(1.0, minx + 0.5, maxy - 0.5)
                .write(&mut pgw_file_out)
                .expect("Unable to write to file");

            drop(pgw_file_out);

            if vege_bitmode {
                let bits = rasters
                    .bits
                    .as_ref()
                    .expect("vege_bitmode draws the one-channel rasters");
                for (name, bit) in [
                    ("vege_bit", &bits.vegetation.to_luma8()),
                    ("undergrowth_bit", &rasters.undergrowth_bit),
                ] {
                    let img = placed(bit, width, height, Luma([0]), -dx as i64, -dy as i64);
                    write_png(fs, format!("{batchoutfolder}/{laz}_{name}.png"), &img);
                    fs.copy(
                        format!("{batchoutfolder}/{laz}_vege.pgw"),
                        format!("{batchoutfolder}/{laz}_{name}.pgw"),
                    )
                    .expect("Could not copy file");
                }
            }
        }

        // the terrain's and the cliffs' crops, after the map that draws them
        if dxf_crops {
            let TileValues {
                terrain, cliffs, ..
            } = values;
            // the terrain and the cliffs are consumed here, without a copy
            if let Some(Terrain {
                contours,
                dot_knolls,
                form_lines,
            }) = terrain
            {
                lines("contours", contours.into_bindxf());
                let points = crop::crop_points(dot_knolls.into_bindxf(), minx, miny, maxx, maxy);
                write("dotknolls", points);
                if let Some(form_lines) = form_lines {
                    lines("formlines", form_lines.to_bindxf());
                }
            }
            if let Some(cliffs) = cliffs {
                let (passable, impassable) = cliffs.into_bindxf();
                lines("c2g", passable);
                lines("c3g", impassable);
            }
        }
        // the tables (Config::vector_tables, the vector mapping's with shapefiles), cropped
        // to the tile like the rasters
        for &table in IsomTable::ALL {
            let path = tmpfolder.join(geojson::file_name(table));
            if fs.exists(&path) {
                geojson::crop_geojson(
                    fs,
                    &path,
                    &Path::new(batchoutfolder).join(geojson::tile_file_name(table, laz)),
                    &file_to_process.header.bounds,
                )
                .unwrap();
            }
        }
        finish_tile_folder(fs, thread, laz, debug_intermediates)
            .expect("Could not clean up the tile's temp folder");
    }
}

/// Remove `tmpfolder` if it exists, so a tile starts from an empty temp folder: files
/// another run left there (such as `low.png` and `high.png`, which a re-render draws
/// when present) would leak into this tile's outputs and debug intermediates.
pub fn clear_tile_folder(fs: &impl FileSystem, tmpfolder: &Path) -> std::io::Result<()> {
    if fs.exists(tmpfolder) {
        fs.remove_dir_all(tmpfolder)?;
    }
    Ok(())
}

/// Clear a batch tile's working files once its outputs are in the batch output folder,
/// so the next tile on `thread` starts from an empty temp folder. With `debug`
/// (debug_intermediates=1) the folder `temp{thread}` is moved to `temp_{laz}_dir` and
/// the rest is kept; otherwise it is removed with the map's working copies
/// `pullautus{thread}.*` and `pullautus_depr{thread}.*`, already cropped into the output
/// folder.
fn finish_tile_folder(
    fs: &impl FileSystem,
    thread: &str,
    laz: &str,
    debug: bool,
) -> std::io::Result<()> {
    let tmpfolder = PathBuf::from(format!("temp{thread}"));
    if debug {
        let kept = PathBuf::from(format!("temp_{laz}_dir"));
        fs.create_dir_all(&kept)?;
        for path in fs.list(&tmpfolder)? {
            if let Some(name) = path.file_name() {
                fs.copy(&path, kept.join(name))?;
            }
        }
    } else {
        let mut files = Vec::new();
        for map in [
            format!("pullautus{thread}"),
            format!("pullautus_depr{thread}"),
        ] {
            for ext in ["png", "pgw", "png.aux.xml"] {
                files.push(format!("{map}.{ext}"));
            }
        }
        for file in files {
            if fs.exists(&file) {
                fs.remove_file(&file)?;
            }
        }
    }
    fs.remove_dir_all(&tmpfolder)
}

/// Whether `name`, a file in a tile's temp folder, is a product of the families in
/// `outputs`: a GeoJSON table (geojson); a DXF of the contours, cliffs, knolls, form
/// lines, vegetation or base map (dxf); the vegetation and undergrowth rasters with
/// their world files, and the one-channel `_bit` rasters with vege_bitmode=1 (raster).
/// Everything else is a debug intermediate.
fn is_tile_product(name: &str, outputs: Outputs, vege_bitmode: bool) -> bool {
    const DXF: [&str; 7] = [
        "out2.dxf",
        "c2g.dxf",
        "c3g.dxf",
        "dotknolls.dxf",
        "formlines.dxf",
        "vegetation.dxf",
        "basemap.dxf",
    ];
    const RASTER: [&str; 4] = [
        "vegetation.png",
        "vegetation.pgw",
        "undergrowth.png",
        "undergrowth.pgw",
    ];
    (outputs.geojson && name.ends_with(".geojson"))
        || (outputs.dxf && DXF.contains(&name))
        || (outputs.raster
            && (RASTER.contains(&name)
                || (vege_bitmode && ["vegetation_bit.png", "undergrowth_bit.png"].contains(&name))))
}

/// Leave only the products ([`is_tile_product`]) in a single tile's temp folder, unless
/// `debug` (debug_intermediates=1) keeps everything. `input`, the name of the file or
/// folder in `tmpfolder` that holds the run's input (such as `xyztemp.xyz.bin`), is kept.
pub fn prune_tile_folder(
    fs: &impl FileSystem,
    tmpfolder: &Path,
    debug: bool,
    outputs: Outputs,
    vege_bitmode: bool,
    input: Option<&std::ffi::OsStr>,
) -> std::io::Result<()> {
    if debug {
        return Ok(());
    }
    for path in fs.list(tmpfolder)? {
        if input.is_some() && path.file_name() == input {
            continue;
        }
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        if is_tile_product(&name, outputs, vege_bitmode) {
            continue;
        }
        // the trait cannot tell a file from a folder: a folder fails remove_file
        if fs.remove_file(&path).is_err() {
            fs.remove_dir_all(&path)?;
        }
    }
    Ok(())
}

/// Remove the batch's intermediates from the batch output folder once the batch (and
/// its merge) is done, unless `debug` (debug_intermediates=1) keeps them: the tiles'
/// `.dxf.bin` crops, and with `intermediate_tables` (the batch's tile names, when this
/// run wrote the tables only as the source of the combined `output.dxf`) the tables it
/// wrote: each tile's, the merged and the combined ones. Other GeoJSON files, such as
/// an earlier run's published tables, are left alone.
pub fn remove_batch_intermediates(
    fs: &impl FileSystem,
    batchoutfolder: impl AsRef<Path>,
    debug: bool,
    intermediate_tables: Option<&[String]>,
) -> std::io::Result<()> {
    let batchoutfolder = batchoutfolder.as_ref();
    // a batch with no tiles may never create the folder
    if debug || !fs.exists(batchoutfolder) {
        return Ok(());
    }
    for path in fs.list(batchoutfolder)? {
        if path.to_string_lossy().ends_with(".dxf.bin") {
            fs.remove_file(&path)?;
        }
    }
    if let Some(tiles) = intermediate_tables {
        for &table in IsomTable::ALL {
            let names = tiles
                .iter()
                .map(|tile| geojson::tile_file_name(table, tile))
                .chain([geojson::merged_file_name(table), geojson::file_name(table)]);
            for name in names {
                let path = batchoutfolder.join(name);
                if fs.exists(&path) {
                    fs.remove_file(&path)?;
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod test {
    use super::*;
    use las::point::Format;

    /// Point data of the given format whose records have the given bytes 15 and 16.
    fn point_data(format: u8, bytes_15_16: &[(u8, u8)]) -> PointData {
        let format = Format::new(format).unwrap();
        let len = format.len() as usize;
        let mut bytes = vec![0; len * bytes_15_16.len()];
        for (rec, &(b15, b16)) in bytes.chunks_exact_mut(len).zip(bytes_15_16) {
            rec[15] = b15;
            rec[16] = b16;
        }
        PointDataBuilder::new()
            .with_format(format)
            .build_from_bytes(bytes)
            .unwrap()
    }

    use crate::io::fs::memory::MemoryFileSystem;

    fn touch(fs: &MemoryFileSystem, path: &str) {
        if let Some(parent) = Path::new(path).parent()
            && parent != Path::new("")
        {
            fs.create_dir_all(parent).unwrap();
        }
        fs.create(path).unwrap();
    }

    /// The files `fs` holds under `dir`, by name, in order.
    fn names(fs: &MemoryFileSystem, dir: &str) -> Vec<String> {
        let mut names: Vec<String> = fs
            .list(dir)
            .unwrap()
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// A batch tile's working files on thread 1, as process_tile and the crop leave them.
    fn tile_working_files(fs: &MemoryFileSystem) {
        for path in [
            "temp1/xyz2.hmap",
            "temp1/out2.dxf.bin",
            "temp1/contours.geojson",
            "pullautus1.png",
            "pullautus1.pgw",
            "pullautus1.png.aux.xml",
            "pullautus_depr1.png",
            "pullautus_depr1.pgw",
        ] {
            touch(fs, path);
        }
    }

    /// A 60 m square tile at (1000, 2000): a ground return every metre on a 12 m high
    /// cone with a 6 m step (a cliff) across its east side, and a block of echoes 6 m
    /// above the ground for the vegetation.
    fn cone_tile() -> Vec<XyzRecord> {
        let mut returns = Vec::new();
        for i in 0..60 {
            for j in 0..60 {
                let (x, y) = (1000.0 + i as f64, 2000.0 + j as f64);
                let r = ((i as f64 - 30.0).powi(2) + (j as f64 - 30.0).powi(2)).sqrt();
                let step = if i >= 50 { 0.0 } else { 6.0 };
                let z = ((12.0 - r * 0.5).max(0.0) + step) as f32 + 100.0;
                let ground = XyzRecord {
                    x,
                    y,
                    z,
                    classification: 2,
                    number_of_returns: 1,
                    return_number: 1,
                    flags: 0,
                };
                returns.push(ground);
                if i < 20 && j < 20 {
                    returns.push(XyzRecord {
                        z: z + 6.0,
                        classification: 5,
                        number_of_returns: 2,
                        ..ground
                    });
                }
            }
        }
        returns
    }

    /// The bytes of every file `fs` holds under `dir`, by name.
    fn contents(fs: &MemoryFileSystem, dir: &str) -> Vec<(String, Vec<u8>)> {
        names(fs, dir)
            .into_iter()
            .map(|name| {
                let mut bytes = Vec::new();
                std::io::Read::read_to_end(
                    &mut fs.open(Path::new(dir).join(&name)).unwrap(),
                    &mut bytes,
                )
                .unwrap();
                (name, bytes)
            })
            .collect()
    }

    #[test]
    fn a_tile_is_the_stages_values_written_and_rendered() {
        let config = Config::from_file(Path::new("pullauta.default.ini")).unwrap();
        let returns = cone_tile();
        let temp = Path::new("temp");

        let fs = MemoryFileSystem::new();
        fs.create_dir_all("in").unwrap();
        crate::io::xyz::write_all(fs.create("in/cone.xyz.bin").unwrap(), &returns).unwrap();
        let input = Path::new("in/cone.xyz.bin");
        let values = process_tile(&fs, &config, "", temp, input, "cone", false).unwrap();
        prune_tile_folder(&fs, temp, false, config.outputs, false, None).unwrap();
        assert_eq!(
            names(&fs, "temp"),
            [
                "c2g.dxf",
                "c3g.dxf",
                "cliffs.geojson",
                "contours.geojson",
                "dotknolls.dxf",
                "formlines.dxf",
                "knolls_points.geojson",
                "out2.dxf",
                "undergrowth.pgw",
                "undergrowth.png",
                "vegetation.dxf",
                "vegetation.pgw",
                "vegetation.png",
                "vegetation_areas.geojson",
            ]
        );
        for map in ["pullautus", "pullautus_depr"] {
            assert!(fs.exists(format!("{map}.png")), "{map}.png");
            assert!(fs.exists(format!("{map}.pgw")), "{map}.pgw");
        }
        assert!(values.terrain.is_some() && values.cliffs.is_some());
        assert!(values.dumps.is_none());
        // the tables append: the form lines (103.000) follow the contours they were
        // selected from
        let mut table = Vec::new();
        std::io::Read::read_to_end(&mut fs.open("temp/contours.geojson").unwrap(), &mut table)
            .unwrap();
        let table: serde_json::Value = serde_json::from_slice(&table).unwrap();
        let form_line: Vec<bool> = table["features"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["properties"]["isom_code"] == "103.000")
            .collect();
        let first = form_line.iter().position(|&f| f).expect("form lines");
        assert!(
            first > 0 && form_line[first..].iter().all(|&f| f),
            "{form_line:?}"
        );

        // the stages need no file system: their values, written, are the same products
        let again = MemoryFileSystem::new();
        again.create_dir_all(temp).unwrap();
        let values = run_stages(&config, &returns, "cone");
        write_tile_products(&again, &config, temp, &values).unwrap();
        prune_tile_folder(&again, temp, false, config.outputs, false, None).unwrap();
        assert_eq!(contents(&again, "temp"), contents(&fs, "temp"));
    }

    /// The batch crops the rendered map in memory: the same pixels as the PNG it used to
    /// write, read back, convert to RGB and overlay, also where the map is translucent.
    #[test]
    fn the_map_crop_from_the_value_is_the_crop_of_the_written_png() {
        let map = image::RgbaImage::from_fn(13, 9, |x, y| {
            Rgba([
                (x * 19) as u8,
                (y * 27) as u8,
                200,
                ((x + y) * 23 % 256) as u8,
            ])
        });
        let (width, height, x, y) = (10, 11, -4, 3);

        let mut png = Vec::new();
        map.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let decoded = image::load_from_memory_with_format(&png, image::ImageFormat::Png)
            .unwrap()
            .to_rgb8();
        let mut old = RgbImage::from_pixel(width, height, Rgb([255, 255, 255]));
        image::imageops::overlay(&mut old, &decoded, x, y);

        let rgb: RgbImage = map.convert();
        let new = placed(&rgb, width, height, Rgb([255, 255, 255]), x, y);
        assert_eq!(new, old);
    }

    #[test]
    fn a_stage_that_does_not_run_leaves_its_value_none() {
        let mut config = Config::from_file(Path::new("pullauta.default.ini")).unwrap();
        config.vegeonly = true;
        config.debug_intermediates = true;
        let values = run_stages(&config, &cone_tile(), "cone");
        assert!(values.terrain.is_none() && values.cliffs.is_none());
        assert!(values.basemap.is_none() && values.blocks.is_none());
        assert!(values.vegetation.is_some_and(|v| v.rasters.is_some()));
        let dumps = values.dumps.unwrap();
        assert!(dumps.lifted.is_none() && dumps.passable_raster.is_none());
    }

    #[test]
    fn a_finished_tile_leaves_no_working_files() {
        let fs = MemoryFileSystem::new();
        tile_working_files(&fs);
        touch(&fs, "out/tile.png");

        finish_tile_folder(&fs, "1", "tile", false).unwrap();

        assert_eq!(names(&fs, "."), ["out"]);
        assert_eq!(names(&fs, "out"), ["tile.png"]);
    }

    #[test]
    fn a_debug_tile_moves_its_folder_and_keeps_the_rest() {
        let fs = MemoryFileSystem::new();
        tile_working_files(&fs);

        finish_tile_folder(&fs, "1", "tile", true).unwrap();

        // the next tile on the thread starts from an empty folder
        assert!(!fs.exists("temp1"));
        assert_eq!(
            names(&fs, "temp_tile_dir"),
            ["contours.geojson", "out2.dxf.bin", "xyz2.hmap"]
        );
        assert!(fs.exists("pullautus1.png"));
    }

    #[test]
    fn a_single_tile_folder_is_pruned_to_its_products() {
        let fs = MemoryFileSystem::new();
        let products = [
            "c2g.dxf",
            "c3g.dxf",
            "cliffs.geojson",
            "contours.geojson",
            "dotknolls.dxf",
            "formlines.dxf",
            "out2.dxf",
            "undergrowth.pgw",
            "undergrowth.png",
            "vegetation.dxf",
            "vegetation.pgw",
            "vegetation.png",
        ];
        let intermediates = [
            "c2g.dxf.bin",
            "contours03.dxf",
            "detected.dxf",
            "out.dxf",
            "pins.bin",
            "undergrowth_bit.png",
            "xyz2.hmap",
            "xyztemp.xyz.bin",
            "sub/extracted.shp",
        ];
        for name in products.iter().chain(&intermediates) {
            touch(&fs, &format!("temp/{name}"));
        }

        prune_tile_folder(&fs, Path::new("temp"), true, Outputs::ALL, false, None).unwrap();
        assert_eq!(
            names(&fs, "temp").len(),
            products.len() + intermediates.len()
        );

        prune_tile_folder(&fs, Path::new("temp"), false, Outputs::ALL, false, None).unwrap();
        assert_eq!(names(&fs, "temp"), products);
    }

    #[test]
    fn a_single_tile_keeps_the_products_of_the_selected_families() {
        let kept = |raster, dxf, geojson| {
            let fs = MemoryFileSystem::new();
            for name in [
                "contours.geojson",
                "c2g.dxf",
                "vegetation.png",
                "vegetation.pgw",
            ] {
                touch(&fs, &format!("temp/{name}"));
            }
            let outputs = Outputs {
                raster,
                dxf,
                geojson,
            };
            prune_tile_folder(&fs, Path::new("temp"), false, outputs, false, None).unwrap();
            names(&fs, "temp")
        };
        assert_eq!(kept(false, false, true), ["contours.geojson"]);
        assert_eq!(kept(false, true, false), ["c2g.dxf"]);
        assert_eq!(
            kept(true, false, false),
            ["vegetation.pgw", "vegetation.png"]
        );
    }

    #[test]
    fn pruning_keeps_the_runs_input() {
        let fs = MemoryFileSystem::new();
        for name in ["xyztemp.xyz.bin", "xyz2.hmap", "out2.dxf"] {
            touch(&fs, &format!("temp/{name}"));
        }
        let input = std::ffi::OsStr::new("xyztemp.xyz.bin");
        prune_tile_folder(
            &fs,
            Path::new("temp"),
            false,
            Outputs::ALL,
            false,
            Some(input),
        )
        .unwrap();
        assert_eq!(names(&fs, "temp"), ["out2.dxf", "xyztemp.xyz.bin"]);
    }

    #[test]
    fn the_bit_rasters_are_products_with_vege_bitmode() {
        let all = Outputs::ALL;
        assert!(is_tile_product("undergrowth_bit.png", all, true));
        assert!(is_tile_product("vegetation_bit.png", all, true));
        assert!(!is_tile_product("undergrowth_bit.png", all, false));
        assert!(!is_tile_product("greens_bit.png", all, true));
        let vectors = Outputs {
            raster: false,
            ..all
        };
        assert!(!is_tile_product("vegetation_bit.png", vectors, true));
    }

    #[test]
    fn the_batch_intermediates_are_removed_unless_debug() {
        let all = [
            "contours.geojson",
            "merged_contours.geojson",
            "other_contours.geojson",
            "parcels.geojson",
            "tile.png",
            "tile_c2g.dxf",
            "tile_c2g.dxf.bin",
            "tile_contours.geojson",
        ];
        let tiles = ["tile".to_string()];
        let left = |debug, intermediate_tables: Option<&[String]>| {
            let fs = MemoryFileSystem::new();
            for name in all {
                touch(&fs, &format!("out/{name}"));
            }
            remove_batch_intermediates(&fs, "out", debug, intermediate_tables).unwrap();
            names(&fs, "out")
        };
        assert_eq!(left(true, Some(&tiles)).len(), all.len());
        // no tables as intermediates (geojson on, or none written): every GeoJSON
        // stays, an earlier run's published tables included
        let mut geojson_kept = all.to_vec();
        geojson_kept.retain(|n| *n != "tile_c2g.dxf.bin");
        assert_eq!(left(false, None), geojson_kept);
        // the tables only fed the combined DXF: this batch's tables go, files it did
        // not write (another tile's, an unrelated layer) stay
        assert_eq!(
            left(false, Some(&tiles)),
            [
                "other_contours.geojson",
                "parcels.geojson",
                "tile.png",
                "tile_c2g.dxf"
            ]
        );
        // a batch with no tiles has no output folder
        let fs = MemoryFileSystem::new();
        remove_batch_intermediates(&fs, "missing", false, None).unwrap();
    }

    #[test]
    fn a_tile_folder_left_by_another_run_is_cleared() {
        let fs = MemoryFileSystem::new();
        touch(&fs, "temp1/low.png");
        clear_tile_folder(&fs, Path::new("temp1")).unwrap();
        assert!(!fs.exists("temp1"));
        // nothing to clear
        clear_tile_folder(&fs, Path::new("temp1")).unwrap();
    }

    #[test]
    fn las_flags_of_formats_0_to_5() {
        let pd = point_data(
            1,
            &[
                (2 | 0x80, 0), // withheld ground
                (2 | 0x20, 0), // synthetic ground
                (12, 0),       // overlap class
                (28, 0),       // reserved class 28: not overlap
                (2, 0xff),     // byte 16 is not a flags byte here
            ],
        );
        let flags: Vec<u8> = las_flags(&pd).collect();
        assert_eq!(
            flags,
            [
                XyzRecord::WITHHELD,
                XyzRecord::SYNTHETIC,
                XyzRecord::OVERLAP,
                0,
                0
            ]
        );
    }

    #[test]
    fn las_flags_of_formats_6_to_10() {
        let pd = point_data(
            6,
            &[
                (0b1000, 2),  // overlap bit
                (0b0001, 2),  // synthetic bit
                (0b0100, 2),  // withheld bit
                (0b0010, 2),  // key-point bit: not kept
                (0, 28),      // class 28: not overlap
                (0, 12),      // class 12 is not overlap in formats 6-10
                (0b1101, 18), // all three
            ],
        );
        let flags: Vec<u8> = las_flags(&pd).collect();
        assert_eq!(
            flags,
            [
                XyzRecord::OVERLAP,
                XyzRecord::SYNTHETIC,
                XyzRecord::WITHHELD,
                0,
                0,
                0,
                XyzRecord::WITHHELD | XyzRecord::SYNTHETIC | XyzRecord::OVERLAP
            ]
        );
    }
}
