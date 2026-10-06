use log::debug;
use log::error;
use log::info;
use pullauta::config::Config;
use pullauta::io::fs::FileSystem;
use pullauta::io::fs::memory::MemoryFileSystem;
use pullauta::io::heightmap::HeightMap;
use pullauta::knolls::{DotKnollSet, KNOLL_GROUND_DUMP};
use pullauta::merge::ContourSet;
use pullauta::render::{GROUND_DUMP, MapInputs};
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

    let mut config = match Config::load_or_create_default() {
        Ok(config) => config,
        Err(e) => {
            error!("Could not load the config file: {e}");
            std::process::exit(1);
        }
    };

    let fs = pullauta::io::fs::local::LocalFileSystem;

    let Invocation {
        thread,
        command,
        args,
    } = Invocation::parse(env::args().skip(1).collect());

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

    let pnorthlinesangle = config.pnorthlinesangle;
    let pnorthlineswidth = config.pnorthlineswidth;

    match command {
        // re-render a tile run with debug_intermediates=1; a normal run keeps only
        // products in temp/, so `pullauta` alone prints the usage there
        Command::Default if !batch && pullauta::render::check_inputs(&fs, &tmpfolder).is_ok() => {
            let loaded = or_exit(read_render_inputs(&fs, &config, &tmpfolder));
            let inputs = &loaded.map_inputs();
            info!("Rendering png map with depressions");
            or_exit(pullauta::render::render(
                &fs,
                &config,
                &thread,
                &tmpfolder,
                inputs,
                pnorthlinesangle,
                pnorthlineswidth,
                false,
            ));
            info!("Rendering png map without depressions");
            or_exit(pullauta::render::render(
                &fs,
                &config,
                &thread,
                &tmpfolder,
                inputs,
                pnorthlinesangle,
                pnorthlineswidth,
                true,
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

        Command::PerlOnly => {
            info!("Not implemented in this version, use the perl version");
        }

        Command::Internal2Xyz => {
            if args.len() < 2 {
                info!("USAGE: internal2xyz [input file] [output file]");
                return;
            }

            pullauta::io::internal2xyz(&fs, &args[0], &args[1]).unwrap();
        }

        Command::Bin2Dxf => {
            if args.len() < 2 {
                info!("USAGE: bin2dxf [.dxf.bin input file] [.dxf output file]");
                return;
            }
            pullauta::io::bin2dxf(&fs, &args[0], &args[1]).unwrap();
        }

        Command::Blocks => {
            let ground = or_exit(read_ground(&fs, &tmpfolder.join(GROUND_DUMP)));
            let returns = or_exit(read_dump(&fs, &tmpfolder.join("xyztemp.xyz.bin"), true));
            pullauta::blocks::blocks(&fs, config.water_class, &tmpfolder, &ground, &returns)
                .unwrap();
        }

        Command::DotKnolls => {
            let lifted = or_exit(read_lifted_ground(&fs, &tmpfolder));
            let contours = or_exit(read_contours(&fs, &tmpfolder));
            let candidates = or_exit(read_dot_knoll_candidates(&fs, &tmpfolder));
            let dot_knolls =
                pullauta::knolls::dotknolls(&contours, &candidates, &lifted, &config.knoll);
            pullauta::knolls::write_dot_knolls(
                &fs,
                &tmpfolder,
                &dot_knolls,
                true,
                config.outputs.dxf,
            )
            .unwrap();
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

        Command::KnollDetector => {
            let ground = or_exit(read_ground(&fs, &tmpfolder.join(KNOLL_GROUND_DUMP)));
            let candidates = or_exit(read_candidates(&fs, &tmpfolder));
            let (detected, pins) =
                pullauta::knolls::knolldetector(&ground, &candidates, &config.knoll);
            pullauta::knolls::write_detected(&fs, &tmpfolder, &detected, &pins, config.outputs.dxf)
                .unwrap();
        }

        Command::MakeCliffs => {
            let ground = or_exit(read_ground(&fs, &tmpfolder.join(GROUND_DUMP)));
            let returns = or_exit(read_dump(&fs, &tmpfolder.join("xyztemp.xyz.bin"), true));
            // no tile name here: the `cliffthin` seed is the empty name
            pullauta::cliffs::makecliffs(
                &fs,
                &config.cliff,
                config.outputs.dxf,
                &tmpfolder,
                "",
                &ground,
                &returns,
            )
            .unwrap();
        }

        Command::MakeVege => {
            let ground = or_exit(read_ground(&fs, &tmpfolder.join(GROUND_DUMP)));
            let returns = or_exit(read_dump(&fs, &tmpfolder.join("xyztemp.xyz.bin"), true));
            let classes = pullauta::vegetation::makevege(
                &fs,
                &config.vegetation,
                &tmpfolder,
                &ground,
                &returns,
            )
            .unwrap();
            if config.outputs.vectorizes_vegetation() {
                pullauta::vege_vector::export_all(&fs, &config, &tmpfolder, &classes).unwrap();
            }
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

        Command::SmoothJoin => {
            let lifted = or_exit(read_lifted_ground(&fs, &tmpfolder));
            let traced = or_exit(read_contour_lines(
                &fs,
                &tmpfolder,
                pullauta::merge::TRACED_DUMP,
                "traced contours",
            ));
            let (contours, candidates) =
                pullauta::merge::smoothjoin(&traced, &lifted, &config.smoothjoin);
            pullauta::merge::write_contours(
                &fs,
                &tmpfolder,
                &contours,
                &candidates,
                true,
                config.outputs.dxf,
            )
            .unwrap();
        }

        Command::XyzKnolls => {
            let ground = or_exit(read_ground(&fs, &tmpfolder.join(KNOLL_GROUND_DUMP)));
            // as in a tile run: with skipknolldetection there are no pins
            let pins = if config.skipknolldetection {
                Vec::new()
            } else {
                or_exit(read_pins(&fs, &tmpfolder))
            };
            let lifted = pullauta::knolls::xyzknolls(&ground, &pins, &config.knoll);
            lifted
                .to_file(&fs, tmpfolder.join(pullauta::knolls::LIFTED_GROUND_DUMP))
                .unwrap();
        }

        #[cfg(feature = "shapefile")]
        Command::UnzipMtk => {
            pullauta::shapefile::unzip_and_render(&fs, &config, &tmpfolder, &args).unwrap();
        }

        #[cfg(feature = "shapefile")]
        Command::MtkShapeRender => {
            pullauta::shapefile::render(&fs, &config, &tmpfolder, false).unwrap();
        }

        // without the shapefile feature these do nothing, like an unknown command
        #[cfg(not(feature = "shapefile"))]
        Command::UnzipMtk | Command::MtkShapeRender => {}

        Command::Xyz2Contours => {
            let cinterval: f64 = args[0].parse::<f64>().unwrap();
            let xyzfilein = args[1].clone();
            let xyzfileout = args[2].clone();
            let dxffile = args[3].clone();
            let returns = or_exit(read_dump(&fs, &tmpfolder.join(&xyzfilein), false));
            let hmap =
                pullauta::contours::xyz2heightmap(&returns, &config.ground, config.water_class);

            if xyzfileout != "null" && !xyzfileout.is_empty() {
                hmap.to_file(&fs, xyzfileout).unwrap();
            }

            pullauta::contours::heightmap2contours(
                &fs,
                &tmpfolder,
                cinterval,
                &hmap,
                &dxffile,
                config.outputs.dxf,
            )
            .unwrap();
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
            let loaded = or_exit(read_render_inputs(&fs, &config, &tmpfolder));
            or_exit(pullauta::render::render(
                &fs,
                &config,
                &thread,
                &tmpfolder,
                &loaded.map_inputs(),
                angle,
                nwidth,
                nodepressions,
            ));
        }

        Command::Zip(first) => {
            let mut zips: Vec<String> = vec![first];
            zips.extend(args);
            let loaded = or_exit(read_render_inputs(&fs, &config, &tmpfolder));
            or_exit(pullauta::process::process_zip(
                &fs,
                &config,
                &thread,
                &tmpfolder,
                &loaded.map_inputs(),
                &zips,
                false,
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

        Command::Unknown => {}
    }
}

/// The returns in the `.xyz.bin` file `path`, for the stage commands. `debug_dump` is
/// set for the implicit `temp/xyztemp.xyz.bin`, which a tile run writes only with
/// debug_intermediates=1: a missing one asks for the flag.
fn read_dump(
    fs: &impl FileSystem,
    path: &Path,
    debug_dump: bool,
) -> Result<Vec<pullauta::io::xyz::XyzRecord>, String> {
    if !fs.exists(path) {
        return Err(if debug_dump {
            format!(
                "cannot read the returns: {} is missing. The stage commands read the tile's \
                 returns from its debug intermediates: re-run the tile with debug_intermediates=1",
                path.display()
            )
        } else {
            format!("{} is missing", path.display())
        });
    }
    fs.open(path)
        .and_then(pullauta::io::xyz::read_all)
        .map_err(|e| format!("cannot read the returns from {}: {e}", path.display()))
}

/// The ground model's debug intermediate at `path` (`xyz_03.hmap` or its copy
/// `xyz2.hmap`), for the stage commands. A tile run writes both only with debug_intermediates=1: a missing one asks
/// for the flag.
fn read_ground(fs: &impl FileSystem, path: &Path) -> Result<HeightMap, String> {
    read_debug_dump(fs, path, "ground model", || HeightMap::from_file(fs, path))
}

/// The lifted ground model dump (`xyz_knolls.hmap`), for smoothjoin and dotknolls.
fn read_lifted_ground(fs: &impl FileSystem, tmpfolder: &Path) -> Result<HeightMap, String> {
    let path = tmpfolder.join(pullauta::knolls::LIFTED_GROUND_DUMP);
    read_debug_dump(fs, &path, "lifted ground model", || {
        HeightMap::from_file(fs, &path)
    })
}

/// The knoll candidate contours dump (`contours03.dxf.bin`), for knolldetector.
fn read_candidates(
    fs: &impl FileSystem,
    tmpfolder: &Path,
) -> Result<Vec<pullauta::geometry::Contour>, String> {
    read_contour_lines(
        fs,
        tmpfolder,
        pullauta::knolls::CANDIDATES_DUMP,
        "knoll candidate contours",
    )
}

/// The contours in the contour file dump `name` (`contours03.dxf.bin`, `out.dxf.bin`),
/// which holds the tile's `what`.
fn read_contour_lines(
    fs: &impl FileSystem,
    tmpfolder: &Path,
    name: &str,
    what: &str,
) -> Result<Vec<pullauta::geometry::Contour>, String> {
    let path = tmpfolder.join(name);
    read_debug_dump(fs, &path, what, || {
        let dxf = pullauta::geometry::BinaryDxf::from_reader(&mut fs.open(&path)?)?;
        match dxf.take_geometry().swap_remove(0) {
            pullauta::geometry::Geometry::Polylines3(lines) => {
                Ok(pullauta::contours::contours_from_lines(&lines))
            }
            _ => Err(anyhow::anyhow!("it holds no 3D contour lines")),
        }
    })
}

/// smoothjoin's contours dump (`out2.dxf.bin`), for dotknolls and a re-render.
fn read_contours(fs: &impl FileSystem, tmpfolder: &Path) -> Result<ContourSet, String> {
    let path = tmpfolder.join(pullauta::merge::CONTOURS_DUMP);
    read_debug_dump(fs, &path, "contours", || {
        ContourSet::from_bindxf(pullauta::geometry::BinaryDxf::from_reader(
            &mut fs.open(&path)?,
        )?)
    })
}

/// The dot knoll candidates dump (`dotknolls.bin`), for dotknolls.
fn read_dot_knoll_candidates(
    fs: &impl FileSystem,
    tmpfolder: &Path,
) -> Result<Vec<pullauta::knolls::DotKnollCandidate>, String> {
    let path = tmpfolder.join(pullauta::merge::DOT_KNOLL_CANDIDATES_DUMP);
    read_debug_dump(fs, &path, "dot knoll candidates", || {
        pullauta::util::read_object(fs.open(&path)?)
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

/// The knoll pins dump (`pins.bin`), for xyzknolls.
fn read_pins(fs: &impl FileSystem, tmpfolder: &Path) -> Result<Vec<pullauta::knolls::Pin>, String> {
    let path = tmpfolder.join(pullauta::knolls::PINS_DUMP);
    read_debug_dump(fs, &path, "knoll pins", || {
        pullauta::util::read_object(fs.open(&path)?)
    })
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
            "cannot read the {what}: {} is missing. The stage commands read the tile's \
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
}

impl RenderInputs {
    fn map_inputs(&self) -> MapInputs<'_> {
        MapInputs {
            ground: &self.ground,
            contours: &self.contours,
            dot_knolls: &self.dot_knolls,
        }
    }
}

/// The inputs of a re-render (`render`, a shape-file zip, `pullauta` in a debug run's
/// folder), once the raster family and every file it reads are there.
fn read_render_inputs(
    fs: &impl FileSystem,
    config: &Config,
    tmpfolder: &Path,
) -> Result<RenderInputs, String> {
    pullauta::render::check_raster(config).map_err(|e| e.to_string())?;
    pullauta::render::check_inputs(fs, tmpfolder).map_err(|e| e.to_string())?;
    Ok(RenderInputs {
        ground: read_ground(fs, &tmpfolder.join(GROUND_DUMP))?,
        contours: read_contours(fs, tmpfolder)?,
        dot_knolls: read_dot_knolls(fs, tmpfolder)?,
    })
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
    fn parse(mut args: Vec<String>) -> Self {
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
            Command::parse(args.remove(0))
        };
        Self {
            thread,
            command,
            args,
        }
    }
}

/// What the command word asks for. `eval` is dispatched before this, as it
/// needs neither the config file nor the temp folder.
#[derive(Debug, PartialEq)]
enum Command {
    /// No command: render the temp folder's map, print the usage, or run the
    /// batch (`batch=1`).
    Default,
    /// A command only the Perl version implements.
    PerlOnly,
    Internal2Xyz,
    Bin2Dxf,
    Blocks,
    DotKnolls,
    DxfMerge,
    Merge,
    KnollDetector,
    MakeCliffs,
    MakeVege,
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
    SmoothJoin,
    XyzKnolls,
    UnzipMtk,
    MtkShapeRender,
    Xyz2Contours,
    Render,
    /// A `.zip` file, the first of the zips to process.
    Zip(String),
    /// A `.las`, `.laz`, `.xyz` or `.xyz.bin` point cloud to process.
    Tile(String),
    /// Anything else: does nothing.
    Unknown,
}

impl Command {
    /// Command names match exactly; file extensions ignore case.
    fn parse(word: String) -> Self {
        match word.as_str() {
            "" => Self::Default,
            "cliffgeneralize" | "ground" | "ground2" | "groundfix" | "profile"
            | "makecliffsold" | "makeheight" | "xyzfixer" | "vege" => Self::PerlOnly,
            "internal2xyz" => Self::Internal2Xyz,
            "bin2dxf" => Self::Bin2Dxf,
            "blocks" => Self::Blocks,
            "dotknolls" => Self::DotKnolls,
            "dxfmerge" => Self::DxfMerge,
            "merge" => Self::Merge,
            "knolldetector" => Self::KnollDetector,
            "makecliffs" => Self::MakeCliffs,
            "makevege" => Self::MakeVege,
            "pngmerge" => Self::PngMerge { depr: false },
            "pngmergedepr" => Self::PngMerge { depr: true },
            "pngmergevege" => Self::PngMergeVege { undergrowth: false },
            "pngmergevegeundergrowth" => Self::PngMergeVege { undergrowth: true },
            "polylinedxfcrop" => Self::PolylineDxfCrop,
            "pointdxfcrop" => Self::PointDxfCrop,
            "smoothjoin" => Self::SmoothJoin,
            "xyzknolls" => Self::XyzKnolls,
            "unzipmtk" => Self::UnzipMtk,
            "mtkshaperender" => Self::MtkShapeRender,
            "xyz2contours" => Self::Xyz2Contours,
            "render" => Self::Render,
            _ if word.to_lowercase().ends_with(".zip") => Self::Zip(word),
            _ if is_las(&word) || {
                let lower = word.to_lowercase();
                lower.ends_with(".xyz") || lower.ends_with(".xyz.bin")
            } =>
            {
                Self::Tile(word)
            }
            _ => Self::Unknown,
        }
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
    fn stage_commands_read_the_dump_or_ask_for_debug_intermediates() {
        let fs = MemoryFileSystem::new();
        let path = Path::new("temp/xyztemp.xyz.bin");
        let err = read_dump(&fs, path, true).unwrap_err();
        assert!(err.contains("temp/xyztemp.xyz.bin is missing"), "{err}");
        assert!(err.contains("debug_intermediates=1"), "{err}");

        let record = pullauta::io::xyz::XyzRecord {
            x: 1.0,
            classification: 2,
            ..Default::default()
        };
        fs.create_dir_all("temp").unwrap();
        pullauta::io::xyz::write_all(fs.create(path).unwrap(), &[record, record]).unwrap();
        assert_eq!(read_dump(&fs, path, true).unwrap(), [record, record]);

        // a file the user named is just missing
        let err = read_dump(&fs, Path::new("temp/named.xyz.bin"), false).unwrap_err();
        assert_eq!(err, "temp/named.xyz.bin is missing");
    }

    #[test]
    fn stage_commands_read_the_ground_model_dump_or_ask_for_debug_intermediates() {
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
    fn terrain_stage_commands_ask_for_debug_intermediates_without_their_dumps() {
        let fs = MemoryFileSystem::new();
        let temp = Path::new("temp");
        let errors = [
            read_candidates(&fs, temp).unwrap_err(),
            read_pins(&fs, temp).unwrap_err(),
            read_lifted_ground(&fs, temp).unwrap_err(),
            read_contour_lines(&fs, temp, pullauta::merge::TRACED_DUMP, "traced contours")
                .unwrap_err(),
            read_contours(&fs, temp).unwrap_err(),
            read_dot_knoll_candidates(&fs, temp).unwrap_err(),
            read_dot_knolls(&fs, temp).unwrap_err(),
        ];
        for (err, dump) in errors.iter().zip([
            "contours03.dxf.bin",
            "pins.bin",
            "xyz_knolls.hmap",
            "out.dxf.bin",
            "out2.dxf.bin",
            "dotknolls.bin",
            "dotknolls.dxf.bin",
        ]) {
            assert!(err.contains(&format!("temp/{dump} is missing")), "{err}");
            assert!(err.contains("debug_intermediates=1"), "{err}");
        }
    }

    fn parse(args: &[&str]) -> Invocation {
        Invocation::parse(args.iter().map(|a| a.to_string()).collect())
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
            ("internal2xyz", Command::Internal2Xyz),
            ("bin2dxf", Command::Bin2Dxf),
            ("blocks", Command::Blocks),
            ("dotknolls", Command::DotKnolls),
            ("dxfmerge", Command::DxfMerge),
            ("merge", Command::Merge),
            ("knolldetector", Command::KnollDetector),
            ("makecliffs", Command::MakeCliffs),
            ("makevege", Command::MakeVege),
            ("pngmerge", Command::PngMerge { depr: false }),
            ("pngmergedepr", Command::PngMerge { depr: true }),
            ("pngmergevege", Command::PngMergeVege { undergrowth: false }),
            (
                "pngmergevegeundergrowth",
                Command::PngMergeVege { undergrowth: true },
            ),
            ("polylinedxfcrop", Command::PolylineDxfCrop),
            ("pointdxfcrop", Command::PointDxfCrop),
            ("smoothjoin", Command::SmoothJoin),
            ("xyzknolls", Command::XyzKnolls),
            ("unzipmtk", Command::UnzipMtk),
            ("mtkshaperender", Command::MtkShapeRender),
            ("xyz2contours", Command::Xyz2Contours),
            ("render", Command::Render),
        ];
        for (word, command) in cases {
            assert_eq!(Command::parse(word.to_string()), command, "{word}");
        }
        assert_eq!(Command::parse("Render".to_string()), Command::Unknown);
        assert_eq!(Command::parse("pngmergefoo".to_string()), Command::Unknown);
    }

    #[test]
    fn perl_only_commands() {
        for word in [
            "cliffgeneralize",
            "ground",
            "ground2",
            "groundfix",
            "profile",
            "makecliffsold",
            "makeheight",
            "xyzfixer",
            "vege",
        ] {
            assert_eq!(
                Command::parse(word.to_string()),
                Command::PerlOnly,
                "{word}"
            );
        }
    }

    #[test]
    fn file_extensions_ignore_case() {
        for word in ["a.las", "a.LAZ", "dir/a.xyz", "a.XYZ.BIN"] {
            assert_eq!(
                Command::parse(word.to_string()),
                Command::Tile(word.to_string()),
                "{word}"
            );
        }
        assert_eq!(
            Command::parse("A.ZIP".to_string()),
            Command::Zip("A.ZIP".to_string())
        );
        assert_eq!(Command::parse("a.tif".to_string()), Command::Unknown);
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
