//! Encoding of app-owned file paths for storage.
//!
//! Files the app keeps under its data folder (`media/…`, lyrics, …) are stored
//! relative to that folder with `/` separators, so a moved data folder
//! (portable copy, another OS) keeps resolving. Anything outside the folder,
//! such as a file the user picked elsewhere, is stored as its absolute path.

use std::path::{Component, Path, PathBuf, Prefix};
use std::sync::Arc;

use super::StorageError;

/// Separator used inside stored relative paths, on every OS.
const SEPARATOR: &str = "/";

/// Converts file paths to and from their stored form, relative to an optional
/// root (the database's folder). Without a root it is the identity.
#[derive(Debug, Clone, Default)]
pub(crate) struct PathCodec {
    root: Option<Arc<Path>>,
}

impl PathCodec {
    /// A codec storing paths under `root` relative to it; `None` stores every
    /// path as given.
    pub(crate) fn new(root: Option<PathBuf>) -> Self {
        Self {
            root: root.map(Arc::from),
        }
    }

    /// The stored form of `path`: relative with `/` separators when it lies
    /// strictly inside the root (and contains no `..`), otherwise the path
    /// unchanged.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::NonUtf8Path`] when `path` is not valid UTF-8.
    pub(crate) fn encode(&self, path: &Path) -> Result<String, StorageError> {
        let text = path
            .to_str()
            .ok_or_else(|| StorageError::NonUtf8Path(path.to_path_buf()))?;
        Ok(self
            .root
            .as_deref()
            .and_then(|root| relative_to(root, path))
            .unwrap_or_else(|| text.to_owned()))
    }

    /// The file a stored value refers to: a relative value is resolved
    /// against the root one `/` segment at a time; anything else (rooted or
    /// absolute, or no root configured) is returned as is.
    pub(crate) fn decode(&self, stored: &str) -> PathBuf {
        let as_is = Path::new(stored);
        let Some(root) = self.root.as_deref() else {
            return as_is.to_path_buf();
        };
        if is_anchored(as_is) {
            return as_is.to_path_buf();
        }
        stored
            .split(SEPARATOR)
            .filter(|segment| !segment.is_empty())
            .fold(root.to_path_buf(), |mut out, segment| {
                out.push(segment);
                out
            })
    }
}

/// Whether `path` is tied to a drive, share or root (so it must not be joined
/// onto the data folder). On Windows `\\x` and `C:x` are not `is_absolute` but
/// are still anchored.
fn is_anchored(path: &Path) -> bool {
    path.is_absolute()
        || path.has_root()
        || matches!(path.components().next(), Some(Component::Prefix(_)))
}

/// One path component in comparable form, plus its original text for
/// `Normal` components (what a relative result is built from).
struct Part<'a> {
    key: String,
    original: Option<&'a str>,
}

/// Splits `path` into comparable parts: verbatim prefixes (`\\?\C:`) compare
/// equal to their plain forms (`C:`), and on Windows names compare without
/// case. Returns `None` for paths with `..` or non-UTF-8 components, which are
/// then never treated as inside the root.
fn parts(path: &Path) -> Option<Vec<Part<'_>>> {
    let mut out = Vec::new();
    for component in path.components() {
        let part = match component {
            Component::Prefix(prefix) => Part {
                key: prefix_key(prefix.kind())?,
                original: None,
            },
            Component::RootDir => Part {
                key: SEPARATOR.to_owned(),
                original: None,
            },
            Component::CurDir => continue,
            Component::ParentDir => return None,
            Component::Normal(name) => {
                let name = name.to_str()?;
                Part {
                    key: fold_case(name),
                    original: Some(name),
                }
            }
        };
        out.push(part);
    }
    Some(out)
}

fn prefix_key(prefix: Prefix<'_>) -> Option<String> {
    Some(match prefix {
        Prefix::Disk(drive) | Prefix::VerbatimDisk(drive) => {
            format!("{}:", char::from(drive).to_ascii_uppercase())
        }
        Prefix::UNC(server, share) | Prefix::VerbatimUNC(server, share) => format!(
            r"\\{}\{}",
            fold_case(server.to_str()?),
            fold_case(share.to_str()?)
        ),
        Prefix::DeviceNS(name) => format!(r"\\.\{}", fold_case(name.to_str()?)),
        Prefix::Verbatim(name) => format!(r"\\?\{}", fold_case(name.to_str()?)),
    })
}

