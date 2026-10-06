use anyhow::Context;
use image::{GrayImage, Luma, Rgb, RgbImage, Rgba, RgbaImage};
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
use crate::geojson;
use crate::io::fs::FileSystem;
use crate::io::heightmap::HeightMap;
use crate::io::xyz::LasClass;
use crate::io::xyz::XyzInternalWriter;
use crate::io::xyz::XyzRecord;
use crate::isom::IsomTable;
use crate::knolls;
use crate::mapframe::WorldFile;
use crate::merge;
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

/// Renders the shape files in `filenames` (or, in a batch, the ones already unzipped)
/// and the map of the tile in `tmpfolder`, whose ground model is `ground`.
pub fn process_zip(
    fs: &impl FileSystem,
    config: &Config,
    thread: &String,
    tmpfolder: &Path,
    ground: &HeightMap,
    filenames: &[String],
    batch: bool,
) -> Result<(), Box<dyn Error>> {
    render::check_raster(config)?;
    let mut timing = Timing::start_now("process_zip");
    let &Config {
        pnorthlineswidth,
        pnorthlinesangle,
        ..
    } = config;
    #[cfg(feature = "shapefile")]
    {
        if !batch && !filenames.is_empty() {
            info!("Rendering shape files");
            timing.start_section("unzip and render shape files");
            crate::shapefile::unzip_and_render(fs, config, tmpfolder, filenames).unwrap();
        } else {
            crate::shapefile::render(fs, config, tmpfolder, true).unwrap();
        }
    }

    let inputs = &render::MapInputs { ground };
    info!("Rendering png map with depressions");
    timing.start_section("Rendering png map with depressions");
    render::render(
        fs,
        config,
        thread,
        tmpfolder,
        inputs,
        pnorthlinesangle,
        pnorthlineswidth,
        false,
    )
    .unwrap();

    info!("Rendering png map without depressions");
    timing.start_section("Rendering png map without depressions");
    render::render(
        fs,
        config,
        thread,
        tmpfolder,
        inputs,
        pnorthlinesangle,
        pnorthlineswidth,
        true,
    )
    .unwrap();

    Ok(())
}

