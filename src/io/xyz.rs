use crate::io::bytes::FromToBytes;
use std::{
    io::{Read, Seek, Write},
    time::Instant,
};

use log::debug;

/// The magic number of the XYZ binary format, version 2: the fourth byte is the version.
/// Version 2 gave the record's last byte a meaning ([`XyzRecord::flags`]); version 1 files
/// (`XYZB`) are rejected, since they lost the flags at ingest.
const XYZ_MAGIC: &[u8; 4] = b"XYZ2";

/// The ASPRS classification of a return (LAS 1.4 R15, table 17), as the data supplier set
/// it. Codes without a name here (reserved, and 64-255 user definable) are `Other`.
///
/// Convert with `From`: `LasClass::from(code)` never yields `Other` for a named code, so
/// the conversion round-trips every `u8`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LasClass {
    CreatedNeverClassified,
    Unclassified,
    Ground,
    LowVegetation,
    MediumVegetation,
    HighVegetation,
    Building,
    /// Class 7, low point: a noise return below the ground.
    LowNoise,
    Water,
    Rail,
    RoadSurface,
    WireGuard,
    WireConductor,
    TransmissionTower,
    WireStructureConnector,
    BridgeDeck,
    /// Class 18, a noise return high above the ground.
    HighNoise,
    Other(u8),
}

impl From<u8> for LasClass {
    fn from(code: u8) -> Self {
        match code {
            0 => Self::CreatedNeverClassified,
            1 => Self::Unclassified,
            2 => Self::Ground,
            3 => Self::LowVegetation,
            4 => Self::MediumVegetation,
            5 => Self::HighVegetation,
            6 => Self::Building,
            7 => Self::LowNoise,
            9 => Self::Water,
            10 => Self::Rail,
            11 => Self::RoadSurface,
            13 => Self::WireGuard,
            14 => Self::WireConductor,
            15 => Self::TransmissionTower,
            16 => Self::WireStructureConnector,
            17 => Self::BridgeDeck,
            18 => Self::HighNoise,
            other => Self::Other(other),
        }
    }
}

impl From<LasClass> for u8 {
    fn from(class: LasClass) -> Self {
        match class {
            LasClass::CreatedNeverClassified => 0,
            LasClass::Unclassified => 1,
            LasClass::Ground => 2,
            LasClass::LowVegetation => 3,
            LasClass::MediumVegetation => 4,
            LasClass::HighVegetation => 5,
            LasClass::Building => 6,
            LasClass::LowNoise => 7,
            LasClass::Water => 9,
            LasClass::Rail => 10,
            LasClass::RoadSurface => 11,
            LasClass::WireGuard => 13,
            LasClass::WireConductor => 14,
            LasClass::TransmissionTower => 15,
            LasClass::WireStructureConnector => 16,
            LasClass::BridgeDeck => 17,
            LasClass::HighNoise => 18,
            LasClass::Other(code) => code,
        }
    }
}

/// A single record of an observed laser data point needed by the algorithms.
#[derive(Debug, Clone, Copy, Default, PartialEq, bytemuck::NoUninit, bytemuck::AnyBitPattern)]
#[repr(C)]
pub struct XyzRecord {
    pub x: f64,
    pub y: f64,
    pub z: f32,
    /// The raw LAS classification code; [`XyzRecord::class`] names it.
    pub classification: u8,
    pub number_of_returns: u8,
    pub return_number: u8,
    /// The LAS classification flags: [`XyzRecord::WITHHELD`], [`XyzRecord::SYNTHETIC`]
    /// and [`XyzRecord::OVERLAP`]. Also keeps the struct exactly 24 bytes long.
    pub flags: u8,
}

impl XyzRecord {
    /// Flag bit: the return is withheld (deleted) and should not be used.
    pub const WITHHELD: u8 = 1 << 0;
    /// Flag bit: the return was created by other means than the laser scan.
    pub const SYNTHETIC: u8 = 1 << 1;
    /// Flag bit: the return lies in the overlap of two or more swaths.
    pub const OVERLAP: u8 = 1 << 2;

    /// The flags byte for the given LAS classification flags.
    pub fn pack_flags(withheld: bool, synthetic: bool, overlap: bool) -> u8 {
        (withheld as u8 * Self::WITHHELD)
            | (synthetic as u8 * Self::SYNTHETIC)
            | (overlap as u8 * Self::OVERLAP)
    }

