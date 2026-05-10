use serde::Deserialize;
use serde_json::Value;

#[derive(Clone, Debug, Deserialize)]
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

#[derive(Clone, Debug, Deserialize)]
pub(super) struct ListReply {
    pub(super) path: String,
    pub(super) parent: Option<String>,
    pub(super) entries: Vec<DirEntry>,
}

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

#[derive(Clone, Debug)]
pub(super) struct FileMenuState {
    pub(super) rel: String,
    pub(super) anchor_x: f64,
    pub(super) anchor_y: f64,
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
    All,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum LiveStackTab {
    Preview,
    Controls,
    Settings,
}

impl SortKey {
    pub(super) fn from_storage(v: Option<String>) -> Self {
        match v.as_deref() {
            Some("date") => Self::Date,
            Some("size") => Self::Size,
            _ => Self::Name,
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
        if v.as_deref() == Some("asc") {
            Self::Asc
        } else {
            Self::Desc
        }
    }

    pub(super) fn storage(self) -> &'static str {
        match self {
            Self::Asc => "asc",
            Self::Desc => "desc",
        }
    }
}

impl FilterKind {
    pub(super) fn from_storage(v: Option<String>) -> Self {
        match v.as_deref() {
            Some("all") => Self::All,
            Some("fits") => Self::Fits,
            Some("jpg") => Self::Jpg,
            _ => Self::Images,
        }
    }

    pub(super) fn storage(self) -> &'static str {
        match self {
            Self::Images => "images",
            Self::Fits => "fits",
            Self::Jpg => "jpg",
            Self::All => "all",
        }
    }
}

impl LiveStackTab {
    pub(super) fn from_storage(v: Option<String>) -> Self {
        match v.as_deref() {
            Some("controls") => Self::Controls,
            Some("settings") => Self::Settings,
            _ => Self::Preview,
        }
    }

    pub(super) fn storage(self) -> &'static str {
        match self {
            Self::Preview => "preview",
            Self::Controls => "controls",
            Self::Settings => "settings",
        }
    }
}
