use log::debug;
use log::error;
use log::info;
use log::warn;
use pullauta::cliffs::CliffSet;
use pullauta::config::Config;
use pullauta::formlines::FormLineSelection;
use pullauta::io::fs::FileSystem;
use pullauta::io::fs::memory::MemoryFileSystem;
use pullauta::io::heightmap::HeightMap;
use pullauta::knolls::DotKnollSet;
use pullauta::merge::ContourSet;
use pullauta::render::{
    BLOCKS_DUMP, GROUND_DUMP, MapInputs, SHAPES_HIGH_DUMP, SHAPES_LOW_DUMP, ShapeLayers,
    UNDERGROWTH_PNG, VEGETATION_PGW, VEGETATION_PNG, VegetationLayers, WATER_BUILDINGS_DUMP,
};
use std::env;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() {
    // setup and configure logging, default to INFO when RUST_LOG is not set
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format(|buf, record| {
            use std::io::Write;
            let ts = buf.timestamp_seconds();
            let level_style = buf.default_level_style(record.level());

            writeln!(
                buf,
                "[{} {:?} {level_style}{}{level_style:#} {}] {}",
                ts,
                std::thread::current().id(),
                record.level(),
                record.module_path().unwrap_or(""),
                record.args()
            )
        })
        .init();

    // eval only reads its inputs: dispatch it before the config file and the
    // temp folder are created
    let raw_args: Vec<String> = env::args().skip(1).collect();
    if raw_args.first().is_some_and(|c| c == "eval") {
        match pullauta::eval::run(&raw_args[1..]) {
            Ok(true) => return,
            Ok(false) => std::process::exit(pullauta::eval::EXIT_CHANGED),
            Err(e) => {
                eprintln!("{e:#}");
                std::process::exit(1);
            }
        }
    }

    // an unknown or removed command word is reported before the config file and the
    // temp folder are created
    let Invocation {
        thread,
        command,
        args,
    } = or_exit(Invocation::parse(raw_args));

    let mut config = match Config::load_or_create_default() {
        Ok(config) => config,
        Err(e) => {
            error!("Could not load the config file: {e}");
            std::process::exit(1);
        }
    };

    let fs = pullauta::io::fs::local::LocalFileSystem;

    if matches!(command, Command::Default | Command::Tile(_)) {
        const VERSION: &str = env!("CARGO_PKG_VERSION");
        println!("Karttapullautin v{VERSION}\nThere is no warranty. Use it at your own risk!\n");
    }

    let batch: bool = config.batch;

    // the input tiles' CRS, unless the `epsg` ini key overrides it; the png merge
    // commands merge the batch's tiles
    let inputs = match &command {
        Command::Default if batch => {
            pullauta::process::batch_tiles(&fs, &config.lazfolder).unwrap_or_default()
        }
        Command::PngMerge { .. } | Command::PngMergeVege { .. } => {
            pullauta::process::batch_tiles(&fs, &config.lazfolder).unwrap_or_default()
        }
        Command::Tile(path) if is_las(path) => vec![PathBuf::from(path)],
        _ => Vec::new(),
    };
    config.epsg = pullauta::crs::resolve_epsg(&fs, config.epsg, &inputs).unwrap_or_else(|e| {
        error!("{e:#}");
        std::process::exit(1);
    });
    let config = Arc::new(config);

    let tmpfolder = PathBuf::from(format!("temp{thread}"));
    fs::create_dir_all(&tmpfolder).expect("Could not create tmp folder");

    match command {
        // re-render a tile run with debug_intermediates=1; a normal run keeps only
        // products in temp/, so `pullauta` alone prints the usage there
        Command::Default if !batch && pullauta::render::check_inputs(&fs, &tmpfolder).is_ok() => {
            let loaded = or_exit(read_render_inputs(&fs, &config, &tmpfolder, true));
            let inputs = &loaded.map_inputs();
            info!("Rendering png map with depressions");
            or_exit(pullauta::process::render_map(
                &fs, &config, &thread, inputs, false,
            ));
            info!("Rendering png map without depressions");
            or_exit(pullauta::process::render_map(
                &fs, &config, &thread, inputs, true,
            ));
            info!("\nAll done!");
        }

        Command::Default if !batch => {
            println!(
                "USAGE:\npullauta [parameter 1] [parameter 2] [parameter 3] ... [parameter n]\nSee README.MD for more details"
            );
        }

        Command::Default => {
            let Config { lazfolder, .. } = &*config;

            let mut zip_files: Vec<String> = Vec::new();
            for path in fs.list(lazfolder).unwrap() {
                if let Some(extension) = path.extension()
                    && extension == "zip"
                {
                    zip_files.push(String::from(path.to_str().unwrap()));
                }
            }

            if config.experimental_use_in_memory_fs {
                // copy all the input files into the memory file system
                let fs = pullauta::io::fs::memory::MemoryFileSystem::new();

                fs.create_dir_all(&config.lazfolder).unwrap();
                for file in fs::read_dir(&config.lazfolder).unwrap() {
                    let file = file.unwrap();
                    let path = file.path();
                    println!("Copying {} into memory fs", path.display());
                    fs.load_from_disk(&path, &path).unwrap();
                }
                // if there is an input vector file, copy it over as well

                if !config.vectorconf.is_empty() {
                    let path = Path::new(&config.vectorconf);
                    println!("Copying {} into memory fs", path.display());
                    fs.load_from_disk(path, path).unwrap();
                }

                pullauta::process::launch_threads(fs.clone(), config.clone(), &zip_files).unwrap();

                // copy the output files back to disk, and the tiles' temp folders
                // (temp_<tile>_dir) with debug_intermediates=1
                let mut folders = vec![PathBuf::from(&config.batchoutfolder)];
                if config.debug_intermediates {
                    folders.extend(fs.list(".").unwrap().into_iter().filter(|p| {
                        p.file_name()
                            .and_then(|n| n.to_str())
                            .is_some_and(|n| n.starts_with("temp_") && n.ends_with("_dir"))
                    }));
                }
                for folder in folders {
                    std::fs::create_dir_all(&folder).unwrap();
                    for path in fs.list(&folder).unwrap() {
                        info!("Copying {} from memory fs to disk", path.display());
                        fs.save_to_disk(&path, &path).unwrap();
                    }
                }
            } else {
                pullauta::process::launch_threads(fs.clone(), config.clone(), &zip_files).unwrap();
            }

            if config.batchmerge {
                info!("Batch done, merging tiles");
                let out = Path::new(&config.batchoutfolder);
                if config.outputs.raster {
                    pullauta::merge::pngmerge(&fs, &config, 4.0, false).unwrap();
                    pullauta::merge::pngmerge(&fs, &config, 4.0, true).unwrap();
                    pullauta::merge::pngmergevege(&fs, &config, 1.0, false).unwrap();
                }
                if config.outputs.dxf || config.debug_intermediates {
                    pullauta::merge::bindxfmerge(&fs, &config).unwrap();
                }
                if config.vector_tables() {
                    pullauta::geojson::merge_geojson(&fs, out).unwrap();
                    pullauta::geojson::export_combined(
                        &fs,
                        out,
                        &config.map_frame,
                        config.epsg,
                        config.outputs,
                    )
                    .unwrap();
                }
            }
            // the tiles' .dxf.bin crops are the merge's input, not output, and so are
            // the tables when they were written only for the combined DXF
            let tiles: Vec<String> = pullauta::process::batch_tiles(&fs, &config.lazfolder)
                .unwrap_or_default()
                .iter()
                .filter_map(|p| Some(p.file_stem()?.to_string_lossy().into_owned()))
                .collect();
            let intermediate_tables = config.vector_tables() && !config.outputs.geojson;
            pullauta::process::remove_batch_intermediates(
                &fs,
                &config.batchoutfolder,
                config.debug_intermediates,
                intermediate_tables.then_some(tiles.as_slice()),
            )
            .unwrap();
        }

        Command::Bin2Dxf => {
            if args.len() < 2 {
                info!("USAGE: bin2dxf [.dxf.bin input file] [.dxf output file]");
                return;
            }
            pullauta::io::bin2dxf(&fs, &args[0], &args[1]).unwrap();
        }

        Command::DxfMerge => {
            pullauta::merge::bindxfmerge(&fs, &config).unwrap();
        }

        Command::Merge => {
            let mut scale = 1.0;
            if !args.is_empty() {
                scale = args[0].parse::<f64>().unwrap();
            }
            pullauta::merge::bindxfmerge(&fs, &config).unwrap();
            pullauta::merge::pngmergevege(&fs, &config, scale, false).unwrap();
        }

        Command::PngMerge { depr } => {
            let mut scale = 4.0;
            if !args.is_empty() {
                scale = args[0].parse::<f64>().unwrap();
            }
            pullauta::merge::pngmerge(&fs, &config, scale, depr).unwrap();
        }

        Command::PngMergeVege { undergrowth } => {
            let mut scale = 1.0;
            if !args.is_empty() {
                scale = args[0].parse::<f64>().unwrap();
            }
            pullauta::merge::pngmergevege(&fs, &config, scale, undergrowth).unwrap();
        }

        Command::PolylineDxfCrop => {
            let dxffilein = Path::new(&args[0]);
            let dxffileout = Path::new(&args[1]);
            let minx = args[2].parse::<f64>().unwrap();
            let miny = args[3].parse::<f64>().unwrap();
            let maxx = args[4].parse::<f64>().unwrap();
            let maxy = args[5].parse::<f64>().unwrap();

            // helpful message if normal dxf file is specified
            if dxffilein.extension().is_some_and(|e| e == "dxf")
                || dxffileout.extension().is_some_and(|e| e == "dxf")
            {
                info!(
                    "The polylinedxfcrop command no longer takes raw DXF files as input and output. Please provide paths to `.bin.dxf` files instead."
                );
                return;
            }

            pullauta::crop::polylinebindxfcrop(
                &fs,
                dxffilein,
                dxffileout,
                config.outputs.dxf,
                minx,
                miny,
                maxx,
                maxy,
            )
            .unwrap();
        }

        Command::PointDxfCrop => {
            let dxffilein = Path::new(&args[0]);
            let dxffileout = Path::new(&args[1]);
            let minx = args[2].parse::<f64>().unwrap();
            let miny = args[3].parse::<f64>().unwrap();
            let maxx = args[4].parse::<f64>().unwrap();
            let maxy = args[5].parse::<f64>().unwrap();
            if dxffilein.extension().is_some_and(|e| e == ".dxf")
                || dxffileout.extension().is_some_and(|e| e == ".dxf")
            {
                info!(
                    "The pointdxfcrop command no longer takes raw DXF files as input and output. Please provide paths to `.bin.dxf` files instead."
                );
                return;
            }
            pullauta::crop::pointbindxfcrop(
                &fs,
                dxffilein,
                dxffileout,
                config.outputs.dxf,
                minx,
                miny,
                maxx,
                maxy,
            )
            .unwrap();
        }

        #[cfg(feature = "shapefile")]
        Command::UnzipMtk => {
            // the layers go to the temp folder, for a re-render
            let frame = pullauta::shapefile::read_vegetation_frame(&fs, &tmpfolder).unwrap();
            pullauta::shapefile::unzip_and_render(&fs, &config, &tmpfolder, &args, frame, true)
                .unwrap();
        }

        #[cfg(feature = "shapefile")]
        Command::MtkShapeRender => {
            let frame = pullauta::shapefile::read_vegetation_frame(&fs, &tmpfolder).unwrap();
            pullauta::shapefile::render(&fs, &config, &tmpfolder, frame, false, true).unwrap();
        }

        #[cfg(not(feature = "shapefile"))]
        Command::UnzipMtk | Command::MtkShapeRender => {
            error!("this pullauta was built without the shapefile feature");
            std::process::exit(1);
        }

        Command::Render => {
            let angle: f64 = args
                .first()
                .and_then(|s| s.parse::<f64>().ok())
                .expect("expected first argument to be angle");
            let nwidth: usize = args
                .get(1)
                .and_then(|s| s.parse::<usize>().ok())
                .expect("expected second argument to be nwidth");
            let nodepressions: bool = args.len() > 2 && args[2] == "nodepressions";
            let loaded = or_exit(read_render_inputs(&fs, &config, &tmpfolder, true));
            let map = pullauta::render::render(
                &config,
                &loaded.map_inputs(),
                angle,
                nwidth,
                nodepressions,
            );
            or_exit(pullauta::render::write_map(
                &fs,
                &pullauta::render::map_stem(&thread, nodepressions),
                &map,
                config.epsg,
            ));
        }

        Command::Zip(first) => {
            let mut zips: Vec<String> = vec![first];
            zips.extend(args);
            let loaded = or_exit(read_render_inputs(&fs, &config, &tmpfolder, false));
            or_exit(pullauta::process::process_zip(
                &fs,
                &config,
                &thread,
                &tmpfolder,
                &loaded.map_inputs(),
                &zips,
                false,
                // a re-render works on the debug intermediates: the shape layers join them
                true,
            ));
        }

        Command::Tile(input) => {
            let mut norender: bool = false;
            if args.len() > 1 {
                norender = args[1].clone() == "norender";
            }
            // the tile name seeds the thinning, also when the in-memory fs renames the input
            let tile = Path::new(&input).file_stem().unwrap().to_string_lossy();

            if config.experimental_use_in_memory_fs {
                let fs = pullauta::io::fs::memory::MemoryFileSystem::new();

                debug!("Copying input file into memory fs: {input}");
                // copy the input file into the memory file system
                fs.load_from_disk(Path::new(&input), Path::new("input.laz"))
                    .expect("Could not copy input file into memory fs");

                debug!("Done");

                or_exit(pullauta::process::process_tile(
                    &fs,
                    &config,
                    &thread,
                    &tmpfolder,
                    Path::new("input.laz"),
                    &tile,
                    norender,
                ));
                pullauta::process::prune_tile_folder(
                    &fs,
                    &tmpfolder,
                    config.debug_intermediates,
                    config.outputs,
                    config.vege_bitmode,
                    None,
                )
                .unwrap();

                // now write the output files to disk: the maps and what the temp folder
                // keeps
                fn copy(fs: &MemoryFileSystem, path: &Path) {
                    if fs.exists(path) {
                        info!("Copying {} from memory fs to disk", path.display());
                        fs.save_to_disk(path, path)
                            .expect("Could not copy from memory fs to disk");
                    }
                }
                copy(&fs, Path::new("pullautus.png"));
                copy(&fs, Path::new("pullautus_depr.png"));
                for path in fs.list(&tmpfolder).unwrap() {
                    copy(&fs, &path);
                }
            } else {
                // start from an empty temp folder, unless the input point file is in it
                // (such as temp/xyztemp.xyz.bin kept by debug_intermediates=1); then the
                // prune keeps it too: the file, or the folder in temp/ that holds it
                let input_in_temp = fs::canonicalize(&input)
                    .ok()
                    .zip(fs::canonicalize(&tmpfolder).ok())
                    .and_then(|(input, temp)| {
                        let first = input.strip_prefix(temp).ok()?.components().next()?;
                        Some(first.as_os_str().to_owned())
                    });
                if input_in_temp.is_none() {
                    pullauta::process::clear_tile_folder(&fs, &tmpfolder).unwrap();
                }
                or_exit(pullauta::process::process_tile(
                    &fs,
                    &config,
                    &thread,
                    &tmpfolder,
                    Path::new(&input),
                    &tile,
                    norender,
                ));
                pullauta::process::prune_tile_folder(
                    &fs,
                    &tmpfolder,
                    config.debug_intermediates,
                    config.outputs,
                    config.vege_bitmode,
                    input_in_temp.as_deref(),
                )
                .unwrap();
            }
        }
    }
}

