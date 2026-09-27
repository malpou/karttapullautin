#!/usr/bin/env bash
# Run the regression jobs with one pullauta build.
#
#   regression/run.sh <pullauta> <source-dir> <out-dir>
#
# <pullauta> is the binary, <source-dir> the checkout it was built from: each job
# starts from that checkout's own pullauta.default.ini and osm.txt, so a build is
# run with its own defaults even when a later branch renames or adds keys.
#
# Jobs, both on the regression tile:
#   single  `pullauta test_file.laz` with the default ini (the out-of-the-box map)
#   batch   the tile and the OSM shapefile zip in in/, with batch=1,
#           vectorconf=osm.txt, vector_vege=1, batchmerge=1 and savetempfolders=1,
#           then `pullauta pngmerge 1` and `pullauta pngmergedepr 1` (full-scale
#           merges; they replace batchmerge's 4x ones)
#
# Layout:
#   <out-dir>/output/single/  the single job's run directory, inputs removed
#   <out-dir>/output/batch/   the batch job's run directory, inputs removed
#   <out-dir>/logs/           each job's pullauta.ini and log
# Compare two runs with `pullauta eval <a>/output <b>/output`; nothing but output
# is left in output/, so eval needs no --ignore.
#
# Inputs are read from $REGRESSION_DATA (test_file.laz and test_file.shp.zip),
# default <source-dir>/target/regression-data, and downloaded there when missing.
set -euo pipefail

if [ $# -ne 3 ]; then
    echo "usage: $0 <pullauta> <source-dir> <out-dir>" >&2
    exit 1
fi
pullauta=$(realpath "$1")
src=$(realpath "$2")
out=$3
data=${REGRESSION_DATA:-$src/target/regression-data}

# job settings: key=value, or new|old=value to set whichever name the build's
# default ini has (for a branch that renames a key; the rebase commit drops old)
single_settings=()
batch_settings=(
    batch=1
    vectorconf=osm.txt
    vector_vege=1
    batchmerge=1
    savetempfolders=1
)

download() { # file url
    if [ ! -f "$data/$1" ]; then
        mkdir -p "$data"
        curl -Lf --retry 3 -o "$data/$1.part" "$2"
        mv "$data/$1.part" "$data/$1"
    fi
}
download test_file.laz https://cdn.routechoic.es/test.laz
download test_file.shp.zip https://cdn.routechoic.es/test-osm.shp.zip

# write the build's default ini with the given settings into ./pullauta.ini
write_ini() {
    cp "$src/pullauta.default.ini" pullauta.ini
    local setting names value name found alternatives
    for setting in "$@"; do
        names=${setting%%=*}
        value=${setting#*=}
        found=
        IFS='|' read -ra alternatives <<<"$names"
        for name in "${alternatives[@]}"; do
            if grep -q "^$name *=" pullauta.ini; then
                awk -v k="$name" -v v="$value" \
                    '$0 ~ "^" k " *=" { print k "=" v; next } { print }' \
                    pullauta.ini >pullauta.ini.new
                mv pullauta.ini.new pullauta.ini
                found=1
                break
            fi
        done
        if [ -z "$found" ]; then
            echo "run.sh: $names is not in $src/pullauta.default.ini" >&2
            exit 1
        fi
    done
}

# run pullauta with the given arguments in the current directory, logging to $log
run() {
    echo "\$ pullauta $*" >>"$log"
    "$pullauta" "$@" >>"$log" 2>&1
}

rm -rf "$out"
mkdir -p "$out/output/single" "$out/output/batch/in" "$out/output/batch/out" "$out/logs"
out=$(realpath "$out")

echo "single job ($out/logs/single.log)"
cd "$out/output/single"
log=$out/logs/single.log
write_ini ${single_settings[@]+"${single_settings[@]}"}
cp "$data/test_file.laz" .
SECONDS=0
run test_file.laz
echo "single job: ${SECONDS} s"
rm test_file.laz
mv pullauta.ini "$out/logs/single.ini"

echo "batch job ($out/logs/batch.log)"
cd "$out/output/batch"
log=$out/logs/batch.log
write_ini "${batch_settings[@]}"
cp "$src/osm.txt" .
cp "$data/test_file.laz" "$data/test_file.shp.zip" in/
SECONDS=0
run
run pngmerge 1
run pngmergedepr 1
echo "batch job: ${SECONDS} s"
rm in/test_file.laz in/test_file.shp.zip osm.txt
mv pullauta.ini "$out/logs/batch.ini"
