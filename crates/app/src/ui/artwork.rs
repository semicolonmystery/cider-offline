//! Library covers: resolved and decoded on the tokio runtime (never on the UI
//! thread), turned into `slint::Image`s on the UI thread (`Image` is not
//! `Send`, `SharedPixelBuffer` is), and kept in a byte-bounded LRU so every
//! row and card showing the same cover shares one decoded image.
//!
//! A cover is identified by its size-independent cache identity
//! ([`cider_offline_core::identity_key`]) plus the [`Bucket`] (decode size) of
//! the context it is shown in. Songs of one album therefore resolve to one
//! [`ArtKey`], one file on disk and one decoded image per bucket.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, OnceLock};

use cider_offline_core::artwork::{LARGE_EDGE, SMALL_EDGE};
use cider_offline_core::{Library, identity_key};
use slint::{Image, Rgba8Pixel, SharedPixelBuffer};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

/// Covers decoded at once across all batches (CPU-bound, blocking pool).
const DECODE_CONCURRENCY: usize = 6;

/// Decoded-image budget for row thumbnails (about 440 at 96 px RGBA).
const ROW_CACHE_BYTES: usize = 16 * 1024 * 1024;
/// Decoded-image budget for grid and detail covers (about 110 at 384 px).
const LARGE_CACHE_BYTES: usize = 64 * 1024 * 1024;

/// The decode size of a display context.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Bucket {
    /// Song-row thumbnail (36–48 px logical).
    Row,
    /// Grid card cover (about 152–270 px logical).
    Grid,
    /// Collection detail header (160–220 px logical).
    Hero,
}

impl Bucket {
    /// Decoded edge in pixels (about 2x the logical display size).
    pub fn edge(self) -> u32 {
        match self {
            Self::Row => 96,
            Self::Grid => 384,
            Self::Hero => 512,
        }
    }

    /// Edge of the source image requested from the cache / CDN.
    pub fn source_edge(self) -> u32 {
        match self {
            Self::Row => SMALL_EDGE,
            Self::Grid | Self::Hero => LARGE_EDGE,
        }
    }

    fn is_large(self) -> bool {
        !matches!(self, Self::Row)
    }
}

/// One decoded cover's identity: image identity + decode bucket.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ArtKey {
    /// Size-independent cache identity of the image.
    pub identity: String,
    /// Decode size.
    pub bucket: Bucket,
}

/// A cover a row wants: its key and the URL (template) to resolve it from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtRequest {
    /// Identity + bucket.
    pub key: ArtKey,
    /// Artwork URL or `{w}x{h}` template.
    pub url: String,
}

impl ArtRequest {
    /// The request for `url` shown in `bucket`.
    pub fn new(url: String, bucket: Bucket) -> Self {
        Self {
            key: ArtKey {
                identity: identity_key(&url),
                bucket,
            },
            url,
        }
    }
}

/// A decoded cover (or a failure, `pixels: None`) for one key.
pub struct Decoded {
    /// What was requested.
    pub key: ArtKey,
    /// RGBA pixels, `None` when nothing could be resolved or decoded.
    pub pixels: Option<SharedPixelBuffer<Rgba8Pixel>>,
    /// `width * height` of the source file on disk (`0` if unknown).
    pub source_pixels: u64,
    /// The source was downloaded for this request (not cached).
    pub fetched: bool,
}

fn decode_limit() -> Arc<Semaphore> {
    static LIMIT: OnceLock<Arc<Semaphore>> = OnceLock::new();
    Arc::clone(LIMIT.get_or_init(|| Arc::new(Semaphore::new(DECODE_CONCURRENCY))))
}

/// Resolves (cache first, CDN only when missing) and decodes every request in
/// parallel on the blocking pool. Returns one [`Decoded`] per request.
pub async fn load(library: Library, requests: Vec<ArtRequest>) -> Vec<Decoded> {
    let limit = decode_limit();
    let mut tasks = JoinSet::new();
    for request in requests {
        let library = library.clone();
        let limit = Arc::clone(&limit);
        tasks.spawn(async move {
            let _permit = limit.acquire_owned().await;
            let edge = request.key.bucket.edge();
            let resolved = library
                .ensure_artwork_sized(&request.url, request.key.bucket.source_edge())
                .await;
            let Some(resolved) = resolved else {
                return Decoded {
                    key: request.key,
                    pixels: None,
                    source_pixels: 0,
                    fetched: false,
                };
            };
            let path = resolved.path;
            let pixels = match tokio::task::spawn_blocking(move || decode(&path, edge)).await {
                Ok(Ok(pixels)) => Some(pixels),
                Ok(Err(error)) => {
                    tracing::debug!(%error, "could not decode artwork");
                    None
                }
                Err(error) => {
                    tracing::debug!(%error, "artwork decode task failed");
                    None
                }
            };
            Decoded {
                key: request.key,
                pixels,
                source_pixels: resolved.pixels,
                fetched: resolved.fetched,
            }
        });
    }
    let mut found = Vec::new();
    while let Some(joined) = tasks.join_next().await {
        match joined {
            Ok(decoded) => found.push(decoded),
            Err(error) => tracing::debug!(%error, "artwork task failed"),
        }
    }
    found
}

/// Decodes an image file and downsizes it to fit `edge`×`edge`.
pub fn decode(path: &Path, edge: u32) -> image::ImageResult<SharedPixelBuffer<Rgba8Pixel>> {
    let image = image::ImageReader::open(path)?
        .with_guessed_format()?
        .decode()?;
    let image = if image.width() > edge || image.height() > edge {
        image.thumbnail(edge, edge)
    } else {
        image
    }
    .into_rgba8();
    Ok(SharedPixelBuffer::clone_from_slice(
        image.as_raw(),
        image.width(),
        image.height(),
    ))
}