/// The ground model's debug intermediate at `path` (`xyz2.hmap`), for a re-render. A
/// tile run writes it only with debug_intermediates=1: a missing one asks for the flag.
fn read_ground(fs: &impl FileSystem, path: &Path) -> Result<HeightMap, String> {
    read_debug_dump(fs, path, "ground model", || HeightMap::from_file(fs, path))
}

/// smoothjoin's contours dump (`out2.dxf.bin`), for a re-render.
fn read_contours(fs: &impl FileSystem, tmpfolder: &Path) -> Result<ContourSet, String> {
    let path = tmpfolder.join(pullauta::merge::CONTOURS_DUMP);
    read_debug_dump(fs, &path, "contours", || {
        ContourSet::from_bindxf(pullauta::geometry::BinaryDxf::from_reader(
            &mut fs.open(&path)?,
        )?)
    })
}

/// The dot knolls dump (`dotknolls.dxf.bin`), for a re-render.
fn read_dot_knolls(fs: &impl FileSystem, tmpfolder: &Path) -> Result<DotKnollSet, String> {
    let path = tmpfolder.join(pullauta::knolls::DOT_KNOLLS_DUMP);
    read_debug_dump(fs, &path, "dot knolls", || {
        DotKnollSet::from_bindxf(pullauta::geometry::BinaryDxf::from_reader(
            &mut fs.open(&path)?,
        )?)
    })
}

