//! A forward-only ZIP writer.
//!
//! The `zip` crate cannot be used here: it seeks backwards to patch each local
//! file header once the size and CRC are known (`write.rs` seeks to
//! `header_start + CRC32_OFFSET`), which a network socket cannot do. Buffering
//! the archive to a temp file first would mean writing every shared byte to
//! disk before the phone receives any of it.
//!
//! The format has a purpose-built answer: general-purpose flag bit 3 says "the
//! sizes and CRC follow the data in a data descriptor", so nothing has to be
//! known up front and nothing is ever rewritten. Everything here is written
//! once, in order.
//!
//! Compression is always Stored. Shared media (JPEG, MP4, MP3) is already
//! compressed, so deflate costs CPU for roughly no size gain.

use std::io::{Read, Result as IoResult, Write};

const SIG_LOCAL: u32 = 0x0403_4b50;
const SIG_DESCRIPTOR: u32 = 0x0807_4b50;
const SIG_CENTRAL: u32 = 0x0201_4b50;
const SIG_EOCD64: u32 = 0x0606_4b50;
const SIG_EOCD64_LOCATOR: u32 = 0x0706_4b50;
const SIG_EOCD: u32 = 0x0605_4b50;

/// Bit 3: sizes and CRC live in a trailing data descriptor.
/// Bit 11: the filename is UTF-8, not the legacy OEM code page.
const FLAG_DESCRIPTOR: u16 = 1 << 3;
const FLAG_UTF8: u16 = 1 << 11;

const METHOD_STORED: u16 = 0;
const VERSION_BASE: u16 = 20;
const VERSION_ZIP64: u16 = 45;

/// The value a 32-bit field carries when the real value lives in a Zip64 extra
/// field instead.
const U32_SENTINEL: u32 = 0xFFFF_FFFF;

/// Entries at or above this switch to Zip64. Kept just below the true limit so
/// a file that grows slightly while being read cannot overflow the field we
/// already committed to in its local header.
const ZIP64_THRESHOLD: u64 = 0xFFFF_F000;

struct CentralEntry {
    name: String,
    crc: u32,
    size: u64,
    offset: u64,
    dos_time: u16,
    dos_date: u16,
}

pub(crate) struct ZipStream<W: Write> {
    out: W,
    offset: u64,
    entries: Vec<CentralEntry>,
}

impl<W: Write> ZipStream<W> {
    pub(crate) fn new(out: W) -> Self {
        Self {
            out,
            offset: 0,
            entries: Vec::new(),
        }
    }

    fn put(&mut self, bytes: &[u8]) -> IoResult<()> {
        self.out.write_all(bytes)?;
        self.offset += bytes.len() as u64;
        Ok(())
    }

    fn put_u16(&mut self, value: u16) -> IoResult<()> {
        self.put(&value.to_le_bytes())
    }
    fn put_u32(&mut self, value: u32) -> IoResult<()> {
        self.put(&value.to_le_bytes())
    }
    fn put_u64(&mut self, value: u64) -> IoResult<()> {
        self.put(&value.to_le_bytes())
    }