/// Byte-bounded LRU of decoded images (UI thread only).
struct Lru {
    budget: usize,
    used: usize,
    tick: u64,
    entries: HashMap<ArtKey, (Image, usize, u64)>,
}

impl Lru {
    fn new(budget: usize) -> Self {
        Self {
            budget,
            used: 0,
            tick: 0,
            entries: HashMap::new(),
        }
    }

    fn get(&mut self, key: &ArtKey) -> Option<Image> {
        self.tick += 1;
        let tick = self.tick;
        self.entries.get_mut(key).map(|(image, _, used)| {
            *used = tick;
            image.clone()
        })
    }

    fn insert(&mut self, key: ArtKey, image: Image, bytes: usize) {
        self.tick += 1;
        if let Some((_, old, _)) = self.entries.insert(key, (image, bytes, self.tick)) {
            self.used -= old;
        }
        self.used += bytes;
        while self.used > self.budget && self.entries.len() > 1 {
            let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, (_, _, used))| *used)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            if let Some((_, bytes, _)) = self.entries.remove(&oldest) {
                self.used -= bytes;
            }
        }
    }

    fn len(&self) -> usize {
        self.entries.len()
    }
}

/// The decoded-image caches plus the bookkeeping of in-flight / failed keys.
pub struct ImageCache {
    rows: Lru,
    large: Lru,
    in_flight: HashSet<ArtKey>,
    failed: HashSet<ArtKey>,
    /// Images created since start (each decode → one `Image`).
    created: usize,
}

impl Default for ImageCache {
    fn default() -> Self {
        Self {
            rows: Lru::new(ROW_CACHE_BYTES),
            large: Lru::new(LARGE_CACHE_BYTES),
            in_flight: HashSet::new(),
            failed: HashSet::new(),
            created: 0,
        }
    }
}

impl ImageCache {
    fn lru(&mut self, bucket: Bucket) -> &mut Lru {
        if bucket.is_large() {
            &mut self.large
        } else {
            &mut self.rows
        }
    }

    /// A cached image for `key`.
    pub fn get(&mut self, key: &ArtKey) -> Option<Image> {
        self.lru(key.bucket).get(key)
    }

    /// Splits `requests` into cache hits and the misses that still need a
    /// decode, skipping keys already in flight or known to fail. Misses are
    /// marked in flight; duplicates are dropped.
    pub fn triage(
        &mut self,
        requests: Vec<ArtRequest>,
    ) -> (HashMap<ArtKey, Image>, Vec<ArtRequest>) {
        let mut hits = HashMap::new();
        let mut misses = Vec::new();
        for request in requests {
            if hits.contains_key(&request.key)
                || self.in_flight.contains(&request.key)
                || self.failed.contains(&request.key)
            {
                continue;
            }
            if let Some(image) = self.get(&request.key) {
                hits.insert(request.key, image);
            } else {
                self.in_flight.insert(request.key.clone());
                misses.push(request);
            }
        }
        (hits, misses)
    }

    /// Turns decoded pixel buffers into images (one per key), caches them,
    /// and returns them keyed for the models.
    pub fn accept(&mut self, decoded: Vec<Decoded>) -> HashMap<ArtKey, Image> {
        let mut images = HashMap::with_capacity(decoded.len());
        for item in decoded {
            self.in_flight.remove(&item.key);
            let Some(pixels) = item.pixels else {
                self.failed.insert(item.key);
                continue;
            };
            let bytes = pixels.as_bytes().len();
            let image = Image::from_rgba8(pixels);
            self.created += 1;
            self.lru(item.key.bucket)
                .insert(item.key.clone(), image.clone(), bytes);
            images.insert(item.key, image);
        }
        images
    }

    /// Releases in-flight marks of keys whose batch was lost (proxy stopped).
    pub fn abandon(&mut self, keys: &[ArtKey]) {
        for key in keys {
            self.in_flight.remove(key);
        }
    }

    /// Forgets failures so covers are retried (after a sync / refresh).
    pub fn retry_failed(&mut self) {
        self.failed.clear();
    }

    /// (images created since start, images currently cached).
    pub fn stats(&self) -> (usize, usize) {
        (self.created, self.rows.len() + self.large.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEMPLATE: &str = "https://is1-ssl.mzstatic.com/image/thumb/M/v4/a.jpg/{w}x{h}bb.{f}";

    #[test]
    fn tracks_of_one_album_should_share_a_key() {
        // The same cover requested at two concrete sizes (as Cider stores
        // per-track URLs) maps to one key per bucket.
        let a = ArtRequest::new(TEMPLATE.to_owned(), Bucket::Row);
        let b = ArtRequest::new(
            TEMPLATE.replace("{w}x{h}bb.{f}", "300x300bb.jpg"),
            Bucket::Row,
        );
        assert_eq!(a.key, b.key);
    }

    #[test]
    fn buckets_should_keep_sizes_apart() {
        let row = ArtRequest::new(TEMPLATE.to_owned(), Bucket::Row);
        let grid = ArtRequest::new(TEMPLATE.to_owned(), Bucket::Grid);
        assert_ne!(row.key, grid.key);
    }

    #[test]
    fn grid_bucket_should_request_large_source() {
        assert!(Bucket::Grid.source_edge() >= 2 * 192);
    }

    #[test]
    fn decode_should_downsize_to_bucket_edge() {
        let dir = tempfile_dir();
        let path = dir.join("cover.png");
        image::RgbaImage::new(600, 600).save(&path).unwrap();
        let pixels = decode(&path, Bucket::Grid.edge()).unwrap();
        assert_eq!((pixels.width(), pixels.height()), (384, 384));
        std::fs::remove_dir_all(dir).ok();
    }

    fn tempfile_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "cider-artwork-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
