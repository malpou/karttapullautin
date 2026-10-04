use std::{cell::RefCell, collections::BTreeSet, path::Path, str::FromStr};

use ini::Ini;
use log::warn;

use crate::cliffs::CliffParams;
use crate::geojson::geojson_types::VegetationPropertiesIsomCode;
use crate::knolls::KnollParams;
use crate::merge::{FormLineMode, SmoothJoinParams};
use crate::render::CurveRenderParams;
use crate::vegetation::{Stratum, VegetationParams};

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

    /// Padding in metres: the strip of the neighbouring tiles processed with each tile
    /// in batch mode, so features meet at the tile edges.
    pub batchbuffer: f64,
    /// Run every merge step after a batch run: the png merges, the dxf merge, the
    /// GeoJSON merge and the combined export.
    pub batchmerge: bool,

    pub scalefactor: f64,
    pub vege_bitmode: bool,
    pub zoff: f64,
    pub thinfactor: f64,

    pub skipknolldetection: bool,
    /// The knoll stage's parameters; `scalefactor` and the trace interval are copied in.
    pub knoll: KnollParams,
    /// smoothjoin's parameters, with `scalefactor`, `contour_interval` and `form_lines`.
    pub smoothjoin: SmoothJoinParams,
    /// draw_curves' parameters, with `scalefactor` and `form_lines`.
    pub curves: CurveRenderParams,

    pub xfactor: f64,
    pub yfactor: f64,
    pub zfactor: f64,

    /// Interval in metres of the extra raw contours in `basemap.dxf.bin` (ini
    /// `basemapinterval`, 0 for None).
    pub basemapcontours: Option<f64>,

    pub detectbuildings: bool,

    /// LAS class of water returns (`waterclass`, default 9, ASPRS water): contours, the
    /// ground model and the blocks treat returns of this class as water.
    pub water_class: u8,

    /// makecliffs' parameters.
    pub cliff: CliffParams,

    /// makevege's parameters, with `scalefactor`, `vege_bitmode` and `water_class`.
    pub vegetation: VegetationParams,

    // vector export
    /// Vectorize the vegetation, yellow and undergrowth grids into GeoJSON and DXF areas,
    /// and write contours, form lines, knolls and cliffs as GeoJSON too.
    pub vector_vege: bool,
    /// Symbol code per greenshade index (1-based); a shorter list repeats its last code.
    /// Empty when `vector_vege` is off.
    pub vector_greenshade_isom: Vec<VegetationPropertiesIsomCode>,
    /// Douglas-Peucker tolerance in metres for vegetation areas; 0 disables simplification.
    pub vector_simplify: f64,
    /// Give each green vegetation area its greenshade index as a `shade` property.
    pub vector_shade: bool,
    /// EPSG code of the input data's projected CRS, declared in every GeoJSON output and
    /// map raster CRS sidecar. Loaded from the `epsg` key; `main` then replaces it, before
    /// any output is written, with the resolved code (the key, else the code the input
    /// tiles declare, see [`crate::crs::resolve_epsg`]), so the writers read one field.
    /// None leaves the declaration out.
    pub epsg: Option<u32>,

    // render
    pub buildingcolor: (u8, u8, u8),
    pub vectorconf: String,
    pub mtkskiplayers: Vec<String>,
    pub cliffdebug: bool,
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
        if let Some(name) = conf.sections().flatten().next() {
            return Err(format!(
                "{}: section [{name}] is not read; the config file has no sections, put \
                 every key above the first [section] line",
                path.display()
            )
            .into());
        }

        let gs = &Keys::new(conf.general_section());

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

        fn parse_typed<T: FromStr>(props: &Keys, name: &str, default: T) -> T {
            props
                .get(name)
                .and_then(|s| s.parse::<T>().ok())
                .unwrap_or(default)
        }

        let laz_parallel: bool = gs.get("parallel_laz_decompression").unwrap_or("0") == "1";
        let output_dxf: bool = gs.get("output_dxf").unwrap_or("0") == "1";

        let pnorthlinesangle: f64 = parse_typed(gs, "northlinesangle", 0.0);
        let pnorthlineswidth: usize = parse_typed(gs, "northlineswidth", 0);

        let processes: u64 = match gs.get("processes") {
            None => {
                return Err(
                    "Key `processes` is missing: the number of tiles processed at once".into(),
                );
            }
            Some(v) => v.parse().map_err(|_| {
                format!("Value {v} of `processes` must be a whole number, 0 or more")
            })?,
        };
        let experimental_use_in_memory_fs: bool =
            gs.get("experimental_use_in_memory_fs").unwrap_or("0") == "1";

        let lazfolder = gs.get("lazfolder").unwrap_or("").to_string();
        let batchoutfolder = gs.get("batchoutfolder").unwrap_or("").to_string();
        let savetempfiles = flag(gs, "savetempfiles", None)?;
        let savetempfolders = flag(gs, "savetempfolders", None)?;
        let batchbuffer: f64 = match gs.get("batchbuffer") {
            None => 127.0,
            Some(v) => match v.trim().parse::<f64>() {
                Ok(b) if b.is_finite() && b > 0.0 => b,
                _ => {
                    return Err(format!(
                        "Value {v} of `batchbuffer` must be a number of metres above 0"
                    )
                    .into());
                }
            },
        };
        let batchmerge = flag(gs, "batchmerge", Some(false))?;

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

        let contour_interval = match gs.get("contour_interval") {
            None => 5.0,
            Some(v) => parse_positive_metres("contour_interval", v)?,
        };
        if let Some(warning) = contour_interval_warning(contour_interval) {
            warn!("{warning}");
        }

        let basemapcontours = Some(parse_typed(gs, "basemapinterval", 0.0)).filter(|&i| i != 0.0);

        let detectbuildings: bool = gs.get("detectbuildings").unwrap_or("0") == "1";

        let water_class: u8 = match gs.get("waterclass") {
            None => 9,
            Some(v) => v
                .parse()
                .map_err(|_| format!("Value {v} of `waterclass` must be a LAS class, 0 to 255"))?,
        };
        let water_blue = flag(gs, "water_blue", Some(false))?;

        let inidotknolls: f64 = parse_typed(gs, "knolls", 0.8);
        let smoothing: f64 = parse_typed(gs, "smoothing", 1.0);
        let curviness: f64 = parse_typed(gs, "curviness", 1.0);
        let form_lines = match gs.get("form_lines").map(str::trim) {
            None | Some("selective") => FormLineMode::Selective,
            Some("none") => FormLineMode::None,
            Some(v) => {
                return Err(format!("Value {v} of `form_lines` must be none or selective").into());
            }
        };

        let depression_length: usize = parse_typed(gs, "depression_length", 181);

        // cliffs
        let cliff4_limit = match gs.get("cliff4_limit") {
            None => 7.15,
            Some(v) => parse_positive_metres("cliff4_limit", v)?,
        };
        let cliff_thin: f64 = parse_typed(gs, "cliffthin", 1.0);
        if !(0.0..=1.0).contains(&cliff_thin) {
            return Err(format!(
                "Value {cliff_thin} of `cliffthin` is outside the allowed range of 0.0 to 1.0"
            )
            .into());
        }
        let cliff = CliffParams {
            c1_limit: parse_typed(gs, "cliff1", 1.0),
            c2_limit: parse_typed(gs, "cliff2", 1.0),
            cliff4_limit,
            cliff_thin,
            steep_factor: parse_typed(gs, "cliffsteepfactor", 0.33),
            flat_place: parse_typed(gs, "cliffflatplace", 6.6),
            no_small_cliffs: Some(parse_typed(gs, "cliffnosmallciffs", 0.0)).filter(|&s| s != 0.0),
            bin_m: 3.0,
            bin_max_points: 31,
            neighbourhood_max_points: 301,
            drop_slope: 0.85,
            dash_half_length_m: 1.47,
        };

        // vegetation

        let mut strata = vec![];
        let mut i: u32 = 1;
        loop {
            let stratum = gs.get(format!("stratum{i}")).unwrap_or("");
            if stratum.is_empty() {
                break;
            }

            let [low, high, roof, factor] = parse_numbers(&format!("stratum{i}"), stratum)?;
            strata.push(Stratum {
                low,
                high,
                roof,
                factor,
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
                let [v0, v1, v2] = parse_numbers(&format!("thresold{i}"), last_threshold)?;
                thresholds.push((v0, v1, v2));
                i += 1;
            }
            thresholds
        };

        let greenshades = gs
            .get("greenshades")
            .unwrap_or("")
            .split('|')
            .map(|v| {
                v.trim().parse::<f64>().map_err(|_| {
                    format!("`greenshades` entry `{v}` must be a number (pipe-separated list)")
                })
            })
            .collect::<Result<Vec<f64>, String>>()?;
        let vegetation = VegetationParams {
            scalefactor,
            vege_bitmode,
            water_class,
            water_blue,
            strata,
            thresholds,
            greenshades,
            greentone: parse_typed(gs, "lightgreentone", 200.0),
            greenground: parse_typed(gs, "greenground", 0.9),
            greenhigh: parse_typed(gs, "greenhigh", 2.0),
            topweight: parse_typed(gs, "topweight", 0.8),
            pointvolumefactor: parse_typed(gs, "pointvolumefactor", 0.1),
            pointvolumeexponent: parse_typed(gs, "pointvolumeexponent", 1.0),
            vegezoffset: parse_typed(gs, "vegezoffset", 0.0),
            addition: parse_typed(gs, "greendotsize", 0),
            firstandlastreturnasground: parse_typed(gs, "firstandlastreturnasground", 1),
            firstandlastfactor: parse_typed(gs, "firstandlastreturnfactor", 0.0),
            lastfactor: parse_typed(gs, "lastreturnfactor", 0.0),
            vegethin: parse_typed(gs, "vegethin", 0),
            greendetectsize: parse_typed(gs, "greendetectsize", 3.0),
            med: parse_typed(gs, "medianboxsize", 0),
            med2: parse_typed(gs, "medianboxsize2", 0),
            yellowheight: parse_typed(gs, "yellowheight", 0.9),
            yellowthreshold: parse_typed(gs, "yellowthresold", 0.9),
            yellowfirstlast: parse_typed(gs, "yellowfirstlast", 1),
            proceed_yellows: gs.get("yellow_smoothing").unwrap_or("0") == "1",
            medyellow: parse_typed(gs, "yellowmedianboxsize", 0),
            uglimit: parse_typed(gs, "undergrowth", 0.35),
            uglimit2: parse_typed(gs, "undergrowth2", 0.56),
            buildings: parse_typed(gs, "buildingsclass", 0),
            waterele: parse_typed(gs, "waterelevation", -999999.0),
        };

        // vector export
        let vector_vege = flag(gs, "vector_vege", Some(false))?;
        let greenshade_isom = gs.get("vector_greenshade_isom");
        let vector_greenshade_isom = if vector_vege {
            parse_greenshade_isom(
                greenshade_isom.unwrap_or("406.000|406.000|408.000|408.000|410.000"),
            )?
        } else {
            Vec::new()
        };
        let vector_shade = match gs.get("vector_shade").unwrap_or("0") {
            "0" => false,
            "1" if vector_vege => true,
            "1" => return Err("`vector_shade=1` requires `vector_vege=1`".into()),
            v => return Err(format!("Value {v} of `vector_shade` must be 0 or 1").into()),
        };
        let epsg: Option<u32> = match gs.get("epsg").map(str::trim).unwrap_or("") {
            "" => None,
            v => match v.parse::<u32>() {
                Ok(code) if code > 0 => Some(code),
                _ => {
                    return Err(format!("Value {v} of `epsg` must be a positive EPSG code").into());
                }
            },
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

        let batch = flag(gs, "batch", None)?;
        if batch && processes == 0 {
            return Err(
                "Value of `processes` cannot be zero if parameter `batch` is 1"
                    .to_string()
                    .into(),
            );
        }
        if batchmerge && !batch {
            return Err("Parameter `batchmerge` is 1 but `batch` is not".into());
        }
        gs.reject_unknown(path)?;
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
            batchbuffer,
            batchmerge,
            savetempfolders,
            savetempfiles,
            scalefactor,
            vege_bitmode,
            zoff,
            thinfactor,
            skipknolldetection,
            knoll: KnollParams {
                scalefactor,
                trace_interval: form_lines.trace_interval(contour_interval),
                ..KnollParams::default()
            },
            smoothjoin: SmoothJoinParams {
                scalefactor,
                contour_interval,
                form_lines,
                smoothing,
                curviness,
                depression_length,
                decorate_depressions,
                inidotknolls,
            },
            curves: CurveRenderParams {
                scalefactor,
                form_lines,
                formlinesteepness,
                formlineaddition,
                dashlength,
                gaplength,
                minimumgap,
                label_depressions,
                remove_touching_contours,
                depressions_color,
            },
            xfactor,
            yfactor,
            zfactor,
            basemapcontours,
            detectbuildings,
            water_class,
            cliff,
            vegetation,
            vector_vege,
            vector_greenshade_isom,
            vector_simplify,
            vector_shade,
            epsg,
            buildingcolor,
            vectorconf,
            mtkskiplayers,
            cliffdebug,
        })
    }
}

