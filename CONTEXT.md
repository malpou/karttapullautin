# Karttapullautin

Turns classified airborne LiDAR into a draft orienteering map that follows ISOM 2017-2.

## Language

### Input

**Point cloud**: The set of returns in one survey or tile. _Avoid_: LAS, LAZ, points
**Tile**: One input point-cloud file and the square of ground it covers. _Avoid_: laz, file, thread
**Padding**: The strip of neighbouring ground processed with a tile so features meet at its edges. _Avoid_: buffer, margin
**Return**: One recorded laser echo with position, class and echo order. _Avoid_: point record, xyz, r3/r4/r5
**Echo order**: A return's position among the echoes of one pulse, first through last. _Avoid_: return number, first/last flag
**Ground return**: A return the data supplier classified as terrain. See also: Return.
**Vector mapping**: A rule file that assigns a symbol code to shapefile records by their attributes, one `name|symbol code|conditions` rule per line; named by the `vectorconf` ini key. _Avoid_: vectorconf (the key), osm.txt (one such file)
**Pulse density**: Laser pulses per square metre of ground in a tile. _Avoid_: point density, return density

### Relief

**Ground model**: The gridded terrain surface derived from ground returns. _Avoid_: heightmap, hmap, xyz, DEM
**Cell size**: Ground distance between neighbouring cells of the ground model. _Avoid_: resolution, grid step
**Local relief**: Height range within a small window around a cell. _Avoid_: steepness, slope
**Contour interval**: Vertical spacing of full contours on the finished map.
**Contour**: A line of equal height at a multiple of the contour interval (ISOM 101).
**Index contour**: Every fifth contour, drawn heavier (ISOM 102).
**Form line**: A half-interval line kept only where contours alone under-describe the ground (ISOM 103). _Avoid_: intermed, formline (as a mode number)
**Depression**: A closed contour whose inside is lower than the line.
**Slope line**: The tick that marks the downhill side of a contour. _Avoid_: tick, decoration
**Knoll**: A closed high point large enough to be drawn as a contour.
**Dot knoll**: A high point too small for a contour, drawn as a point symbol (ISOM 109). _Avoid_: dotknoll, 1010
**Knoll lift**: Raising the ground model under a prominent small knoll so that it earns a contour.
**Ring**: A closed contour line, first vertex repeated last, tested for what it encloses. _Avoid_: polygon, closed polyline
**Cliff**: An abrupt drop mapped as a line; passable or impassable by height (ISOM 201/202). _Avoid_: c2g, c3g, cliff2, cliff3

### Vegetation

**Canopy top**: Highest vegetation return above the ground model in a cell. _Avoid_: roof, top
**Stratum**: A height band above ground within which returns are counted. _Avoid_: zone, zones
**Vegetation density**: Share of returns intercepted within the running-height strata.
**Green shade**: One of the ordered runnability classes drawn in green (ISOM 406, 408, 410).
**Open land**: Ground with almost no returns above knee height (ISOM 401/403). _Avoid_: yellow
**Undergrowth**: Dense low vegetation under otherwise runnable forest (ISOM 407/409). _Avoid_: ug, ugg

### Output

**Symbol code**: The ISOM 2017-2 number a feature is drawn with, written "NNN.NNN" (`101.000`; a variant takes a non-zero suffix, e.g. slope line `101.001`, large building `521.001`). GeoJSON carries it as `isom_code`; a DXF layer is named with it. _Avoid_: layer (a DXF layer is a container, never the ISOM number), symbol (as a property name), plain `101`, ISOM 2000 numbering
**Upper level**: A mapped feature that passes over the others, such as a bridge, drawn on top of them. A `T` suffix on the code in a vector mapping; `upper_level` in GeoJSON. _Avoid_: T code, top
**Symbol table**: The machine-readable list of ISOM 2017-2 symbols the vector output may emit: the vendored, pinned `isom.yaml` of MetsaApp/isom-maplibre, giving each code its table and drawing. _Avoid_: symbol set (a mapping program's file), legend
**Table**: The named group of vector features one style source reads, e.g. `contours`, `cliffs`, `knolls_points`, `vegetation_areas`, `water`, `paths`, `manmade`; each GeoJSON output is one table. MapLibre calls it a source layer. _Avoid_: layer (only a DXF container), output kind
**Map frame**: Scale, resolution, origin and north rotation of the rendered sheet. At 600 dpi and 1:10 000 one map inch is 254 ground metres, so the sheet has 600/254 pixels per metre before scalefactor.
**World file**: The sidecar that places a raster image on the ground by origin and pixel size. _Avoid_: pgw, georeference file

### Development

**Fidelity**: Agreement of output with what the Perl program produces for the same input. _Avoid_: faithfulness, classic
**Output change**: Any difference in pixels, debug intermediates or vector output on the regression tile against the previous baseline. _Avoid_: regression (for an intended change)
**Regression tile**: The fixed test tile whose rendered map and vector output every branch is compared against. _Avoid_: test file, golden tile
**Debug intermediates**: Stage results written to disk only on request, for inspection, with no format guarantee. _Avoid_: temp files, cache