/// The cliffs dumps (`c2g.dxf.bin`, `c3g.dxf.bin`), for a re-render.
fn read_cliffs(fs: &impl FileSystem, tmpfolder: &Path) -> Result<CliffSet, String> {
    let read = |name| {
        let path = tmpfolder.join(name);
        read_debug_dump(fs, &path, "cliffs", || {
            pullauta::geometry::BinaryDxf::from_reader(&mut fs.open(&path)?)
        })
    };
    let passable = read(pullauta::cliffs::PASSABLE_DUMP)?;
    let impassable = read(pullauta::cliffs::IMPASSABLE_DUMP)?;
    CliffSet::from_bindxf(passable, impassable)
        .map_err(|e| format!("cannot read the cliffs from {}: {e}", tmpfolder.display()))
}

/// Read the debug dump at `path`, which holds the tile's `what`, with `read`. A tile run
/// writes the dumps only with debug_intermediates=1: a missing one asks for the flag.
fn read_debug_dump<T, E: std::fmt::Display>(
    fs: &impl FileSystem,
    path: &Path,
    what: &str,
    read: impl FnOnce() -> Result<T, E>,
) -> Result<T, String> {
    if !fs.exists(path) {
        return Err(format!(
            "cannot read the {what}: {} is missing. A re-render reads the tile's \
             {what} from its debug intermediates: re-run the tile with \
             debug_intermediates=1",
            path.display()
        ));
    }
    read().map_err(|e| format!("cannot read the {what} from {}: {e}", path.display()))
}

