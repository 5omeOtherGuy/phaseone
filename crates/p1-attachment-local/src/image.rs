//! Image validation without a codec: the format by its signature, the dimensions from its
//! header, and metadata removed where that is a lossless cut (PNG chunks, JPEG segments).
//! Nothing is decoded or re-encoded, so a per-route downscale is not possible here.

use p1_contracts::{AttachmentError, ImageLimits, ImageType};

/// A validated image: its format, dimensions and the bytes to store.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Validated {
    pub media_type: ImageType,
    pub width: u32,
    pub height: u32,
    pub bytes: Vec<u8>,
}

const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";

/// Sniff, measure and strip `bytes`, then hold the result to `limits`.
pub(crate) fn validate(bytes: Vec<u8>, limits: ImageLimits) -> Result<Validated, AttachmentError> {
    // Refuse an oversized input before walking it; the stripped size is checked again.
    check_size(bytes.len(), limits)?;
    let media_type = sniff(&bytes).ok_or(AttachmentError::UnsupportedImage)?;
    let (width, height, bytes) = match media_type {
        ImageType::Png => {
            let (width, height) = png_dimensions(&bytes)?;
            (width, height, strip_png(&bytes)?)
        }
        ImageType::Jpeg => {
            let (width, height) = jpeg_dimensions(&bytes)?;
            (width, height, strip_jpeg(&bytes)?)
        }
        ImageType::Gif => {
            let (width, height) = gif_dimensions(&bytes)?;
            (width, height, bytes)
        }
        ImageType::Webp => {
            let (width, height) = webp_dimensions(&bytes)?;
            (width, height, bytes)
        }
    };
    if width == 0 || height == 0 {
        return Err(AttachmentError::MalformedImage("zero width or height"));
    }
    if width > limits.max_dimension || height > limits.max_dimension {
        return Err(AttachmentError::TooManyPixels {
            width,
            height,
            limit: limits.max_dimension,
        });
    }
    check_size(bytes.len(), limits)?;
    Ok(Validated {
        media_type,
        width,
        height,
        bytes,
    })
}

fn check_size(len: usize, limits: ImageLimits) -> Result<(), AttachmentError> {
    let actual = len as u64;
    if actual > limits.max_bytes {
        Err(AttachmentError::TooLarge {
            actual,
            limit: limits.max_bytes,
        })
    } else {
        Ok(())
    }
}

pub(crate) fn sniff(bytes: &[u8]) -> Option<ImageType> {
    if bytes.starts_with(PNG_SIGNATURE) {
        Some(ImageType::Png)
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some(ImageType::Jpeg)
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some(ImageType::Gif)
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some(ImageType::Webp)
    } else {
        None
    }
}

fn be32(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

fn be16(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes(bytes.get(at..at + 2)?.try_into().ok()?))
}

fn le16(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(bytes.get(at..at + 2)?.try_into().ok()?))
}

fn le24(bytes: &[u8], at: usize) -> Option<u32> {
    let b = bytes.get(at..at + 3)?;
    Some(u32::from(b[0]) | (u32::from(b[1]) << 8) | (u32::from(b[2]) << 16))
}

fn png_dimensions(bytes: &[u8]) -> Result<(u32, u32), AttachmentError> {
    const MALFORMED: AttachmentError = AttachmentError::MalformedImage("PNG without IHDR first");
    if be32(bytes, 8) != Some(13) || bytes.get(12..16) != Some(b"IHDR") {
        return Err(MALFORMED);
    }
    Ok((
        be32(bytes, 16).ok_or(MALFORMED)?,
        be32(bytes, 20).ok_or(MALFORMED)?,
    ))
}

/// Ancillary chunks that only carry metadata (text, EXIF, modification time). Every other
/// chunk, including colour, transparency and animation chunks, is kept.
const PNG_METADATA: [&[u8; 4]; 5] = [b"tEXt", b"zTXt", b"iTXt", b"eXIf", b"tIME"];

