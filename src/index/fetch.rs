//! Reading index files from an [`IndexSource`]: from a directory, or over
//! the sparse protocol with an on-disk cache revalidated through ETag /
//! Last-Modified.
//!
//! A cache file holds a header and the index file as served:
//!
//! ```text
//! resolvo-cargo index cache 1
//! found                      (or `not-found` for a crate the registry lacks)
//! etag: "..."                (optional)
//! last-modified: ...         (optional)
//!
//! <the index file>
//! ```

use std::{
    io,
    path::{Path, PathBuf},
};

use reqwest::{
    Client, StatusCode,
    header::{ETAG, HeaderMap, IF_MODIFIED_SINCE, IF_NONE_MATCH, LAST_MODIFIED},
};
use url::Url;

use super::IndexSource;
use crate::{error::IndexError, names::PackageName};

/// The first line of every cache file; a file starting otherwise is
/// ignored and refetched.
const CACHE_MAGIC: &str = "resolvo-cargo index cache 1";

/// The user agent sent to registries.
const USER_AGENT: &str = concat!("resolvo-cargo/", env!("CARGO_PKG_VERSION"));

/// The path of a crate's index file relative to the index root, cargo's
/// `make_dep_path`: `1/a`, `2/ab`, `3/a/abc`, `se/rd/serde`. `name` must
/// already be lowercased.
pub(super) fn index_path(name: &str) -> String {
    let characters: Vec<char> = name.chars().collect();
    let prefix = |range: std::ops::Range<usize>| -> String {
        characters.get(range).unwrap_or_default().iter().collect()
    };
    match characters.len() {
        1 => format!("1/{name}"),
        2 => format!("2/{name}"),
        3 => format!("3/{}/{name}", prefix(0..1)),
        _ => format!("{}/{}/{name}", prefix(0..2), prefix(2..4)),
    }
}

/// Reads index files from one [`IndexSource`].
pub(super) enum Fetcher {
    /// Index files under a directory.
    Directory(PathBuf),
    /// A sparse registry, fetched and cached.
    Online {
        /// Shared by every request of the load.
        client: Client,
        /// The index root.
        url: Url,
        /// Where fetched files are cached.
        cache_dir: PathBuf,
    },
    /// A sparse registry's cache only.
    Offline {
        /// Where fetched files were cached.
        cache_dir: PathBuf,
    },
}

impl Fetcher {
    /// Prepares reading from `source`; builds the HTTP client of an online
    /// sparse registry.
    ///
    /// # Errors
    ///
    /// [`IndexError::Client`] when the HTTP client cannot be built.
    pub(super) fn new(source: &IndexSource) -> Result<Fetcher, IndexError> {
        match source {
            IndexSource::Directory(root) => Ok(Fetcher::Directory(root.clone())),
            IndexSource::Sparse {
                cache_dir,
                offline: true,
                ..
            } => Ok(Fetcher::Offline {
                cache_dir: cache_dir.clone(),
            }),
            IndexSource::Sparse {
                url,
                cache_dir,
                offline: false,
            } => {
                let client = Client::builder()
                    .user_agent(USER_AGENT)
                    .build()
                    .map_err(IndexError::Client)?;
                Ok(Fetcher::Online {
                    client,
                    url: url.clone(),
                    cache_dir: cache_dir.clone(),
                })
            }
        }
    }

    /// The index file of the crate `name`, or `None` when the registry
    /// has no such crate.
    ///
    /// # Errors
    ///
    /// [`IndexError`] when the file cannot be fetched or read, or, offline,
    /// is not cached.
    pub(super) async fn fetch(&self, name: &PackageName) -> Result<Option<Vec<u8>>, IndexError> {
        let path = index_path(&name.as_str().to_lowercase());
        match self {
            Fetcher::Directory(root) => {
                let file = join_index_path(root, &path);
                let contents = read_optional(&file).await?;
                tracing::debug!(
                    name = %name,
                    path = %file.display(),
                    found = contents.is_some(),
                    "read index file",
                );
                Ok(contents)
            }
            Fetcher::Offline { cache_dir } => {
                let file = join_index_path(cache_dir, &path);
                let cached = read_cache(&file)
                    .await?
                    .ok_or_else(|| IndexError::NotCached { name: name.clone() })?;
                tracing::debug!(
                    name = %name,
                    found = cached.contents.is_some(),
                    "read cached index file offline",
                );
                Ok(cached.contents)
            }
            Fetcher::Online {
                client,
                url,
                cache_dir,
            } => fetch_online(client, url, &join_index_path(cache_dir, &path), name, &path).await,
        }
    }
}