/// The general section of the config file, recording every key the parser asks for: a
/// key in the file that was never asked for is unknown ([`Keys::reject_unknown`]).
struct Keys<'a> {
    props: &'a ini::Properties,
    asked: RefCell<BTreeSet<String>>,
}

impl<'a> Keys<'a> {
    fn new(props: &'a ini::Properties) -> Self {
        Self {
            props,
            asked: RefCell::default(),
        }
    }

    fn get(&self, key: impl AsRef<str>) -> Option<&'a str> {
        let key = key.as_ref();
        self.asked.borrow_mut().insert(key.to_string());
        self.props.get(key)
    }

    /// An error naming every key in the file the parser never asked for, each with the
    /// nearest known key when one is a typo away.
    fn reject_unknown(&self, path: &Path) -> Result<(), String> {
        let asked = self.asked.borrow();
        let unknown: BTreeSet<&str> = self
            .props
            .iter()
            .map(|(key, _)| key)
            .filter(|key| !asked.contains(*key))
            .collect();
        if unknown.is_empty() {
            return Ok(());
        }
        let described: Vec<String> = unknown
            .iter()
            .map(|key| {
                if let Some(why) = removed_key(key) {
                    return format!("`{key}` (removed: {why})");
                }
                let nearest = asked
                    .iter()
                    .map(|known| (edit_distance(key, known), known))
                    .min();
                match nearest {
                    Some((d, known)) if d <= 2 => format!("`{key}` (did you mean `{known}`?)"),
                    _ => format!("`{key}`"),
                }
            })
            .collect();
        Err(format!(
            "{}: unknown key{} {}",
            path.display(),
            if unknown.len() == 1 { "" } else { "s" },
            described.join(", ")
        ))
    }
}