fn strip_png(bytes: &[u8]) -> Result<Vec<u8>, AttachmentError> {
    const MALFORMED: AttachmentError = AttachmentError::MalformedImage("truncated PNG chunk");
    let mut out = Vec::with_capacity(bytes.len());
    out.extend_from_slice(PNG_SIGNATURE);
    let mut at = PNG_SIGNATURE.len();
    loop {
        let len = be32(bytes, at).ok_or(MALFORMED)? as usize;
        // length, type, data, CRC
        let end = at
            .checked_add(12)
            .and_then(|n| n.checked_add(len))
            .filter(|&end| end <= bytes.len())
            .ok_or(MALFORMED)?;
        let kind = &bytes[at + 4..at + 8];
        if !PNG_METADATA.iter().any(|meta| meta.as_slice() == kind) {
            out.extend_from_slice(&bytes[at..end]);
        }
        at = end;
        // Whatever follows IEND is not part of the image and is dropped with it.
        if kind == b"IEND" {
            return Ok(out);
        }
    }
}

fn gif_dimensions(bytes: &[u8]) -> Result<(u32, u32), AttachmentError> {
    const MALFORMED: AttachmentError = AttachmentError::MalformedImage("truncated GIF header");
    Ok((
        u32::from(le16(bytes, 6).ok_or(MALFORMED)?),
        u32::from(le16(bytes, 8).ok_or(MALFORMED)?),
    ))
}

fn webp_dimensions(bytes: &[u8]) -> Result<(u32, u32), AttachmentError> {
    const MALFORMED: AttachmentError = AttachmentError::MalformedImage("unreadable WebP header");
    let data = 20;
    match bytes.get(12..16).ok_or(MALFORMED)? {
        b"VP8 " => {
            // A 3-byte frame tag, then the key-frame start code and two 14-bit sizes.
            if bytes.get(data + 3..data + 6) != Some(&[0x9d, 0x01, 0x2a]) {
                return Err(MALFORMED);
            }
            let width = le16(bytes, data + 6).ok_or(MALFORMED)? & 0x3fff;
            let height = le16(bytes, data + 8).ok_or(MALFORMED)? & 0x3fff;
            Ok((u32::from(width), u32::from(height)))
        }
        b"VP8L" => {
            if bytes.get(data) != Some(&0x2f) {
                return Err(MALFORMED);
            }
            let b = bytes.get(data + 1..data + 5).ok_or(MALFORMED)?;
            let bits = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
            Ok(((bits & 0x3fff) + 1, ((bits >> 14) & 0x3fff) + 1))
        }
        b"VP8X" => Ok((
            le24(bytes, data + 4).ok_or(MALFORMED)? + 1,
            le24(bytes, data + 7).ok_or(MALFORMED)? + 1,
        )),
        _ => Err(MALFORMED),
    }
}

/// The JPEG segments before the scan, in order: `(marker, start, end)` over the whole
/// segment including its marker, and the offset of the start-of-scan marker.
fn jpeg_segments(bytes: &[u8]) -> Result<(Vec<(u8, usize, usize)>, usize), AttachmentError> {
    const MALFORMED: AttachmentError = AttachmentError::MalformedImage("truncated JPEG segment");
    let mut segments = Vec::new();
    let mut at = 2;
    loop {
        if bytes.get(at) != Some(&0xff) {
            return Err(MALFORMED);
        }
        let start = at;
        // Fill bytes: any number of 0xFF before the marker code.
        while bytes.get(at + 1) == Some(&0xff) {
            at += 1;
        }
        let marker = *bytes.get(at + 1).ok_or(MALFORMED)?;
        if marker == 0xda {
            return Ok((segments, start));
        }
        if marker == 0x01 || (0xd0..=0xd7).contains(&marker) {
            segments.push((marker, start, at + 2));
            at += 2;
            continue;
        }
        let len = usize::from(be16(bytes, at + 2).ok_or(MALFORMED)?);
        let end = at + 2 + len;
        if len < 2 || end > bytes.len() {
            return Err(MALFORMED);
        }
        segments.push((marker, start, end));
        at = end;
    }
}

