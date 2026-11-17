//! Artwork images stored as files under the artwork directory, indexed in SQLite.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use bytes::Bytes;
use hudsucker::hyper::Uri;

use crate::apple;
use crate::storage::{ArtworkFile, Storage, StorageError};

/// Largest artwork body that is stored.
pub(crate) const MAX_ARTWORK_BYTES: u64 = 20 * 1024 * 1024;

/// Errors storing artwork.
#[derive(Debug, thiserror::Error)]
pub enum ArtworkError {
    /// Database error.
    #[error(transparent)]
    Storage(#[from] StorageError),
    /// File system error.
    #[error("artwork file error: {0}")]
    Io(#[from] std::io::Error),
}

/// Key identifying an image independently of the requested size and CDN
/// shard.
///
/// For the mzstatic CDN this is the URL path without its last (size) segment
/// (`…/source.jpg/300x300bb.jpg`, or `{w}x{h}bb.jpg` in templates) **plus the
/// identity query**. The query is essential: Apple's generated library-playlist
/// covers all share the path `/image/thumb/gen/{w}x{h}AM.PDCXS11.jpg` and differ
/// only by their query (`c1..c4` colours, `signature`, `t` = base64 title), so
/// a path-only key made every generated cover resolve to whichever one was
/// cached largest. Other hosts (e.g. signed `blobstore.apple.com` uploads) keep
/// their full path, since their last segment is not a size.
pub fn base_key(uri: &Uri) -> String {
    let trimmed = uri.path().trim_end_matches('/');
    let sized = uri.host().is_none_or(apple::is_artwork_host);
    let image = match trimmed.rsplit_once('/') {
        Some((base, _)) if sized && !base.is_empty() => base,
        _ => trimmed,
    };
    with_identity_query(image.to_owned(), uri)
}

/// Exact key of one concrete image URL: host, full path and identity query.
/// Unlike the HTTP response cache key it keeps every query parameter that
/// selects the image (including `t`, which the response cache treats as a
/// volatile timestamp), dropping only per-request signing parameters.
pub(crate) fn exact_key(uri: &Uri) -> Option<String> {
    let host = uri.host()?.to_ascii_lowercase();
    Some(with_identity_query(format!("{host}{}", uri.path()), uri))
}

/// Whether a query parameter is per-request URL signing (AWS `SigV4` on
/// `blobstore.apple.com` artwork) rather than part of the image identity.
fn is_signing_param(name: &str) -> bool {
    name.len() >= 6 && name[..6].eq_ignore_ascii_case("x-amz-")
}

/// Appends `?sorted-identity-query` (all non-empty, non-signing pairs sorted)
/// to `key` when there is one.
fn with_identity_query(mut key: String, uri: &Uri) -> String {
    let mut pairs: Vec<&str> = uri
        .query()
        .unwrap_or_default()
        .split('&')
        .filter(|pair| {
            let name = pair.split_once('=').map_or(*pair, |(name, _)| name);
            !name.is_empty() && !is_signing_param(name)
        })
        .collect();
    if !pairs.is_empty() {
        pairs.sort_unstable();
        key.push('?');
        key.push_str(&pairs.join("&"));
    }
    key
}

/// Source edge (px) requested for large covers: grid cards and the detail
/// header (2x a ~270px card, and Apple's own library-grid size).
pub const LARGE_EDGE: u32 = 600;

/// Source edge (px) requested for track-row thumbnails (2x a ~48px thumb plus
/// headroom). Any cached size at least this large is reused, so a 600px cover
/// already on disk serves rows too and no second download happens.
pub const SMALL_EDGE: u32 = 120;

/// The concrete URL of `url` at `edge`×`edge` pixels.
///
/// Fills Apple's `{w}x{h}` / `{c}` / `{f}` template placeholders (raw or
/// percent-encoded), and rewrites an existing mzstatic size segment
/// (`…/300x300bb.jpg`, `…/gen/1200x1200AM.PDCXS11.jpg?…`) to the requested
/// size. Any other URL is returned unchanged.
pub fn sized_url(url: &str, edge: u32) -> String {
    let edge = edge.to_string();
    let filled = url
        .replace("%7B", "{")
        .replace("%7D", "}")
        .replace("{w}", &edge)
        .replace("{h}", &edge)
        .replace("{c}", "bb")
        .replace("{f}", "jpg");
    let Ok(uri) = filled.parse::<Uri>() else {
        return filled;
    };
    if !uri.host().is_some_and(apple::is_artwork_host) {
        return filled;
    }
    // Rewrite the leading `<digits>x<digits>` of the last path segment.
    let path = uri.path();
    let Some(slash) = path.rfind('/') else {
        return filled;
    };
    let last = &path[slash + 1..];
    let width_len = last.bytes().take_while(u8::is_ascii_digit).count();
    let rest = &last[width_len..];
    let Some(after_x) = rest.strip_prefix('x') else {
        return filled;
    };
    let height_len = after_x.bytes().take_while(u8::is_ascii_digit).count();
    if width_len == 0 || height_len == 0 {
        return filled;
    }
    let tail = &after_x[height_len..];
    let Some(path_start) = filled.find(path) else {
        return filled;
    };
    let segment_start = path_start + slash + 1;
    let segment_end = segment_start + last.len();
    format!(
        "{}{edge}x{edge}{tail}{}",
        &filled[..segment_start],
        &filled[segment_end..]
    )
}

/// Parses `width * height` from a size segment such as `600x600bb.jpg`.
pub(crate) fn pixels(uri: &Uri) -> u64 {
    let last = uri.path().rsplit('/').next().unwrap_or_default();
    let mut numbers = last
        .split(|c: char| !c.is_ascii_digit())
        .filter(|s| !s.is_empty())
        .map(|s| s.parse::<u64>().unwrap_or(0));
    match (numbers.next(), numbers.next()) {
        (Some(w), Some(h)) if last.contains('x') => w.saturating_mul(h),
        _ => 0,
    }
}

/// Edge (px) of the image a URL asks for: the larger side of its
/// `<w>x<h>` size segment, or [`LARGE_EDGE`] when the URL names no size.
pub(crate) fn requested_edge(uri: &Uri) -> u32 {
    let last = uri.path().rsplit('/').next().unwrap_or_default();
    let mut numbers = last
        .split(|c: char| !c.is_ascii_digit())
        .filter(|s| !s.is_empty())
        .map(|s| s.parse::<u32>().unwrap_or(0));
    match (numbers.next(), numbers.next()) {
        (Some(w), Some(h)) if last.contains('x') && w > 0 && h > 0 => w.max(h),
        _ => LARGE_EDGE,
    }
}

fn extension_for(content_type: &str) -> &'static str {
    match content_type {
        "image/jpeg" | "image/jpg" => "jpg",
        "image/png" => "png",
        "image/webp" => "webp",
        "image/gif" => "gif",
        "image/heic" => "heic",
        "image/avif" => "avif",
        _ => "img",
    }
}

/// Artwork file store.
#[derive(Debug, Clone)]
pub(crate) struct ArtworkStore {
    dir: PathBuf,
    storage: Storage,
}

/// A stored artwork image resolved to a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StoredArtwork {
    pub(crate) path: PathBuf,
    pub(crate) content_type: String,
    /// `width * height` parsed from the stored URL (`0` when unknown).
    pub(crate) pixels: u64,
}

static FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

impl ArtworkStore {
    pub(crate) fn new(dir: PathBuf, storage: Storage) -> Self {
        Self { dir, storage }
    }

    /// Writes the image to a new file and indexes it under the URL's cache key.
    pub(crate) async fn store(
        &self,
        uri: &Uri,
        content_type: &str,
        body: Bytes,
    ) -> Result<(), ArtworkError> {
        let Some(key) = exact_key(uri) else {
            return Ok(());
        };
        let unique = FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        let file_name = format!("{stamp:x}-{unique:x}.{}", extension_for(content_type));
        tokio::fs::create_dir_all(&self.dir).await?;
        let tmp = self.dir.join(format!("{file_name}.part"));
        tokio::fs::write(&tmp, &body).await?;
        tokio::fs::rename(&tmp, self.dir.join(&file_name)).await?;

        let replaced = self
            .storage
            .put_artwork(ArtworkFile {
                cache_key: key,
                base_key: base_key(uri),
                file_name,
                content_type: content_type.to_owned(),
                size: body.len() as u64,
                pixels: pixels(uri),
            })
            .await?;
        if let Some(old) = replaced {
            remove_quietly(&self.dir.join(old)).await;
        }
        Ok(())
    }

    /// Finds stored artwork for a URL: the exact URL first, then the largest
    /// stored size of the same image. Files missing on disk are ignored.
    pub(crate) async fn lookup(&self, uri: &Uri) -> Result<Option<StoredArtwork>, StorageError> {
        let exact = match exact_key(uri) {
            Some(key) => self.storage.get_artwork(&key).await?,
            None => None,
        };
        let candidate = match exact {
            Some(file) => Some(file),
            None => self.storage.best_artwork(&base_key(uri)).await?,
        };
        Ok(self.existing(candidate).await)
    }