    /// The ASPRS class of this return.
    pub fn class(&self) -> LasClass {
        self.classification.into()
    }

    pub fn is_withheld(&self) -> bool {
        self.flags & Self::WITHHELD != 0
    }

    pub fn is_synthetic(&self) -> bool {
        self.flags & Self::SYNTHETIC != 0
    }

    pub fn is_overlap(&self) -> bool {
        self.flags & Self::OVERLAP != 0
    }
}

pub struct XyzInternalWriter<W: Write + Seek> {
    inner: Option<W>,
    records_written: u64,
    // for stats
    start: Option<Instant>,
}

impl<W: Write + Seek> XyzInternalWriter<W> {
    pub fn new(inner: W) -> Self {
        Self {
            inner: Some(inner),
            records_written: 0,
            start: None,
        }
    }

    pub fn write_records(&mut self, records: &[XyzRecord]) -> std::io::Result<()> {
        let inner = self
            .inner
            .as_mut()
            .ok_or_else(|| std::io::Error::other("writer has already been finished"))?;

        if records.is_empty() {
            return Ok(()); // nothing to write
        }

        // write the header (format + length) on the first write
        if self.records_written == 0 {
            self.start = Some(Instant::now());

            inner.write_all(XYZ_MAGIC)?;
            // Write the temporary number of records as all FF
            u64::MAX.to_bytes(inner)?;
        }

        let bytes: &[u8] = bytemuck::cast_slice(records);
        inner.write_all(bytes)?;

        self.records_written += records.len() as u64;
        Ok(())
    }

    pub fn finish(&mut self) -> std::io::Result<W> {
        let mut inner = self
            .inner
            .take()
            .ok_or_else(|| std::io::Error::other("writer has already been finished"))?;

        // seek to the beginning of the file and write the number of records
        inner.seek(std::io::SeekFrom::Start(XYZ_MAGIC.len() as u64))?;
        self.records_written.to_bytes(&mut inner)?;

        // log statistics about the written records
        if let Some(start) = self.start {
            let elapsed = start.elapsed();
            debug!(
                "Wrote {} records in {:.2?} ({:.2?}/record, {:.3}M records/s, {:.2}MB/s)",
                self.records_written,
                elapsed,
                elapsed / self.records_written as u32,
                self.records_written as f64 / (10e6 * elapsed.as_secs_f64()),
                self.records_written as f64 * size_of::<XyzRecord>() as f64
                    / (1024.0 * 1024.0 * elapsed.as_secs_f64()),
            );
        }
        Ok(inner)
    }
}

impl<W: Write + Seek> Drop for XyzInternalWriter<W> {
    fn drop(&mut self) {
        if self.inner.is_some() {
            self.finish().expect("failed to finish writer in Drop");
        }
    }
}

pub struct XyzInternalReader<R: Read> {
    inner: R,
    n_records: u64,
    records_read: u64,
    // for stats
    start: Option<Instant>,
    buffer: [XyzRecord; 1024],
}

impl<R: Read> XyzInternalReader<R> {
    pub fn new(mut inner: R) -> std::io::Result<Self> {
        // read and check the magic number
        let mut buff = [0; XYZ_MAGIC.len()];
        inner.read_exact(&mut buff)?;
        if &buff == b"XYZB" {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                ".xyz.bin version 1 was written by an older build; regenerate it from the LAS/LAZ file",
            ));
        }
        if &buff != XYZ_MAGIC {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("not an .xyz.bin file: magic {buff:?}, expected {XYZ_MAGIC:?}"),
            ));
        }

        // read the number of records, defined by the first u64
        let n_records = u64::from_bytes(&mut inner)?;
        Ok(Self {
            inner,
            n_records,
            records_read: 0,
            start: None,
            buffer: [XyzRecord::default(); 1024],
        })
    }

    pub fn next_chunk(&mut self) -> std::io::Result<Option<&[XyzRecord]>> {
        if self.records_read >= self.n_records {
            // TODO: log statistics about the read records
            if let Some(start) = self.start {
                let elapsed = start.elapsed();
                debug!(
                    "Read {} records in {:.2?} ({:.2?}/record, {:.3}M records/s, {:.2}MB/s)",
                    self.records_read,
                    elapsed,
                    elapsed / self.records_read as u32,
                    self.records_read as f64 / (10e6 * elapsed.as_secs_f64()),
                    self.records_read as f64 * size_of::<XyzRecord>() as f64
                        / (1024.0 * 1024.0 * elapsed.as_secs_f64()),
                );
            }

            return Ok(None);
        }

        if self.records_read == 0 {
            self.start = Some(Instant::now());
        }

        // read as many as we can fit in the buffer
        let records_left = self.n_records - self.records_read;
        let records_to_read = (self.buffer.len() as u64).min(records_left);

        // treat buffer as mutable slice of bytes
        let records_buffer = &mut self.buffer[..records_to_read as usize];
        let buffer: &mut [u8] = bytemuck::cast_slice_mut(records_buffer);
        self.inner.read_exact(buffer)?;
        self.records_read += records_to_read;

        // return reference to it
        Ok(Some(records_buffer))
    }
}

