//! Writing a framebuffer out as a PNG.
//!
//! Here rather than in `frontend-native` because two callers want it and neither is presentation:
//! the screenshot key, and `frontend-headless`, whose `--save-frame` is what makes validating
//! dmg-acid2 and cgb-acid2 against their published reference images a one-command job instead of a
//! project.
//!
//! # Why this is written out by hand
//!
//! A screenshot key needs a file format every tool can open, and PNG is that format. Reaching for
//! the `png` crate would pull in a DEFLATE implementation to compress about 150 KiB — and the
//! project's rule is that a dependency is a permanent cost paid by every consumer of the
//! workspace, so a small one wants justifying.
//!
//! DEFLATE has a **stored** block type that copies bytes verbatim. A PNG built from stored blocks
//! is a completely valid PNG with a compression ratio of 1, which for a 240×160 screenshot means
//! about 154 KiB instead of maybe 8 KiB. That is the trade: a file a few times larger than it
//! needs to be, against a compression library in the dependency graph. For a screenshot the user
//! took deliberately and will look at once, the file size is not the interesting number.
//!
//! Everything here is checked against the PNG specification's own test vectors for CRC-32 and
//! Adler-32 in the tests below, because a checksum that is subtly wrong produces a file that looks
//! written and cannot be opened.

use core_common::Framebuffer;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Encode a framebuffer as an 8-bit RGBA PNG.
///
/// The core's framebuffer is already `RGBA8` in the byte order PNG wants, so there is no colour
/// conversion step to get wrong.
pub fn encode_png(framebuffer: &Framebuffer) -> Vec<u8> {
    let width = framebuffer.width();
    let height = framebuffer.height();
    let mut png = Vec::with_capacity(framebuffer.as_bytes().len() + 4096);

    png.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);

    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.push(8); // bit depth
    ihdr.push(6); // colour type: truecolour with alpha
    ihdr.push(0); // compression: DEFLATE, the only one PNG defines
    ihdr.push(0); // filter method
    ihdr.push(0); // no interlacing
    write_chunk(&mut png, b"IHDR", &ihdr);

    // Each scanline is prefixed with its filter type. Zero means "no filtering": the row is the
    // pixels as they are. A real encoder would pick a filter per row to help the compressor, which
    // is pointless when the compressor is storing bytes verbatim.
    let stride = width as usize * 4;
    let mut raw = Vec::with_capacity(height as usize * (stride + 1));
    for y in 0..height as usize {
        raw.push(0);
        let start = y * stride;
        raw.extend_from_slice(&framebuffer.as_bytes()[start..start + stride]);
    }

    write_chunk(&mut png, b"IDAT", &zlib_stored(&raw));
    write_chunk(&mut png, b"IEND", &[]);
    png
}

/// Write a screenshot into the screenshots directory, named for the ROM and the moment.
///
/// Returns the path written. The name carries a timestamp rather than a counter so two runs of the
/// application cannot overwrite each other's screenshots, which a counter starting from zero every
/// launch would do immediately.
pub fn save_screenshot(
    dir: &Path,
    title: &str,
    framebuffer: &Framebuffer,
) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let safe: String = title
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    let path = dir.join(format!("{safe}-{stamp}.png"));
    let mut file = std::fs::File::create(&path)?;
    file.write_all(&encode_png(framebuffer))?;
    file.sync_all()?;
    Ok(path)
}

fn write_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let mut crc = Crc32::new();
    crc.update(kind);
    crc.update(data);
    out.extend_from_slice(&crc.finish().to_be_bytes());
}

/// Wrap bytes in a zlib stream whose DEFLATE payload is stored, not compressed.
fn zlib_stored(data: &[u8]) -> Vec<u8> {
    // Compression method 8 (DEFLATE) with a 32 KiB window, no preset dictionary. The two header
    // bytes read as a big-endian number that must be a multiple of 31; 0x78 0x01 is the
    // conventional "no compression" pairing and satisfies it.
    let mut out = vec![0x78, 0x01];

    // A stored block's length field is 16 bits, so long inputs become several blocks. An empty
    // input still needs one block, or the stream has no final block and every decoder rejects it.
    const MAX_BLOCK: usize = 0xFFFF;
    let mut offset = 0;
    loop {
        let end = (offset + MAX_BLOCK).min(data.len());
        let block = &data[offset..end];
        let is_final = end == data.len();
        out.push(if is_final { 1 } else { 0 });
        out.extend_from_slice(&(block.len() as u16).to_le_bytes());
        out.extend_from_slice(&(!(block.len() as u16)).to_le_bytes());
        out.extend_from_slice(block);
        offset = end;
        if is_final {
            break;
        }
    }

    out.extend_from_slice(&adler32(data).to_be_bytes());
    out
}