fn jpeg_dimensions(bytes: &[u8]) -> Result<(u32, u32), AttachmentError> {
    let (segments, _) = jpeg_segments(bytes)?;
    for (marker, start, end) in segments {
        // Every start-of-frame marker but DHT (C4), JPG (C8) and DAC (CC).
        if (0xc0..=0xcf).contains(&marker) && ![0xc4, 0xc8, 0xcc].contains(&marker) {
            let segment = &bytes[start..end];
            // The marker code follows any fill bytes; length (2) and precision (1) follow it.
            let at = segment.iter().position(|&b| b != 0xff).unwrap_or(0);
            let height = be16(segment, at + 4);
            let width = be16(segment, at + 6);
            if let (Some(width), Some(height)) = (width, height) {
                return Ok((u32::from(width), u32::from(height)));
            }
            return Err(AttachmentError::MalformedImage(
                "truncated JPEG frame header",
            ));
        }
    }
    Err(AttachmentError::MalformedImage(
        "JPEG without a frame header",
    ))
}

/// Segments kept before the scan: everything but comments and the application segments
/// that carry metadata. APP0 (JFIF), APP2 (ICC colour profile) and APP14 (Adobe colour
/// transform) change how the pixels decode, so they stay. APP1 goes whole, EXIF and XMP
/// alike: with it go the camera's location and the orientation flag (the pixels cannot be
/// rotated without re-encoding).
fn keeps_jpeg_segment(marker: u8) -> bool {
    !(marker == 0xfe || ((0xe1..=0xef).contains(&marker) && ![0xe2, 0xee].contains(&marker)))
}

