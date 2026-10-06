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
#           vectorconf=osm.txt, every product family (outputs) and batchmerge=1, then
#           `pullauta pngmerge 1` and `pullauta pngmergedepr 1` (full-scale merges;
#           they replace batchmerge's 4x ones)
# Both jobs leave only their products (debug_intermediates=0): the gate compares the
# rendered maps and the vector output, not the debug intermediates (ADR 0004).
#
# Layout:
#   <out-dir>/output/single/  the single job's run directory, inputs removed
#   <out-dir>/output/batch/   the batch job's run directory, inputs removed
#   <out-dir>/logs/           each job's pullauta.ini and log, and times.txt
# <out-dir> is replaced; the script refuses one that is or contains the checkout,
# or an existing non-empty directory without this layout.
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

# refuse to replace anything but an earlier run (or an empty directory)
if [ -z "$out" ] || [ "$out" = / ] || [ "$out" = . ]; then
    echo "run.sh: refusing out-dir '$out'" >&2
    exit 1
fi
if [ -e "$out" ]; then
    abs=$(cd "$out" && pwd -P)
    prefix=${abs%/}/
    case "$src/" in
    "$prefix"*)
        echo "run.sh: out-dir $out is or contains $src" >&2
        exit 1
        ;;
    esac
    if [ -n "$(ls -A "$abs")" ] && { [ ! -d "$abs/output" ] || [ ! -d "$abs/logs" ]; }; then
        echo "run.sh: $out is not empty and holds no earlier run (output/ and logs/)" >&2
        exit 1
    fi
fi

# job settings: key=value, or alternatives separated by | to set the first name the
# build's default ini has: new|old=value (a renamed key, same value) or
# new=value|old=value (a replaced key with its own value). The rebase commit drops old.
# A key the default ini only has commented out (`#key=...`, unset for a computed
# default) is set on that line.
single_settings=()
batch_settings=(
    batch=1
    vectorconf=osm.txt
    'outputs=raster,dxf,geojson'
    batchmerge=1
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

# split a setting into its |-separated alternatives in the array `alternatives`; a |
# starts an alternative only before `name=`, or before a bare `name` while no value has
# started, so a value's own pipes (vector_greenshade_isom=406.000|408.000) stay in it
split_alternatives() {
    local part parts seen_value=
    alternatives=()
    IFS='|' read -ra parts <<<"$1"
    for part in "${parts[@]}"; do
        if [[ $part =~ ^[A-Za-z_][A-Za-z0-9_{}]*= ]] ||
            { [ -z "$seen_value" ] && [[ $part =~ ^[A-Za-z_][A-Za-z0-9_{}]*$ ]]; } ||
            [ ${#alternatives[@]} -eq 0 ]; then
            alternatives+=("$part")
        else
            alternatives[${#alternatives[@]}-1]+="|$part"
        fi
        case $part in *=*) seen_value=1 ;; esac
    done
}

# write the build's default ini with the given settings into ./pullauta.ini
write_ini() {
    cp "$src/pullauta.default.ini" pullauta.ini
    local setting value name found alternative last
    for setting in "$@"; do
        found=
        split_alternatives "$setting"
        last=${alternatives[${#alternatives[@]}-1]}
        for alternative in "${alternatives[@]}"; do
            name=${alternative%%=*}
            case $alternative in
            *=*) value=${alternative#*=} ;;
            *) value=${last#*=} ;; # new|old=value: the last one's value
            esac
            # the key's line, or, when the template only has it commented out
            # (`#name=value`: unset means a computed default), its first commented line
            if grep -q "^$name *=" pullauta.ini; then
                awk -v k="$name" -v v="$value" \
                    '$0 ~ "^" k " *=" { print k "=" v; next } { print }' \
                    pullauta.ini >pullauta.ini.new
            elif grep -q "^# *$name *=" pullauta.ini; then
                awk -v k="$name" -v v="$value" \
                    '!done && $0 ~ "^# *" k " *=" { print k "=" v; done = 1; next } { print }' \
                    pullauta.ini >pullauta.ini.new
            else
                continue
            fi
            mv pullauta.ini.new pullauta.ini
            found=1
            break
        done
        if [ -z "$found" ]; then
            echo "run.sh: none of $setting is in $src/pullauta.default.ini" >&2
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
echo "single job: ${SECONDS} s" | tee -a "$out/logs/times.txt"
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
echo "batch job: ${SECONDS} s" | tee -a "$out/logs/times.txt"
rm in/test_file.laz in/test_file.shp.zip osm.txt
mv pullauta.ini "$out/logs/batch.ini"
