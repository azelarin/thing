//! bounded png decoding and encoding for transferred image data.
//!
//! these run on worker threads, and decoding never guesses another format
//! from the bytes.
//!
//! decoding first walks the chunk list without decompressing anything. it
//! checks the header's size and pixel format against the limits, and passes
//! the decoder only the chunks that describe pixels: the critical chunks and
//! `tRNS`. color profiles, text, and other metadata never reach the decoder,
//! so they cannot make it allocate. the remaining allocations are the decoder's
//! output and the rgba copy made from it, which together stay within
//! `MAX_DECODE_BYTES`, plus a copy of the pixel chunks when metadata had to be
//! removed, which is never larger than the input, and the decoder's per-row
//! and inflate buffers.
//!
//! encoding writes into a buffer that refuses to grow past `MAX_PNG_BYTES`, so
//! an incompressible image fails while it is being encoded.

use std::{
    borrow::Cow,
    io::{self, Cursor, Write},
};

use image::{ExtendedColorType, ImageEncoder, ImageFormat, codecs::png::PngEncoder};

/// the largest png accepted or produced.
pub const MAX_PNG_BYTES: usize = 16 * 1024 * 1024;
/// the largest width or height of a decoded or encoded image.
pub const MAX_PNG_DIMENSION: u32 = 8192;
/// the most pixels a decoded or encoded image may hold: 64 MiB of rgba.
pub const MAX_PNG_PIXELS: u64 = 16 * 1024 * 1024;
/// the most a decode may hold for pixels at once: the decoder's output, which
/// can use two bytes per sample, and the 8-bit rgba copy made from it. this
/// admits the full pixel limit for 8-bit images and about 10 million pixels
/// for 16-bit ones.
pub const MAX_DECODE_BYTES: u64 = 128 * 1024 * 1024;

const SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";
const RGBA: u8 = 6;

/// a decoded image as unpremultiplied rgba8 rows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RgbaImage {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

fn check_dimensions(width: u32, height: u32) -> Result<(), String> {
    if width == 0 || height == 0 {
        return Err("the image is empty".into());
    }
    if width > MAX_PNG_DIMENSION
        || height > MAX_PNG_DIMENSION
        || u64::from(width) * u64::from(height) > MAX_PNG_PIXELS
    {
        return Err(format!(
            "the image is {width}×{height}; images are limited to {MAX_PNG_DIMENSION} pixels per side and {MAX_PNG_PIXELS} pixels"
        ));
    }
    Ok(())
}