    /// Finds stored artwork for a URL at a display size: the exact URL first,
    /// then the smallest stored size of the same image with at least
    /// `edge`×`edge` pixels, then its largest stored size. Callers compare
    /// [`StoredArtwork::pixels`] with the wanted size to decide whether a
    /// sharper copy should be fetched.
    pub(crate) async fn lookup_sized(
        &self,
        uri: &Uri,
        edge: u32,
    ) -> Result<Option<StoredArtwork>, StorageError> {
        let exact = match exact_key(uri) {
            Some(key) => self.storage.get_artwork(&key).await?,
            None => None,
        };
        let candidate = if let Some(file) = exact {
            Some(file)
        } else {
            let min = u64::from(edge) * u64::from(edge);
            self.storage.artwork_at_least(&base_key(uri), min).await?
        };
        Ok(self.existing(candidate).await)
    }

    /// Resolves an index row to its file, ignoring rows whose file is gone.
    async fn existing(&self, file: Option<ArtworkFile>) -> Option<StoredArtwork> {
        let file = file?;
        let path = self.dir.join(&file.file_name);
        tokio::fs::try_exists(&path)
            .await
            .unwrap_or(false)
            .then_some(StoredArtwork {
                path,
                content_type: file.content_type,
                pixels: file.pixels,
            })
    }

    /// Deletes every stored artwork file and index row.
    pub(crate) async fn clear(&self) -> Result<usize, ArtworkError> {
        let names = self.storage.clear_artwork_index().await?;
        let count = names.len();
        for name in names {
            remove_quietly(&self.dir.join(name)).await;
        }
        Ok(count)
    }
}

