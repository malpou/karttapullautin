use std::{path::Path, str::FromStr};

use ini::Ini;

use crate::geojson::geojson_types::VegetationPropertiesSymbol;

/// The config parsed from the .ini configuration file.
pub struct Config {
    pub batch: bool,
    pub processes: u64,

    /// If true, use parallel decompression for LAZ files.
    pub laz_parallel: bool,

    pub experimental_use_in_memory_fs: bool,

    /// Whether to output the result as DXF.
    pub output_dxf: bool,

    // only one can be set at a time
    pub vegeonly: bool,
    pub cliffsonly: bool,
    pub contoursonly: bool,

    pub pnorthlinesangle: f64,
    pub pnorthlineswidth: usize,

    pub lazfolder: String,
    pub batchoutfolder: String,
    pub savetempfiles: bool,
    pub savetempfolders: bool,

    pub scalefactor: f64,
    pub vege_bitmode: bool,
    pub zoff: f64,
    pub thinfactor: f64,

    pub skipknolldetection: bool,
    pub vegemode: bool,

    pub xfactor: f64,
    pub yfactor: f64,
    pub zfactor: f64,

    pub contour_interval: f64,
    pub basemapcontours: f64,

    pub detectbuildings: bool,

    pub water_class: u8,

    // merge
    pub inidotknolls: f64,
    pub smoothing: f64,
    pub curviness: f64,
    pub indexcontours: f64,
    pub formline: f64,
    pub depression_length: usize,

    // cliffs
    pub c1_limit: f64,
    pub c2_limit: f64,
    pub cliff_thin: f64,
    pub steep_factor: f64,
    pub flat_place: f64,
    pub no_small_ciffs: f64,

    // vegetation
    pub zones: Vec<Zone>,
    pub thresholds: Vec<(f64, f64, f64)>,
    pub greenshades: Vec<f64>,
    pub yellowheight: f64,
    pub yellowthreshold: f64,
    pub greenground: f64,
    pub pointvolumefactor: f64,
    pub pointvolumeexponent: f64,
    pub greenhigh: f64,
    pub topweight: f64,
    pub greentone: f64,
    pub vegezoffset: f64,
    pub uglimit: f64,
    pub uglimit2: f64,
    pub addition: i32,
    pub firstandlastreturnasground: u32,
    pub firstandlastfactor: f64,
    pub lastfactor: f64,
    pub yellowfirstlast: u32,
    pub vegethin: u32,
    pub greendetectsize: f64,
    pub proceed_yellows: bool,
    pub med: u32,
    pub med2: u32,
    pub medyellow: u32,
    pub water: u8,
    pub buildings: u8,
    pub waterele: f64,

    // vector export
    /// Vectorize the vegetation, yellow and undergrowth grids into GeoJSON and DXF areas.
    pub vector_vege: bool,
    /// Symbol code per greenshade index (1-based); a shorter list repeats its last code.
    /// Empty when `vector_vege` is off.
    pub vector_greenshade_isom: Vec<VegetationPropertiesSymbol>,
    /// Douglas-Peucker tolerance in metres for vegetation areas; 0 disables simplification.
    pub vector_simplify: f64,

    // render
    pub buildingcolor: (u8, u8, u8),
    pub vectorconf: String,
    pub mtkskiplayers: Vec<String>,
    pub cliffdebug: bool,

    pub formlinesteepness: f64,
    // pub formline: f64,
    pub formlineaddition: f64,
    pub dashlength: f64,
    pub gaplength: f64,
    pub minimumgap: u32,
    pub label_depressions: bool,
    pub remove_touching_contours: bool,

    pub depressions_color: (u8, u8, u8),
    pub decorate_depressions: bool,
}

pub struct Zone {
    pub low: f64,
    pub high: f64,
    pub roof: f64,
    pub factor: f64,
}

const DEFAULT_CONFIG_FILE: &str = "pullauta.ini";

impl Config {
    pub fn load_or_create_default() -> Result<Self, Box<dyn std::error::Error>> {
        let path = Path::new(DEFAULT_CONFIG_FILE);
        // populate the default if no file was found
        if !path.exists() {
            std::fs::write(path, include_bytes!("../pullauta.default.ini"))?;
        }
        Self::from_file(path)
    }