/// CRC-32 as PNG specifies it: the reflected polynomial `0xEDB88320`, pre- and post-inverted.
struct Crc32(u32);

impl Crc32 {
    fn new() -> Self {
        Self(0xFFFF_FFFF)
    }

    fn update(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.0 ^= byte as u32;
            for _ in 0..8 {
                // Branch on the low bit rather than using a lookup table. A 1 KiB table would be
                // faster and this runs once per screenshot.
                self.0 = if self.0 & 1 != 0 {
                    (self.0 >> 1) ^ 0xEDB8_8320
                } else {
                    self.0 >> 1
                };
            }
        }
    }

    fn finish(self) -> u32 {
        self.0 ^ 0xFFFF_FFFF
    }
}

/// Adler-32, the zlib stream's own check value.
fn adler32(data: &[u8]) -> u32 {
    const MOD: u32 = 65521;
    let (mut a, mut b) = (1u32, 0u32);
    // Chunked so the accumulators cannot overflow before the modulo: 5552 is the largest number of
    // 255-valued bytes that fits, and is the constant zlib itself uses.
    for chunk in data.chunks(5552) {
        for &byte in chunk {
            a += byte as u32;
            b += a;
        }
        a %= MOD;
        b %= MOD;
    }
    (b << 16) | a
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_common::Rgba8;

    fn framebuffer(width: u32, height: u32) -> Framebuffer {
        let mut fb = Framebuffer::new(width, height);
        for y in 0..height {
            for x in 0..width {
                fb.set_pixel(
                    x,
                    y,
                    Rgba8 {
                        r: (x * 7) as u8,
                        g: (y * 11) as u8,
                        b: 0x40,
                        a: 0xFF,
                    },
                );
            }
        }
        fb
    }

    #[test]
    fn crc32_matches_the_published_check_value() {
        // The standard CRC-32 check: "123456789" is 0xCBF43926. A wrong polynomial or a missing
        // inversion produces a file that every viewer refuses, with no other symptom.
        let mut crc = Crc32::new();
        crc.update(b"123456789");
        assert_eq!(crc.finish(), 0xCBF4_3926);
    }

    #[test]
    fn adler32_matches_the_published_check_value() {
        // RFC 1950's own example: Adler-32 of "Wikipedia" is 0x11E60398.
        assert_eq!(adler32(b"Wikipedia"), 0x11E6_0398);
        assert_eq!(adler32(b""), 1, "the empty string's Adler-32 is 1");
    }

    #[test]
    fn the_signature_and_chunk_order_are_what_png_requires() {
        let png = encode_png(&framebuffer(4, 3));
        assert_eq!(
            &png[0..8],
            &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]
        );
        assert_eq!(&png[12..16], b"IHDR");
        assert_eq!(&png[png.len() - 8..png.len() - 4], b"IEND");
    }

    #[test]
    fn the_header_records_the_actual_dimensions_and_format() {
        let png = encode_png(&framebuffer(240, 160));
        let width = u32::from_be_bytes(png[16..20].try_into().unwrap());
        let height = u32::from_be_bytes(png[20..24].try_into().unwrap());
        assert_eq!((width, height), (240, 160));
        assert_eq!(png[24], 8, "bit depth");
        assert_eq!(png[25], 6, "RGBA");
        assert_eq!(png[28], 0, "not interlaced");
    }

    #[test]
    fn every_chunk_carries_a_crc_that_verifies() {
        // Walk the file the way a decoder does. This catches a length written before the data it
        // describes, which is the mistake that produces a file that opens in one viewer and not
        // another.
        let png = encode_png(&framebuffer(9, 7));
        let mut offset = 8;
        let mut kinds = Vec::new();
        while offset < png.len() {
            let length = u32::from_be_bytes(png[offset..offset + 4].try_into().unwrap()) as usize;
            let kind = &png[offset + 4..offset + 8];
            let data = &png[offset + 8..offset + 8 + length];
            let stored = u32::from_be_bytes(
                png[offset + 8 + length..offset + 12 + length]
                    .try_into()
                    .unwrap(),
            );

            let mut crc = Crc32::new();
            crc.update(kind);
            crc.update(data);
            assert_eq!(
                crc.finish(),
                stored,
                "CRC mismatch in {:?}",
                String::from_utf8_lossy(kind)
            );

            kinds.push(String::from_utf8_lossy(kind).into_owned());
            offset += 12 + length;
        }
        assert_eq!(offset, png.len(), "the chunks must tile the file exactly");
        assert_eq!(kinds, vec!["IHDR", "IDAT", "IEND"]);
    }

    /// Decode the stored-DEFLATE payload back and compare it with what went in.
    ///
    /// A round trip is the only test that proves the block framing is right. Reading the length
    /// and its complement is exactly what a decoder does, so a wrong `NLEN` fails here as it
    /// would there.
    fn inflate_stored(zlib: &[u8]) -> Vec<u8> {
        assert_eq!(&zlib[0..2], &[0x78, 0x01], "zlib header");
        assert_eq!(
            (u16::from_be_bytes([zlib[0], zlib[1]])) % 31,
            0,
            "the zlib header must be a multiple of 31"
        );
        let mut out = Vec::new();
        let mut offset = 2;
        loop {
            let header = zlib[offset];
            assert_eq!(header & 0x06, 0, "must be a stored block");
            let is_final = header & 1 != 0;
            let len = u16::from_le_bytes([zlib[offset + 1], zlib[offset + 2]]) as usize;
            let nlen = u16::from_le_bytes([zlib[offset + 3], zlib[offset + 4]]);
            assert_eq!(nlen, !(len as u16), "LEN and NLEN must be complements");
            out.extend_from_slice(&zlib[offset + 5..offset + 5 + len]);
            offset += 5 + len;
            if is_final {
                break;
            }
        }
        let checksum = u32::from_be_bytes(zlib[offset..offset + 4].try_into().unwrap());
        assert_eq!(checksum, adler32(&out), "Adler-32 over the inflated data");
        assert_eq!(offset + 4, zlib.len(), "trailing bytes after the stream");
        out
    }

    #[test]
    fn the_pixel_data_survives_the_round_trip_with_its_filter_bytes() {
        let fb = framebuffer(5, 4);
        let png = encode_png(&fb);
        let idat_start = 8 + 12 + 13; // signature + IHDR chunk
        let idat_len =
            u32::from_be_bytes(png[idat_start..idat_start + 4].try_into().unwrap()) as usize;
        let idat = &png[idat_start + 8..idat_start + 8 + idat_len];

        let raw = inflate_stored(idat);
        assert_eq!(raw.len(), 4 * (1 + 5 * 4), "one filter byte per row");
        for y in 0..4usize {
            let row = &raw[y * 21..(y + 1) * 21];
            assert_eq!(row[0], 0, "filter type for row {y}");
            assert_eq!(
                &row[1..],
                &fb.as_bytes()[y * 20..(y + 1) * 20],
                "pixels of row {y}"
            );
        }
    }

    #[test]
    fn an_image_larger_than_one_stored_block_is_split_correctly() {
        // 200×200 RGBA with filter bytes is 160 200 bytes, which needs three stored blocks. A
        // single-block encoder passes every smaller test and produces an unopenable file for any
        // real screenshot.
        let fb = framebuffer(200, 200);
        let png = encode_png(&fb);
        let idat_start = 8 + 12 + 13;
        let idat_len =
            u32::from_be_bytes(png[idat_start..idat_start + 4].try_into().unwrap()) as usize;
        let idat = &png[idat_start + 8..idat_start + 8 + idat_len];

        let raw = inflate_stored(idat);
        assert_eq!(raw.len(), 200 * (1 + 200 * 4));
        assert!(
            raw.len() > 0xFFFF * 2,
            "this test is pointless unless it needs three blocks"
        );
    }

    #[test]
    fn a_real_screenshot_lands_on_disk_under_a_safe_name() {
        let dir = std::env::temp_dir().join(format!("alpha-shot-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let path = save_screenshot(&dir, "Game/Boy: Title?", &framebuffer(16, 16)).unwrap();
        assert!(path.exists());
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            !name.contains('/') && !name.contains(':') && !name.contains('?'),
            "unsafe file name: {name}"
        );
        assert!(name.ends_with(".png"));
        assert_eq!(
            std::fs::read(&path).unwrap(),
            encode_png(&framebuffer(16, 16))
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
