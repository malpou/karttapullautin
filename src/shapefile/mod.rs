use std::{error::Error, path::Path};

use log::info;
use std::path::PathBuf;

use crate::{config::Config, io::fs::FileSystem, render::ShapeLayers, vegetation::VegetationFrame};

mod canvas;
mod mapping;
mod render;

pub use render::{read_vegetation_frame, render, vector_tables};

/// Unzips the shape files into `tmpfolder` and draws them as [`render`] does.
pub fn unzip_and_render(
    fs: &impl FileSystem,
    config: &Config,
    tmpfolder: &Path,
    filenames: &[String],
    frame: Option<VegetationFrame>,
    debug: bool,
) -> Result<Option<ShapeLayers>, Box<dyn Error>> {
    for zip_name in filenames.iter() {
        info!("Opening zip file {zip_name}");
        fs.extract_zip(zip_name, tmpfolder)?;
    }
    render::render(fs, config, tmpfolder, frame, false, debug)
}

/// Unzips the shape files to specific folder
pub fn unzip_shapefiles(fs: &impl FileSystem, filenames: &[String]) -> Result<(), Box<dyn Error>> {
    let tmpfolder = PathBuf::from("temp_shapefiles".to_string());
    for zip_name in filenames.iter() {
        info!("Opening zip file {zip_name}");
        fs.extract_zip(zip_name, &tmpfolder)?;
    }
    Ok(())
}