/// Windows file names are case-insensitive; elsewhere they are compared as is.
fn fold_case(name: &str) -> String {
    if cfg!(windows) {
        name.to_lowercase()
    } else {
        name.to_owned()
    }
}

/// `path` relative to `root` with `/` separators, when `path` is absolute and
/// lies strictly inside `root`.
fn relative_to(root: &Path, path: &Path) -> Option<String> {
    if !path.is_absolute() {
        return None;
    }
    let root = parts(root)?;
    let path = parts(path)?;
    if path.len() <= root.len()
        || root
            .iter()
            .zip(&path)
            .any(|(root, part)| root.key != part.key)
    {
        return None;
    }
    let rest = path[root.len()..]
        .iter()
        .map(|part| part.original)
        .collect::<Option<Vec<_>>>()?;
    Some(rest.join(SEPARATOR))
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;

    fn rooted() -> (tempfile::TempDir, PathBuf, PathCodec) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("data");
        let codec = PathCodec::new(Some(root.clone()));
        (dir, root, codec)
    }

    #[test]
    fn encode_should_store_file_under_root_relative_with_forward_slashes() {
        let (_dir, root, codec) = rooted();
        let file = root.join("media").join("1 2 雪.m4a");
        assert_eq!(codec.encode(&file).unwrap(), "media/1 2 雪.m4a");
    }

    #[test]
    fn encode_should_keep_paths_outside_root_absolute() {
        let (dir, _root, codec) = rooted();
        let file = dir.path().join("other_dir").join("a.mp3");
        assert_eq!(codec.encode(&file).unwrap(), file.to_str().unwrap());
    }

    #[test]
    fn encode_should_keep_parent_dir_escapes_absolute() {
        let (_dir, root, codec) = rooted();
        let file = root.join("..").join("x.mp3");
        assert_eq!(codec.encode(&file).unwrap(), file.to_str().unwrap());
    }

    #[test]
    fn decode_should_resolve_relative_against_root() {
        let (_dir, root, codec) = rooted();
        assert_eq!(
            codec.decode("media/x.m4a"),
            root.join("media").join("x.m4a")
        );
    }

    #[test]
    fn decode_should_leave_absolute_untouched() {
        let (dir, _root, codec) = rooted();
        let file = dir.path().join("elsewhere").join("a.flac");
        assert_eq!(codec.decode(file.to_str().unwrap()), file);
    }

    #[test]
    fn rootless_codec_should_be_identity() {
        let codec = PathCodec::default();
        assert_eq!(codec.encode(Path::new("a.m4a")).unwrap(), "a.m4a");
        assert_eq!(codec.decode("a.m4a"), PathBuf::from("a.m4a"));
    }

    #[test]
    fn encode_should_keep_the_root_itself_absolute() {
        let (_dir, root, codec) = rooted();
        assert_eq!(codec.encode(&root).unwrap(), root.to_str().unwrap());
    }

    #[test]
    fn relative_input_should_encode_as_itself() {
        let (_dir, _root, codec) = rooted();
        assert_eq!(codec.encode(Path::new("a.m4a")).unwrap(), "a.m4a");
    }

    #[cfg(windows)]
    #[test]
    fn encode_should_match_root_despite_verbatim_prefix_and_case() {
        let codec = PathCodec::new(Some(PathBuf::from(r"C:\Users\John Doe\AppData\Local\data")));
        let file = PathBuf::from(r"\\?\c:\users\john doe\appdata\local\DATA\media\1.m4a");
        assert_eq!(codec.encode(&file).unwrap(), "media/1.m4a");
    }

    #[cfg(windows)]
    #[test]
    fn verbatim_root_should_match_plain_file_path() {
        let codec = PathCodec::new(Some(PathBuf::from(r"\\?\C:\data")));
        assert_eq!(
            codec.encode(Path::new(r"C:\data\media\a b.m4a")).unwrap(),
            "media/a b.m4a"
        );
    }

    #[cfg(windows)]
    #[test]
    fn encode_should_not_match_other_drive() {
        let codec = PathCodec::new(Some(PathBuf::from(r"C:\data")));
        assert_eq!(
            codec.encode(Path::new(r"D:\data\media\a.m4a")).unwrap(),
            r"D:\data\media\a.m4a"
        );
    }

    #[cfg(windows)]
    #[test]
    fn decode_should_keep_rooted_windows_path_untouched() {
        let codec = PathCodec::new(Some(PathBuf::from(r"C:\data")));
        assert_eq!(
            codec.decode("/elsewhere/c.mp3"),
            PathBuf::from("/elsewhere/c.mp3")
        );
    }
}
