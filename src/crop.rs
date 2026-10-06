use std::path::Path;

use crate::geometry::{BinaryDxf, Geometry, Points, Polylines};
use crate::io::fs::FileSystem;

/// Crop the lines that fall outside the bounds by cutting existing lines.
#[allow(clippy::too_many_arguments)]
pub fn polylinebindxfcrop(
    fs: &impl FileSystem,
    input: &Path,
    output: &Path,
    output_dxf: bool,
    minx: f64,
    miny: f64,
    maxx: f64,
    maxy: f64,
) -> anyhow::Result<()> {
    log::debug!("Cropping polylines in binary DXF file: {input:?} to {output:?}");

    let input = BinaryDxf::from_reader(&mut fs.open(input)?)?;
    let out = crop_polylines(input, minx, miny, maxx, maxy)?;
    write_crop(fs, &out, output, output_dxf)
}

/// The lines of `input` (2D or 3D) cut to the bounds, as [`polylinebindxfcrop`] crops a
/// file; the bounds of `input` are kept.
pub fn crop_polylines(
    input: BinaryDxf,
    minx: f64,
    miny: f64,
    maxx: f64,
    maxy: f64,
) -> anyhow::Result<BinaryDxf> {
    let bounds = input.bounds().clone();

    let output_lines = match input.take_geometry().swap_remove(0) {
        Geometry::Polylines2(polylines) => {
            crop_lines(polylines, minx, miny, maxx, maxy, |p| (p.x, p.y)).into()
        }
        Geometry::Polylines3(polylines) => {
            crop_lines(polylines, minx, miny, maxx, maxy, |p| (p.x, p.y)).into()
        }
        _ => anyhow::bail!("input file should contain 2D or 3D lines"),
    };

    // (TODO: should we populate the new bounds here or keep the old?)
    Ok(BinaryDxf::new(bounds, vec![output_lines]))
}

/// Write a crop to `output` (a `.dxf.bin` name) and, with `output_dxf`, as text DXF next
/// to it.
pub fn write_crop(
    fs: &impl FileSystem,
    crop: &BinaryDxf,
    output: &Path,
    output_dxf: bool,
) -> anyhow::Result<()> {
    crop.to_writer(&mut fs.create(output)?)?;

    if output_dxf {
        // remove the .bin extension for the DXF output
        crop.to_dxf(&mut fs.create(output.with_extension(""))?)?;
    }

    Ok(())
}

/// Generic inner logic to work with any point type and Classification. Only need to provide an
/// extractor function that will get the x & y components (which is what we are cropping)
fn crop_lines<P: Clone, C: Copy>(
    input_lines: Polylines<P, C>,
    minx: f64,
    miny: f64,
    maxx: f64,
    maxy: f64,
    xy_fn: impl Fn(&P) -> (f64, f64),
) -> Polylines<P, C> {
    let mut output_lines = Polylines::<_, _>::new();

    for (p, c) in input_lines.into_iter() {
        let mut pre = None;
        let mut prex = 0.0;
        let mut prey = 0.0;
        let mut pointcount = 0;
        let mut poly = Vec::with_capacity(p.len());
        for point in p {
            let (valx, valy) = xy_fn(&point);
            if valx >= minx && valx <= maxx && valy >= miny && valy <= maxy {
                if let Some(pre) = pre
                    && pointcount == 0
                    && (prex < minx || prey < miny)
                {
                    poly.push(pre);
                    pointcount += 1;
                }
                poly.push(point.clone());
                pointcount += 1;
            } else if pointcount > 1 {
                if valx < minx || valy < miny {
                    poly.push(point.clone());
                }

                output_lines.push(poly, c);
                poly = Vec::new();
                pointcount = 0;
            }
            pre = Some(point);
            prex = valx;
            prey = valy;
        }
        if pointcount > 1 {
            output_lines.push(poly, c);
        }
    }
    output_lines
}

/// Removes points that fall outside the provided bounds and writes the remaining points to the
/// output file.
#[allow(clippy::too_many_arguments)]
pub fn pointbindxfcrop(
    fs: &impl FileSystem,
    input: &Path,
    output: &Path,
    output_dxf: bool,
    minx: f64,
    miny: f64,
    maxx: f64,
    maxy: f64,
) -> anyhow::Result<()> {
    log::debug!("Cropping points in binary DXF file: {input:?} to {output:?}");
    let input = BinaryDxf::from_reader(&mut fs.open(input)?)?;
    let out = crop_points(input, minx, miny, maxx, maxy)?;
    write_crop(fs, &out, output, output_dxf)
}

/// The points of `input` inside the bounds, as [`pointbindxfcrop`] crops a file; the
/// bounds of `input` are kept.
pub fn crop_points(
    input: BinaryDxf,
    minx: f64,
    miny: f64,
    maxx: f64,
    maxy: f64,
) -> anyhow::Result<BinaryDxf> {
    let bounds = input.bounds().clone();
    let Geometry::Points(points) = input.take_geometry().swap_remove(0) else {
        anyhow::bail!("input file should contain points");
    };

    // filter all the points
    let mut output_points = Points::with_capacity(points.len());
    for (p, c) in points.into_iter() {
        if p.x >= minx && p.x <= maxx && p.y >= miny && p.y <= maxy {
            output_points.push(p, c);
        }
    }

    // (TODO: should we populate the new bounds here or keep the old?)
    Ok(BinaryDxf::new(bounds, vec![output_points.into()]))
}