fn malformed(reason: impl std::fmt::Display) -> String {
    format!("the png is malformed: {reason}")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Header {
    width: u32,
    height: u32,
    depth: u8,
    color: u8,
}

impl Header {
    fn parse(data: &[u8]) -> Result<Self, String> {
        let [w0, w1, w2, w3, h0, h1, h2, h3, depth, color, ..] = *data else {
            return Err(malformed("the header is truncated"));
        };
        Ok(Self {
            width: u32::from_be_bytes([w0, w1, w2, w3]),
            height: u32::from_be_bytes([h0, h1, h2, h3]),
            depth,
            color,
        })
    }

    /// an upper bound on the bytes the decoder's output and the rgba copy
    /// need together. the decoder expands every format to at most four
    /// samples per pixel; only 8-bit rgba is used without a copy.
    fn decode_bytes(self) -> u64 {
        let pixels = u64::from(self.width) * u64::from(self.height);
        let sample = if self.depth == 16 { 2 } else { 1 };
        let copy = if self.color == RGBA && self.depth == 8 {
            0
        } else {
            pixels * 4
        };
        pixels * 4 * sample + copy
    }
}

/// the header and the bytes the decoder needs: the signature, the critical
/// chunks, and `tRNS`, in their original order. nothing is decompressed, and
/// the input is borrowed unless a chunk had to be removed.
fn pixel_chunks(bytes: &[u8]) -> Result<(Header, Cow<'_, [u8]>), String> {
    let mut header = None;
    let mut copy: Option<Vec<u8>> = None;
    let mut offset = SIGNATURE.len();
    while offset < bytes.len() {
        let rest = &bytes[offset..];
        let [l0, l1, l2, l3, k0, k1, k2, k3, ..] = *rest else {
            return Err(malformed("a chunk is truncated"));
        };
        let length = usize::try_from(u32::from_be_bytes([l0, l1, l2, l3]))
            .map_err(|_| malformed("a chunk is too long"))?;
        let end = length
            .checked_add(12)
            .filter(|end| *end <= rest.len())
            .ok_or_else(|| malformed("a chunk is truncated"))?;
        let kind = [k0, k1, k2, k3];
        let chunk = &rest[..end];
        if header.is_none() {
            if &kind != b"IHDR" || length != 13 {
                return Err(malformed("the header chunk is missing"));
            }
            header = Some(Header::parse(&chunk[8..8 + length])?);
        }
        // critical chunks have an uppercase first letter. tRNS is the only
        // ancillary chunk that changes decoded pixels.
        let keep = kind[0].is_ascii_uppercase() || &kind == b"tRNS";
        match (&mut copy, keep) {
            (Some(copy), true) => copy.extend_from_slice(chunk),
            (Some(_), false) => {}
            (None, true) => {}
            (None, false) => {
                // the copy is never larger than the input.
                let mut kept = Vec::new();
                kept.try_reserve_exact(bytes.len())
                    .map_err(|_| "cannot allocate memory for the png".to_owned())?;
                kept.extend_from_slice(&bytes[..offset]);
                copy = Some(kept);
            }
        }
        offset += end;
        if &kind == b"IEND" {
            break;
        }
    }
    let header = header.ok_or_else(|| malformed("the header chunk is missing"))?;
    let pixels = match copy {
        Some(copy) => Cow::Owned(copy),
        None => Cow::Borrowed(&bytes[..offset]),
    };
    Ok((header, pixels))
}

/// decodes png bytes, and only png bytes, into rgba.
pub fn decode_png(bytes: &[u8]) -> Result<RgbaImage, String> {
    if bytes.len() > MAX_PNG_BYTES {
        return Err(format!(
            "the png is larger than {} MiB",
            MAX_PNG_BYTES / 1024 / 1024
        ));
    }
    if !bytes.starts_with(SIGNATURE) {
        return Err("the data is not a png image".into());
    }
    let (header, pixels) = pixel_chunks(bytes)?;
    check_dimensions(header.width, header.height)?;
    if header.decode_bytes() > MAX_DECODE_BYTES {
        return Err(format!(
            "the image is {}×{} with {}-bit samples; decoding it would need more than {} MiB",
            header.width,
            header.height,
            header.depth,
            MAX_DECODE_BYTES / 1024 / 1024
        ));
    }
    let mut reader = image::ImageReader::with_format(Cursor::new(&*pixels), ImageFormat::Png);
    reader.limits(limits_for_decode());
    let decoded = reader.decode().map_err(malformed)?;
    drop(pixels);
    if (decoded.width(), decoded.height()) != (header.width, header.height) {
        return Err(malformed("the decoded size differs from the header"));
    }
    let image = decoded.into_rgba8();
    Ok(RgbaImage {
        width: image.width(),
        height: image.height(),
        rgba: image.into_raw(),
    })
}

/// a backstop: `decode_png` has already checked the header against these.
fn limits_for_decode() -> image::Limits {
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_PNG_DIMENSION);
    limits.max_image_height = Some(MAX_PNG_DIMENSION);
    limits.max_alloc = Some(MAX_DECODE_BYTES);
    limits
}

/// an output buffer that refuses to grow past its limit.
struct Bounded {
    bytes: Vec<u8>,
    limit: usize,
    exceeded: bool,
}

impl Bounded {
    fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            limit,
            exceeded: false,
        }
    }
}

