use image::{DynamicImage, Rgb, RgbImage, Rgba, RgbaImage};
use imageproc::drawing::draw_filled_rect_mut;
use imageproc::filter::median_filter;
use imageproc::rect::Rect;
use log::info;
use std::{error::Error, path::Path};

use crate::config::Config;
use crate::io::{
    bytes::FromToBytes,
    fs::FileSystem,
    heightmap::HeightMap,
    xyz::{LasClass, XyzRecord},
};

/// Draws the `returns` [`is_block`] picks as blocks; `config.water_class` names the water class.
pub fn blocks(
    fs: &impl FileSystem,
    config: &Config,
    tmpfolder: &Path,
    returns: &[XyzRecord],
) -> Result<(), Box<dyn Error>> {
    info!("Identifying blocks...");

    let heightmap_in = tmpfolder.join("xyz2.hmap");
    let hmap = HeightMap::from_bytes(&mut fs.open(heightmap_in)?)?;

    let xstartxyz = hmap.xoffset;
    let ystartxyz = hmap.yoffset;
    let size = hmap.scale;

    let xmax = hmap.grid.width() - 1;
    let ymax = hmap.grid.height() - 1;

    let mut img = RgbImage::from_pixel(xmax as u32 * 2, ymax as u32 * 2, Rgb([255, 255, 255]));
    let mut img2 = RgbaImage::from_pixel(xmax as u32 * 2, ymax as u32 * 2, Rgba([0, 0, 0, 0]));

    let black = Rgb([0, 0, 0]);
    let white = Rgba([255, 255, 255, 255]);

    for r in returns {
        let (x, y) = (r.x, r.y);

        let xx = ((x - xstartxyz) / size).floor() as usize;
        let yy = ((y - ystartxyz) / size).floor() as usize;
        let ground = hmap.grid.get((xx, yy)).copied().unwrap_or(0.0);
        if is_block(r, config.water_class, ground) {
            draw_filled_rect_mut(
                &mut img,
                Rect::at(
                    (x - xstartxyz - 1.0) as i32,
                    (ystartxyz + 2.0 * ymax as f64 - y - 1.0) as i32,
                )
                .of_size(3, 3),
                black,
            );
        } else {
            draw_filled_rect_mut(
                &mut img2,
                Rect::at(
                    (x - xstartxyz - 1.0) as i32,
                    (ystartxyz + 2.0 * ymax as f64 - y - 1.0) as i32,
                )
                .of_size(3, 3),
                white,
            );
        }
    }

    img2.write_to(
        &mut fs
            .create(tmpfolder.join("blocks2.png"))
            .expect("error saving png"),
        image::ImageFormat::Png,
    )
    .expect("error saving png");

    let mut img = DynamicImage::ImageRgb8(img);

    image::imageops::overlay(&mut img, &DynamicImage::ImageRgba8(img2), 0, 0);

    let filter_size = 2;
    img = image::DynamicImage::ImageRgb8(median_filter(&img.to_rgb8(), filter_size, filter_size));

    img.write_to(
        &mut fs
            .create(tmpfolder.join("blocks.png"))
            .expect("error saving png"),
        image::ImageFormat::Png,
    )
    .expect("error saving png");
    info!("Done");
    Ok(())
}

/// Whether a return is drawn as a block: neither ground nor of `water_class`, the only echo
/// of its pulse, and more than 2 m above `ground`, the ground model's height under it.
fn is_block(r: &XyzRecord, water_class: u8, ground: f64) -> bool {
    r.class() != LasClass::Ground
        && r.classification != water_class
        && r.number_of_returns == 1
        && r.return_number == 1
        && r.z as f64 - ground > 2.0
}

#[cfg(test)]
mod test {
    use super::*;

    fn single_echo(classification: u8) -> XyzRecord {
        XyzRecord {
            z: 10.0,
            classification,
            number_of_returns: 1,
            return_number: 1,
            ..Default::default()
        }
    }

    #[test]
    fn blocks_exclude_ground_and_the_configured_water_class() {
        assert!(!is_block(&single_echo(2), 9, 0.0));
        assert!(!is_block(&single_echo(9), 9, 0.0));
        assert!(is_block(&single_echo(9), 42, 0.0));
        assert!(!is_block(&single_echo(42), 42, 0.0));
        assert!(is_block(&single_echo(6), 9, 0.0));
        assert!(!is_block(&single_echo(6), 9, 8.0));
    }
}
