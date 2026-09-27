//! The coordinate reference system (CRS) an input tile declares in its LAS
//! variable-length records (VLRs), reduced to the EPSG code the GeoJSON outputs and the
//! map rasters' sidecars declare.
//!
//! A LAS file declares its CRS either as OGC WKT (record 2112, LAS 1.4) or as GeoTIFF
//! keys (records 34735-34737, older files). Only a projected CRS with an EPSG code is
//! used: the outputs are in the input's projected metres, and without a CRS database
//! (ADR 0002: no proj/gdal) an unnamed CRS cannot be turned into a code, so it is never
//! guessed: it is left undeclared, or refused in a batch whose other tiles name a code.

use log::{info, warn};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::io::fs::FileSystem;

/// The EPSG code the outputs declare: `config_epsg` (the `epsg` ini key) when set,
/// otherwise the one code the LAS/LAZ `tiles` declare. Every tile that declares a CRS
/// must name that code: a different code, or a CRS without a usable EPSG code, is
/// refused, since its outputs would not line up with the others. A tile declaring
/// nothing (or unreadable here; processing reports it) is assumed to share the code.
pub fn resolve_epsg(
    fs: &impl FileSystem,
    config_epsg: Option<u32>,
    tiles: &[PathBuf],
) -> anyhow::Result<Option<u32>> {
    let mut declared: Vec<(&Path, u32)> = Vec::new();
    let mut unusable: Vec<(&Path, String)> = Vec::new();
    let mut undeclared: Vec<&Path> = Vec::new();
    for path in tiles {
        let header = fs
            .open(path)
            .map_err(anyhow::Error::from)
            .and_then(|file| Ok(las::Reader::new(file)?.header().clone()));
        match header.map(|h| header_epsg(&h)) {
            Ok(Ok(Some(code))) => declared.push((path, code)),
            Ok(Ok(None)) => undeclared.push(path),
            Ok(Err(reason)) => unusable.push((path, reason)),
            Err(e) => {
                warn!("{}: CRS not read: {e}", path.display());
                undeclared.push(path);
            }
        }
    }

    if let Some(code) = config_epsg {
        for (path, other) in declared.iter().filter(|(_, c)| *c != code) {
            warn!(
                "{} declares EPSG:{other}; using EPSG:{code} from `epsg`",
                path.display()
            );
        }
        for (path, reason) in &unusable {
            warn!(
                "{}: {reason}; using EPSG:{code} from `epsg`",
                path.display()
            );
        }
        return Ok(Some(code));
    }

    let Some(&(_, code)) = declared.first() else {
        for (path, reason) in &unusable {
            warn!("{}: {reason}; no CRS declared", path.display());
        }
        return Ok(None);
    };
    let mut refused: Vec<String> = declared
        .iter()
        .filter(|(_, c)| *c != code)
        .map(|(path, c)| format!("{} declares EPSG:{c}", path.display()))
        .collect();
    refused.extend(
        unusable
            .iter()
            .map(|(path, reason)| format!("{}: {reason}", path.display())),
    );
    if !refused.is_empty() {
        anyhow::bail!(
            "input tiles do not all declare EPSG:{code} ({}); reproject them to one CRS, or set `epsg` if the declarations are wrong",
            refused.join("; ")
        );
    }
    info!("Input declares EPSG:{code}");
    for path in undeclared {
        warn!(
            "{} declares no CRS; assuming EPSG:{code} like the other tiles",
            path.display()
        );
    }
    Ok(Some(code))
}

/// Write the GDAL PAM sidecar `<raster>.aux.xml` naming `epsg`, the CRS of the
/// coordinates in the raster's world file, which has no CRS of its own. GDAL and QGIS
/// read it. Without a code nothing is written and a sidecar already there is kept: a
/// merge command run after the batch, with its tiles gone, rewrites the same raster of
/// the same ground.
pub fn write_raster_crs(
    fs: &impl FileSystem,
    raster: impl AsRef<Path>,
    epsg: Option<u32>,
) -> std::io::Result<()> {
    let Some(code) = epsg else {
        return Ok(());
    };
    let mut path = raster.as_ref().as_os_str().to_owned();
    path.push(".aux.xml");
    write!(
        fs.create(&path)?,
        "<PAMDataset>\n  <SRS>EPSG:{code}</SRS>\n</PAMDataset>\n"
    )
}

