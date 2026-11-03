use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("manifest error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("server did not return a usable response (status {0})")]
    BadStatus(u16),
    #[error("server does not support range requests")]
    NoRangeSupport,
    #[error("remote file changed since the partial download was created")]
    RemoteChanged,
    #[error("cannot resume: {0}")]
    NotResumable(String),
    /// The server answered with an HTML document; this is a web page, not a file.
    #[error(
        "refusing to save a web page: server returned {content_type} for {url}; \
         use an extractor such as yt-dlp for video pages"
    )]
    WebPage {
        content_type: String,
        url: String,
    },
    /// The caller asked the download to stop; partial state is kept for resume.
    #[error("download cancelled")]
    Cancelled,
    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, Error>;