/// Keys earlier versions read, with what to do instead: reported as removed rather than
/// unknown. A key ending in `{i}` stands for that prefix and a number.
const REMOVED_KEYS: [(&str, &str); 7] = [
    ("groundboxsize", "it was never read; delete it"),
    ("vegemode", "only vegemode=0 was supported; delete it"),
    ("draw_slopelines", "renamed to decorate_depressions"),
    (
        "parallell_laz_decompression",
        "renamed to parallel_laz_decompression",
    ),
    (
        "formline",
        "formline=2 is form_lines=selective; formline=0 at contour_interval=I is \
         form_lines=none at contour_interval=I/2; formline=1 is gone",
    ),
    (
        "indexcontours",
        "index contours are every fifth contour; delete it",
    ),
    ("zone{i}", "renamed to `stratum{i}`, same value"),
];

/// What to do instead of `key`, when it is one of the [`REMOVED_KEYS`]; a `{i}` in the
/// advice is replaced by the key's number.
fn removed_key(key: &str) -> Option<String> {
    REMOVED_KEYS
        .iter()
        .find_map(|&(removed, why)| match removed.strip_suffix("{i}") {
            Some(prefix) => key
                .strip_prefix(prefix)
                .filter(|i| !i.is_empty() && i.bytes().all(|b| b.is_ascii_digit()))
                .map(|i| why.replace("{i}", i)),
            None => (removed == key).then(|| why.to_string()),
        })
}