/// The EPSG code of the projected CRS a LAS header declares: from the WKT record when
/// present, otherwise from the GeoTIFF ProjectedCSTypeGeoKey (3072). Ok(None) when the
/// header declares no CRS; Err (the reason) when it declares one without a usable code.
pub fn header_epsg(header: &las::Header) -> Result<Option<u32>, String> {
    if let Some(bytes) = header.get_wkt_crs_bytes() {
        let wkt = String::from_utf8_lossy(bytes);
        let wkt = wkt.trim_start_matches('\u{feff}').trim_end_matches('\0');
        return wkt_epsg(wkt).map(Some);
    }
    let Some(geotiff) = header
        .get_geotiff_crs()
        .map_err(|e| format!("unreadable GeoTIFF CRS keys: {e}"))?
    else {
        return Ok(None);
    };
    match geotiff.get_projected_crs_geo_key_value() {
        // 1024..=32766 are EPSG codes; 0 is omitted, 32767 user-defined, higher private
        Some(code @ 1024..=32766) => Ok(Some(code.into())),
        Some(value) => Err(format!(
            "GeoTIFF ProjectedCSTypeGeoKey {value} is not an EPSG code"
        )),
        None => Err("GeoTIFF CRS keys declare no projected CRS".into()),
    }
}

/// The EPSG code of the first projected CRS in `wkt` (WKT1 `PROJCS`, WKT2 `PROJCRS`),
/// possibly inside a compound or bound CRS: its own `AUTHORITY["EPSG","n"]` (WKT1) or
/// `ID["EPSG",n]` (WKT2), not those of the CRSs nested in it.
fn wkt_epsg(wkt: &str) -> Result<u32, String> {
    let root = Parser {
        s: wkt.as_bytes(),
        pos: 0,
    }
    .root()
    .ok_or("unreadable WKT CRS")?;
    let projected = root
        .find(&["PROJCS", "PROJCRS", "PROJECTEDCRS"])
        .ok_or("the WKT CRS is not projected")?;
    projected
        .epsg()
        .ok_or_else(|| "the WKT projected CRS has no EPSG code".into())
}

/// A WKT node, `KEYWORD[arg, ...]`.
struct Node {
    keyword: String,
    args: Vec<Arg>,
}

enum Arg {
    Node(Node),
    /// A quoted string (unquoted) or a bare number or enum.
    Text(String),
}

impl Node {
    /// The first node, this one or a descendant, depth first, named one of `keywords`.
    fn find(&self, keywords: &[&str]) -> Option<&Node> {
        if keywords
            .iter()
            .any(|k| self.keyword.eq_ignore_ascii_case(k))
        {
            return Some(self);
        }
        self.args.iter().find_map(|arg| match arg {
            Arg::Node(node) => node.find(keywords),
            Arg::Text(_) => None,
        })
    }

    /// The code of this node's own EPSG `AUTHORITY` or `ID` child.
    fn epsg(&self) -> Option<u32> {
        self.args.iter().find_map(|arg| match arg {
            Arg::Node(Node { keyword, args })
                if keyword.eq_ignore_ascii_case("AUTHORITY")
                    || keyword.eq_ignore_ascii_case("ID") =>
            {
                match args.as_slice() {
                    [Arg::Text(name), Arg::Text(code), ..] if name.eq_ignore_ascii_case("EPSG") => {
                        code.trim().parse().ok().filter(|&code: &u32| code > 0)
                    }
                    _ => None,
                }
            }
            _ => None,
        })
    }
}

/// Deepest nesting read; real CRSs stay far below it, and it keeps a hostile file from
/// overflowing the stack.
const MAX_DEPTH: usize = 32;

/// Recursive descent over WKT1 and WKT2 alike: `[` or `(` brackets, `""` escaping a quote
/// inside a quoted string. Returns None on malformed input.
struct Parser<'a> {
    s: &'a [u8],
    pos: usize,
}

