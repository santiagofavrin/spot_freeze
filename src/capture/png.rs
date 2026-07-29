//! PNG encoding of [`DibBuffer`] frames (BGRA → RGBA). Pure module — the
//! clipboard/export format on platforms without a native DIB clipboard.

use crate::capture::DibBuffer;
use anyhow::{Context, Result, bail};

/// PNG magic bytes at the start of every PNG stream.
const PNG_SIGNATURE: [u8; 8] = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];

/// Encode a frame as an 8-bit RGBA PNG, converting the [`DibBuffer`] BGRA
/// channel order (single swapped copy; alpha passed through).
pub fn encode_png(dib: &DibBuffer) -> Result<Vec<u8>> {
    if dib.width == 0 || dib.height == 0 {
        bail!("cannot encode an empty buffer as PNG");
    }
    if dib.stride != dib.width * 4 || dib.pixels.len() != dib.stride as usize * dib.height as usize
    {
        bail!(
            "buffer layout violates the DibBuffer contract: {}x{}, stride {}, {} bytes",
            dib.width,
            dib.height,
            dib.stride,
            dib.pixels.len()
        );
    }

    // BGRA -> RGBA.
    let mut rgba = Vec::with_capacity(dib.pixels.len());
    for px in dib.pixels.chunks_exact(4) {
        rgba.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
    }

    let mut out = Vec::new();
    let mut encoder = ::png::Encoder::new(&mut out, dib.width, dib.height);
    encoder.set_color(::png::ColorType::Rgba);
    encoder.set_depth(::png::BitDepth::Eight);
    let mut writer = encoder.write_header().context("writing the PNG header")?;
    writer
        .write_image_data(&rgba)
        .context("writing the PNG image data")?;
    drop(writer); // finish the stream before handing out the buffer
    debug_assert!(out.starts_with(&PNG_SIGNATURE));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2×2 buffer with distinct channels to catch swap bugs. Rows top-down:
    /// (0,0)=[B:1,G:2,R:3,A:4] (0,1)=[5,6,7,8] (1,0)=[9,10,11,12] (1,1)=[13,14,15,16].
    fn sample_2x2() -> DibBuffer {
        DibBuffer {
            width: 2,
            height: 2,
            stride: 8,
            pixels: vec![
                1, 2, 3, 4, 5, 6, 7, 8, // row 0
                9, 10, 11, 12, 13, 14, 15, 16, // row 1
            ],
        }
    }

    /// The sample's expected RGBA pixel stream (B↔R swapped, alpha kept).
    fn sample_2x2_rgba() -> Vec<u8> {
        vec![
            3, 2, 1, 4, 7, 6, 5, 8, // row 0
            11, 10, 9, 12, 15, 14, 13, 16, // row 1
        ]
    }

    #[test]
    fn starts_with_the_png_signature() {
        let png = encode_png(&sample_2x2()).unwrap();
        assert!(png.len() > PNG_SIGNATURE.len());
        assert_eq!(png[..8], PNG_SIGNATURE);
    }

    #[test]
    fn ihdr_carries_dimensions_and_format() {
        let png = encode_png(&sample_2x2()).unwrap();
        // The IHDR chunk always follows the 8-byte signature:
        // 4 length bytes, "IHDR", then width/height as big-endian u32.
        assert_eq!(&png[12..16], b"IHDR");
        assert_eq!(u32::from_be_bytes(png[16..20].try_into().unwrap()), 2); // width
        assert_eq!(u32::from_be_bytes(png[20..24].try_into().unwrap()), 2); // height
        assert_eq!(png[24], 8); // bit depth
        assert_eq!(png[25], 6); // color type 6 = truecolor with alpha (RGBA)
    }

    #[test]
    fn decode_round_trip_restores_the_rgba_pixels() {
        let src = sample_2x2();
        let png = encode_png(&src).unwrap();

        let decoder = ::png::Decoder::new(std::io::Cursor::new(&png));
        let mut reader = decoder.read_info().unwrap();
        let mut buf = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut buf).unwrap();

        assert_eq!((info.width, info.height), (2, 2));
        assert_eq!(info.color_type, ::png::ColorType::Rgba);
        assert_eq!(info.bit_depth, ::png::BitDepth::Eight);
        buf.truncate(info.buffer_size());
        assert_eq!(buf, sample_2x2_rgba());
    }

    #[test]
    fn single_pixel_encodes() {
        let dib = DibBuffer {
            width: 1,
            height: 1,
            stride: 4,
            pixels: vec![200, 100, 50, 255],
        };
        let png = encode_png(&dib).unwrap();
        assert!(png.starts_with(&PNG_SIGNATURE));
    }

    #[test]
    fn empty_buffer_errors() {
        assert!(encode_png(&DibBuffer::new(0, 0)).is_err());
        assert!(encode_png(&DibBuffer::new(4, 0)).is_err());
        assert!(encode_png(&DibBuffer::new(0, 4)).is_err());
    }

    #[test]
    fn layout_violating_the_contract_errors() {
        // Stride not equal to width * 4.
        let mut dib = sample_2x2();
        dib.stride = 12;
        assert!(encode_png(&dib).is_err());
        // Pixel count not matching stride * height.
        let mut dib = sample_2x2();
        dib.pixels.truncate(4);
        assert!(encode_png(&dib).is_err());
    }
}