    /// Stream one file into the archive.
    ///
    /// `declared_size` comes from the directory entry and decides the Zip64
    /// choice; the actual byte count is whatever the reader yields, and that is
    /// what the data descriptor reports.
    pub(crate) fn write_file<R: Read>(
        &mut self,
        name: &str,
        declared_size: u64,
        mtime_ms: u64,
        reader: &mut R,
    ) -> IoResult<()> {
        let zip64 = declared_size >= ZIP64_THRESHOLD || self.offset >= ZIP64_THRESHOLD;
        let (dos_time, dos_date) = dos_datetime(mtime_ms);
        let offset = self.offset;
        let name_bytes = name.as_bytes().to_vec();

        // ---- local file header ----
        self.put_u32(SIG_LOCAL)?;
        self.put_u16(if zip64 { VERSION_ZIP64 } else { VERSION_BASE })?;
        self.put_u16(FLAG_DESCRIPTOR | FLAG_UTF8)?;
        self.put_u16(METHOD_STORED)?;
        self.put_u16(dos_time)?;
        self.put_u16(dos_date)?;
        // CRC and both sizes are unknown until the data has been streamed;
        // bit 3 is what makes writing zeros here legal.
        self.put_u32(0)?;
        self.put_u32(if zip64 { U32_SENTINEL } else { 0 })?;
        self.put_u32(if zip64 { U32_SENTINEL } else { 0 })?;
        self.put_u16(name_bytes.len() as u16)?;
        // Zip64 extended information: 2-byte id, 2-byte length, then the two
        // 8-bit sizes -- still zero here, real values go in the descriptor.
        self.put_u16(if zip64 { 20 } else { 0 })?;
        self.put(&name_bytes)?;
        if zip64 {
            self.put_u16(0x0001)?;
            self.put_u16(16)?;
            self.put_u64(0)?;
            self.put_u64(0)?;
        }

        // ---- file data ----
        let mut crc = crc32fast::Hasher::new();
        let mut written = 0u64;
        // 64 KiB: large enough that the per-write channel handoff is amortised,
        // small enough that a stalled client is not holding megabytes.
        let mut buffer = vec![0u8; 64 * 1024];
        // A non-Zip64 entry cannot describe more than 4 GiB, so stop there
        // rather than emit a descriptor that silently wraps.
        let cap = if zip64 { u64::MAX } else { U32_SENTINEL as u64 };

        loop {
            let want = if written >= cap {
                break;
            } else {
                buffer.len().min((cap - written) as usize)
            };
            let read = reader.read(&mut buffer[..want])?;
            if read == 0 {
                break;
            }
            crc.update(&buffer[..read]);
            self.put(&buffer[..read])?;
            written += read as u64;
        }
        let crc = crc.finalize();

        // ---- data descriptor ----
        self.put_u32(SIG_DESCRIPTOR)?;
        self.put_u32(crc)?;
        if zip64 {
            self.put_u64(written)?;
            self.put_u64(written)?;
        } else {
            self.put_u32(written as u32)?;
            self.put_u32(written as u32)?;
        }

        self.entries.push(CentralEntry {
            name: name.to_string(),
            crc,
            size: written,
            offset,
            dos_time,
            dos_date,
        });
        Ok(())
    }