/// Runs every stage on the returns of `input_file` and, unless `skip_rendering`, renders
/// the map. Returns the tile's ground model, which a batch draws the shape files' map on.
pub fn process_tile(
    fs: &impl FileSystem,
    config: &Config,
    thread: &String,
    tmpfolder: &Path,
    input_file: &Path,
    tile: &str,
    skip_rendering: bool,
) -> Result<HeightMap, Box<dyn Error>> {
    let mut timing = Timing::start_now("process_tile");
    fs.create_dir_all(tmpfolder)
        .expect("Could not create tmp folder");

    let &Config {
        pnorthlinesangle,
        pnorthlineswidth,
        skipknolldetection,
        ..
    } = config;

    timing.start_section("preparing input file");
    info!("Preparing input file");
    let returns = read_returns(fs, config, input_file, tile)?;
    if config.debug_intermediates {
        let target_file = tmpfolder.join("xyztemp.xyz.bin");
        debug!("Writing records to {:?}", target_file);
        crate::io::xyz::write_all(fs.create(&target_file)?, &returns)?;
    }
    info!("Done");

    info!("Knoll detection part 1");
    timing.start_section("knoll detection part 1");

    let &Config {
        vegeonly,
        cliffsonly,
        contoursonly,
        ..
    } = config;

    // every stage takes the ground model from here, not from its debug intermediates
    let ground = contours::xyz2heightmap(&returns, &config.ground, config.water_class);
    if config.debug_intermediates {
        // the same bytes under the two names the stages used to read
        ground
            .to_file(fs, tmpfolder.join(knolls::KNOLL_GROUND_DUMP))
            .unwrap();
        fs.copy(
            tmpfolder.join(knolls::KNOLL_GROUND_DUMP),
            tmpfolder.join(render::GROUND_DUMP),
        )
        .expect("Could not copy file");
    }

    if !(vegeonly || cliffsonly) {
        contours::heightmap2contours(
            fs,
            tmpfolder,
            config.knoll.candidate_interval_m,
            &ground,
            "contours03.dxf.bin", // dxf curves generated from the heightmap
            config.outputs.dxf,
        )
        .expect("contour generation failed");
    }

    // out.dxf.bin is traced at the levels smoothjoin reads it at
    let trace_interval = config.smoothjoin.levels().trace_interval;

    if !vegeonly && !cliffsonly {
        if let Some(basemapcontours) = config.basemapcontours {
            info!("Basemap contours");
            contours::heightmap2contours(
                fs,
                tmpfolder,
                basemapcontours,
                &ground,
                "basemap.dxf.bin", // generate dxf contours
                config.outputs.dxf,
            )
            .expect("contour generation failed");
        }
        if !skipknolldetection {
            info!("Knoll detection part 2");
            timing.start_section("knoll detection part 2");
            knolls::knolldetector(fs, &config.knoll, config.outputs.dxf, tmpfolder, &ground)
                .map_err(|e| {
                    format!(
                        "knoll detection (knolldetector) in {}: {e:#}",
                        tmpfolder.display()
                    )
                })?;
        }
        info!("Contour generation part 1");
        timing.start_section("contour generation part 1");
        // writes a lifted copy of the ground model to xyz_knolls.hmap
        knolls::xyzknolls(fs, &config.knoll, tmpfolder, &ground).map_err(|e| {
            format!(
                "knoll lifting (xyzknolls) in {}: {e:#}",
                tmpfolder.display()
            )
        })?;

        info!("Contour generation part 2");
        timing.start_section("contour generation part 2");
        if !skipknolldetection {
            // contours 2.5
            let xyz_knolls = HeightMap::from_file(fs, tmpfolder.join("xyz_knolls.hmap"))
                .expect("could not read xyz_knolls heightmap");
            contours::heightmap2contours(
                fs,
                tmpfolder,
                trace_interval,
                &xyz_knolls,
                "out.dxf.bin", // generates dxf curves
                config.outputs.dxf,
            )
            .unwrap();
        } else {
            // the unlifted ground model: xyz2heightmap again would build the same one
            contours::heightmap2contours(
                fs,
                tmpfolder,
                trace_interval,
                &ground,
                "out.dxf.bin", // generate dxf curves
                config.outputs.dxf,
            )
            .unwrap();
        }
        info!("Contour generation part 3");
        timing.start_section("contour generation part 3");
        merge::smoothjoin(fs, &config.smoothjoin, config.outputs.dxf, tmpfolder).unwrap();

        info!("Contour generation part 4");
        timing.start_section("contour generation part 4");
        knolls::dotknolls(fs, &config.knoll, config.outputs.dxf, tmpfolder).unwrap();

        // The terrain reaches vector output as GeoJSON written next to its source: the
        // .dxf.bin files are intermediates.
        if config.vector_tables() {
            for (input, source) in [
                ("out2.dxf.bin", geojson::Source::Contours),
                ("dotknolls.dxf.bin", geojson::Source::Knolls),
            ] {
                geojson::bindxf_to_tables(
                    fs,
                    &[tmpfolder.join(input)],
                    tmpfolder,
                    source,
                    config.epsg,
                )
                .unwrap();
            }
        }
    }

    if !cliffsonly && !contoursonly {
        info!("Vegetation generation");
        timing.start_section("vegetation generation");
        let classes =
            vegetation::makevege(fs, &config.vegetation, tmpfolder, &ground, &returns).unwrap();
        if config.outputs.vectorizes_vegetation() {
            vege_vector::export_all(fs, config, tmpfolder, &classes).unwrap();
        }
    }

    if !vegeonly && !contoursonly {
        info!("Cliff generation");
        timing.start_section("cliff generation");
        cliffs::makecliffs(
            fs,
            &config.cliff,
            config.outputs.dxf,
            tmpfolder,
            tile,
            &ground,
            &returns,
        )
        .unwrap();

        if config.vector_tables() {
            geojson::bindxf_to_tables(
                fs,
                &[tmpfolder.join("c2g.dxf.bin"), tmpfolder.join("c3g.dxf.bin")],
                tmpfolder,
                geojson::Source::Cliffs,
                config.epsg,
            )
            .unwrap();
        }
    }
    if !vegeonly && !contoursonly && !cliffsonly && config.detectbuildings {
        info!("Detecting buildings");
        timing.start_section("detecting buildings");
        blocks::blocks(fs, config.water_class, tmpfolder, &ground, &returns).unwrap();
    }
    // rendering reads the stages' outputs, not the returns
    drop(returns);
    // the map, or without the raster family the form lines alone (the renderer selects
    // them) when a vector family takes them
    let terrain = !vegeonly && !contoursonly && !cliffsonly;
    if !skip_rendering && terrain && config.outputs.raster {
        let inputs = &render::MapInputs { ground: &ground };
        info!("Rendering png map with depressions");
        timing.start_section("rendering png map with depressions");
        render::render(
            fs,
            config,
            thread,
            tmpfolder,
            inputs,
            pnorthlinesangle,
            pnorthlineswidth,
            false,
        )
        .unwrap();

        info!("Rendering png map without depressions");
        timing.start_section("rendering png map without depressions");
        render::render(
            fs,
            config,
            thread,
            tmpfolder,
            inputs,
            pnorthlinesangle,
            pnorthlineswidth,
            true,
        )
        .unwrap();
    } else if contoursonly || (terrain && !config.outputs.raster) {
        // outputs is never empty: without raster a vector family takes the form lines
        info!("Selecting formlines");
        timing.start_section("selecting formlines");
        let mut img = RgbaImage::from_pixel(1, 1, Rgba([0, 0, 0, 0]));
        if let Some(formlines) = render::draw_curves(
            fs,
            &config.curves,
            &mut img,
            tmpfolder,
            &ground,
            false,
            false,
        )
        .unwrap()
        {
            render::write_formlines(fs, config, tmpfolder, &formlines).unwrap();
        }
    } else {
        info!("Skipped rendering");
    }
    info!("All done!");
    Ok(ground)
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

    let mut returns = Vec::new();
    if filename.ends_with(".xyz") {
        // if we are here we don't know if the file has at least 6 columns, but we assume that it is in the format
        // x y z classification number_of_returns return_number

        info!("Reading points from .xyz");
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
    } else if filename.ends_with(".laz") || filename.ends_with(".las") {
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
    } else if filename.ends_with(".xyz.bin") {
        info!("Reading points from .xyz.bin");
        returns = crate::io::xyz::read_all(fs.open(input_file)?)?;
    } else {
        return Err(format!("Unsupported input file: {}", input_file.display()).into());
    }
    if returns.is_empty() {
        return Err(format!("no returns in {}", input_file.display()).into());
    }
    // the returns are held through every stage: no growth slack
    returns.shrink_to_fit();
    Ok(returns)
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
        vegeonly,
        cliffsonly,
        contoursonly,
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

        // Process the tile
        // the tile's returns, buffered from its neighbours, are staged by launch_threads
        let staged = &file_to_process.staging_path;
        let ground = process_tile(fs, conf, thread, &tmpfolder, staged, laz, has_zip)
            .unwrap_or_else(|e| panic!("processing tile {laz} failed: {e}"));
        // debug_intermediates=1 keeps them in the tile folder as xyztemp.xyz.bin
        fs.remove_file(staged)
            .expect("Could not remove the staged point file");

        if has_zip && !vegeonly && !cliffsonly && !contoursonly {
            if outputs.raster {
                process_zip(fs, conf, thread, &tmpfolder, &ground, &[], true).unwrap();
            } else if conf.vector_tables() && !conf.vectorconf.is_empty() {
                // the vector mapping's tables, without drawing the shapes
                #[cfg(feature = "shapefile")]
                crate::shapefile::vector_tables(fs, conf, &tmpfolder).unwrap();
            }
        }
        // the crop below re-encodes the PNGs: free the ground model first
        drop(ground);

        // crop
        let tfw_in = PathBuf::from(format!("pullautus{thread}.pgw"));
        if outputs.raster && fs.exists(&tfw_in) {
            let tfw = WorldFile::read(fs, &tfw_in).expect("PGW file does not exist");

            let dx = minx - tfw.x_origin;
            let dy = -maxy + tfw.y_origin;

            let mut pgw_file_out = fs.create(&tfw_in).expect("Unable to create file");
            WorldFile {
                x_origin: minx + tfw.pixel_size_x / 2.0,
                y_origin: maxy - tfw.pixel_size_x / 2.0,
                ..tfw
            }
            .write(&mut pgw_file_out)
            .expect("Unable to write to file");

            drop(pgw_file_out);
            fs.copy(
                Path::new(&format!("pullautus{thread}.pgw")),
                Path::new(&format!("pullautus_depr{thread}.pgw")),
            )
            .expect("Could not copy file");

            let orig_img = fs
                .read_image_png(format!("pullautus{thread}.png"))
                .expect("Opening image failed");
            let mut img = RgbImage::from_pixel(
                (frame.to_px(maxx - minx) + 2.0) as u32,
                (frame.to_px(maxy - miny) + 2.0) as u32,
                Rgb([255, 255, 255]),
            );
            image::imageops::overlay(
                &mut img,
                &orig_img.to_rgb8(),
                frame.to_px(-dx) as i64,
                frame.to_px(-dy) as i64,
            );

            img.write_to(
                &mut fs
                    .create(format!("pullautus{thread}.png"))
                    .expect("could not save output png"),
                image::ImageFormat::Png,
            )
            .expect("could not save output png");

            let orig_img = fs
                .read_image_png(format!("pullautus_depr{thread}.png"))
                .expect("Opening image failed");
            let mut img = RgbImage::from_pixel(
                (frame.to_px(maxx - minx) + 2.0) as u32,
                (frame.to_px(maxy - miny) + 2.0) as u32,
                Rgb([255, 255, 255]),
            );
            image::imageops::overlay(
                &mut img,
                &orig_img.to_rgb8(),
                frame.to_px(-dx) as i64,
                frame.to_px(-dy) as i64,
            );

            img.write_to(
                &mut fs
                    .create(format!("pullautus_depr{thread}.png"))
                    .expect("could not save output png"),
                image::ImageFormat::Png,
            )
            .expect("could not save output png");

            fs.copy(format!("pullautus{thread}.png"), outfile)
                .expect("Could not copy file to output folder");
            fs.copy(
                format!("pullautus{thread}.pgw"),
                format!("{batchoutfolder}/{laz}.pgw"),
            )
            .expect("Could not copy file to output folder");
            fs.copy(
                format!("pullautus_depr{thread}.png"),
                format!("{batchoutfolder}/{laz}_depr.png"),
            )
            .expect("Could not copy file to output folder");
            fs.copy(
                format!("pullautus_depr{thread}.pgw"),
                format!("{batchoutfolder}/{laz}_depr.pgw"),
            )
            .expect("Could not copy file to output folder");
            for png in [
                outfile.to_path_buf(),
                PathBuf::from(format!("{batchoutfolder}/{laz}_depr.png")),
            ] {
                crate::crs::write_raster_crs(fs, png, conf.epsg)
                    .expect("Could not write raster CRS sidecar");
            }
        }

        // the vegetation rasters, cropped to the tile like the map
        if outputs.raster && !contoursonly && !cliffsonly {
            let path = format!("temp{thread}/undergrowth.pgw");
            let tfw_in = Path::new(&path);
            let tfw = WorldFile::read(fs, tfw_in).expect("PGW file does not exist");

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
                ..tfw
            }
            .write(&mut pgw_file_out)
            .expect("Unable to write to file");
            drop(pgw_file_out);

            let mut orig_img_reader = image::ImageReader::new(
                fs.open(format!("temp{thread}/undergrowth.png"))
                    .expect("Opening undergrowth image failed"),
            );
            orig_img_reader.set_format(image::ImageFormat::Png);
            orig_img_reader.no_limits();
            let orig_img = orig_img_reader.decode().unwrap();
            let mut img = RgbaImage::from_pixel(
                (frame.to_px(maxx - minx) + 2.0) as u32,
                (frame.to_px(maxy - miny) + 2.0) as u32,
                Rgba([255, 255, 255, 0]),
            );
            image::imageops::overlay(
                &mut img,
                &orig_img,
                frame.to_px(-dx) as i64,
                frame.to_px(-dy) as i64,
            );

            img.write_to(
                &mut fs
                    .create(format!("{batchoutfolder}/{laz}_undergrowth.png"))
                    .expect("could not save output png"),
                image::ImageFormat::Png,
            )
            .expect("could not save output png");

            let mut orig_img_reader = image::ImageReader::new(
                fs.open(format!("temp{thread}/vegetation.png"))
                    .expect("Opening vegetation image failed"),
            );
            orig_img_reader.set_format(image::ImageFormat::Png);
            orig_img_reader.no_limits();
            let orig_img = orig_img_reader.decode().unwrap();
            let mut img = RgbImage::from_pixel(
                ((maxx - minx) + 1.0) as u32,
                ((maxy - miny) + 1.0) as u32,
                Rgb([255, 255, 255]),
            );
            image::imageops::overlay(&mut img, &orig_img.to_rgb8(), -dx as i64, -dy as i64);

            img.write_to(
                &mut fs
                    .create(format!("{batchoutfolder}/{laz}_vege.png"))
                    .expect("could not save output png"),
                image::ImageFormat::Png,
            )
            .expect("could not save output png");

            let mut pgw_file_out = fs
                .create(format!("{batchoutfolder}/{laz}_vege.pgw"))
                .expect("Unable to create file");
            WorldFile::north_up(1.0, minx + 0.5, maxy - 0.5)
                .write(&mut pgw_file_out)
                .expect("Unable to write to file");

            drop(pgw_file_out);

            if vege_bitmode {
                let mut orig_img_reader = image::ImageReader::new(
                    fs.open(format!("temp{thread}/vegetation_bit.png"))
                        .expect("Opening vegetation bit bit image failed"),
                );
                orig_img_reader.set_format(image::ImageFormat::Png);
                orig_img_reader.no_limits();
                let orig_img = orig_img_reader.decode().unwrap();
                let mut img = GrayImage::from_pixel(
                    ((maxx - minx) + 1.0) as u32,
                    ((maxy - miny) + 1.0) as u32,
                    Luma([0]),
                );
                image::imageops::overlay(&mut img, &orig_img.to_luma8(), -dx as i64, -dy as i64);
                img.write_to(
                    &mut fs
                        .create(format!("{batchoutfolder}/{laz}_vege_bit.png"))
                        .expect("could not save output png"),
                    image::ImageFormat::Png,
                )
                .expect("could not save output png");

                let mut orig_img_reader = image::ImageReader::new(
                    fs.open(format!("temp{thread}/undergrowth_bit.png"))
                        .expect("Opening undergrowth bit image failed"),
                );
                orig_img_reader.set_format(image::ImageFormat::Png);
                orig_img_reader.no_limits();
                let orig_img = orig_img_reader.decode().unwrap();
                let mut img = GrayImage::from_pixel(
                    ((maxx - minx) + 1.0) as u32,
                    ((maxy - miny) + 1.0) as u32,
                    Luma([0]),
                );
                image::imageops::overlay(&mut img, &orig_img.to_luma8(), -dx as i64, -dy as i64);
                img.write_to(
                    &mut fs
                        .create(format!("{batchoutfolder}/{laz}_undergrowth_bit.png"))
                        .expect("could not save output png"),
                    image::ImageFormat::Png,
                )
                .expect("could not save output png");

                fs.copy(
                    format!("{batchoutfolder}/{laz}_vege.pgw"),
                    format!("{batchoutfolder}/{laz}_vege_bit.pgw"),
                )
                .expect("Could not copy file");

                fs.copy(
                    format!("{batchoutfolder}/{laz}_vege.pgw"),
                    format!("{batchoutfolder}/{laz}_undergrowth_bit.pgw"),
                )
                .expect("Could not copy file");
            }
        }

        // the .dxf.bin crops: the batch merge's input for the merged DXF, and with the
        // dxf family each tile's DXF crop. The .dxf.bin crops are removed after the
        // batch unless debug_intermediates=1 (remove_batch_intermediates); contours03,
        // which the merge does not read, and detected, the knoll candidates, are debug
        // only.
        if outputs.dxf || debug_intermediates {
            let out2_path = PathBuf::from(format!("temp{thread}/out2.dxf.bin"));
            if fs.exists(&out2_path) {
                crop::polylinebindxfcrop(
                    fs,
                    &out2_path,
                    Path::new(&format!("{batchoutfolder}/{laz}_contours.dxf.bin")),
                    outputs.dxf,
                    minx,
                    miny,
                    maxx,
                    maxy,
                )
                .unwrap();
            }
            let dxf_files: &[&str] = if debug_intermediates {
                &[
                    "c2g",
                    "c3g",
                    "contours03",
                    "detected",
                    "formlines",
                    "vegetation",
                ]
            } else {
                &["c2g", "c3g", "formlines", "vegetation"]
            };
            for dxf_file in dxf_files {
                let dxf_path = PathBuf::from(format!("temp{thread}/{dxf_file}.dxf.bin"));
                if fs.exists(&dxf_path) {
                    crop::polylinebindxfcrop(
                        fs,
                        &dxf_path,
                        Path::new(&format!("{batchoutfolder}/{laz}_{dxf_file}.dxf.bin")),
                        outputs.dxf,
                        minx,
                        miny,
                        maxx,
                        maxy,
                    )
                    .unwrap();
                }
            }
            let dotknolls_file = PathBuf::from(format!("temp{thread}/dotknolls.dxf.bin"));
            if fs.exists(&dotknolls_file) {
                crop::pointbindxfcrop(
                    fs,
                    &dotknolls_file,
                    Path::new(&format!("{batchoutfolder}/{laz}_dotknolls.dxf.bin")),
                    outputs.dxf,
                    minx,
                    miny,
                    maxx,
                    maxy,
                )
                .unwrap();
            }
            let basemap_file = PathBuf::from(format!("temp{thread}/basemap.dxf.bin"));
            if fs.exists(&basemap_file) {
                crop::polylinebindxfcrop(
                    fs,
                    &basemap_file,
                    Path::new(&format!("{batchoutfolder}/{laz}_basemap.dxf.bin")),
                    outputs.dxf,
                    minx,
                    miny,
                    maxx,
                    maxy,
                )
                .unwrap();
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
/// another run left there (such as `low.png` and `high.png`, drawn into the map when
/// present) would leak into this tile's outputs.
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