/// The values a re-render draws the map from, read from a debug run's intermediates.
struct RenderInputs {
    ground: HeightMap,
    contours: ContourSet,
    dot_knolls: DotKnollSet,
    cliffs: CliffSet,
    form_lines: Option<FormLineSelection>,
    vegetation: VegetationLayers,
    blocks: Option<image::RgbImage>,
    shapes: Option<ShapeLayers>,
}

impl RenderInputs {
    fn map_inputs(&self) -> MapInputs<'_> {
        MapInputs {
            ground: &self.ground,
            contours: &self.contours,
            dot_knolls: &self.dot_knolls,
            cliffs: &self.cliffs,
            form_lines: self.form_lines.as_ref(),
            vegetation: &self.vegetation,
            blocks: self.blocks.as_ref(),
            shapes: self.shapes.as_ref(),
        }
    }
}

/// The inputs of a re-render (`render`, a shape-file zip, `pullauta` in a debug run's
/// folder), once the raster family and every file it reads are there. The form lines
/// are selected again from the ground model and the contours, and written as a tile run
/// writes them (the dump too: a re-render works on the debug intermediates). With
/// `shapes` the shape files' layers left by an earlier run are loaded; a shape-file zip
/// draws its own.
fn read_render_inputs(
    fs: &impl FileSystem,
    config: &Config,
    tmpfolder: &Path,
    shapes: bool,
) -> Result<RenderInputs, String> {
    pullauta::render::check_raster(config).map_err(|e| e.to_string())?;
    pullauta::render::check_inputs(fs, tmpfolder).map_err(|e| e.to_string())?;
    let ground = read_ground(fs, &tmpfolder.join(GROUND_DUMP))?;
    let contours = read_contours(fs, tmpfolder)?;
    let form_lines =
        pullauta::process::make_form_lines(fs, config, tmpfolder, &ground, &contours, true)
            .map_err(|e| format!("form lines in {}: {e}", tmpfolder.display()))?;
    Ok(RenderInputs {
        ground,
        contours,
        dot_knolls: read_dot_knolls(fs, tmpfolder)?,
        cliffs: read_cliffs(fs, tmpfolder)?,
        form_lines,
        vegetation: read_vegetation_layers(fs, tmpfolder)?,
        blocks: read_optional_png(fs, tmpfolder, BLOCKS_DUMP)?.map(|png| png.to_rgb8()),
        shapes: if shapes {
            read_shape_layers(fs, tmpfolder)?
        } else {
            None
        },
    })
}

