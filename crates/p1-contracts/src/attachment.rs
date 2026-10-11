//! Attachment seam (ADR-0180): images and files a user hands an agent.
//!
//! History and journal hold references ([`ImageRef`], [`FileRef`]), never bytes: the
//! bytes live in an [`AttachmentStore`], a host-side OS service. Right before a request
//! the host resolves what the route needs (an image's bytes, a file's read-only path)
//! into fields serde skips, so a resolved block can cross to a provider but can never be
//! journalled with its bytes.

use std::borrow::Cow;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::BoxFuture;

/// Content address of a stored attachment: the lowercase hex SHA-256 of its stored bytes.
/// Checked on construction and on deserialization, so a store can join it into a path.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct AttachmentId(String);

impl AttachmentId {
    /// A digest in its 64-character lowercase hex form.
    pub fn new(hex: impl Into<String>) -> Result<Self, AttachmentError> {
        let hex = hex.into();
        let valid = hex.len() == 64
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
        if valid {
            Ok(Self(hex))
        } else {
            Err(AttachmentError::InvalidId)
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for AttachmentId {
    type Error = AttachmentError;
    fn try_from(hex: String) -> Result<Self, Self::Error> {
        Self::new(hex)
    }
}

impl From<AttachmentId> for String {
    fn from(id: AttachmentId) -> Self {
        id.0
    }
}

impl fmt::Display for AttachmentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The image formats a store accepts, by their content, never by a file name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageType {
    Png,
    Jpeg,
    Gif,
    Webp,
}

impl ImageType {
    /// The IANA media type, as the Messages wire names it.
    pub fn media_type(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Gif => "image/gif",
            Self::Webp => "image/webp",
        }
    }
}

/// A stored, validated image. `bytes`, `width` and `height` describe the stored object.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ImageRef {
    pub id: AttachmentId,
    pub media_type: ImageType,
    pub bytes: u64,
    pub width: u32,
    pub height: u32,
}

impl ImageRef {
    /// What a route whose model takes no image input shows in the image's place.
    pub fn placeholder(&self) -> String {
        format!(
            "[image {}: {} {}x{}, {} bytes; this model does not take image input]",
            short(&self.id),
            self.media_type.media_type(),
            self.width,
            self.height,
            self.bytes
        )
    }

    /// What a request shows in the image's place when it is not sent: the image is
    /// unreadable, or older images were left out to keep the request within its budget.
    pub fn unavailable(&self, reason: &str) -> String {
        format!(
            "[image {}: {} {}x{}, {} bytes; not sent: {reason}]",
            short(&self.id),
            self.media_type.media_type(),
            self.width,
            self.height,
            self.bytes
        )
    }
}

/// A stored file, kept verbatim. `name` is the name the user gave it, for display only.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FileRef {
    pub id: AttachmentId,
    pub name: String,
    pub bytes: u64,
}

impl FileRef {
    /// The one text line a route shows for the file: its name, size and read-only path.
    pub fn line(&self, path: Option<&str>) -> String {
        match path {
            Some(path) => format!(
                "[attached file {:?}, {} bytes, read-only at {path}]",
                self.name, self.bytes
            ),
            None => format!(
                "[attached file {:?}, {} bytes; its path is unavailable]",
                self.name, self.bytes
            ),
        }
    }
}

fn short(id: &AttachmentId) -> &str {
    &id.as_str()[..12]
}

/// An image's bytes as base64 text, resolved by the host for one request. Shared, so a
/// cloned history does not copy them; its `Debug` names only the length.
#[derive(Clone, PartialEq, Eq)]
pub struct ImageData(Arc<str>);

impl ImageData {
    /// Standard base64 (RFC 4648 §4, padded), as the Messages wire carries it.
    pub fn from_base64(text: impl Into<Arc<str>>) -> Self {
        Self(text.into())
    }

    /// Encode raw image bytes.
    pub fn encode(bytes: &[u8]) -> Self {
        Self(base64(bytes).into())
    }

