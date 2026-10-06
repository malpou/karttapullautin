# The "Map Machine" ***(Karttapullautin)***
## What it this?

The rust-lang source code of the map generator application that go through the alias ***pullauta*** which is available as binary executable for Linux, Mac and Windows (find attachment in each releases).

## What is ***pullauta***?

***pullauta*** is an application designed to generate highly accurate maps out of LiDAR data input files that supports many file formats, namely LAS, LAZ, and XYZ files. It uses advanced algorithms for filtering, classification, and feature extraction, ensuring that users can generate highly accurate maps with ease.

## Download ***pullauta*** 

Download the latest binary for your platform (Linux, Mac or Windows) from https://github.com/karttapullautin/karttapullautin/releases/latest and extract the files where you want to use them.

## Compiling ***pullauta*** from source code

1) You need first to install the rust toolchain.

 - See https://rustup.rs  

2) Then download the latest code at https://github.com/karttapullautin/karttapullautin/releases/latest

3) Finally compile it
   
    ```
   cargo build --release
    ```

    For maximum performance, it is recommended to compile targeting the native cpu by specifying the `target-cpu` flag. This makes sure that any instruction set extensions such as SIMD and FMA are used. This includes if your processor supports AVX, AVX2 and AVX512 (and NEON on ARM targets) as 
the default release binaries are compiled without these enabled to be as portable as possible. If you have a relatively recent CPU (eg. Intel `skylake` or later) you should instead compile like this:
    
    ```
    RUSTFLAGS="-C target-cpu=native" cargo build --release
    ```

4) The ***pullauta*** binary will be accessible in the `target/release/` directory. You can proceed and copy it to your desired directory.


### Converting a LiDAR file

***pullauta*** accepts .LAS, .LAZ or .XYZ file with classification (xyzc).

You can run the `pullauta` executable with the path to your file as argument:  
    
    ./pullauta L3323H3.laz

> Note: By defaut messages with the log level _info_ will be printed to the console. To show more information (eg. timings of each operation),
> set the `RUST_LOG` environment variable to `debug` or specify it on the command line like so:
> ```bash
> RUST_LOG=debug ./pullauta [..]
> ```
> Other log level available is `warn`, in which no info of current run will be displayed, `error`, which will only show errors, and `trace` which will output a lot of log messages about small details during the processing.