    /// Write the central directory and the end-of-central-directory records.
    pub(crate) fn finish(mut self) -> IoResult<W> {
        let central_start = self.offset;

        let entries = std::mem::take(&mut self.entries);
        for entry in &entries {
            let big_size = entry.size >= ZIP64_THRESHOLD;
            let big_offset = entry.offset >= ZIP64_THRESHOLD;
            let zip64 = big_size || big_offset;

            // Zip64 extra field carries only the fields that actually
            // overflowed, in a fixed order: sizes first, then the offset.
            let mut extra: Vec<u8> = Vec::new();
            if zip64 {
                let mut payload: Vec<u8> = Vec::new();
                if big_size {
                    payload.extend_from_slice(&entry.size.to_le_bytes());
                    payload.extend_from_slice(&entry.size.to_le_bytes());
                }
                if big_offset {
                    payload.extend_from_slice(&entry.offset.to_le_bytes());
                }
                extra.extend_from_slice(&0x0001u16.to_le_bytes());
                extra.extend_from_slice(&(payload.len() as u16).to_le_bytes());
                extra.extend_from_slice(&payload);
            }

            let name_bytes = entry.name.as_bytes().to_vec();

            self.put_u32(SIG_CENTRAL)?;
            // Version made by: 0x03 (Unix) in the high byte would imply Unix
            // permissions in the external attributes; 0 (MS-DOS/FAT) matches
            // the zero attributes we write.
            self.put_u16(if zip64 { VERSION_ZIP64 } else { VERSION_BASE })?;
            self.put_u16(if zip64 { VERSION_ZIP64 } else { VERSION_BASE })?;
            self.put_u16(FLAG_DESCRIPTOR | FLAG_UTF8)?;
            self.put_u16(METHOD_STORED)?;
            self.put_u16(entry.dos_time)?;
            self.put_u16(entry.dos_date)?;
            self.put_u32(entry.crc)?;
            self.put_u32(if big_size { U32_SENTINEL } else { entry.size as u32 })?;
            self.put_u32(if big_size { U32_SENTINEL } else { entry.size as u32 })?;
            self.put_u16(name_bytes.len() as u16)?;
            self.put_u16(extra.len() as u16)?;
            self.put_u16(0)?; // comment length
            self.put_u16(0)?; // disk number start
            self.put_u16(0)?; // internal attributes
            self.put_u32(0)?; // external attributes
            self.put_u32(if big_offset {
                U32_SENTINEL
            } else {
                entry.offset as u32
            })?;
            self.put(&name_bytes)?;
            self.put(&extra)?;
        }

        let central_size = self.offset - central_start;
        let count = entries.len() as u64;
        let need_eocd64 =
            count > u16::MAX as u64 || central_start >= ZIP64_THRESHOLD || central_size >= ZIP64_THRESHOLD;

        if need_eocd64 {
            let eocd64_offset = self.offset;
            self.put_u32(SIG_EOCD64)?;
            // Size of this record MINUS its own 12-byte signature+size fields.
            self.put_u64(44)?;
            self.put_u16(VERSION_ZIP64)?;
            self.put_u16(VERSION_ZIP64)?;
            self.put_u32(0)?; // this disk
            self.put_u32(0)?; // disk with central directory
            self.put_u64(count)?;
            self.put_u64(count)?;
            self.put_u64(central_size)?;
            self.put_u64(central_start)?;

            self.put_u32(SIG_EOCD64_LOCATOR)?;
            self.put_u32(0)?;
            self.put_u64(eocd64_offset)?;
            self.put_u32(1)?; // total disks
        }

        self.put_u32(SIG_EOCD)?;
        self.put_u16(0)?;
        self.put_u16(0)?;
        let count16 = if count > u16::MAX as u64 {
            u16::MAX
        } else {
            count as u16
        };
        self.put_u16(count16)?;
        self.put_u16(count16)?;
        self.put_u32(if central_size >= ZIP64_THRESHOLD {
            U32_SENTINEL
        } else {
            central_size as u32
        })?;
        self.put_u32(if central_start >= ZIP64_THRESHOLD {
            U32_SENTINEL
        } else {
            central_start as u32
        })?;
        self.put_u16(0)?; // comment length

        self.out.flush()?;
        Ok(self.out)
    }
}

/// Convert epoch milliseconds to the MS-DOS (time, date) pair the format uses.
///
/// The DOS epoch is 1980 and its seconds field has 2-second resolution.
/// Anything before 1980 clamps to 1980-01-01, which is what every other
/// implementation does with such timestamps.
fn dos_datetime(mtime_ms: u64) -> (u16, u16) {
    let secs = (mtime_ms / 1000) as i64;
    let days = secs.div_euclid(86_400);
    let secs_of_day = secs.rem_euclid(86_400);

    let (year, month, day) = civil_from_days(days);
    if year < 1980 {
        // 1980-01-01 00:00:00
        return (0, (1 << 5) | 1);
    }

    let time = (((secs_of_day / 3600) as u16) << 11)
        | ((((secs_of_day % 3600) / 60) as u16) << 5)
        | (((secs_of_day % 60) / 2) as u16);
    let date = (((year - 1980) as u16) << 9) | ((month as u16) << 5) | (day as u16);
    (time, date)
}

/// Howard Hinnant's days-from-civil, inverted. Avoids a date-library
/// dependency for the one place a calendar date is needed.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}