/// The shape files' layers (`low.png` and `high.png`), for a re-render; None without
/// them, or with a warning when only one of the two is there.
fn read_shape_layers(
    fs: &impl FileSystem,
    tmpfolder: &Path,
) -> Result<Option<ShapeLayers>, String> {
    match (
        read_optional_png(fs, tmpfolder, SHAPES_LOW_DUMP)?,
        read_optional_png(fs, tmpfolder, SHAPES_HIGH_DUMP)?,
    ) {
        (Some(low), Some(high)) => Ok(Some(ShapeLayers {
            low: low.to_rgba8(),
            high: high.to_rgba8(),
        })),
        (None, None) => Ok(None),
        (low, _) => {
            let (found, missing) = if low.is_some() {
                (SHAPES_LOW_DUMP, SHAPES_HIGH_DUMP)
            } else {
                (SHAPES_HIGH_DUMP, SHAPES_LOW_DUMP)
            };
            warn!(
                "{} has {found} but not {missing}: the shape files are not drawn",
                tmpfolder.display()
            );
            Ok(None)
        }
    }
}

/// The vegetation rasters (`vegetation.png`, `undergrowth.png`, `vegetation.pgw` and,
/// when there, `blueblack.png`), for a re-render.
fn read_vegetation_layers(
    fs: &impl FileSystem,
    tmpfolder: &Path,
) -> Result<VegetationLayers, String> {
    let png = |name| {
        let path = tmpfolder.join(name);
        read_debug_dump(fs, &path, "vegetation rasters", || fs.read_image_png(&path))
            .map(|png| png.to_rgba8())
    };
    let path = tmpfolder.join(VEGETATION_PGW);
    let world = read_debug_dump(fs, &path, "vegetation rasters", || {
        pullauta::mapframe::WorldFile::read(fs, &path)
    })?;
    Ok(VegetationLayers {
        vegetation: png(VEGETATION_PNG)?,
        undergrowth: png(UNDERGROWTH_PNG)?,
        water_buildings: read_optional_png(fs, tmpfolder, WATER_BUILDINGS_DUMP)?
            .map(|png| png.to_rgba8()),
        world,
    })
}

