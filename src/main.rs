use log::debug;
use log::error;
use log::info;
use pullauta::config::Config;
use pullauta::io::fs::FileSystem;
use pullauta::io::fs::memory::MemoryFileSystem;
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

    let mut thread: String = String::new();

    let mut config = match Config::load_or_create_default() {
        Ok(config) => config,
        Err(e) => {
            error!("Could not load the config file: {e}");
            std::process::exit(1);
        }
    };

    let fs = pullauta::io::fs::local::LocalFileSystem;

    let mut args: Vec<String> = env::args().collect();

    args.remove(0); // program name

    if !args.is_empty() && args[0].trim().parse::<usize>().is_ok() {
        thread = args.remove(0);
    }

    let command = if !args.is_empty() {
        args.remove(0)
    } else {
        String::new()
    };

    let command_lowercase = command.to_lowercase();

    if command.is_empty()
        || command_lowercase.ends_with(".las")
        || command_lowercase.ends_with(".laz")
        || command_lowercase.ends_with(".xyz")
        || command_lowercase.ends_with(".xyz.bin")
    {
        const VERSION: &str = env!("CARGO_PKG_VERSION");
        println!("Karttapullautin v{VERSION}\nThere is no warranty. Use it at your own risk!\n");
    }

    let batch: bool = config.batch;

    // the input tiles' CRS, unless the `epsg` ini key overrides it; the png merge
    // commands merge the batch's tiles
    let inputs = if (command.is_empty() && batch) || command.starts_with("pngmerge") {
        pullauta::process::batch_tiles(&fs, &config.lazfolder).unwrap_or_default()
    } else if command_lowercase.ends_with(".las") || command_lowercase.ends_with(".laz") {
        vec![PathBuf::from(&command)]
    } else {
        Vec::new()
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

    if command.is_empty() && fs.exists(tmpfolder.join("vegetation.png")) && !batch {
        info!("Rendering png map with depressions");
        pullauta::render::render(
            &fs,
            &config,
            &thread,
            &tmpfolder,
            pnorthlinesangle,
            pnorthlineswidth,
            false,
        )
        .unwrap();
        info!("Rendering png map without depressions");
        pullauta::render::render(
            &fs,
            &config,
            &thread,
            &tmpfolder,
            pnorthlinesangle,
            pnorthlineswidth,
            true,
        )
        .unwrap();
        info!("\nAll done!");
        return;
    }

    if command.is_empty() && !batch {
        println!(
            "USAGE:\npullauta [parameter 1] [parameter 2] [parameter 3] ... [parameter n]\nSee README.MD for more details"
        );
        return;
    }

    if command == "cliffgeneralize" {
        info!("Not implemented in this version, use the perl version");
        return;
    }

    if command == "ground" {
        info!("Not implemented in this version, use the perl version");
        return;
    }

    if command == "ground2" {
        info!("Not implemented in this version, use the perl version");
        return;
    }

    if command == "groundfix" {
        info!("Not implemented in this version, use the perl version");
        return;
    }

    if command == "profile" {
        info!("Not implemented in this version, use the perl version");
        return;
    }

    if command == "makecliffsold" {
        info!("Not implemented in this version, use the perl version");
        return;
    }

    if command == "makeheight" {
        info!("Not implemented in this version, use the perl version");
        return;
    }

    if command == "xyzfixer" {
        info!("Not implemented in this version, use the perl version");
        return;
    }

    if command == "vege" {
        info!("Not implemented in this version, use the perl version");
        return;
    }

    if command == "internal2xyz" {
        if args.len() < 2 {
            info!("USAGE: internal2xyz [input file] [output file]");
            return;
        }

        pullauta::io::internal2xyz(&fs, &args[0], &args[1]).unwrap();
        return;
    }

    if command == "bin2dxf" {
        if args.len() < 2 {
            info!("USAGE: bin2dxf [.dxf.bin input file] [.dxf output file]");
            return;
        }
        pullauta::io::bin2dxf(&fs, &args[0], &args[1]).unwrap();
        return;
    }

    if command == "blocks" {
        pullauta::blocks::blocks(&fs, &config, &tmpfolder).unwrap();
        return;
    }

    if command == "dotknolls" {
        pullauta::knolls::dotknolls(&fs, &config.knoll, config.output_dxf, &tmpfolder).unwrap();
        return;
    }

    if command == "dxfmerge" {
        pullauta::merge::bindxfmerge(&fs, &config).unwrap();
        return;
    }

    if command == "merge" {
        let mut scale = 1.0;
        if !args.is_empty() {
            scale = args[0].parse::<f64>().unwrap();
        }
        pullauta::merge::bindxfmerge(&fs, &config).unwrap();
        pullauta::merge::pngmergevege(&fs, &config, scale, false).unwrap();
        return;
    }

    if command == "knolldetector" {
        pullauta::knolls::knolldetector(&fs, &config.knoll, config.output_dxf, &tmpfolder).unwrap();
        return;
    }

    if command == "makecliffs" {
        // no tile name here: the `cliffthin` seed is the empty name
        pullauta::cliffs::makecliffs(&fs, &config.cliff, config.output_dxf, &tmpfolder, "")
            .unwrap();
        return;
    }

    if command == "makevege" {
        let classes = pullauta::vegetation::makevege(&fs, &config.vegetation, &tmpfolder).unwrap();
        if config.vector_vege {
            pullauta::vege_vector::export_all(&fs, &config, &tmpfolder, &classes).unwrap();
        }
    }

    if command == "pngmerge" || command == "pngmergedepr" {
        let mut scale = 4.0;
        if !args.is_empty() {
            scale = args[0].parse::<f64>().unwrap();
        }
        pullauta::merge::pngmerge(&fs, &config, scale, command == "pngmergedepr").unwrap();
        return;
    }

    if command == "pngmergevege" {
        let mut scale = 1.0;
        if !args.is_empty() {
            scale = args[0].parse::<f64>().unwrap();
        }
        pullauta::merge::pngmergevege(&fs, &config, scale, false).unwrap();
        return;
    }

    if command == "pngmergevegeundergrowth" {
        let mut scale = 1.0;
        if !args.is_empty() {
            scale = args[0].parse::<f64>().unwrap();
        }
        pullauta::merge::pngmergevege(&fs, &config, scale, true).unwrap();
        return;
    }

    if command == "polylinedxfcrop" {
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
            config.output_dxf,
            minx,
            miny,
            maxx,
            maxy,
        )
        .unwrap();
        return;
    }

    if command == "pointdxfcrop" {
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
            config.output_dxf,
            minx,
            miny,
            maxx,
            maxy,
        )
        .unwrap();
        return;
    }

    if command == "smoothjoin" {
        pullauta::merge::smoothjoin(&fs, &config.smoothjoin, config.output_dxf, &tmpfolder)
            .unwrap();
    }

    if command == "xyzknolls" {
        pullauta::knolls::xyzknolls(&fs, &config.knoll, &tmpfolder).unwrap();
    }

    #[cfg(feature = "shapefile")]
    if command == "unzipmtk" {
        pullauta::shapefile::unzip_and_render(&fs, &config, &tmpfolder, &args).unwrap();
    }

    #[cfg(feature = "shapefile")]
    if command == "mtkshaperender" {
        pullauta::shapefile::render(&fs, &config, &tmpfolder, false).unwrap();
    }

    if command == "xyz2contours" {
        let cinterval: f64 = args[0].parse::<f64>().unwrap();
        let xyzfilein = args[1].clone();
        let xyzfileout = args[2].clone();
        let dxffile = args[3].clone();
        let hmap = pullauta::contours::xyz2heightmap(&fs, &config, &tmpfolder, &xyzfilein).unwrap();

        if xyzfileout != "null" && !xyzfileout.is_empty() {
            hmap.to_file(&fs, xyzfileout).unwrap();
        }

        pullauta::contours::heightmap2contours(
            &fs,
            &tmpfolder,
            cinterval,
            &hmap,
            &dxffile,
            config.output_dxf,
        )
        .unwrap();
        return;
    }

    if command == "render" {
        let angle: f64 = args
            .first()
            .and_then(|s| s.parse::<f64>().ok())
            .expect("expected first argument to be angle");
        let nwidth: usize = args
            .get(1)
            .and_then(|s| s.parse::<usize>().ok())
            .expect("expected second argument to be nwidth");
        let nodepressions: bool = args.len() > 2 && args[2] == "nodepressions";
        pullauta::render::render(
            &fs,
            &config,
            &thread,
            &tmpfolder,
            angle,
            nwidth,
            nodepressions,
        )
        .unwrap();
        return;
    }
    if command.is_empty() && batch {
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

            // copy the output files back to disk
            std::fs::create_dir_all(&config.batchoutfolder).unwrap();
            for path in fs.list(&config.batchoutfolder).unwrap() {
                info!("Copying {} from memory fs to disk", path.display());
                fs.save_to_disk(&path, &path).unwrap();
            }
        } else {
            pullauta::process::launch_threads(fs.clone(), config.clone(), &zip_files).unwrap();
        }

        if config.batchmerge {
            info!("Batch done, merging tiles");
            let out = Path::new(&config.batchoutfolder);
            pullauta::merge::pngmerge(&fs, &config, 4.0, false).unwrap();
            pullauta::merge::pngmerge(&fs, &config, 4.0, true).unwrap();
            pullauta::merge::pngmergevege(&fs, &config, 1.0, false).unwrap();
            // a merged.dxf.bin left by an earlier run would stand in for this run's
            // contours and cliffs; bindxfmerge writes a new one only when the tiles
            // kept their .dxf.bin files (savetempfiles=1)
            let merged_bin = Path::new(pullauta::merge::MERGED_DXF_BIN);
            if fs.exists(merged_bin) {
                fs.remove_file(merged_bin).unwrap();
            }
            pullauta::merge::bindxfmerge(&fs, &config).unwrap();
            pullauta::geojson::merge_geojson(&fs, out).unwrap();
            pullauta::geojson::export_combined(&fs, out, merged_bin, config.epsg).unwrap();
        }
        return;
    }

    if command_lowercase.ends_with(".zip") {
        let mut zips: Vec<String> = vec![command];
        zips.extend(args);
        pullauta::process::process_zip(&fs, &config, &thread, &tmpfolder, &zips, false).unwrap();
        return;
    }

    if command_lowercase.ends_with(".las")
        || command_lowercase.ends_with(".laz")
        || command_lowercase.ends_with(".xyz")
        || command_lowercase.ends_with(".xyz.bin")
    {
        let mut norender: bool = false;
        if args.len() > 1 {
            norender = args[1].clone() == "norender";
        }
        // the tile name seeds the thinning, also when the in-memory fs renames the input
        let tile = Path::new(&command).file_stem().unwrap().to_string_lossy();

        if config.experimental_use_in_memory_fs {
            let fs = pullauta::io::fs::memory::MemoryFileSystem::new();

            debug!("Copying input file into memory fs: {command}");
            // copy the input file into the memory file system
            fs.load_from_disk(Path::new(&command), Path::new("input.laz"))
                .expect("Could not copy input file into memory fs");

            debug!("Done");

            pullauta::process::process_tile(
                &fs,
                &config,
                &thread,
                &tmpfolder,
                Path::new("input.laz"),
                &tile,
                norender,
            )
            .unwrap();

            // now write the output files to disk
            fn copy(fs: &MemoryFileSystem, name: &str) {
                if fs.exists(name) {
                    info!("Copying {name} from memory fs to disk");
                    fs.save_to_disk(name, name)
                        .expect("Could not copy from memory fs to disk");
                }
            }
            copy(&fs, "pullautus.png");
            copy(&fs, "pullautus_depr.png");
        } else {
            pullauta::process::process_tile(
                &fs,
                &config,
                &thread,
                &tmpfolder,
                Path::new(&command),
                &tile,
                norender,
            )
            .unwrap();
        }
    }
}