impl Write for Bounded {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        let Some(needed) = self
            .bytes
            .len()
            .checked_add(data.len())
            .filter(|needed| *needed <= self.limit)
        else {
            self.exceeded = true;
            return Err(io::Error::other("the encoded png exceeds its limit"));
        };
        if needed > self.bytes.capacity() {
            let target = needed
                .max(self.bytes.capacity().saturating_mul(2))
                .min(self.limit);
            self.bytes
                .try_reserve_exact(target - self.bytes.len())
                .map_err(|_| io::Error::from(io::ErrorKind::OutOfMemory))?;
        }
        self.bytes.extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// encodes unpremultiplied rgba as a png no larger than `MAX_PNG_BYTES`.
pub fn encode_png(width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>, String> {
    encode_within(width, height, rgba, MAX_PNG_BYTES)
}

fn encode_within(width: u32, height: u32, rgba: &[u8], limit: usize) -> Result<Vec<u8>, String> {
    check_dimensions(width, height)?;
    if u64::try_from(rgba.len()).ok() != Some(u64::from(width) * u64::from(height) * 4) {
        return Err("the image's pixel data does not match its size".into());
    }
    let mut output = Bounded::new(limit);
    let result =
        PngEncoder::new(&mut output).write_image(rgba, width, height, ExtendedColorType::Rgba8);
    if output.exceeded {
        return Err(format!(
            "the encoded png is larger than {} MiB",
            limit / 1024 / 1024
        ));
    }
    result.map_err(|error| format!("cannot encode the png: {error}"))?;
    Ok(output.bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn png_round_trips_and_rejects_other_or_oversized_data() {
        let rgba: Vec<u8> = (0..4 * 3 * 2).map(|value| value as u8 * 9).collect();
        let png = encode_png(3, 2, &rgba).unwrap();
        assert!(png.starts_with(SIGNATURE));
        assert_eq!(
            decode_png(&png).unwrap(),
            RgbaImage {
                width: 3,
                height: 2,
                rgba
            }
        );
        // a valid jpeg header is still not a png.
        assert!(
            decode_png(b"\xff\xd8\xff\xe0\0\x10JFIF")
                .unwrap_err()
                .contains("not a png")
        );
        assert!(
            decode_png(&png[..png.len() / 2])
                .unwrap_err()
                .contains("malformed")
        );
        assert!(
            decode_png(&vec![0; MAX_PNG_BYTES + 1])
                .unwrap_err()
                .contains("larger")
        );
        assert!(encode_png(0, 4, &[]).is_err());
        assert!(
            encode_png(2, 2, &[0; 4])
                .unwrap_err()
                .contains("does not match")
        );
        assert!(
            encode_png(MAX_PNG_DIMENSION + 1, 1, &[])
                .unwrap_err()
                .contains("limited")
        );
    }

    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc = !0_u32;
        for byte in bytes {
            crc ^= u32::from(*byte);
            for _ in 0..8 {
                crc = if crc & 1 == 1 {
                    (crc >> 1) ^ 0xedb8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }

    /// one chunk with a correct crc.
    fn chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut bytes = u32::try_from(data.len()).unwrap().to_be_bytes().to_vec();
        bytes.extend_from_slice(kind);
        bytes.extend_from_slice(data);
        let mut checked = kind.to_vec();
        checked.extend_from_slice(data);
        bytes.extend_from_slice(&crc32(&checked).to_be_bytes());
        bytes
    }

    /// a png whose header is valid, crc included, but whose pixels are only a
    /// token idat: anything that gets past the header checks fails to decode.
    fn header_only(width: u32, height: u32, depth: u8, color: u8) -> Vec<u8> {
        let mut header = width.to_be_bytes().to_vec();
        header.extend_from_slice(&height.to_be_bytes());
        header.extend_from_slice(&[depth, color, 0, 0, 0]);
        let mut png = SIGNATURE.to_vec();
        png.extend(chunk(b"IHDR", &header));
        png.extend(chunk(b"IDAT", &[0x78, 0x9c, 0x03, 0x00]));
        png.extend(chunk(b"IEND", &[]));
        png
    }

    fn valid_header(png: &[u8]) -> (u32, u32) {
        image::ImageReader::with_format(Cursor::new(png), ImageFormat::Png)
            .into_dimensions()
            .expect("the unlimited decoder accepts the header and its crc")
    }

    #[test]
    fn png_dimensions_are_checked_before_decoding_pixels() {
        // the header's crc is valid, so only the size gate can reject it.
        let oversized = header_only(20_000, 20_000, 8, 6);
        assert_eq!(valid_header(&oversized), (20_000, 20_000));
        let error = decode_png(&oversized).unwrap_err();
        assert!(error.contains("limited"), "{error}");

        // a header that only changes the size bytes fails its crc instead,
        // which is why size regressions must repair it.
        let mut corrupted = encode_png(1, 1, &[1, 2, 3, 4]).unwrap();
        corrupted[16..20].copy_from_slice(&20_000_u32.to_be_bytes());
        assert!(
            image::ImageReader::with_format(Cursor::new(&corrupted), ImageFormat::Png)
                .into_dimensions()
                .is_err()
        );

        // within the limits, the same token pixels are simply malformed.
        let allowed = header_only(4096, 4096, 8, 6);
        assert_eq!(valid_header(&allowed), (4096, 4096));
        let error = decode_png(&allowed).unwrap_err();
        assert!(error.contains("malformed"), "{error}");
    }

    #[test]
    fn sixteen_bit_images_are_checked_against_the_decode_budget() {
        // 16 million 16-bit rgba pixels need 128 MiB of output and 64 MiB of
        // rgba: within the pixel limit, beyond the decode budget.
        let deep = header_only(4096, 4096, 16, 6);
        assert_eq!(valid_header(&deep), (4096, 4096));
        let error = decode_png(&deep).unwrap_err();
        assert!(error.contains("128 MiB"), "{error}");
        let (header, _) = pixel_chunks(&deep).unwrap();
        assert!(header.decode_bytes() > MAX_DECODE_BYTES);

        // 8-bit rgb at the pixel limit fits, with its rgba copy.
        let (header, _) = pixel_chunks(&header_only(4096, 4096, 8, 2)).unwrap();
        assert_eq!(header.decode_bytes(), MAX_DECODE_BYTES);
        // a smaller 16-bit image is admitted and decodes.
        let rgba16: Vec<u8> = (0..2 * 2 * 8).map(|value| value as u8).collect();
        let mut png = Vec::new();
        PngEncoder::new(&mut png)
            .write_image(&rgba16, 2, 2, ExtendedColorType::Rgba16)
            .unwrap();
        assert_eq!(decode_png(&png).unwrap().rgba.len(), 2 * 2 * 4);
    }

    #[test]
    fn metadata_never_reaches_the_decoder() {
        let rgba: Vec<u8> = (0..4 * 2 * 2).map(|value| value as u8 * 7).collect();
        let png = encode_png(2, 2, &rgba).unwrap();
        let (_, borrowed) = pixel_chunks(&png).unwrap();
        assert!(matches!(borrowed, Cow::Borrowed(_)));

        // 12 MiB of text and a profile, inserted after the header.
        let header_end = SIGNATURE.len() + 12 + 13;
        let mut decorated = png[..header_end].to_vec();
        decorated.extend(chunk(b"tEXt", &vec![b'x'; 12 * 1024 * 1024]));
        decorated.extend(chunk(b"iCCP", b"profile\0\0\x78\x9c\x03\x00"));
        decorated.extend_from_slice(&png[header_end..]);
        assert!(decorated.len() <= MAX_PNG_BYTES);
        let (_, stripped) = pixel_chunks(&decorated).unwrap();
        assert!(matches!(stripped, Cow::Owned(_)));
        assert!(stripped.len() <= png.len());
        assert!(
            !stripped
                .windows(4)
                .any(|kind| kind == b"tEXt" || kind == b"iCCP")
        );
        assert_eq!(decode_png(&decorated).unwrap().rgba, rgba);

        // transparency is kept, because it changes the decoded pixels.
        let mut header = 1_u32.to_be_bytes().to_vec();
        header.extend_from_slice(&1_u32.to_be_bytes());
        header.extend_from_slice(&[8, 2, 0, 0, 0]);
        let mut transparent = SIGNATURE.to_vec();
        transparent.extend(chunk(b"IHDR", &header));
        transparent.extend(chunk(b"tRNS", &[0, 1, 0, 2, 0, 3]));
        transparent.extend(chunk(b"tEXt", b"comment\0kept out"));
        // one unfiltered row holding the pixel (1, 2, 3), stored uncompressed.
        transparent.extend(chunk(
            b"IDAT",
            &[
                0x78, 0x01, 0x01, 0x04, 0x00, 0xfb, 0xff, 0x00, 0x01, 0x02, 0x03, 0x00, 0x0e, 0x00,
                0x07,
            ],
        ));
        transparent.extend(chunk(b"IEND", &[]));
        assert_eq!(decode_png(&transparent).unwrap().rgba, [1, 2, 3, 0]);
    }

    #[test]
    fn encoding_stops_at_the_output_limit() {
        // incompressible pixels cannot fit in 1 KiB.
        let mut state = 0x2545_f491_u32;
        let noise: Vec<u8> = (0..64 * 64 * 4)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                state as u8
            })
            .collect();
        let error = encode_within(64, 64, &noise, 1024).unwrap_err();
        assert!(error.contains("larger"), "{error}");
        assert!(encode_within(64, 64, &noise, MAX_PNG_BYTES).is_ok());

        // the buffer never grows past its limit, even while doubling.
        let mut output = Bounded::new(1000);
        for _ in 0..9 {
            output.write_all(&[0; 100]).unwrap();
        }
        assert!(output.write_all(&[0; 101]).is_err());
        assert!(output.exceeded);
        assert_eq!(output.bytes.len(), 900);
        assert!(output.bytes.capacity() <= 1000);
    }
}