/// The PNG `name` in `tmpfolder`, None when it is not there: a re-render draws the
/// [`pullauta::render::OPTIONAL_RENDER_INPUTS`] it finds.
fn read_optional_png(
    fs: &impl FileSystem,
    tmpfolder: &Path,
    name: &str,
) -> Result<Option<image::DynamicImage>, String> {
    let path = tmpfolder.join(name);
    if !fs.exists(&path) {
        return Ok(None);
    }
    fs.read_image_png(&path)
        .map(Some)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))
}

/// The value, or exit with the error: for an error the user can act on, such as
/// re-rendering without the temp files.
fn or_exit<T, E: std::fmt::Display>(result: Result<T, E>) -> T {
    result.unwrap_or_else(|e| {
        error!("{e}");
        std::process::exit(1);
    })
}

/// The command line after the program name: an optional thread number, the
/// command word and the command's own arguments.
#[derive(Debug, PartialEq)]
struct Invocation {
    /// Names the temp folder (`temp{thread}`); empty when not given.
    thread: String,
    command: Command,
    args: Vec<String>,
}

impl Invocation {
    /// An error naming what to do instead when the command word is not a command.
    fn parse(mut args: Vec<String>) -> Result<Self, String> {
        let thread = if args
            .first()
            .is_some_and(|a| a.trim().parse::<usize>().is_ok())
        {
            args.remove(0)
        } else {
            String::new()
        };
        let command = if args.is_empty() {
            Command::Default
        } else {
            Command::parse(args.remove(0))?
        };
        Ok(Self {
            thread,
            command,
            args,
        })
    }
}

/// What the command word asks for. `eval` is dispatched before this, as it
/// needs neither the config file nor the temp folder.
#[derive(Debug, PartialEq)]
enum Command {
    /// No command: render the temp folder's map, print the usage, or run the
    /// batch (`batch=1`).
    Default,
    Bin2Dxf,
    DxfMerge,
    Merge,
    /// `pngmerge`, or `pngmergedepr` for the map with depressions.
    PngMerge {
        depr: bool,
    },
    /// `pngmergevege`, or `pngmergevegeundergrowth` for the undergrowth.
    PngMergeVege {
        undergrowth: bool,
    },
    PolylineDxfCrop,
    PointDxfCrop,
    UnzipMtk,
    MtkShapeRender,
    Render,
    /// A `.zip` file, the first of the zips to process.
    Zip(String),
    /// A `.las`, `.laz`, `.xyz` or `.xyz.bin` point cloud to process.
    Tile(String),
}

/// The stage commands that ran one stage on a tile's debug intermediates; the stages now
/// pass their values in memory (ADR 0004), so they run only as part of a tile.
const REMOVED_STAGE_COMMANDS: [&str; 8] = [
    "blocks",
    "dotknolls",
    "knolldetector",
    "makecliffs",
    "makevege",
    "smoothjoin",
    "xyzknolls",
    "xyz2contours",
];

