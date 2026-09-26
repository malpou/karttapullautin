use std::{env, fmt::Write, fs, path::Path};

fn main() {
    println!("cargo::rerun-if-changed=schema/geojson.schema.json");

    let content = fs::read_to_string("schema/geojson.schema.json")
        .expect("failed to read schema/geojson.schema.json");
    let schema: schemars::schema::RootSchema =
        serde_json::from_str(&content).expect("failed to parse schema");

    let mut type_space = typify::TypeSpace::new(&typify::TypeSpaceSettings::default());
    type_space
        .add_root_schema(schema)
        .expect("failed to generate types");

    let tokens = type_space.to_stream();
    let file = syn::parse2::<syn::File>(tokens).expect("failed to parse generated tokens");
    let contents = prettyplease::unparse(&file);

    let out_dir = env::var("OUT_DIR").unwrap();
    let out_path = Path::new(&out_dir).join("geojson_types.rs");
    fs::write(&out_path, contents).expect("failed to write geojson_types.rs");

    isom_table(Path::new(&out_dir));
}

const ISOM_YAML: &str = "vendor/isom-maplibre/isom.yaml";

/// One distinct `stack[].code` of isom.yaml.
struct IsomSymbol<'a> {
    code: &'a str,
    /// The table of the code's first stack entry.
    table: &'a str,
    /// Some entry of the code is a fill.
    has_fill: bool,
    /// Every entry of the code is a circle or a point-placed icon.
    all_point: bool,
}

/// Generates `IsomTable` and `IsomCode` from the vendored isom-maplibre symbol table
/// (ADR 0006) into `isom_table.rs`, and writes the parsed table as `isom.json` for the
/// test that validates it against the vendored schema. Reads only the vendored copy.
fn isom_table(out_dir: &Path) {
    println!("cargo::rerun-if-changed={ISOM_YAML}");

    let yaml = fs::read_to_string(ISOM_YAML).expect("failed to read isom.yaml");
    // Parsing into a JSON value resolves the YAML anchors (`*black`).
    let doc: serde_json::Value = serde_norway::from_str(&yaml).expect("failed to parse isom.yaml");
    fs::write(out_dir.join("isom.json"), doc.to_string()).expect("failed to write isom.json");

    let tables: Vec<&str> = doc["tables"]
        .as_array()
        .expect("isom.yaml: tables must be a list")
        .iter()
        .map(|t| t.as_str().expect("isom.yaml: table names are strings"))
        .collect();

    // One symbol per distinct code. A code drawn in several parts (casing and core,
    // fill and outline) keeps the table of its first entry in stack order.
    let mut symbols: Vec<IsomSymbol> = Vec::new();
    let entries = doc["stack"]
        .as_array()
        .expect("isom.yaml: stack must be a list")
        .iter()
        .flat_map(|group| {
            group["symbols"]
                .as_array()
                .expect("isom.yaml: stack[].symbols must be a list")
        });
    for entry in entries {
        let code = entry["code"].as_str().expect("isom.yaml: code is a string");
        let table = entry["table"]
            .as_str()
            .expect("isom.yaml: table is a string");
        assert!(
            tables.contains(&table),
            "isom.yaml: {code} names table {table}, which is not in tables"
        );
        let is_fill = entry.get("fill").is_some();
        let is_point = entry.get("circle").is_some()
            || entry
                .get("icon")
                .is_some_and(|icon| icon["placement"].as_str().unwrap_or("point") == "point");
        match symbols.iter_mut().find(|s| s.code == code) {
            Some(s) => {
                if s.table != table {
                    println!(
                        "cargo::warning=isom.yaml: {code} is listed in {} and {table}; IsomCode::table() keeps {}",
                        s.table, s.table
                    );
                }
                s.has_fill |= is_fill;
                s.all_point &= is_point;
            }
            None => symbols.push(IsomSymbol {
                code,
                table,
                has_fill: is_fill,
                all_point: is_point,
            }),
        }
    }
    symbols.sort_by_key(|s| s.code);

    let code_variant = |code: &str| format!("C{}", code.replace('.', "_"));
    let table_variant = |table: &str| -> String {
        table
            .split('_')
            .map(|w| {
                let mut chars = w.chars();
                let first = chars
                    .next()
                    .expect("isom.yaml: table names have no empty `_` segment");
                first.to_uppercase().chain(chars).collect::<String>()
            })
            .collect()
    };

    let mut src = String::new();
    let w = &mut src;

    // IsomTable
    w.push_str("/// A table of the isom-maplibre style: the source it reads a feature from.\n");
    w.push_str("#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)] pub enum IsomTable {");
    for t in &tables {
        write!(w, "{},", table_variant(t)).unwrap();
    }
    w.push_str("} impl IsomTable {");
    w.push_str("/// Every table, in isom.yaml order.\npub const ALL: &[IsomTable] = &[");
    for t in &tables {
        write!(w, "IsomTable::{},", table_variant(t)).unwrap();
    }
    w.push_str("];");
    w.push_str("/// The table name, e.g. `contours`.\n");
    w.push_str("pub const fn as_str(self) -> &'static str { match self {");
    for t in &tables {
        write!(w, "IsomTable::{} => {t:?},", table_variant(t)).unwrap();
    }
    w.push_str("} } }");

    // IsomCode
    w.push_str("/// An ISOM 2017-2 symbol code the isom-maplibre style draws.\n");
    w.push_str("#[allow(non_camel_case_types)] #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)] pub enum IsomCode {");
    for s in &symbols {
        write!(w, "#[doc = \" `{}`\"] {},", s.code, code_variant(s.code)).unwrap();
    }
    w.push_str("} impl IsomCode {");
    w.push_str("/// Every code in the table, in ascending order.\npub const ALL: &[IsomCode] = &[");
    for s in &symbols {
        write!(w, "IsomCode::{},", code_variant(s.code)).unwrap();
    }
    w.push_str("];");
    w.push_str("/// The code as isom-maplibre reads it, \"NNN.NNN\".\n");
    w.push_str("pub const fn as_str(self) -> &'static str { match self {");
    for s in &symbols {
        write!(w, "IsomCode::{} => {:?},", code_variant(s.code), s.code).unwrap();
    }
    w.push_str("} }");
    w.push_str("/// The table the style reads this code from; for a code listed in several tables, the first in stack order.\n");
    w.push_str("pub const fn table(self) -> IsomTable { match self {");
    for s in &symbols {
        write!(
            w,
            "IsomCode::{} => IsomTable::{},",
            code_variant(s.code),
            table_variant(s.table)
        )
        .unwrap();
    }
    w.push_str("} }");
    w.push_str("/// The geometry the style draws this code with: an area if any of its entries is a fill, a point if every entry is a circle or a point-placed icon, a line otherwise.\n");
    w.push_str("pub const fn geometry(self) -> SymbolGeometry { match self {");
    for s in &symbols {
        let geometry = match (s.has_fill, s.all_point) {
            (true, _) => "Area",
            (false, true) => "Point",
            (false, false) => "Line",
        };
        write!(
            w,
            "IsomCode::{} => SymbolGeometry::{geometry},",
            code_variant(s.code)
        )
        .unwrap();
    }
    w.push_str("} } }");

    let file = syn::parse_str::<syn::File>(&src).expect("failed to parse generated isom code");
    fs::write(out_dir.join("isom_table.rs"), prettyplease::unparse(&file))
        .expect("failed to write isom_table.rs");
}