impl Parser<'_> {
    fn root(&mut self) -> Option<Node> {
        self.skip_ws();
        let keyword = self.bare();
        self.node(keyword, 0)
    }

    /// The bracketed arguments of the node named `keyword`.
    fn node(&mut self, keyword: String, depth: usize) -> Option<Node> {
        if keyword.is_empty() || depth > MAX_DEPTH {
            return None;
        }
        self.skip_ws();
        let close = match self.next()? {
            b'[' => b']',
            b'(' => b')',
            _ => return None,
        };
        let mut args = Vec::new();
        loop {
            self.skip_ws();
            if self.s.get(self.pos) == Some(&b'"') {
                args.push(Arg::Text(self.quoted()?));
            } else {
                let bare = self.bare();
                self.skip_ws();
                if matches!(self.s.get(self.pos), Some(b'[' | b'(')) {
                    args.push(Arg::Node(self.node(bare, depth + 1)?));
                } else {
                    args.push(Arg::Text(bare));
                }
            }
            self.skip_ws();
            match self.next()? {
                b',' => {}
                c if c == close => return Some(Node { keyword, args }),
                _ => return None,
            }
        }
    }

    fn quoted(&mut self) -> Option<String> {
        self.pos += 1;
        let mut text = Vec::new();
        loop {
            match self.next()? {
                b'"' if self.s.get(self.pos) == Some(&b'"') => {
                    self.pos += 1;
                    text.push(b'"');
                }
                b'"' => return Some(String::from_utf8_lossy(&text).into_owned()),
                c => text.push(c),
            }
        }
    }

    /// A keyword, number or enum: everything up to a delimiter or whitespace.
    fn bare(&mut self) -> String {
        let start = self.pos;
        while let Some(c) = self.s.get(self.pos)
            && !matches!(c, b',' | b'[' | b']' | b'(' | b')' | b'"')
            && !c.is_ascii_whitespace()
        {
            self.pos += 1;
        }
        String::from_utf8_lossy(&self.s[start..self.pos]).into_owned()
    }

    fn skip_ws(&mut self) {
        while self.s.get(self.pos).is_some_and(u8::is_ascii_whitespace) {
            self.pos += 1;
        }
    }

    fn next(&mut self) -> Option<u8> {
        let c = *self.s.get(self.pos)?;
        self.pos += 1;
        Some(c)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::fs::memory::MemoryFileSystem;
    use las::{Builder, Vlr};
    use std::io::Write;

    const WKT1_3067: &str = r#"PROJCS["ETRS89 / TM35FIN(E,N)",GEOGCS["ETRS89",DATUM["European_Terrestrial_Reference_System_1989",SPHEROID["GRS 1980",6378137,298.257222101,AUTHORITY["EPSG","7019"]],AUTHORITY["EPSG","6258"]],PRIMEM["Greenwich",0,AUTHORITY["EPSG","8901"]],UNIT["degree",0.0174532925199433,AUTHORITY["EPSG","9122"]],AUTHORITY["EPSG","4258"]],PROJECTION["Transverse_Mercator"],PARAMETER["latitude_of_origin",0],PARAMETER["central_meridian",27],PARAMETER["scale_factor",0.9996],PARAMETER["false_easting",500000],PARAMETER["false_northing",0],UNIT["metre",1,AUTHORITY["EPSG","9001"]],AXIS["Easting",EAST],AXIS["Northing",NORTH],AUTHORITY["EPSG","3067"]]"#;

    const WKT2_25832: &str = r#"PROJCRS["ETRS89 / UTM zone 32N",
        BASEGEOGCRS["ETRS89",
            DATUM["European Terrestrial Reference System 1989",
                ELLIPSOID["GRS 1980",6378137,298.257222101,LENGTHUNIT["metre",1]]],
            ID["EPSG",4258]],
        CONVERSION["UTM zone 32N",METHOD["Transverse Mercator",ID["EPSG",9807]]],
        CS[Cartesian,2],
            AXIS["(E)",east,ORDER[1]],
            AXIS["(N)",north,ORDER[2]],
        USAGE[SCOPE["Engineering survey"],AREA["Europe"],BBOX[38.76,6,84.33,12]],
        ID["EPSG",25832]]"#;

    /// The GeoTIFF key directory (record 34735) of the regression tile: model type
    /// projected, ProjectedCSTypeGeoKey 3067, linear and vertical units metre.
    const GEOTIFF_3067: &[u8] = &[
        1, 0, 1, 0, 0, 0, 4, 0, 0, 4, 0, 0, 1, 0, 1, 0, 0, 12, 0, 0, 1, 0, 0xfb, 0x0b, 4, 12, 0, 0,
        1, 0, 0x29, 0x23, 3, 16, 0, 0, 1, 0, 0x29, 0x23,
    ];

    fn projection_vlr(record_id: u16, data: Vec<u8>) -> Vlr {
        Vlr {
            user_id: "LASF_Projection".into(),
            record_id,
            description: String::new(),
            data,
        }
    }

    /// Write an empty LAS file of `version` with `vlrs` to `path`.
    fn write_las(fs: &MemoryFileSystem, path: &str, version: (u8, u8), vlrs: Vec<Vlr>) {
        let mut builder = Builder::from(las::Version::new(version.0, version.1));
        builder.has_wkt_crs = vlrs.iter().any(Vlr::is_wkt_crs);
        builder.vlrs = vlrs;
        let header = builder.into_header().unwrap();
        let writer = las::Writer::new(std::io::Cursor::new(Vec::new()), header).unwrap();
        let bytes = writer.into_inner().unwrap().into_inner();
        fs.create(path).unwrap().write_all(&bytes).unwrap();
    }

    fn wkt_las(fs: &MemoryFileSystem, path: &str, wkt: &str) {
        write_las(fs, path, (1, 4), vec![projection_vlr(2112, wkt.into())]);
    }

    fn geotiff_las(fs: &MemoryFileSystem, path: &str, keys: &[u8]) {
        write_las(fs, path, (1, 2), vec![projection_vlr(34735, keys.to_vec())]);
    }

    fn epsg_of(fs: &MemoryFileSystem, path: &str) -> Result<Option<u32>, String> {
        header_epsg(las::Reader::new(fs.open(path).unwrap()).unwrap().header())
    }

    fn resolve(fs: &MemoryFileSystem, config_epsg: Option<u32>, paths: &[&str]) -> Option<u32> {
        let inputs: Vec<PathBuf> = paths.iter().map(PathBuf::from).collect();
        resolve_epsg(fs, config_epsg, &inputs).unwrap()
    }

    #[test]
    fn wkt_epsg_is_the_projected_crs_own_code() {
        assert_eq!(wkt_epsg(WKT1_3067), Ok(3067));
        assert_eq!(wkt_epsg(WKT2_25832), Ok(25832));
        // compound: the horizontal projected CRS, not the compound or vertical code
        let compound = format!(
            r#"COMPD_CS["ETRS89 / TM35FIN + N2000",{WKT1_3067},VERT_CS["N2000 height",VERT_DATUM["N2000",2005,AUTHORITY["EPSG","1030"]],AUTHORITY["EPSG","3900"]],AUTHORITY["EPSG","9999"]]"#
        );
        assert_eq!(wkt_epsg(&compound), Ok(3067));
        // parentheses as brackets, escaped quotes, whitespace
        assert_eq!(
            wkt_epsg(r#" PROJCS ( "a ""b""" , AUTHORITY ( "epsg" , "32632" ) ) "#),
            Ok(32632)
        );
    }

    #[test]
    fn wkt_without_a_projected_epsg_code_has_none() {
        let geographic = r#"GEOGCS["ETRS89",DATUM["x",SPHEROID["GRS 1980",6378137,298.257222101]],AUTHORITY["EPSG","4258"]]"#;
        assert!(wkt_epsg(geographic).is_err());
        // the base CRS's code is not the projected CRS's
        let unnamed =
            r#"PROJCS["local",GEOGCS["ETRS89",AUTHORITY["EPSG","4258"]],UNIT["metre",1]]"#;
        assert!(wkt_epsg(unnamed).is_err());
        assert!(wkt_epsg(r#"PROJCS["x",AUTHORITY["ESRI","102100"]]"#).is_err());
        // not a valid EPSG code, like the `epsg` key
        assert!(wkt_epsg(r#"PROJCS["x",AUTHORITY["EPSG","0"]]"#).is_err());
        for bad in [
            "",
            "PROJCS",
            r#"PROJCS["x",AUTHORITY["EPSG","3067"]"#,
            "[[[[",
        ] {
            assert!(wkt_epsg(bad).is_err(), "{bad}");
        }
        let deep = "A[".repeat(1000) + &"]".repeat(1000);
        assert!(wkt_epsg(&deep).is_err());
    }

    #[test]
    fn header_epsg_reads_wkt_geotiff_or_nothing() {
        let fs = MemoryFileSystem::new();
        wkt_las(&fs, "wkt.las", &format!("\u{feff}{WKT1_3067}\0"));
        assert_eq!(epsg_of(&fs, "wkt.las"), Ok(Some(3067)));
        geotiff_las(&fs, "geotiff.las", GEOTIFF_3067);
        assert_eq!(epsg_of(&fs, "geotiff.las"), Ok(Some(3067)));
        write_las(&fs, "none.las", (1, 2), vec![]);
        assert_eq!(epsg_of(&fs, "none.las"), Ok(None));
    }

    #[test]
    fn header_epsg_refuses_unnamed_crs() {
        let fs = MemoryFileSystem::new();
        // ProjectedCSTypeGeoKey 32767 = user-defined
        let mut user_defined = GEOTIFF_3067.to_vec();
        user_defined[22..24].copy_from_slice(&32767u16.to_le_bytes());
        geotiff_las(&fs, "user.las", &user_defined);
        assert!(epsg_of(&fs, "user.las").is_err());
        // geographic model: GeodeticCRSGeoKey (2048) 4258, no projected key
        let geographic: &[u8] = &[1, 0, 1, 0, 0, 0, 1, 0, 0, 8, 0, 0, 1, 0, 0xa2, 0x10];
        geotiff_las(&fs, "geographic.las", geographic);
        assert!(epsg_of(&fs, "geographic.las").is_err());
        wkt_las(&fs, "unnamed.las", r#"PROJCS["local",UNIT["metre",1]]"#);
        assert!(epsg_of(&fs, "unnamed.las").is_err());
    }

    #[test]
    fn resolve_epsg_prefers_the_ini_key() {
        let fs = MemoryFileSystem::new();
        wkt_las(&fs, "a.las", WKT2_25832);
        write_las(&fs, "none.las", (1, 2), vec![]);
        assert_eq!(resolve(&fs, None, &["a.las"]), Some(25832));
        assert_eq!(resolve(&fs, Some(3067), &["a.las"]), Some(3067));
        assert_eq!(resolve(&fs, None, &["none.las"]), None);
        assert_eq!(resolve(&fs, Some(3067), &["none.las"]), Some(3067));
        assert_eq!(resolve(&fs, None, &[]), None);
    }

    #[test]
    fn resolve_epsg_requires_tiles_to_agree() {
        let fs = MemoryFileSystem::new();
        wkt_las(&fs, "a.las", WKT1_3067);
        geotiff_las(&fs, "b.las", GEOTIFF_3067);
        write_las(&fs, "none.las", (1, 2), vec![]);
        wkt_las(&fs, "c.las", WKT2_25832);
        wkt_las(&fs, "unnamed.las", r#"PROJCS["local",UNIT["metre",1]]"#);
        fs.create("corrupt.laz")
            .unwrap()
            .write_all(b"LASF")
            .unwrap();
        // undeclared and unreadable tiles take the others' code
        assert_eq!(
            resolve(&fs, None, &["a.las", "b.las", "none.las", "corrupt.laz"]),
            Some(3067)
        );
        // a lone unusable declaration declares nothing
        assert_eq!(resolve(&fs, None, &["unnamed.las", "none.las"]), None);

        for (other, expected) in [
            ("c.las", "c.las declares EPSG:25832"),
            (
                "unnamed.las",
                "unnamed.las: the WKT projected CRS has no EPSG code",
            ),
        ] {
            let inputs: Vec<PathBuf> = ["a.las", "b.las", other].map(PathBuf::from).into();
            let err = resolve_epsg(&fs, None, &inputs).unwrap_err().to_string();
            assert!(err.contains(expected), "{err}");
            // the override settles it
            assert_eq!(resolve_epsg(&fs, Some(3067), &inputs).unwrap(), Some(3067));
        }
    }

    #[test]
    fn raster_crs_sidecar_names_the_code_or_is_kept() {
        let fs = MemoryFileSystem::new();
        write_raster_crs(&fs, "pullautus.png", Some(3067)).unwrap();
        assert_eq!(
            fs.read_to_string("pullautus.png.aux.xml").unwrap(),
            "<PAMDataset>\n  <SRS>EPSG:3067</SRS>\n</PAMDataset>\n"
        );
        write_raster_crs(&fs, "pullautus.png", None).unwrap();
        assert!(fs.exists("pullautus.png.aux.xml"));
        write_raster_crs(&fs, "other.png", None).unwrap();
        assert!(!fs.exists("other.png.aux.xml"));
    }
}
