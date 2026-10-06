use fs::FileSystem;

use crate::geometry::BinaryDxf;

pub mod bytes;
pub mod fs;
pub mod heightmap;
pub mod xyz;

/// Helper for converting a binary DXF file to a regular DXF file.
pub fn bin2dxf(fs: &impl FileSystem, input: &str, output: &str) -> anyhow::Result<()> {
    let binary = BinaryDxf::from_reader(&mut fs.open(input)?)?;
    binary.to_dxf(&mut fs.create(output)?)?;
    Ok(())
}
