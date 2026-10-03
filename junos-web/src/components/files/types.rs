//! Files tab: the JSON shapes returned by `/api/files/*`, and the list's
//! sort / filter choices.

use serde::Deserialize;
use serde_json::Value;

use super::planning::parse::Kind;
use super::utils::{is_fits_ext, is_image_ext, is_jpg_ext};

/// One entry of `GET /api/files/list`.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub(super) struct DirEntry {
    pub(super) name: String,
    pub(super) kind: String,
    #[serde(default)]
    pub(super) size: u64,
    #[serde(default)]
    pub(super) mtime: u64,
    #[serde(default)]
    pub(super) ext: String,
}

/// Reply of `GET /api/files/list`. Mirrors the server's JSON, so `parent` is
/// decoded but unused.
#[allow(dead_code)]
#[derive(Clone, Debug, Deserialize)]
pub(super) struct ListReply {
    pub(super) path: String,
    pub(super) parent: Option<String>,
    pub(super) entries: Vec<DirEntry>,
}

/// Reply of `GET /api/files/meta`. Mirrors the server's JSON, so `ext` is
/// decoded but unused.
#[allow(dead_code)]
#[derive(Clone, Debug, Deserialize)]
pub(super) struct FileMeta {
    pub(super) name: String,
    #[serde(default)]
    pub(super) size: u64,
    #[serde(default)]
    pub(super) mtime: u64,
    #[serde(default)]
    pub(super) ext: String,
    pub(super) fits: Option<FitsInfo>,
}

#[derive(Clone, Debug, Deserialize)]
pub(super) struct FitsInfo {
    pub(super) header: Vec<FitsRow>,
    pub(super) parsed: Value,
}

#[derive(Clone, Debug, Deserialize)]
pub(super) struct FitsRow {
    pub(super) key: String,
    pub(super) value: String,
    pub(super) comment: String,
}

#[derive(Clone, Debug, Deserialize)]
pub(super) struct ResolveReply {
    #[serde(default)]
    pub(super) in_sandbox: bool,
    #[serde(default)]
    pub(super) relative: String,
    #[serde(default)]
    pub(super) parent: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SortKey {
    Name,
    Date,
    Size,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SortDir {
    Asc,
    Desc,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum FilterKind {
    Images,
    Fits,
    Jpg,
    /// Schedules and sequences (a mosaic import writes them here).
    Planning,
    All,
}

impl SortKey {
    /// Newest first unless the user picked otherwise.
    pub(super) fn from_storage(v: Option<String>) -> Self {
        match v.as_deref() {
            Some("name") => Self::Name,
            Some("size") => Self::Size,
            _ => Self::Date,
        }
    }

    pub(super) fn storage(self) -> &'static str {
        match self {
            Self::Name => "name",
            Self::Date => "date",
            Self::Size => "size",
        }
    }
}

impl SortDir {
    pub(super) fn from_storage(v: Option<String>) -> Self {
        if v.as_deref() == Some("asc") { Self::Asc } else { Self::Desc }
    }

    pub(super) fn storage(self) -> &'static str {
        match self {
            Self::Asc => "asc",
            Self::Desc => "desc",
        }
    }
}

impl FilterKind {
    pub(super) const ALL: [Self; 5] = [Self::Images, Self::Fits, Self::Jpg, Self::Planning, Self::All];

    pub(super) fn from_storage(v: Option<String>) -> Self {
        match v.as_deref() {
            Some("all") => Self::All,
            Some("fits") => Self::Fits,
            Some("jpg") => Self::Jpg,
            Some("planning") => Self::Planning,
            _ => Self::Images,
        }
    }

    pub(super) fn storage(self) -> &'static str {
        match self {
            Self::Images => "images",
            Self::Fits => "fits",
            Self::Jpg => "jpg",
            Self::Planning => "planning",
            Self::All => "all",
        }
    }

    pub(super) fn accepts(self, ext: &str) -> bool {
        match self {
            Self::Images => is_image_ext(ext),
            Self::Fits => is_fits_ext(ext),
            Self::Jpg => is_jpg_ext(ext),
            Self::Planning => Kind::from_ext(ext).is_some(),
            Self::All => true,
        }
    }
}
