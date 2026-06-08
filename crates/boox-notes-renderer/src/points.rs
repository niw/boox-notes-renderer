//! Parser for the custom binary `#points` stroke files.
//!
//! Layout (all integers big-endian):
//!
//! ```text
//! Header (76 bytes):
//!   version  : u32
//!   page_id  : [u8; 36]   (UTF-8 UUID, space/NUL padded)
//!   points_id: [u8; 36]
//! Body: point records, addressed by the table below.
//! Tail (last 4 bytes): u32 offset where the stroke table starts.
//! Stroke table (from that offset to end-4), each entry 44 bytes:
//!   stroke_id: [u8; 36]
//!   start_addr: u32        (byte offset of the first point)
//!   packed    : u32        (point_count = packed >> 4, flag = packed & 0xF)
//! Point record (16 bytes):
//!   timestamp_rel: u32
//!   x: f32
//!   y: f32
//!   tilt_x: i8
//!   tilt_y: i8
//!   pressure: u16          (0..=4095)
//! ```

use std::collections::HashMap;
use std::io::{self, Cursor, Read, Seek, SeekFrom};

use byteorder::{BE, ReadBytesExt};

use crate::ids;

/// A malformed-data error. Parse failures here are per-file and recoverable
/// (the caller warns and skips the file), so this module returns plain
/// `io::Error` rather than `crate::Error` — same convention as
/// `container::read_nested_zip_single`.
fn bad(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

const HEADER_LEN: u64 = 4 + 36 + 36;
const TAIL_LEN: u64 = 4;
const TABLE_ENTRY_LEN: u64 = 36 + 4 + 4;
const POINT_LEN: u64 = 16;

#[derive(Debug, Clone, Copy)]
pub struct Point {
    pub x: f32,
    pub y: f32,
    pub tilt_x: i8,
    pub tilt_y: i8,
    /// Stylus pressure, hardware range 0..=4095.
    pub pressure: u16,
    /// The point's `timestamp_rel` (ms; cumulative from stroke start). Used to
    /// estimate pen speed for velocity-sensitive pens.
    pub t: u32,
}

#[derive(Debug, Clone)]
pub struct Stroke {
    pub points: Vec<Point>,
}

/// All strokes contained in one `#points` file, keyed by normalized stroke UUID.
#[derive(Debug, Clone, Default)]
pub struct PointsFile {
    pub strokes: HashMap<String, Stroke>,
}

impl PointsFile {
    pub fn parse(bytes: &[u8]) -> io::Result<Self> {
        if bytes.len() < (HEADER_LEN + TAIL_LEN) as usize {
            return Err(bad(format!(
                "points file too small ({} bytes)",
                bytes.len()
            )));
        }
        let mut cur = Cursor::new(bytes);

        // Skip the header (version + page_id + points_id); none is needed.
        cur.seek(SeekFrom::Start(HEADER_LEN))?;

        // Stroke table location.
        cur.seek(SeekFrom::End(-4))?;
        let table_start = cur.read_u32::<BE>()? as u64;
        let table_end = (bytes.len() as u64) - TAIL_LEN;
        if table_start < HEADER_LEN || table_start > table_end {
            return Err(bad(format!("invalid stroke table offset {table_start}")));
        }
        if !(table_end - table_start).is_multiple_of(TABLE_ENTRY_LEN) {
            return Err(bad("stroke table has a partial entry".to_string()));
        }

        // Read table entries.
        let mut entries = Vec::new();
        cur.seek(SeekFrom::Start(table_start))?;
        while cur.stream_position()? + TABLE_ENTRY_LEN <= table_end {
            let mut id_buf = [0u8; 36];
            cur.read_exact(&mut id_buf)?;
            let stroke_id = ids::from_padded_bytes(&id_buf);
            let start_addr = cur.read_u32::<BE>()?;
            let packed = cur.read_u32::<BE>()?;
            let count = packed >> 4;
            validate_point_range(&stroke_id, start_addr as u64, count, table_start)?;
            entries.push((stroke_id, start_addr as u64, count));
        }

        // Read each stroke's points.
        let mut strokes = HashMap::new();
        for (stroke_id, start_addr, count) in entries {
            cur.seek(SeekFrom::Start(start_addr)).map_err(|e| {
                io::Error::new(e.kind(), format!("seeking to stroke {stroke_id}: {e}"))
            })?;
            let count_usize =
                usize::try_from(count).map_err(|_| bad(format!("stroke {stroke_id} too large")))?;
            let mut points = Vec::with_capacity(count_usize);
            for _ in 0..count {
                let t = cur.read_u32::<BE>()?;
                let x = cur.read_f32::<BE>()?;
                let y = cur.read_f32::<BE>()?;
                let tilt_x = cur.read_i8()?;
                let tilt_y = cur.read_i8()?;
                let pressure = cur.read_u16::<BE>()?;
                points.push(Point {
                    x,
                    y,
                    tilt_x,
                    tilt_y,
                    pressure,
                    t,
                });
            }
            strokes.insert(stroke_id, Stroke { points });
        }

        Ok(Self { strokes })
    }
}

fn validate_point_range(
    stroke_id: &str,
    start_addr: u64,
    count: u32,
    table_start: u64,
) -> io::Result<()> {
    if count == 0 {
        if start_addr > table_start {
            return Err(bad(format!(
                "stroke {stroke_id} starts after the stroke table"
            )));
        }
        return Ok(());
    }
    if start_addr < HEADER_LEN {
        return Err(bad(format!(
            "stroke {stroke_id} starts inside the points header"
        )));
    }
    let byte_len = u64::from(count)
        .checked_mul(POINT_LEN)
        .ok_or_else(|| bad(format!("stroke {stroke_id} point byte length overflow")))?;
    let end = start_addr
        .checked_add(byte_len)
        .ok_or_else(|| bad(format!("stroke {stroke_id} point range overflow")))?;
    if end > table_start {
        return Err(bad(format!(
            "stroke {stroke_id} point data overlaps the stroke table"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use byteorder::{BE, WriteBytesExt};
    use std::io::Write;

    #[test]
    fn parses_single_stroke() {
        // Build a minimal points file with one stroke of two points.
        let mut buf = Vec::new();
        buf.write_u32::<BE>(1).unwrap(); // version
        buf.extend_from_slice(b"03b2164818834f23839fb3d803780d9c    "); // page_id
        buf.extend_from_slice(b"20068748-578c-46b2-849b-62ae8e61ec11"); // points_id
        let body_start = buf.len() as u32; // 76
        // point 1
        buf.write_u32::<BE>(0).unwrap();
        buf.write_f32::<BE>(100.0).unwrap();
        buf.write_f32::<BE>(200.0).unwrap();
        buf.write_i8(1).unwrap();
        buf.write_i8(2).unwrap();
        buf.write_u16::<BE>(2048).unwrap();
        // point 2
        buf.write_u32::<BE>(16).unwrap();
        buf.write_f32::<BE>(110.0).unwrap();
        buf.write_f32::<BE>(210.0).unwrap();
        buf.write_i8(1).unwrap();
        buf.write_i8(2).unwrap();
        buf.write_u16::<BE>(4095).unwrap();
        let table_start = buf.len() as u32;
        // stroke table: 1 entry
        buf.write_all(b"20068748578c46b2849b62ae8e61ec11    ")
            .unwrap();
        buf.write_u32::<BE>(body_start).unwrap();
        buf.write_u32::<BE>(2 << 4).unwrap(); // count=2
        buf.write_u32::<BE>(table_start).unwrap(); // tail

        let pf = PointsFile::parse(&buf).unwrap();
        let stroke = pf
            .strokes
            .get("20068748578c46b2849b62ae8e61ec11")
            .expect("stroke present");
        assert_eq!(stroke.points.len(), 2);
        assert_eq!(stroke.points[0].x, 100.0);
        assert_eq!(stroke.points[1].pressure, 4095);
    }

    /// Append one 16-byte point record.
    fn write_point(buf: &mut Vec<u8>, x: f32, y: f32, pressure: u16) {
        buf.write_u32::<BE>(0).unwrap();
        buf.write_f32::<BE>(x).unwrap();
        buf.write_f32::<BE>(y).unwrap();
        buf.write_i8(0).unwrap();
        buf.write_i8(0).unwrap();
        buf.write_u16::<BE>(pressure).unwrap();
    }

    /// Append one 44-byte stroke-table entry.
    fn write_entry(buf: &mut Vec<u8>, id36: &[u8; 36], start_addr: u32, count: u32) {
        buf.write_all(id36).unwrap();
        buf.write_u32::<BE>(start_addr).unwrap();
        buf.write_u32::<BE>(count << 4).unwrap(); // low nibble is a flag we ignore
    }

    #[test]
    fn rejects_too_small() {
        assert!(PointsFile::parse(&[0u8; 40]).is_err());
    }

    #[test]
    fn rejects_table_offset_past_end() {
        let mut buf = vec![0u8; 80];
        // Tail says the table starts well past the file end.
        let last = buf.len() - 4;
        (&mut buf[last..]).write_u32::<BE>(9999).unwrap();
        assert!(PointsFile::parse(&buf).is_err());
    }

    #[test]
    fn rejects_table_offset_inside_header() {
        let mut buf = vec![0u8; 80];
        let last = buf.len() - 4;
        (&mut buf[last..]).write_u32::<BE>(4).unwrap();
        assert!(PointsFile::parse(&buf).is_err());
    }

    #[test]
    fn rejects_partial_table_entry() {
        let mut buf = vec![0u8; 81];
        let last = buf.len() - 4;
        (&mut buf[last..])
            .write_u32::<BE>(HEADER_LEN as u32)
            .unwrap();
        assert!(PointsFile::parse(&buf).is_err());
    }

    #[test]
    fn rejects_point_data_overlapping_table() {
        let mut buf = vec![0u8; HEADER_LEN as usize];
        let table_start = buf.len() as u32;
        write_entry(
            &mut buf,
            b"cccccccccccccccccccccccccccccccccccc",
            table_start,
            1,
        );
        buf.write_u32::<BE>(table_start).unwrap();

        assert!(PointsFile::parse(&buf).is_err());
    }

    #[test]
    fn rejects_huge_point_count_before_allocating() {
        let mut buf = vec![0u8; HEADER_LEN as usize];
        let table_start = buf.len() as u32;
        buf.write_all(b"dddddddddddddddddddddddddddddddddddd")
            .unwrap();
        buf.write_u32::<BE>(HEADER_LEN as u32).unwrap();
        buf.write_u32::<BE>(u32::MAX).unwrap();
        buf.write_u32::<BE>(table_start).unwrap();

        assert!(PointsFile::parse(&buf).is_err());
    }

    #[test]
    fn parses_two_strokes_with_separate_point_runs() {
        let mut buf = Vec::new();
        buf.write_u32::<BE>(1).unwrap();
        buf.extend_from_slice(&[b' '; 72]); // page_id + points_id (unused here)
        let a_start = buf.len() as u32;
        write_point(&mut buf, 1.0, 2.0, 10);
        let b_start = buf.len() as u32;
        write_point(&mut buf, 3.0, 4.0, 20);
        write_point(&mut buf, 5.0, 6.0, 30);
        let table_start = buf.len() as u32;
        write_entry(
            &mut buf,
            b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            a_start,
            1,
        );
        write_entry(
            &mut buf,
            b"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            b_start,
            2,
        );
        buf.write_u32::<BE>(table_start).unwrap();

        let pf = PointsFile::parse(&buf).unwrap();
        assert_eq!(pf.strokes.len(), 2);
        assert_eq!(pf.strokes["a".repeat(36).as_str()].points.len(), 1);
        let b = &pf.strokes["b".repeat(36).as_str()];
        assert_eq!(b.points.len(), 2);
        assert_eq!(b.points[1].x, 5.0);
        assert_eq!(b.points[1].pressure, 30);
    }
}