As output Karttapullautin writes two 600 dpi png map images. One without depressions and one with purple depressions. It also writes contours and cliffs as dxf files and GeoJSON tables to temp folder to be post processed, for example using Open Orienteering Mapper or OCAD. The temp folder keeps only these products (see [Vectors](#vectors)); every other stage result is a debug intermediate, kept only with `debug_intermediates=1`, with no format guarantee.

The ini key `outputs` selects the product families written to disk, comma separated in any order: `raster` (the map pngs, the vegetation and undergrowth rasters, and the merged pngs), `dxf` (the dxf files) and `geojson` (the GeoJSON tables). The default is `outputs=raster,dxf,geojson`. Without `raster` no map is rendered: `outputs=geojson` runs only the stages the tables need and leaves only the tables.

You can re-render png map files (like with changed north line settings) by running the binary without arguments. Re-rendering reads the debug intermediates, so the tile must have been processed with `debug_intermediates=1`:
    
    ./pullauta

Karttapullautin can also render zip files containing shape files downloaded from differents sources. After normal process (with `debug_intermediates=1`) just run the binary with the zip(s) as arguments. You must define your configuration file describing the shape file content, in the ini file, parameter `vectorconf` (see osm.txt and fastighetskartan.txt).

    ./pullauta yourzipfile1.zip yourzipfile2.zip yourzipfile3.zip yourzipfile4.zip

The configuration file has one rule per line, `name|symbol code|conditions`, for example `road|502.000T|highway=primary&bridge=yes`. Conditions are `field=value` or `field!=value` on the shape file's attributes, joined with `&`; the first rule whose conditions all hold and whose symbol code Karttapullautin can draw wins. Symbol codes are the ISOM 2017-2 codes of the [isom-maplibre](https://github.com/MetsaApp/isom-maplibre) symbol table, written "NNN.NNN"; a code the table does not list is an error. Karttapullautin draws 502.000 wide road, 503.000 road, 504.000 vehicle track, 506.000 small footpath, 509.000 railway, 510.000 power line, 516.000 fence, 521.000 building, 520.000 area that shall not be entered, 501.000 paved area, 401.000 open land, 415.000 cultivation boundary, 301.000 water, 305.000 watercourse and 308.000 marsh. A `T` suffix (e.g. `502.000T`) marks an upper level, such as a bridge, drawn on top of other features. The matched features are also written as GeoJSON in the temp folder, each to the table of its code (`paths.geojson`, `manmade.geojson`, `water.geojson`, `vegetation_areas.geojson`), with the properties `isom_code`, `category` (the rule's name) and `upper_level` (only when true).

For Finns: Karttapullautin render Maastotietokanta zip files (shape files) downloaded from the download site of Maanmittauslaitos without setting a configuration file. Just leave `vectorconf` parameter empty.

To print a map at right scale, you download for example IrfanView http://www.irfanview.com/ open png map, Image -> Information, set resolution 600 x 600 DPI and push "change" button and save.  Then crop map if needed (Select area with mouse and Edit -> crop selection). Print using "Print size: Original Size srom DPI". Like this your map should end up 1:10000 scale on paper.

#### Creating shape file from OSM file

You can download OSM files from Open Street Map website https://www.openstreetmap.org/export in a form of a .osm file extension. To convert this file in something that can be used by karttapullautin you'll need the GDAL ogr2ogr program (Download from https://gdal.org/en/latest/download.html)

Run the following commands in your terminal
```
ogr2ogr --config OSM_USE_CUSTOM_INDEXING NO -skipfailures -f "ESRI Shapefile" output_shapes map.osm -overwrite -t_srs EPSG:3067
zip -r -j map.shp.zip output_shapes/*
```

Replace `EPSG:3067` by the coordinates ESPG codename of that the LAZ file uses.

You will have a zip file `map.shp.zip` that you can use with karttapullautin.

#### Converting the internal XYZ format

Previously, Karttapullautin used regular text-based `.xyz` files to store the temporary files which could be opened and visualized by many external tools. But with the introduction of an internal (non-stable) binary format for increased performance and reduced disk usage, there is now a new command that can do the conversion into the previous format for you. This will, for example, convert the `xyztemp.xyz.bin` file (kept with `debug_intermediates=1`) into a regular `xyztemp.xyz` file (with one line per point) which can be opened by external tools:
```
./pullauta internal2xyz temp/xyztemp.xyz.bin temp/xyztemp.xyz
```
> Note: this also works for the binary `.hmap` files.

#### Converting the internal binary geometry format to DXF

Similar as the XYZ files mentioned above, Karttapullautin previously used regular text-based `.dxf` files to store the temporary geometry which could be opened and visualized by many external tools. But with the introduction of an internal (non-stable) binary format for increased performance and reduced disk usage, there is now a new command that can do the conversion into `DXF` for you. Example usage:
```
./pullauta bin2dxf temp/c2g.dxf.bin temp/c2g.dxf
```

With `dxf` in `outputs` regular `.dxf` files are written next to the binary files. The `.dxf.bin` files are debug intermediates, kept only with `debug_intermediates=1`.

### Fine tuning the output

`pullauta` creates a `pullauta.ini` file if it doesn't already exists. Your settings are there. For the second run you can change settings as you wish. Experiment with small file to find best settings for your taste/terrain/lidar data.

For Ini file configuration explanation, see ini file comments.

### Re-processing steps again

When the process is done and you find there is too much green or too small cliffs, you can make parts of the process again with different parameters without having to do it all again. The stages read the previous run's debug intermediates, so run the tile with `debug_intermediates=1` first. To re-generate only vegetation type from command line:

    ./pullauta makevege
    ./pullauta 

To make cliffs again:

    ./pullauta makecliffs xyztemp.xyz 1.0 1.15
    ./pullauta

### Vectors

In additon to the png raster map imges, Karttapullautin makes also vector contours and cliffs and also some raster vector files one might find intresting for mapping use. After the process you can find them in temp folder, which keeps only these products unless `debug_intermediates=1`. The DXF files are written with `dxf` in `outputs`, the rasters with `raster` and the tables with `geojson`.

- `out2.dxf`: final contours with 2.5 m interval
- `dotknolls.dxf`: dot knolls and small U -depressions. Some are not rendered to png files for legibility reasons.
- `c2g.dxf`: small cliffs
- `c3g.dxf`: big cliffs
- `formlines.dxf`: the form lines the renderer drew
- `vegetation.dxf`: the vegetation areas, symbol codes as layers, and `basemap.dxf` (with `basemapinterval` above 0)
- `vegetation.png + vegetation.pgw`: generalized green/yellow as raster, same as at the background of final map png files; `undergrowth.png + undergrowth.pgw` the undergrowth (and with `vege_bitmode=1` the one-channel `vegetation_bit.png` and `undergrowth_bit.png`).
- `<table>.geojson`: the vector output for the [isom-maplibre](https://github.com/MetsaApp/isom-maplibre) style, one file per table it reads (`contours`, `knolls_points`, `cliffs`, `vegetation_areas`, and with a `vectorconf` also `water`, `paths`, `manmade`), each feature with its ISOM 2017-2 symbol code as `isom_code` (`"101.000"`).

For importing Maastotietokanta, try reading shape filed directly to your mapping app. Note that the `dxf` files need to be converted from the internal `.bin.dxf` format using the command `bin2dxf` as mentioned above.

### Batch processing

Karttapulautin can also batch process all las/las files + Maastotietokanta zips in a directory. To do it, turn batch processing on in ini file. configure your input file directory and output directory for map tiles. Copy your input files to input directory and run `./pullauta`. It starts processing las/laz files one by one until everything is done. If you have several cores 
in your CPU, you can make use of all of them to process multiple file at once. you can configure it with `processes` parameter in ini file. Note, processes parameter effects only batch mode, in normal mode it uses just one worker process. You will also need lots of RAM to process simultaneously several large laser files. To re-process tiles in bach mode you need to remove previous png files from output folder. Each tile's temp folder is removed when the tile is done; with `debug_intermediates=1` it is kept as `temp_<tile>_dir`.

You can merge png files in output folder with Karttapullautin.

Without the depressions

    ./pullauta pngmerge 1

and depression versions

    ./pullauta pngmergedepr 1

vegetation backround images (each tile's `<tile>_vege.png` and `<tile>_undergrowth.png` in the output folder)

    ./pullauta pngmergevege


The last paramameter (number) is scale factor. 2 reduces size to 50%, 4 to 25%, 20 to 5% and so on. Command writes out jpg and png versions (merged.png, merged.jpg and their world files) into the batch output folder. 
Note, you easily run out of memory if you try merging together too large area with too high resolution.

You can also merge dxf files. The merge reads each tile's `.dxf.bin` crops, which the batch removes when it is done unless `debug_intermediates=1`; `batchmerge=1` merges them during the run. With `dxf` in `outputs` each tile's crops are also kept as `<tile>_<layer>.dxf` in the output folder, and the merge writes `merged.dxf` and `merged_<layer>.dxf` in the working directory.

    ./pullauta dxfmerge

With `batchmerge=1` the batch run does all of the merging itself when the tiles are done: the png merges (`raster`), the dxf merge (`dxf`), and each tile's tables cropped to the tile, merged into `merged_<table>.geojson`, and published as one `<table>.geojson` per table (`geojson`), one `output.dxf` (symbol codes as DXF layers) and `output.ocdCrt` (the cross reference table for OCAD's DXF import) (`dxf`), all in the batch output folder. `output.dxf` is made from the tables: without `geojson` they are written, merged and removed when the batch is done. The GeoJSON files declare the coordinate system the input LAS/LAZ files declare (as an EPSG code, from their WKT or GeoTIFF CRS records), and each map png/jpg with a world file gets a `<name>.png.aux.xml` sidecar naming it, which QGIS and GDAL read; set `epsg` to override it, for files with a missing or wrong CRS. A batch stops before processing when a tile declares a different coordinate system, or one without an EPSG code, unless `epsg` is set; tiles that declare none are assumed to share the others'.

### Note:

Some commands from the original perl karttapullatin that are either obsolete or not necessary for the map generation are not supported by this new rust version:  

They are:
  - `cliffgeneralize`
  - `ground`
  - `ground2`
  - `groundfix`
  - `makecliffsold`
  - `makeheight`
  - `vege`
  - `profile`
  - `xyzfixer`

If you need to run one of those, you must use the original perl script https://www.routegadget.net/karttapullautin/ or https://github.com/linville/kartta-pack for mac and linux

## Development

Make your changes, then youd run:

    cargo build --release

The new binary will be accessible in the `target/release/` directory

### Measuring an output change

A change that alters the map should say by how much. `eval` compares two
outputs and prints a report:

    ./pullauta eval <baseline> <candidate> [--tolerance <metres>] [--diff-dir <dir>] [--format text|json] [--ignore <suffix>]... [--fail-on-change] [--expected <report.json>]

`baseline` and `candidate` are two files or two directories. Directories are
walked recursively and files are paired by relative path; files present on
only one side are listed. `--ignore <suffix>` (repeatable) leaves out files
whose relative path ends with the suffix, such as `log.txt`. Two directories
holding no `.png` or `.geojson` file at all are an error, so a run that
crashed before writing its map cannot pass. The baseline can be the base
branch's output or a reference map in the same formats. A reference map in
one GeoJSON file can also be compared with a directory: the directory's
`<table>.geojson` files (`contours`, `cliffs`, `knolls_points`,
`vegetation_areas`, `water`, `paths`, `manmade`, as in `temp/` or the combined
export) are read as one map.

- `*.png`: changed pixels (count and percentage). When the two images hold at
  most 32 colours between them (such as `temp/vegetation.png`), every colour
  is also scored as a class: pixel counts on each side and
  intersection-over-union. `--diff-dir` writes `<name>.diff.png` for every
  pair that differs: the baseline in light grey, changed pixels in red.
  Images must be the same size.
- `*.geojson`: features are grouped by symbol code (the `isom_code` property,
  `NNN.NNN`). Features with no code, or a code the symbol table does not list,
  are counted by the value found; a different count is a change.
  Per code: feature, point, line and polygon counts, total line length, total
  polygon area and proper crossings between lines of that code (lines of
  different codes are not tested) on each side; the number of features whose
  other properties (`level_m`, `shade`, `category`, ...) match no feature on
  the other side; for lines, the share of candidate length within the
  tolerance of a baseline line (precision), the share of baseline length
  within the tolerance of a candidate line (recall), the Hausdorff distance
  and the mean distance each way; for points, the share on each side with a
  counterpart within the tolerance and the Hausdorff distance; for polygons,
  the same line measures over their rings, so a moved polygon shows even when
  its area does not change. Collection members such as `crs` are compared too.
  The tolerance defaults to 1 m and must be at least 0.01 m; a tolerance that
  would take more than 20 million distance samples is refused. Coordinates are
  read as projected metres, as the pipeline writes them. A malformed file, or
  a geometry type other than Point, LineString, MultiLineString, Polygon and
  MultiPolygon, is an error naming the feature.
- Every other file (world files, DXF, `.aux.xml`, `.ocdCrt`, ...): compared
  byte for byte.

A typical check runs both builds in separate directories on the same input,
then compares them:

    (cd base && /path/to/base/pullauta ../test_file.laz)
    (cd branch && /path/to/branch/pullauta ../test_file.laz)
    ./pullauta eval base branch --diff-dir diffs --ignore log.txt

`--format json` prints the report as JSON. It is deterministic, so it can be
committed and diffed: keys are sorted, the input and diff-image paths are left
out, sums are taken in a sorted order, and every measure is rounded to six
decimals.

Without a gate option `eval` only reports and exits with status 0, even when a
pair could not be read (the report says so). Two options make it a gate that
exits with status 2 on failure; status 1 is kept for errors such as bad
arguments, nothing to compare, an unreadable directory or expected report:

- `--fail-on-change` fails when anything differs: a changed pixel, a changed
  per-code total or property, a non-zero Hausdorff distance, changed bytes, a
  pair that cannot be read, or a file present on one side only.
- `--expected <report.json>` passes when nothing differs or when the JSON
  report equals the given file, and it decides alone when both options are
  given. A pair that cannot be read always fails, and an expected report that
  records one is an error. When nothing differs, a warning says the expected
  report is stale. Commit the report of an intended change and the gate
  accepts exactly that change:

      ./pullauta eval base branch --format json > expected.json
      ./pullauta eval base branch --expected expected.json

`eval` only reads its inputs; it never runs the pipeline.

## Contributors

@jagge @rphlo @antbern