/// Fetches an index file over the sparse protocol, revalidating and
/// updating the cache file `cache_file`.
async fn fetch_online(
    client: &Client,
    root: &Url,
    cache_file: &Path,
    name: &PackageName,
    path: &str,
) -> Result<Option<Vec<u8>>, IndexError> {
    let url = root.join(path).map_err(|source| IndexError::InvalidUrl {
        name: name.clone(),
        root: root.clone(),
        source,
    })?;
    let cached = read_cache(cache_file).await?;

    let mut request = client.get(url.clone());
    // Revalidate only a cached file; a cached absence is simply refetched.
    if let Some(cached) = &cached
        && cached.contents.is_some()
    {
        if let Some(etag) = &cached.etag {
            request = request.header(IF_NONE_MATCH, etag);
        } else if let Some(last_modified) = &cached.last_modified {
            request = request.header(IF_MODIFIED_SINCE, last_modified);
        }
    }
    let fetch_error = |source| IndexError::Fetch {
        name: name.clone(),
        url: url.clone(),
        source,
    };
    let response = request.send().await.map_err(fetch_error)?;
    let status = response.status();
    tracing::debug!(name = %name, url = %url, status = status.as_u16(), "fetched index file");

    match status {
        StatusCode::OK => {
            let validators = Validators::from_headers(response.headers());
            let body = response.bytes().await.map_err(fetch_error)?.to_vec();
            let entry = CacheEntry {
                contents: Some(body),
                etag: validators.etag,
                last_modified: validators.last_modified,
            };
            write_cache(cache_file, &entry).await?;
            Ok(entry.contents)
        }
        StatusCode::NOT_MODIFIED => match cached {
            Some(CacheEntry {
                contents: Some(contents),
                ..
            }) => Ok(Some(contents)),
            Some(CacheEntry { contents: None, .. }) | None => Err(IndexError::Status {
                name: name.clone(),
                url,
                status,
            }),
        },
        StatusCode::NOT_FOUND | StatusCode::GONE | StatusCode::UNAVAILABLE_FOR_LEGAL_REASONS => {
            let entry = CacheEntry {
                contents: None,
                etag: None,
                last_modified: None,
            };
            write_cache(cache_file, &entry).await?;
            Ok(None)
        }
        _ => Err(IndexError::Status {
            name: name.clone(),
            url,
            status,
        }),
    }
}

/// The revalidation headers of a response.
struct Validators {
    etag: Option<String>,
    last_modified: Option<String>,
}

impl Validators {
    fn from_headers(headers: &HeaderMap) -> Validators {
        let text = |name| {
            headers
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty())
        };
        Validators {
            etag: text(ETAG),
            last_modified: text(LAST_MODIFIED),
        }
    }
}

/// A cached response for one index file.
struct CacheEntry {
    /// The index file, or `None` when the registry has no such crate.
    contents: Option<Vec<u8>>,
    /// The `ETag` it was served with.
    etag: Option<String>,
    /// The `Last-Modified` it was served with.
    last_modified: Option<String>,
}

impl CacheEntry {
    /// Parses a cache file; `None` for a file in an unknown format.
    fn decode(bytes: &[u8]) -> Option<CacheEntry> {
        let separator = bytes.windows(2).position(|window| window == b"\n\n")?;
        let (header, rest) = bytes.split_at(separator);
        let body = rest.get(2..)?;
        let header = std::str::from_utf8(header).ok()?;
        let mut lines = header.lines();
        if lines.next()? != CACHE_MAGIC {
            return None;
        }
        let found = match lines.next()? {
            "found" => true,
            "not-found" => false,
            _ => return None,
        };
        let mut entry = CacheEntry {
            contents: found.then(|| body.to_vec()),
            etag: None,
            last_modified: None,
        };
        for line in lines {
            if let Some(etag) = line.strip_prefix("etag: ") {
                entry.etag = Some(etag.to_owned());
            } else if let Some(last_modified) = line.strip_prefix("last-modified: ") {
                entry.last_modified = Some(last_modified.to_owned());
            } else {
                return None;
            }
        }
        Some(entry)
    }

    fn encode(&self) -> Vec<u8> {
        let mut header = format!("{CACHE_MAGIC}\n");
        header.push_str(match self.contents {
            Some(_) => "found\n",
            None => "not-found\n",
        });
        if let Some(etag) = &self.etag {
            header.push_str(&format!("etag: {etag}\n"));
        }
        if let Some(last_modified) = &self.last_modified {
            header.push_str(&format!("last-modified: {last_modified}\n"));
        }
        header.push('\n');
        let mut bytes = header.into_bytes();
        bytes.extend_from_slice(self.contents.as_deref().unwrap_or_default());
        bytes
    }
}

/// Reads the cache file `file`; `None` when it does not exist or is in an
/// unknown format.
async fn read_cache(file: &Path) -> Result<Option<CacheEntry>, IndexError> {
    let Some(bytes) = read_optional(file).await? else {
        return Ok(None);
    };
    let entry = CacheEntry::decode(&bytes);
    if entry.is_none() {
        tracing::debug!(path = %file.display(), "ignoring index cache file in an unknown format");
    }
    Ok(entry)
}

/// Replaces the cache file `file` atomically, so that a concurrent reader
/// sees the old or the new entry, never half of one.
async fn write_cache(file: &Path, entry: &CacheEntry) -> Result<(), IndexError> {
    let io_error = |path: &Path| {
        let path = path.to_owned();
        move |source| IndexError::Io { path, source }
    };
    if let Some(parent) = file.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(io_error(parent))?;
    }
    let mut temporary = file.as_os_str().to_owned();
    temporary.push(format!(".{}.tmp", std::process::id()));
    let temporary = PathBuf::from(temporary);
    tokio::fs::write(&temporary, entry.encode())
        .await
        .map_err(io_error(&temporary))?;
    tokio::fs::rename(&temporary, file)
        .await
        .map_err(io_error(file))
}

/// Reads `file`, `None` when it does not exist.
async fn read_optional(file: &Path) -> Result<Option<Vec<u8>>, IndexError> {
    match tokio::fs::read(file).await {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(IndexError::Io {
            path: file.to_owned(),
            source,
        }),
    }
}

/// `root` joined with a `/`-separated [`index_path`].
fn join_index_path(root: &Path, path: &str) -> PathBuf {
    let mut file = root.to_owned();
    file.extend(path.split('/'));
    file
}