/// The commands only the Perl version implements.
const PERL_ONLY_COMMANDS: [&str; 9] = [
    "cliffgeneralize",
    "ground",
    "ground2",
    "groundfix",
    "profile",
    "makecliffsold",
    "makeheight",
    "xyzfixer",
    "vege",
];

impl Command {
    /// Command names match exactly; file extensions ignore case. Anything else is an
    /// error saying what to do instead.
    fn parse(word: String) -> Result<Self, String> {
        Ok(match word.as_str() {
            "" => Self::Default,
            "bin2dxf" => Self::Bin2Dxf,
            "dxfmerge" => Self::DxfMerge,
            "merge" => Self::Merge,
            "pngmerge" => Self::PngMerge { depr: false },
            "pngmergedepr" => Self::PngMerge { depr: true },
            "pngmergevege" => Self::PngMergeVege { undergrowth: false },
            "pngmergevegeundergrowth" => Self::PngMergeVege { undergrowth: true },
            "polylinedxfcrop" => Self::PolylineDxfCrop,
            "pointdxfcrop" => Self::PointDxfCrop,
            "unzipmtk" => Self::UnzipMtk,
            "mtkshaperender" => Self::MtkShapeRender,
            "render" => Self::Render,
            _ if word.to_lowercase().ends_with(".zip") => Self::Zip(word),
            _ if is_las(&word) || {
                let lower = word.to_lowercase();
                lower.ends_with(".xyz") || lower.ends_with(".xyz.bin")
            } =>
            {
                Self::Tile(word)
            }
            w if REMOVED_STAGE_COMMANDS.contains(&w) => {
                return Err(format!(
                    "the `{w}` command was removed: the stages no longer run alone on a \
                     tile's temp files. Change pullauta.ini and run the tile again \
                     (`pullauta <tile.laz>`); with debug_intermediates=1, `pullauta` alone \
                     re-renders the map from its temp folder"
                ));
            }
            "internal2xyz" => {
                return Err(
                    "the `internal2xyz` command was removed: the .xyz.bin and .hmap \
                            files are debug intermediates with no format guarantee"
                        .to_string(),
                );
            }
            w if PERL_ONLY_COMMANDS.contains(&w) => {
                return Err(format!(
                    "`{w}` is a command of the Perl karttapullautin, which this version does \
                     not implement: see README.md"
                ));
            }
            w => {
                return Err(format!(
                    "unknown command `{w}`: see README.md for the commands, or give a \
                     .las, .laz, .xyz, .xyz.bin or .zip file"
                ));
            }
        })
    }
}