    fn from_file(path: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        let conf = Ini::load_from_file(path)?;

        let gs = conf.general_section();

        // only one can be set at a time
        let vegeonly: bool = gs.get("vegeonly").unwrap_or("0") == "1";
        let cliffsonly: bool = gs.get("cliffsonly").unwrap_or("0") == "1";
        let contoursonly: bool = gs.get("contoursonly").unwrap_or("0") == "1";

        // clippy complains about this, but we want it like this for understandability
        #[allow(clippy::nonminimal_bool)]
        if (vegeonly && (cliffsonly || contoursonly))
            || (cliffsonly && (vegeonly || contoursonly))
            || (contoursonly && (vegeonly || cliffsonly))
        {
            return Err(
                "Only one of vegeonly, cliffsonly, or contoursonly can be set!"
                    .to_string()
                    .into(),
            );
        }

        fn parse_typed<T: FromStr>(props: &ini::Properties, name: &str, default: T) -> T {
            props
                .get(name)
                .and_then(|s| s.parse::<T>().ok())
                .unwrap_or(default)
        }

        let laz_parallel: bool = gs.get("parallel_laz_decompression").unwrap_or("0") == "1";
        let output_dxf: bool = gs.get("output_dxf").unwrap_or("0") == "1";

        let pnorthlinesangle: f64 = parse_typed(gs, "northlinesangle", 0.0);
        let pnorthlineswidth: usize = parse_typed(gs, "northlineswidth", 0);

        let processes: u64 = gs.get("processes").unwrap().parse::<u64>().unwrap();
        let experimental_use_in_memory_fs: bool =
            gs.get("experimental_use_in_memory_fs").unwrap_or("0") == "1";

        let lazfolder = gs.get("lazfolder").unwrap_or("").to_string();
        let batchoutfolder = gs.get("batchoutfolder").unwrap_or("").to_string();
        let savetempfiles: bool = gs.get("savetempfiles").unwrap() == "1";
        let savetempfolders: bool = gs.get("savetempfolders").unwrap() == "1";

        let scalefactor: f64 = parse_typed(gs, "scalefactor", 1.0);
        let vege_bitmode: bool = gs.get("vege_bitmode").unwrap_or("0") == "1";
        let zoff = parse_typed(gs, "zoffset", 0.0);
        let mut thinfactor: f64 = parse_typed(gs, "thinfactor", 1.0);
        if !(0.0..=1.0).contains(&thinfactor) {
            return Err(format!(
                "Value {thinfactor} of `thinfactor` is outside the allowed range of 0.0 to 1.0"
            )
            .into());
        }
        if thinfactor == 0.0 {
            thinfactor = 1.0;
        }

        let skipknolldetection = gs.get("skipknolldetection").unwrap_or("0") == "1";
        let vegemode: bool = gs.get("vegemode").unwrap_or("0") == "1";
        if vegemode {
            return Err("vegemode=1 not implemented, use perl version"
                .to_string()
                .into());
        }

        let mut xfactor: f64 = parse_typed(gs, "coordxfactor", 1.0);
        let mut yfactor: f64 = parse_typed(gs, "coordyfactor", 1.0);
        let mut zfactor: f64 = parse_typed(gs, "coordzfactor", 1.0);
        if xfactor == 0.0 {
            xfactor = 1.0;
        }
        if yfactor == 0.0 {
            yfactor = 1.0;
        }
        if zfactor == 0.0 {
            zfactor = 1.0;
        }

        let contour_interval: f64 = parse_typed(gs, "contour_interval", 5.0);

        let basemapcontours: f64 = parse_typed(gs, "basemapinterval", 0.0);

        let detectbuildings: bool = gs.get("detectbuildings").unwrap_or("0") == "1";

        let water_class = parse_typed(gs, "waterclass", 9);

        let inidotknolls: f64 = parse_typed(gs, "knolls", 0.8);
        let smoothing: f64 = parse_typed(gs, "smoothing", 1.0);
        let curviness: f64 = parse_typed(gs, "curviness", 1.0);
        let indexcontours: f64 = parse_typed(gs, "indexcontours", 12.5);
        let formline: f64 = parse_typed(gs, "formline", 2.0);

        let depression_length: usize = parse_typed(gs, "depression_length", 181);

        // cliffs
        let c1_limit: f64 = parse_typed(gs, "cliff1", 1.0);
        let c2_limit: f64 = parse_typed(gs, "cliff2", 1.0);
        let cliff_thin: f64 = parse_typed(gs, "cliffthin", 1.0);
        if !(0.0..=1.0).contains(&cliff_thin) {
            return Err(format!(
                "Value {cliff_thin} of `cliffthin` is outside the allowed range of 0.0 to 1.0"
            )
            .into());
        }
        let steep_factor: f64 = parse_typed(gs, "cliffsteepfactor", 0.33);
        let flat_place: f64 = parse_typed(gs, "cliffflatplace", 6.6);
        let no_small_ciffs: f64 = parse_typed(gs, "cliffnosmallciffs", 0.0);

        // vegetation

        let mut zones = vec![];
        let mut i: u32 = 1;
        loop {
            let zone = gs.get(format!("zone{i}")).unwrap_or("");
            if zone.is_empty() {
                break;
            }

            let mut parts = zone.split('|');

            zones.push(Zone {
                low: parts.next().unwrap().parse::<f64>().unwrap(),
                high: parts.next().unwrap().parse::<f64>().unwrap(),
                roof: parts.next().unwrap().parse::<f64>().unwrap(),
                factor: parts.next().unwrap().parse::<f64>().unwrap(),
            });
            i += 1;
        }
        let thresholds = {
            let mut thresholds = vec![];
            let mut i: u32 = 1;
            loop {
                let last_threshold = gs.get(format!("thresold{i}")).unwrap_or("");
                if last_threshold.is_empty() {
                    break;
                }
                // parse the threshold values
                let mut parts = last_threshold.split('|');
                let v0: f64 = parts.next().unwrap().parse::<f64>().unwrap();
                let v1: f64 = parts.next().unwrap().parse::<f64>().unwrap();
                let v2: f64 = parts.next().unwrap().parse::<f64>().unwrap();

                thresholds.push((v0, v1, v2));
                i += 1;
            }
            thresholds
        };

        let greenshades = gs
            .get("greenshades")
            .unwrap_or("")
            .split('|')
            .map(|v| v.parse::<f64>().unwrap())
            .collect::<Vec<f64>>();
        let yellowheight: f64 = parse_typed(gs, "yellowheight", 0.9);
        let yellowthreshold: f64 = parse_typed(gs, "yellowthresold", 0.9);
        let greenground: f64 = parse_typed(gs, "greenground", 0.9);
        let pointvolumefactor: f64 = parse_typed(gs, "pointvolumefactor", 0.1);
        let pointvolumeexponent: f64 = parse_typed(gs, "pointvolumeexponent", 1.0);
        let greenhigh: f64 = parse_typed(gs, "greenhigh", 2.0);
        let topweight: f64 = parse_typed(gs, "topweight", 0.8);
        let greentone: f64 = parse_typed(gs, "lightgreentone", 200.0);
        let vegezoffset: f64 = parse_typed(gs, "vegezoffset", 0.0);
        let uglimit: f64 = parse_typed(gs, "undergrowth", 0.35);
        let uglimit2: f64 = parse_typed(gs, "undergrowth2", 0.56);
        let addition: i32 = parse_typed(gs, "greendotsize", 0);
        let firstandlastreturnasground = parse_typed(gs, "firstandlastreturnasground", 1);
        let firstandlastfactor = parse_typed(gs, "firstandlastreturnfactor", 0.0);
        let lastfactor = parse_typed(gs, "lastreturnfactor", 0.0);

        let yellowfirstlast = parse_typed(gs, "yellowfirstlast", 1);
        let vegethin: u32 = parse_typed(gs, "vegethin", 0);

        let greendetectsize: f64 = parse_typed(gs, "greendetectsize", 3.0);
        let proceed_yellows: bool = gs.get("yellow_smoothing").unwrap_or("0") == "1";
        let med: u32 = parse_typed(gs, "medianboxsize", 0);
        let med2: u32 = parse_typed(gs, "medianboxsize2", 0);
        let medyellow: u32 = parse_typed(gs, "yellowmedianboxsize", 0);
        let water = parse_typed(gs, "waterclass", 0);
        let buildings = parse_typed(gs, "buildingsclass", 0);
        let waterele = parse_typed(gs, "waterelevation", -999999.0);

        // vector export
        let vector_vege = match gs.get("vector_vege").unwrap_or("0") {
            "0" => false,
            "1" => true,
            v => return Err(format!("Value {v} of `vector_vege` must be 0 or 1").into()),
        };
        let vector_greenshade_isom = if vector_vege {
            parse_greenshade_isom(
                gs.get("vector_greenshade_isom")
                    .unwrap_or("406|406|408|408|410"),
            )?
        } else {
            Vec::new()
        };
        let vector_simplify: f64 = match gs.get("vector_simplify") {
            None => 2.0,
            Some(v) => match v.trim().parse::<f64>() {
                Ok(eps) if eps.is_finite() && eps >= 0.0 => eps,
                _ => {
                    return Err(format!(
                        "Value {v} of `vector_simplify` must be a number of metres, 0 or more"
                    )
                    .into());
                }
            },
        };

        // render
        let buildingcolor: (u8, u8, u8) = {
            let mut split = gs.get("buildingcolor").unwrap_or("0,0,0").split(',');
            (
                split.next().unwrap_or("0").parse::<u8>().unwrap_or(0),
                split.next().unwrap_or("0").parse::<u8>().unwrap_or(0),
                split.next().unwrap_or("0").parse::<u8>().unwrap_or(0),
            )
        };

        let vectorconf: String = gs.get("vectorconf").unwrap_or("").into();
        if !vectorconf.is_empty() && !Path::new(&vectorconf).is_file() {
            return Err(format!("vectorconf file {vectorconf} does not exist").into());
        }
        let mtkskiplayers: Vec<String> = gs
            .get("mtkskiplayers")
            .unwrap_or("")
            .split(',')
            .map(Into::into)
            .collect();

        let cliffdebug: bool = gs.get("cliffdebug").unwrap_or("0") == "1";

        let formlinesteepness: f64 = parse_typed(gs, "formlinesteepness", 0.37);
        let formlineaddition: f64 = parse_typed(gs, "formlineaddition", 13.0);
        let dashlength: f64 = parse_typed(gs, "dashlength", 60.0);
        let gaplength: f64 = parse_typed(gs, "gaplength", 12.0);
        let minimumgap: u32 = parse_typed(gs, "minimumgap", 30);
        let label_depressions: bool = gs.get("label_formlines_depressions").unwrap_or("0") == "1";
        let remove_touching_contours: bool =
            gs.get("remove_touching_contours").unwrap_or("0") == "1";

        let depressions_color: (u8, u8, u8) = {
            let mut split = gs
                .get("depressions_color")
                .unwrap_or("200,0,200")
                .split(',');
            (
                split.next().unwrap_or("0").parse::<u8>().unwrap_or(0),
                split.next().unwrap_or("0").parse::<u8>().unwrap_or(0),
                split.next().unwrap_or("0").parse::<u8>().unwrap_or(0),
            )
        };
        let decorate_depressions = gs.get("decorate_depressions").unwrap_or("0") == "1";

        let batch = gs.get("batch").unwrap() == "1";
        if batch && processes == 0 {
            return Err(
                "Value of `processes` cannot be zero if parameter `batch` is 1"
                    .to_string()
                    .into(),
            );
        }
        Ok(Self {
            batch,
            processes,
            output_dxf,
            laz_parallel,
            experimental_use_in_memory_fs,
            vegeonly,
            cliffsonly,
            contoursonly,
            pnorthlinesangle,
            pnorthlineswidth,
            lazfolder,
            batchoutfolder,
            savetempfolders,
            savetempfiles,
            scalefactor,
            vege_bitmode,
            zoff,
            thinfactor,
            skipknolldetection,
            vegemode,
            xfactor,
            yfactor,
            zfactor,
            contour_interval,
            basemapcontours,
            detectbuildings,
            water_class,
            inidotknolls,
            smoothing,
            curviness,
            indexcontours,
            formline,
            depression_length,
            c1_limit,
            c2_limit,
            cliff_thin,
            steep_factor,
            flat_place,
            no_small_ciffs,
            zones,
            thresholds,
            greenshades,
            yellowheight,
            yellowthreshold,
            greenground,
            pointvolumefactor,
            pointvolumeexponent,
            greenhigh,
            topweight,
            greentone,
            vegezoffset,
            uglimit,
            uglimit2,
            addition,
            firstandlastreturnasground,
            firstandlastfactor,
            lastfactor,
            yellowfirstlast,
            vegethin,
            greendetectsize,
            proceed_yellows,
            med,
            med2,
            medyellow,
            water,
            buildings,
            waterele,
            vector_vege,
            vector_greenshade_isom,
            vector_simplify,
            buildingcolor,
            vectorconf,
            mtkskiplayers,
            cliffdebug,
            formlinesteepness,
            formlineaddition,
            dashlength,
            gaplength,
            minimumgap,
            label_depressions,
            remove_touching_contours,
            depressions_color,
            decorate_depressions,
        })
    }
}