/// Parse a pipe-separated `key` value of exactly `N` numbers, such as `stratum1=1.0|2.65|99|1`.
fn parse_numbers<const N: usize>(key: &str, value: &str) -> Result<[f64; N], String> {
    let numbers: Vec<f64> = value
        .split('|')
        .map(|v| v.trim().parse::<f64>())
        .collect::<Result<_, _>>()
        .map_err(|_| format!("Value {value} of `{key}` must be {N} pipe-separated numbers"))?;
    numbers
        .try_into()
        .map_err(|_| format!("Value {value} of `{key}` must be {N} pipe-separated numbers"))
}

/// Levenshtein distance between two keys.
fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut diagonal = row[0];
        row[0] = i + 1;
        for (j, &cb) in b.iter().enumerate() {
            let above = row[j + 1];
            row[j + 1] = (diagonal + usize::from(ca != cb))
                .min(row[j] + 1)
                .min(above + 1);
            diagonal = above;
        }
    }
    row[b.len()]
}

/// A `0|1` flag: `default` when the key is missing; an error when it is missing without
/// a default, or has any other value.
fn flag(gs: &Keys, name: &str, default: Option<bool>) -> Result<bool, String> {
    match (gs.get(name), default) {
        (Some("0"), _) => Ok(false),
        (Some("1"), _) => Ok(true),
        (None, Some(default)) => Ok(default),
        (None, None) => Err(format!("Key `{name}` is missing: set it to 0 or 1")),
        (Some(v), _) => Err(format!("Value {v} of `{name}` must be 0 or 1")),
    }
}