fn strip_jpeg(bytes: &[u8]) -> Result<Vec<u8>, AttachmentError> {
    let (segments, scan) = jpeg_segments(bytes)?;
    let mut out = Vec::with_capacity(bytes.len());
    out.extend_from_slice(&bytes[..2]);
    for (marker, start, end) in segments {
        if keeps_jpeg_segment(marker) {
            out.extend_from_slice(&bytes[start..end]);
        }
    }
    out.extend_from_slice(&bytes[scan..]);
    Ok(out)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A chunk with a zero CRC: nothing here checks CRCs, the cut keeps them as they are.
    fn chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut out = (data.len() as u32).to_be_bytes().to_vec();
        out.extend_from_slice(kind);
        out.extend_from_slice(data);
        out.extend_from_slice(&[0, 0, 0, 0]);
        out
    }

    pub(crate) fn png(width: u32, height: u32, extra: &[Vec<u8>]) -> Vec<u8> {
        let mut ihdr = width.to_be_bytes().to_vec();
        ihdr.extend_from_slice(&height.to_be_bytes());
        ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
        let mut out = PNG_SIGNATURE.to_vec();
        out.extend(chunk(b"IHDR", &ihdr));
        for part in extra {
            out.extend_from_slice(part);
        }
        out.extend(chunk(b"IDAT", &[1, 2, 3]));
        out.extend(chunk(b"IEND", &[]));
        out
    }

    pub(crate) fn text_chunk(text: &str) -> Vec<u8> {
        chunk(b"tEXt", text.as_bytes())
    }

    fn segment(marker: u8, data: &[u8]) -> Vec<u8> {
        let mut out = vec![0xff, marker];
        out.extend_from_slice(&((data.len() + 2) as u16).to_be_bytes());
        out.extend_from_slice(data);
        out
    }

    pub(crate) fn jpeg(width: u16, height: u16, exif: &str) -> Vec<u8> {
        let mut out = vec![0xff, 0xd8];
        out.extend(segment(0xe0, b"JFIF\0\x01\x01\0\0\x01\0\x01\0\0"));
        out.extend(segment(0xe1, format!("Exif\0\0{exif}").as_bytes()));
        out.extend(segment(0xe2, b"ICC_PROFILE\0"));
        out.extend(segment(0xfe, b"a comment"));
        let mut sof = vec![8];
        sof.extend_from_slice(&height.to_be_bytes());
        sof.extend_from_slice(&width.to_be_bytes());
        sof.extend_from_slice(&[1, 1, 0x11, 0]);
        out.extend(segment(0xc0, &sof));
        out.extend(segment(0xda, &[1, 1, 0, 0, 0x3f, 0]));
        out.extend_from_slice(&[0x12, 0x34, 0xff, 0xd9]);
        out
    }

    fn limits() -> ImageLimits {
        ImageLimits::default()
    }

    #[test]
    fn png_text_chunks_are_cut_and_the_rest_kept_byte_for_byte() {
        let original = png(640, 480, &[text_chunk("GPS 52.5N 13.4E")]);
        let validated = validate(original, limits()).unwrap();
        assert_eq!(validated.media_type, ImageType::Png);
        assert_eq!((validated.width, validated.height), (640, 480));
        assert_eq!(validated.bytes, png(640, 480, &[]));
    }

    #[test]
    fn jpeg_exif_and_comments_are_cut_and_colour_segments_kept() {
        let validated = validate(jpeg(300, 200, "GPS 52.5N"), limits()).unwrap();
        assert_eq!(validated.media_type, ImageType::Jpeg);
        assert_eq!((validated.width, validated.height), (300, 200));
        let text = String::from_utf8_lossy(&validated.bytes);
        assert!(
            !text.contains("GPS") && !text.contains("a comment"),
            "{text}"
        );
        assert!(
            text.contains("JFIF") && text.contains("ICC_PROFILE"),
            "{text}"
        );
        assert!(validated.bytes.ends_with(&[0x12, 0x34, 0xff, 0xd9]));
    }

    #[test]
    fn gif_and_webp_dimensions_come_from_their_headers() {
        let mut gif = b"GIF89a".to_vec();
        gif.extend_from_slice(&[0x20, 0x01, 0x10, 0x00, 0, 0, 0]);
        let validated = validate(gif, limits()).unwrap();
        assert_eq!((validated.width, validated.height), (288, 16));

        let mut vp8x = b"RIFF\0\0\0\0WEBPVP8X\x0a\0\0\0\0\0\0\0".to_vec();
        vp8x.extend_from_slice(&[0x3f, 0x01, 0x00, 0xc7, 0x00, 0x00]);
        let validated = validate(vp8x, limits()).unwrap();
        assert_eq!((validated.width, validated.height), (320, 200));

        // VP8L: width-1 = 99, height-1 = 49 packed in 14-bit fields.
        let bits: u32 = 99 | (49 << 14);
        let mut vp8l = b"RIFF\0\0\0\0WEBPVP8L\x05\0\0\0\x2f".to_vec();
        vp8l.extend_from_slice(&bits.to_le_bytes());
        let validated = validate(vp8l, limits()).unwrap();
        assert_eq!((validated.width, validated.height), (100, 50));
    }

    #[test]
    fn non_images_and_broken_headers_are_refused() {
        assert_eq!(
            validate(b"%PDF-1.7".to_vec(), limits()),
            Err(AttachmentError::UnsupportedImage)
        );
        assert!(matches!(
            validate(PNG_SIGNATURE.to_vec(), limits()),
            Err(AttachmentError::MalformedImage(_))
        ));
        let mut truncated = png(1, 1, &[]);
        truncated.truncate(truncated.len() - 6);
        assert!(matches!(
            validate(truncated, limits()),
            Err(AttachmentError::MalformedImage(_))
        ));
        assert!(matches!(
            validate(png(0, 1, &[]), limits()),
            Err(AttachmentError::MalformedImage(_))
        ));
        assert!(matches!(
            validate(vec![0xff, 0xd8, 0xff, 0xe0, 0x00], limits()),
            Err(AttachmentError::MalformedImage(_))
        ));
    }

    #[test]
    fn limits_bound_bytes_and_dimensions() {
        let small = ImageLimits {
            max_bytes: 40,
            max_dimension: 100,
        };
        assert!(matches!(
            validate(png(10, 10, &[]), small),
            Err(AttachmentError::TooLarge { limit: 40, .. })
        ));
        let roomy = ImageLimits {
            max_bytes: 1_000,
            max_dimension: 100,
        };
        assert_eq!(
            validate(png(101, 10, &[]), roomy),
            Err(AttachmentError::TooManyPixels {
                width: 101,
                height: 10,
                limit: 100
            })
        );
    }
}