/// Parse `vector_greenshade_isom`: pipe-separated vegetation symbol codes, at least one,
/// each one the schema allows for vegetation areas.
fn parse_greenshade_isom(
    value: &str,
) -> Result<Vec<VegetationPropertiesSymbol>, Box<dyn std::error::Error>> {
    value
        .split('|')
        .map(|code| {
            code.trim().parse().map_err(|_| {
                format!(
                    "`vector_greenshade_isom` entry `{code}` is not a vegetation symbol code \
                     of the GeoJSON schema"
                )
                .into()
            })
        })
        .collect()
}

#[cfg(test)]
mod test {
    use std::path::Path;

    use super::Config;

    #[test]
    fn should_load_config_template_successfully() {
        Config::from_file(Path::new("pullauta.default.ini"))
            .expect("Could not load and parse the default config template");
    }

    /// The default template with the given `key=value` lines replaced (every key must
    /// already be in the template), written to a temp file and loaded.
    fn load_with(settings: &[(&str, &str)]) -> Result<Config, String> {
        let template = std::fs::read_to_string("pullauta.default.ini").unwrap();
        let mut lines: Vec<String> = template.lines().map(String::from).collect();
        for (key, value) in settings {
            let prefix = format!("{key}=");
            let line = lines
                .iter_mut()
                .find(|l| l.starts_with(&prefix))
                .unwrap_or_else(|| panic!("{key} is not in pullauta.default.ini"));
            *line = format!("{key}={value}");
        }
        // tests run in parallel: a unique file per call
        static CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let call = CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("pullauta-config-{}-{call}.ini", std::process::id()));
        std::fs::write(&path, lines.join("\n")).unwrap();
        let config = Config::from_file(&path).map_err(|e| e.to_string());
        std::fs::remove_file(&path).unwrap();
        config
    }

    fn load_with_vectorconf(vectorconf: &str) -> Result<Config, String> {
        load_with(&[("vectorconf", vectorconf)])
    }

    #[test]
    fn vectorconf_must_exist() {
        let Err(err) = load_with_vectorconf("no-such-mapping.txt") else {
            panic!("a missing vectorconf file must fail the config load");
        };
        assert!(err.contains("no-such-mapping.txt"), "{err}");

        let config = load_with_vectorconf("osm.txt").unwrap();
        assert_eq!(config.vectorconf, "osm.txt");
    }

    #[test]
    fn vector_vege_defaults_off_with_default_simplify() {
        let config = load_with(&[]).unwrap();
        assert!(!config.vector_vege);
        assert!(config.vector_greenshade_isom.is_empty());
        assert_eq!(config.vector_simplify, 2.0);
    }

    #[test]
    fn vector_vege_must_be_0_or_1() {
        let err = load_with(&[("vector_vege", "yes")]).err().unwrap();
        assert!(err.contains("vector_vege"), "{err}");
    }

    #[test]
    fn vector_greenshade_isom_parses_vegetation_codes() {
        use crate::geojson::geojson_types::VegetationPropertiesSymbol as S;
        let config = load_with(&[("vector_vege", "1")]).unwrap();
        assert_eq!(
            config.vector_greenshade_isom,
            [S::X406, S::X406, S::X408, S::X408, S::X410]
        );
        let config = load_with(&[("vector_vege", "1"), ("vector_greenshade_isom", "403")]).unwrap();
        assert_eq!(config.vector_greenshade_isom, [S::X403]);
    }

    #[test]
    fn vector_greenshade_isom_rejects_empty_and_non_vegetation_codes() {
        for bad in ["", "406||410", "406|409", "406.000", "101"] {
            let err = load_with(&[("vector_vege", "1"), ("vector_greenshade_isom", bad)])
                .err()
                .unwrap_or_else(|| panic!("`{bad}` must fail the config load"));
            assert!(err.contains("vector_greenshade_isom"), "{err}");
        }
        // not read while vector_vege is off
        load_with(&[("vector_greenshade_isom", "")]).unwrap();
    }

    #[test]
    fn vector_simplify_must_be_a_non_negative_number() {
        let config = load_with(&[("vector_simplify", "0")]).unwrap();
        assert_eq!(config.vector_simplify, 0.0);
        for bad in ["-1", "abc", "", "NaN", "inf"] {
            let err = load_with(&[("vector_simplify", bad)])
                .err()
                .unwrap_or_else(|| panic!("`{bad}` must fail the config load"));
            assert!(err.contains("vector_simplify"), "{err}");
        }
    }
}