/// A `.las` or `.laz` file, ignoring case: the inputs whose CRS is read.
fn is_las(path: &str) -> bool {
    let lower = path.to_lowercase();
    lower.ends_with(".las") || lower.ends_with(".laz")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_re_render_reads_the_ground_model_dump_or_asks_for_debug_intermediates() {
        let fs = MemoryFileSystem::new();
        let path = Path::new("temp/xyz2.hmap");
        let err = read_ground(&fs, path).unwrap_err();
        assert!(err.contains("temp/xyz2.hmap is missing"), "{err}");
        assert!(err.contains("debug_intermediates=1"), "{err}");

        let ground = HeightMap {
            xoffset: 300.0,
            yoffset: 600.0,
            scale: 2.0,
            grid: pullauta::vec2d::Vec2D::new(3, 2, 100.5),
        };
        fs.create_dir_all("temp").unwrap();
        ground.to_file(&fs, path).unwrap();
        assert_eq!(read_ground(&fs, path).unwrap(), ground);
    }

    #[test]
    fn a_re_render_asks_for_debug_intermediates_without_the_terrain_dumps() {
        let fs = MemoryFileSystem::new();
        let temp = Path::new("temp");
        let errors = [
            read_contours(&fs, temp).unwrap_err(),
            read_dot_knolls(&fs, temp).unwrap_err(),
        ];
        for (err, dump) in errors.iter().zip(["out2.dxf.bin", "dotknolls.dxf.bin"]) {
            assert!(err.contains(&format!("temp/{dump} is missing")), "{err}");
            assert!(err.contains("debug_intermediates=1"), "{err}");
        }
    }

    fn parse(args: &[&str]) -> Invocation {
        Invocation::parse(args.iter().map(|a| a.to_string()).collect()).unwrap()
    }

    fn strings(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| a.to_string()).collect()
    }

    #[test]
    fn no_arguments_is_the_default_command() {
        assert_eq!(
            parse(&[]),
            Invocation {
                thread: String::new(),
                command: Command::Default,
                args: Vec::new(),
            }
        );
        assert_eq!(parse(&[""]).command, Command::Default);
    }

    #[test]
    fn a_leading_number_is_the_thread() {
        let inv = parse(&["3", "render", "0", "2"]);
        assert_eq!(inv.thread, "3");
        assert_eq!(inv.command, Command::Render);
        assert_eq!(inv.args, strings(&["0", "2"]));

        // the thread may stand alone, and is kept untrimmed for `temp{thread}`
        assert_eq!(
            parse(&[" 7"]),
            Invocation {
                thread: " 7".to_string(),
                command: Command::Default,
                args: Vec::new(),
            }
        );
    }

    #[test]
    fn only_the_first_argument_can_be_the_thread() {
        let inv = parse(&["pngmerge", "2"]);
        assert_eq!(inv.thread, "");
        assert_eq!(inv.command, Command::PngMerge { depr: false });
        assert_eq!(inv.args, strings(&["2"]));
    }

    #[test]
    fn command_names_match_exactly() {
        let cases = [
            ("bin2dxf", Command::Bin2Dxf),
            ("dxfmerge", Command::DxfMerge),
            ("merge", Command::Merge),
            ("pngmerge", Command::PngMerge { depr: false }),
            ("pngmergedepr", Command::PngMerge { depr: true }),
            ("pngmergevege", Command::PngMergeVege { undergrowth: false }),
            (
                "pngmergevegeundergrowth",
                Command::PngMergeVege { undergrowth: true },
            ),
            ("polylinedxfcrop", Command::PolylineDxfCrop),
            ("pointdxfcrop", Command::PointDxfCrop),
            ("unzipmtk", Command::UnzipMtk),
            ("mtkshaperender", Command::MtkShapeRender),
            ("render", Command::Render),
        ];
        for (word, command) in cases {
            assert_eq!(Command::parse(word.to_string()), Ok(command), "{word}");
        }
        for word in ["Render", "pngmergefoo"] {
            let err = Command::parse(word.to_string()).unwrap_err();
            assert!(err.starts_with("unknown command"), "{err}");
        }
    }

    #[test]
    fn removed_commands_say_what_to_do_instead() {
        for word in REMOVED_STAGE_COMMANDS {
            let err = Command::parse(word.to_string()).unwrap_err();
            assert!(err.contains("was removed"), "{err}");
            assert!(err.contains("run the tile again"), "{err}");
        }
        let err = Command::parse("internal2xyz".to_string()).unwrap_err();
        assert!(err.contains("was removed"), "{err}");
        // the error ends the invocation, whatever follows
        let err = Invocation::parse(strings(&["2", "makevege"])).unwrap_err();
        assert!(err.contains("`makevege`"), "{err}");
    }

    #[test]
    fn perl_only_commands() {
        for word in PERL_ONLY_COMMANDS {
            let err = Command::parse(word.to_string()).unwrap_err();
            assert!(err.contains("Perl"), "{err}");
        }
    }

    #[test]
    fn file_extensions_ignore_case() {
        for word in ["a.las", "a.LAZ", "dir/a.xyz", "a.XYZ.BIN"] {
            assert_eq!(
                Command::parse(word.to_string()),
                Ok(Command::Tile(word.to_string())),
                "{word}"
            );
        }
        assert_eq!(
            Command::parse("A.ZIP".to_string()),
            Ok(Command::Zip("A.ZIP".to_string()))
        );
        assert!(Command::parse("a.tif".to_string()).is_err());
    }

    #[test]
    fn only_las_and_laz_carry_a_crs() {
        assert!(is_las("a.LAS"));
        assert!(is_las("a.laz"));
        assert!(!is_las("a.xyz"));
        assert!(!is_las("a.xyz.bin"));
    }

    #[test]
    fn zips_keep_the_remaining_arguments() {
        let inv = parse(&["1", "a.zip", "b.zip"]);
        assert_eq!(inv.thread, "1");
        assert_eq!(inv.command, Command::Zip("a.zip".to_string()));
        assert_eq!(inv.args, strings(&["b.zip"]));
    }
}