/// Removes a file, ignoring "not found" and logging other errors.
pub(crate) async fn remove_quietly(path: &Path) {
    match tokio::fs::remove_file(path).await {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => tracing::warn!(path = %path.display(), %error, "failed to remove file"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uri(s: &str) -> Uri {
        s.parse().unwrap()
    }

    const IMAGE: &str =
        "https://is1-ssl.mzstatic.com/image/thumb/Music116/v4/cb/e9/190296350849.jpg/300x300bb.jpg";

    #[test]
    fn base_key_should_drop_size_segment() {
        assert_eq!(
            base_key(&uri(IMAGE)),
            "/image/thumb/Music116/v4/cb/e9/190296350849.jpg"
        );
    }

    #[test]
    fn template_should_share_base_key() {
        let template = "https://is3-ssl.mzstatic.com/image/thumb/Music116/v4/cb/e9/190296350849.jpg/{w}x{h}bb.jpg"
            .replace('{', "%7B")
            .replace('}', "%7D");
        assert_eq!(base_key(&uri(&template)), base_key(&uri(IMAGE)));
    }

    #[test]
    fn pixels_should_parse_size_segment() {
        assert_eq!(pixels(&uri(IMAGE)), 90_000);
    }

    #[tokio::test]
    async fn stored_artwork_should_be_found_by_other_size() {
        let dir = tempfile::tempdir().unwrap();
        let store = ArtworkStore::new(dir.path().to_path_buf(), Storage::open_in_memory().unwrap());
        store
            .store(&uri(IMAGE), "image/jpeg", Bytes::from_static(b"jpg"))
            .await
            .unwrap();
        let other = IMAGE.replace("300x300", "1200x1200");
        let found = store.lookup(&uri(&other)).await.unwrap().unwrap();
        assert_eq!(tokio::fs::read(found.path).await.unwrap(), b"jpg");
    }

    #[tokio::test]
    async fn clear_should_delete_files() {
        let dir = tempfile::tempdir().unwrap();
        let store = ArtworkStore::new(dir.path().to_path_buf(), Storage::open_in_memory().unwrap());
        store
            .store(&uri(IMAGE), "image/jpeg", Bytes::from_static(b"jpg"))
            .await
            .unwrap();
        store.clear().await.unwrap();
        assert!(std::fs::read_dir(dir.path()).unwrap().next().is_none());
    }

    #[test]
    fn sized_url_should_fill_template() {
        let template = "https://is1-ssl.mzstatic.com/image/thumb/M/x.jpg/{w}x{h}bb.{f}";
        assert_eq!(
            sized_url(template, 120),
            "https://is1-ssl.mzstatic.com/image/thumb/M/x.jpg/120x120bb.jpg"
        );
    }

    #[test]
    fn sized_url_should_rewrite_concrete_size_segment() {
        assert_eq!(
            sized_url(IMAGE, 600),
            IMAGE.replace("300x300bb.jpg", "600x600bb.jpg")
        );
    }

    #[test]
    fn sized_url_should_keep_generated_cover_query() {
        assert_eq!(
            sized_url(GEN_RALLY, 600),
            GEN_RALLY.replace("1200x1200AM", "600x600AM")
        );
    }

    #[test]
    fn sized_url_should_leave_other_hosts_alone() {
        let other = "https://example.com/cover/300x300.jpg";
        assert_eq!(sized_url(other, 600), other);
    }

    #[test]
    fn sizes_of_one_cover_should_share_base_key() {
        let template = "https://is1-ssl.mzstatic.com/image/thumb/M/x.jpg/{w}x{h}bb.{f}";
        let small = uri(&sized_url(template, SMALL_EDGE));
        let large = uri(&sized_url(template, LARGE_EDGE));
        assert_eq!(base_key(&small), base_key(&large));
    }

    #[test]
    fn sizes_of_one_cover_should_have_distinct_exact_keys() {
        let template = "https://is1-ssl.mzstatic.com/image/thumb/M/x.jpg/{w}x{h}bb.{f}";
        let small = uri(&sized_url(template, SMALL_EDGE));
        let large = uri(&sized_url(template, LARGE_EDGE));
        assert_ne!(exact_key(&small), exact_key(&large));
    }

    #[tokio::test]
    async fn sized_lookup_should_not_serve_small_file_as_exact_large() {
        // Only a 120px copy is cached: a 600px request still finds it (so the
        // UI has something), but reports its real size so callers refetch.
        let dir = tempfile::tempdir().unwrap();
        let store = ArtworkStore::new(dir.path().to_path_buf(), Storage::open_in_memory().unwrap());
        let small = IMAGE.replace("300x300", "120x120");
        store
            .store(&uri(&small), "image/jpeg", Bytes::from_static(b"s"))
            .await
            .unwrap();
        let large = uri(&IMAGE.replace("300x300", "600x600"));
        let found = store.lookup_sized(&large, 600).await.unwrap().unwrap();
        assert_eq!(found.pixels, 120 * 120);
    }

    #[tokio::test]
    async fn sized_lookup_should_prefer_smallest_sufficient_copy() {
        let dir = tempfile::tempdir().unwrap();
        let store = ArtworkStore::new(dir.path().to_path_buf(), Storage::open_in_memory().unwrap());
        for size in ["1200x1200", "600x600", "60x60"] {
            store
                .store(
                    &uri(&IMAGE.replace("300x300", size)),
                    "image/jpeg",
                    Bytes::copy_from_slice(size.as_bytes()),
                )
                .await
                .unwrap();
        }
        let row = uri(&IMAGE.replace("300x300", "120x120"));
        let found = store.lookup_sized(&row, 120).await.unwrap().unwrap();
        assert_eq!(tokio::fs::read(found.path).await.unwrap(), b"600x600");
    }

    const GEN_RALLY: &str = "https://is1-ssl.mzstatic.com/image/thumb/gen/1200x1200AM.PDCXS11.jpg?c1=1384FF&signature=aaa&t=UmFsbHkgMjAyNg%3D%3D&vkey=1";
    const GEN_GOOD: &str = "https://is1-ssl.mzstatic.com/image/thumb/gen/{w}x{h}AM.PDCXS15.jpg?c1=F8F5EC&signature=bbb&t=R29vZCBtb29k&vkey=1";

    #[test]
    fn generated_covers_should_have_distinct_base_keys() {
        let good = GEN_GOOD.replace('{', "%7B").replace('}', "%7D");
        assert_ne!(base_key(&uri(GEN_RALLY)), base_key(&uri(&good)));
    }

    #[test]
    fn generated_cover_template_should_share_base_key_with_concrete_size() {
        let template = GEN_RALLY.replace("1200x1200", "%7Bw%7Dx%7Bh%7D");
        assert_eq!(base_key(&uri(&template)), base_key(&uri(GEN_RALLY)));
    }

    #[test]
    fn signed_blobstore_url_should_ignore_signature_params() {
        let a = "https://store-035.blobstore.apple.com/sq/da/image?X-Amz-Date=1&X-Amz-Signature=x";
        let b = "https://store-035.blobstore.apple.com/sq/da/image?X-Amz-Date=2&X-Amz-Signature=y";
        assert_eq!(base_key(&uri(a)), base_key(&uri(b)));
    }

    #[tokio::test]
    async fn other_generated_cover_should_not_resolve_to_cached_one() {
        let dir = tempfile::tempdir().unwrap();
        let store = ArtworkStore::new(dir.path().to_path_buf(), Storage::open_in_memory().unwrap());
        store
            .store(&uri(GEN_RALLY), "image/jpeg", Bytes::from_static(b"rally"))
            .await
            .unwrap();
        let good = GEN_GOOD.replace('{', "%7B").replace('}', "%7D");
        assert!(store.lookup(&uri(&good)).await.unwrap().is_none());
    }
}