#[cfg(test)]
mod test {
    use std::io::Cursor;

    use crate::io::xyz::XyzRecord;

    use super::*;

    #[test]
    fn test_writer_reader_many() {
        let cursor = Cursor::new(Vec::new());
        let mut writer = XyzInternalWriter::new(cursor);

        let record = XyzRecord {
            x: 1.0,
            y: 2.0,
            z: 3.0,
            classification: 4,
            number_of_returns: 5,
            return_number: 6,
            flags: XyzRecord::WITHHELD | XyzRecord::OVERLAP,
        };

        writer.write_records(&[record]).unwrap();
        writer.write_records(&[record]).unwrap();
        writer.write_records(&[record]).unwrap();

        // now read the records
        let data = writer.finish().unwrap().into_inner();
        let cursor = Cursor::new(data);
        let mut reader = super::XyzInternalReader::new(cursor).unwrap();
        let chunk = reader.next_chunk().unwrap().unwrap();

        assert_eq!(chunk.len(), 3);
        assert_eq!(chunk[0], record);
        assert_eq!(chunk[1], record);
        assert_eq!(chunk[2], record);
        assert_eq!(reader.next_chunk().unwrap(), None);
    }

    #[test]
    fn record_is_24_bytes() {
        assert_eq!(size_of::<XyzRecord>(), 24);
    }

    #[test]
    fn las_class_round_trips_every_code() {
        for code in 0..=u8::MAX {
            assert_eq!(u8::from(LasClass::from(code)), code);
        }
        assert_eq!(LasClass::from(2), LasClass::Ground);
        assert_eq!(LasClass::from(7), LasClass::LowNoise);
        assert_eq!(LasClass::from(9), LasClass::Water);
        assert_eq!(LasClass::from(18), LasClass::HighNoise);
        assert_eq!(LasClass::from(12), LasClass::Other(12));
        assert_eq!(LasClass::from(64), LasClass::Other(64));
    }

    #[test]
    fn flags_pack_into_distinct_bits_and_survive_the_file() {
        for bits in 0..8u8 {
            let (w, s, o) = (bits & 1 != 0, bits & 2 != 0, bits & 4 != 0);
            let record = XyzRecord {
                flags: XyzRecord::pack_flags(w, s, o),
                ..Default::default()
            };
            let mut writer = XyzInternalWriter::new(Cursor::new(Vec::new()));
            writer.write_records(&[record]).unwrap();
            let data = writer.finish().unwrap().into_inner();
            let mut reader = XyzInternalReader::new(Cursor::new(data)).unwrap();
            let read = reader.next_chunk().unwrap().unwrap()[0];
            assert_eq!(
                (read.is_withheld(), read.is_synthetic(), read.is_overlap()),
                (w, s, o)
            );
        }
    }

    #[test]
    fn rejects_version_1_files_with_a_regenerate_hint() {
        let mut data = b"XYZB".to_vec();
        0u64.to_bytes(&mut data).unwrap();
        let err = XyzInternalReader::new(Cursor::new(data)).err().unwrap();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("older build"), "{err}");
        assert!(err.to_string().contains("regenerate"), "{err}");
    }

    #[test]
    fn rejects_an_unknown_magic() {
        let err = XyzInternalReader::new(Cursor::new(b"XYZ3\0\0\0\0\0\0\0\0".to_vec()))
            .err()
            .unwrap();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }
}