    pub fn as_base64(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ImageData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ImageData({} base64 chars)", self.0.len())
    }
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for (index, shift) in [18u32, 12, 6, 0].into_iter().enumerate() {
            if index <= chunk.len() {
                out.push(ALPHABET[((n >> shift) & 0x3f) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// One block of user input that carries attachments (`Item::UserContent`).
///
/// The `data` and `path` fields are filled by the host for one provider request and are
/// skipped by serde: a journal record, a summary or a replaced history never holds them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "block", rename_all = "snake_case")]
pub enum UserBlock {
    Text {
        text: String,
    },
    Image {
        image: ImageRef,
        #[serde(skip)]
        data: Option<ImageData>,
    },
    File {
        file: FileRef,
        #[serde(skip)]
        path: Option<String>,
    },
}

impl UserBlock {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }

    pub fn image(image: ImageRef) -> Self {
        Self::Image { image, data: None }
    }

    pub fn file(file: FileRef) -> Self {
        Self::File { file, path: None }
    }

    /// This block as text, for a route that carries it no other way: text as is, an image
    /// as its placeholder, a file as its line.
    pub fn as_text(&self) -> Cow<'_, str> {
        match self {
            Self::Text { text } => Cow::Borrowed(text),
            Self::Image { image, .. } => Cow::Owned(image.placeholder()),
            Self::File { file, path } => Cow::Owned(file.line(path.as_deref())),
        }
    }
}

/// The whole input as text, blocks joined by a newline: what a text-only wire sends.
pub fn text_projection(blocks: &[UserBlock]) -> String {
    let parts: Vec<Cow<'_, str>> = blocks.iter().map(UserBlock::as_text).collect();
    parts.join("\n")
}

/// What a store accepts as an image.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageLimits {
    /// Largest stored image, in bytes.
    pub max_bytes: u64,
    /// Largest width and height, in pixels.
    pub max_dimension: u32,
}

impl Default for ImageLimits {
    /// The Messages API's per-image limits: 5 MB and 8000 pixels a side.
    fn default() -> Self {
        Self {
            max_bytes: 5 * 1024 * 1024,
            max_dimension: 8000,
        }
    }
}

/// The size a route can take for one image; a stored image above it is a per-route
/// variant the store must produce (or refuse, while it cannot re-encode).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageTarget {
    pub max_bytes: u64,
    pub max_dimension: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AttachmentError {
    #[error("not a 64-character lowercase hex SHA-256 attachment id")]
    InvalidId,
    #[error("the content is not a supported image (PNG, JPEG, GIF or WebP)")]
    UnsupportedImage,
    #[error("the image is malformed: {0}")]
    MalformedImage(&'static str),
    #[error("the image is {actual} bytes, the limit is {limit}")]
    TooLarge { actual: u64, limit: u64 },
    #[error("the image is {width}x{height} pixels, the limit is {limit} a side")]
    TooManyPixels { width: u32, height: u32, limit: u32 },
    #[error("attachment {0} is not in the store")]
    NotFound(AttachmentId),
    #[error("attachment {0} does not match its content address")]
    Corrupt(AttachmentId),
    #[error("the attachment store failed: {0}")]
    Io(String),
}

/// The host-side port every attachment goes through (ADR-0180). An adapter validates and
/// stores; a read is re-verified against the content address.
pub trait AttachmentStore: Send + Sync {
    fn limits(&self) -> ImageLimits;

    /// Validate an image against [`Self::limits`], strip what metadata the adapter can
    /// without re-encoding, store it and return its reference.
    fn save_image(
        &self,
        bytes: Vec<u8>,
        name: Option<String>,
    ) -> BoxFuture<'_, Result<ImageRef, AttachmentError>>;

    /// The stored bytes of `image`, re-verified. With a `target` the image must fit it.
    fn read_image<'a>(
        &'a self,
        image: &'a ImageRef,
        target: Option<ImageTarget>,
    ) -> BoxFuture<'a, Result<Vec<u8>, AttachmentError>>;

    /// Store any file verbatim.
    fn save_file(
        &self,
        bytes: Vec<u8>,
        name: String,
    ) -> BoxFuture<'_, Result<FileRef, AttachmentError>>;

    /// A read-only path a model's file tools can read the file at.
    fn file_path(&self, file: &FileRef) -> PathBuf;
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEX: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn an_id_is_a_lowercase_sha256_hex_digest() {
        assert!(AttachmentId::new(HEX).is_ok());
        for bad in [
            "",
            "../../etc/passwd",
            &HEX.to_uppercase(),
            &HEX[..63],
            &format!("{HEX}0"),
        ] {
            assert_eq!(
                AttachmentId::new(bad),
                Err(AttachmentError::InvalidId),
                "{bad}"
            );
        }
        let parsed: Result<AttachmentId, _> = serde_json::from_str("\"../x\"");
        assert!(parsed.is_err());
    }

    #[test]
    fn base64_matches_rfc_4648_vectors() {
        for (input, expected) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(ImageData::encode(input.as_bytes()).as_base64(), expected);
        }
    }

    #[test]
    fn resolved_data_and_paths_are_never_serialized() {
        let id = AttachmentId::new(HEX).unwrap();
        let blocks = vec![
            UserBlock::Image {
                image: ImageRef {
                    id: id.clone(),
                    media_type: ImageType::Png,
                    bytes: 3,
                    width: 1,
                    height: 1,
                },
                data: Some(ImageData::encode(b"abc")),
            },
            UserBlock::File {
                file: FileRef {
                    id,
                    name: "notes.txt".into(),
                    bytes: 3,
                },
                path: Some("/secret/path".into()),
            },
        ];
        let json = serde_json::to_string(&blocks).unwrap();
        assert!(
            !json.contains("YWJj") && !json.contains("/secret/path"),
            "{json}"
        );
        let back: Vec<UserBlock> = serde_json::from_str(&json).unwrap();
        assert!(matches!(&back[0], UserBlock::Image { data: None, .. }));
        assert!(matches!(&back[1], UserBlock::File { path: None, .. }));
    }
}