/// The contour intervals ISOM 2017-2 allows, in metres.
const ISOM_CONTOUR_INTERVALS: [f64; 2] = [2.5, 5.0];

/// Parse the `key` value `v`: a finite number of metres above 0.
fn parse_positive_metres(key: &str, v: &str) -> Result<f64, String> {
    match v.trim().parse::<f64>() {
        Ok(metres) if metres.is_finite() && metres > 0.0 => Ok(metres),
        _ => Err(format!(
            "Value {v} of `{key}` must be a number of metres above 0"
        )),
    }
}

/// The warning for a contour interval ISOM 2017-2 does not allow; any positive interval
/// is still used (sprint maps use 2-2.5 m).
fn contour_interval_warning(interval: f64) -> Option<String> {
    (!ISOM_CONTOUR_INTERVALS.contains(&interval)).then(|| {
        format!("contour_interval={interval} is not an ISOM 2017-2 contour interval (2.5 or 5 m)")
    })
}

/// Parse `vector_greenshade_isom`: pipe-separated vegetation symbol codes, at least one,
/// each one the schema allows for vegetation areas.
fn parse_greenshade_isom(
    value: &str,
) -> Result<Vec<VegetationPropertiesIsomCode>, Box<dyn std::error::Error>> {
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
        load_text(&lines.join("\n"))
    }

    /// Write `text` to a temp file and load it.
    fn load_text(text: &str) -> Result<Config, String> {
        // tests run in parallel: a unique file per call
        static CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let call = CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("pullauta-config-{}-{call}.ini", std::process::id()));
        std::fs::write(&path, text).unwrap();
        let config = Config::from_file(&path).map_err(|e| e.to_string());
        std::fs::remove_file(&path).unwrap();
        config
    }

    /// The default template with `lines` appended.
    fn load_appended(lines: &str) -> Result<Config, String> {
        let template = std::fs::read_to_string("pullauta.default.ini").unwrap();
        load_text(&format!("{template}\n{lines}\n"))
    }

    /// The default template without the `key=` line.
    fn load_without(key: &str) -> Result<Config, String> {
        let template = std::fs::read_to_string("pullauta.default.ini").unwrap();
        let prefix = format!("{key}=");
        let lines: Vec<&str> = template.lines().collect();
        assert!(lines.iter().any(|l| l.starts_with(&prefix)), "{key}");
        let kept: Vec<&str> = lines
            .into_iter()
            .filter(|l| !l.starts_with(&prefix))
            .collect();
        load_text(&kept.join("\n"))
    }

    #[test]
    fn unknown_keys_are_rejected_with_the_nearest_known_key() {
        let err = load_appended("vectorvege=1\nfrobnicate=2").err().unwrap();
        assert!(err.contains("unknown keys"), "{err}");
        assert!(
            err.contains("`vectorvege` (did you mean `vector_vege`?)"),
            "{err}"
        );
        assert!(err.contains("`frobnicate`"), "{err}");
        assert!(!err.contains("`frobnicate` (did you mean"), "{err}");
    }

    #[test]
    fn deleted_keys_are_unknown() {
        for key in [
            "groundboxsize",
            "vegemode",
            "draw_slopelines",
            "parallell_laz_decompression",
            "formline",
            "indexcontours",
            "zone1",
        ] {
            let err = load_appended(&format!("{key}=1")).err().unwrap();
            assert!(err.contains(&format!("`{key}` (removed: ")), "{err}");
        }
    }

    #[test]
    fn sections_are_rejected() {
        let err = load_appended("[extra]\nprocesses=4").err().unwrap();
        assert!(err.contains("[extra]"), "{err}");
    }

    #[test]
    fn required_keys_error_when_missing_or_malformed() {
        for key in ["processes", "savetempfiles", "savetempfolders", "batch"] {
            let err = load_without(key).err().unwrap();
            assert!(err.contains(&format!("`{key}` is missing")), "{err}");
            let err = load_with(&[(key, "yes")]).err().unwrap();
            assert!(err.contains(&format!("`{key}`")), "{err}");
        }
        assert_eq!(load_with(&[("processes", "7")]).unwrap().processes, 7);
    }

    #[test]
    fn waterclass_is_one_class_and_water_blue_draws_it() {
        let cases = [
            (&[][..], 9, false),
            (&[("waterclass", "9")][..], 9, false),
            (&[("water_blue", "1")][..], 9, true),
            (&[("waterclass", "7")][..], 7, false),
            (&[("waterclass", "7"), ("water_blue", "1")][..], 7, true),
        ];
        for (settings, class, blue) in cases {
            let config = load_with(settings).unwrap();
            assert_eq!(
                (config.water_class, config.vegetation.water_blue),
                (class, blue),
                "{settings:?}"
            );
        }
        for bad in ["yes", "2", ""] {
            let err = load_with(&[("water_blue", bad)]).err().unwrap();
            assert!(err.contains("water_blue"), "{bad}: {err}");
        }
        for bad in ["water", "256", "-1", ""] {
            let err = load_with(&[("waterclass", bad)]).err().unwrap();
            assert!(err.contains("waterclass"), "{bad}: {err}");
        }
    }

    #[test]
    fn malformed_vegetation_lists_error_naming_the_key() {
        for (key, bad) in [
            ("stratum1", "1|2"),
            ("stratum2", "1|2|x|1"),
            ("stratum3", "1|2|3|4|5"),
            ("thresold1", "0.2|3"),
            ("thresold2", "a|b|c"),
            ("greenshades", "0.2|x|0.5"),
        ] {
            let err = load_with(&[(key, bad)]).err().unwrap();
            assert!(err.contains(&format!("`{key}`")), "{key}={bad}: {err}");
        }
    }

    /// The template's strata, in order; the stratum loop stops at the first missing key.
    #[test]
    fn strata_parse_in_order() {
        use crate::vegetation::Stratum;
        let stratum = |low, high, roof, factor| Stratum {
            low,
            high,
            roof,
            factor,
        };
        let config = load_with(&[("stratum2", "2|3|11|0.75")]).unwrap();
        assert_eq!(
            config.vegetation.strata,
            [
                stratum(1.0, 2.65, 99.0, 1.0),
                stratum(2.0, 3.0, 11.0, 0.75),
                stratum(3.4, 5.5, 8.0, 0.2),
            ]
        );
        let err = load_without("stratum2").err().unwrap();
        assert!(err.contains("unknown key `stratum3`"), "{err}");
    }

    /// `zone{i}` is `stratum{i}` now; every number is reported, not other `zone` keys.
    #[test]
    fn zone_keys_are_removed_in_favour_of_strata() {
        let err = load_appended("zone1=1|2|99|1\nzone12=1|2|99|1")
            .err()
            .unwrap();
        for i in [1, 12] {
            assert!(
                err.contains(&format!(
                    "`zone{i}` (removed: renamed to `stratum{i}`, same value)"
                )),
                "{err}"
            );
        }
        assert_eq!(super::removed_key("zones"), None);
        assert_eq!(super::removed_key("zone"), None);
        assert_eq!(super::removed_key("zone1x"), None);
    }

    #[test]
    fn contour_interval_is_a_positive_number() {
        for (value, interval) in [("5", 5.0), ("2.5", 2.5), ("2", 2.0), ("10", 10.0)] {
            let config = load_with(&[("contour_interval", value)]).unwrap();
            assert_eq!(config.smoothjoin.contour_interval, interval);
        }
        for bad in ["0", "-5", "NaN", "inf", "five", ""] {
            let err = load_with(&[("contour_interval", bad)]).err().unwrap();
            assert!(err.contains("contour_interval"), "{bad}: {err}");
        }
    }

    #[test]
    fn contour_interval_warns_outside_isom() {
        use super::contour_interval_warning;
        assert_eq!(contour_interval_warning(5.0), None);
        assert_eq!(contour_interval_warning(2.5), None);
        for interval in [2.0, 1.25, 10.0] {
            let warning = contour_interval_warning(interval).unwrap();
            assert!(warning.contains("contour_interval"), "{warning}");
        }
    }

    #[test]
    fn knoll_params_take_scalefactor_and_trace_interval() {
        use crate::knolls::KnollParams;
        let config = load_with(&[("scalefactor", "1.5"), ("contour_interval", "2.5")]).unwrap();
        let expected = KnollParams {
            scalefactor: 1.5,
            trace_interval: 1.25,
            ..KnollParams::default()
        };
        assert_eq!(config.knoll, expected);
    }

    /// The template's cliff values reach `CliffParams`; the rest are the constants
    /// makecliffs had inline.
    #[test]
    fn cliff_params_take_the_template_values() {
        use crate::cliffs::CliffParams;
        let expected = CliffParams {
            c1_limit: 1.15,
            c2_limit: 2.0,
            cliff4_limit: 7.15,
            cliff_thin: 1.0,
            steep_factor: 0.38,
            flat_place: 3.5,
            no_small_cliffs: Some(5.5),
            bin_m: 3.0,
            bin_max_points: 31,
            neighbourhood_max_points: 301,
            drop_slope: 0.85,
            dash_half_length_m: 1.47,
        };
        assert_eq!(load_with(&[]).unwrap().cliff, expected);
    }

    #[test]
    fn cliff4_limit_parses_and_defaults_to_the_old_constant() {
        assert_eq!(
            load_with(&[("cliff4_limit", "5.5")])
                .unwrap()
                .cliff
                .cliff4_limit,
            5.5
        );
        let absent = load_without("cliff4_limit").unwrap().cliff.cliff4_limit;
        // the hidden constant it replaces, bit for bit
        assert_eq!(absent.to_bits(), (2.6_f64 * 2.75).to_bits());
        for bad in ["0", "-1", "inf", "high"] {
            let err = load_with(&[("cliff4_limit", bad)]).err().unwrap();
            assert!(err.contains("`cliff4_limit`"), "{bad}: {err}");
        }
    }

    #[test]
    fn cliffnosmallciffs_zero_is_none() {
        let steep = |v| {
            load_with(&[("cliffnosmallciffs", v)])
                .unwrap()
                .cliff
                .no_small_cliffs
        };
        assert_eq!(steep("0"), None);
        assert_eq!(steep("5.5"), Some(5.5));
        assert_eq!(
            load_without("cliffnosmallciffs")
                .unwrap()
                .cliff
                .no_small_cliffs,
            None
        );
    }

    /// `contour_interval` is the map's contour interval in both form line modes; lines
    /// are traced at it, or at half of it between contours with form lines.
    #[test]
    fn form_lines_set_the_trace_interval() {
        use crate::merge::FormLineMode::{None, Selective};
        // (form_lines, contour_interval) -> (mode, trace interval, half-interval lines,
        // index contour interval)
        for (value, interval, mode, trace, half, index) in [
            ("none", "2.5", None, 2.5, false, 12.5),
            ("none", "5", None, 5.0, false, 25.0),
            ("selective", "2.5", Selective, 1.25, true, 12.5),
            ("selective", "5", Selective, 2.5, true, 25.0),
        ] {
            let config =
                load_with(&[("form_lines", value), ("contour_interval", interval)]).unwrap();
            assert_eq!(config.smoothjoin.form_lines, mode);
            assert_eq!(config.curves.form_lines, mode);
            let levels = config.smoothjoin.levels();
            assert_eq!(levels.trace_interval, trace, "{value} {interval}");
            assert_eq!(levels.half_interval_lines, half);
            assert_eq!(levels.index_interval, index);
            assert_eq!(config.knoll.trace_interval, trace);
        }
        assert_eq!(
            load_without("form_lines").unwrap().smoothjoin.form_lines,
            Selective
        );
        let err = load_with(&[("form_lines", "2")]).err().unwrap();
        assert!(err.contains("form_lines"), "{err}");
    }

    /// The release note's equivalence: old `formline=0, contour_interval=5` is
    /// `form_lines=none, contour_interval=2.5`. Old mode 0 traced at 5 / 2 = 2.5 m, drew
    /// index contours every `indexcontours=12.5` m, had no half-interval lines, and the
    /// knoll stage stepped by 2.5 m with a threshold ratio of 5 / 5 = 1.
    #[test]
    fn form_lines_none_at_half_the_interval_is_old_formline_0() {
        use crate::geometry::ContourLevels;
        let config = load_with(&[("form_lines", "none"), ("contour_interval", "2.5")]).unwrap();
        let old_mode_0 = ContourLevels {
            trace_interval: 2.5,
            index_interval: 12.5,
            half_interval_lines: false,
        };
        assert_eq!(config.smoothjoin.levels(), old_mode_0);
        assert_eq!(config.knoll.trace_interval, 2.5);
        // knolldetector's threshold ratio, trace_interval / 2.5 * scalefactor
        assert_eq!(
            config.knoll.trace_interval / 2.5 * config.knoll.scalefactor,
            1.0
        );
    }

    #[test]
    fn basemapinterval_0_draws_no_basemap_contours() {
        assert_eq!(load_with(&[]).unwrap().basemapcontours, None);
        let config = load_with(&[("basemapinterval", "1.125")]).unwrap();
        assert_eq!(config.basemapcontours, Some(1.125));
    }

    #[test]
    fn edit_distance_counts_single_character_edits() {
        use super::edit_distance;
        assert_eq!(edit_distance("vectorvege", "vector_vege"), 1);
        assert_eq!(edit_distance("zone1", "zone1"), 0);
        assert_eq!(edit_distance("kitten", "sitting"), 3);
        assert_eq!(edit_distance("", "abc"), 3);
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
        use crate::geojson::geojson_types::VegetationPropertiesIsomCode as S;
        let config = load_with(&[("vector_vege", "1")]).unwrap();
        assert_eq!(
            config.vector_greenshade_isom,
            [S::X406000, S::X406000, S::X408000, S::X408000, S::X410000]
        );
        let config =
            load_with(&[("vector_vege", "1"), ("vector_greenshade_isom", "403.000")]).unwrap();
        assert_eq!(config.vector_greenshade_isom, [S::X403000]);
    }

    #[test]
    fn vector_greenshade_isom_rejects_empty_and_non_vegetation_codes() {
        for bad in ["", "406.000||410.000", "406.000|409.000", "406", "101.000"] {
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

    #[test]
    fn vector_shade_is_0_or_1_and_needs_vector_vege() {
        assert!(!load_with(&[]).unwrap().vector_shade);
        let config = load_with(&[("vector_vege", "1"), ("vector_shade", "1")]).unwrap();
        assert!(config.vector_shade);
        let err = load_with(&[("vector_shade", "1")]).err().unwrap();
        assert!(err.contains("vector_vege"), "{err}");
        for bad in ["yes", "", "2"] {
            let err = load_with(&[("vector_vege", "1"), ("vector_shade", bad)])
                .err()
                .unwrap_or_else(|| panic!("`{bad}` must fail the config load"));
            assert!(err.contains("vector_shade"), "{err}");
        }
    }

    #[test]
    fn batch_keys_default_to_the_old_behaviour() {
        let config = load_with(&[]).unwrap();
        assert_eq!(config.batchbuffer, 127.0);
        assert!(!config.batchmerge);
        assert_eq!(config.epsg, None);
    }

    #[test]
    fn batchbuffer_must_be_above_zero() {
        let config = load_with(&[("batchbuffer", "50.5")]).unwrap();
        assert_eq!(config.batchbuffer, 50.5);
        for bad in ["0", "-1", "wide", "", "inf"] {
            let err = load_with(&[("batchbuffer", bad)]).err().unwrap();
            assert!(err.contains("batchbuffer"), "{bad}: {err}");
        }
    }

    #[test]
    fn batchmerge_needs_batch() {
        let config = load_with(&[("batch", "1"), ("batchmerge", "1")]).unwrap();
        assert!(config.batchmerge);
        let err = load_with(&[("batchmerge", "1")]).err().unwrap();
        assert!(err.contains("batchmerge"), "{err}");
        let err = load_with(&[("batch", "1"), ("batchmerge", "yes")])
            .err()
            .unwrap();
        assert!(err.contains("batchmerge"), "{err}");
    }

    #[test]
    fn epsg_is_a_positive_code_or_empty() {
        assert_eq!(load_with(&[("epsg", "25832")]).unwrap().epsg, Some(25832));
        assert_eq!(load_with(&[("epsg", " ")]).unwrap().epsg, None);
        for bad in ["0", "-3067", "EPSG:3067", "3067.5"] {
            let err = load_with(&[("epsg", bad)]).err().unwrap();
            assert!(err.contains("epsg"), "{bad}: {err}");
        }
    }
}
