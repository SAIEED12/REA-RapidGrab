use reqwest::header::{
    ACCEPT_RANGES, CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_RANGE, COOKIE, ETAG, LAST_MODIFIED,
    RANGE, USER_AGENT,
};
use reqwest::StatusCode;

use crate::error::{Error, Result};

/// Result of the capability probe: what the server will let us do.
#[derive(Debug, Clone)]
pub struct Probe {
    pub final_url: String,
    pub total: u64,
    pub accept_ranges: bool,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub content_type: Option<String>,
    pub content_disposition: Option<String>,
}

fn header_str(resp: &reqwest::Response, name: reqwest::header::HeaderName) -> Option<String> {
    resp.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
}

fn content_length_header(resp: &reqwest::Response) -> u64 {
    header_str(resp, CONTENT_LENGTH)
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(0)
}

/// Probe a URL: prefer HEAD, validate advertised ranges, and fall back to
/// `GET Range: bytes=0-0`.
///
/// This mirrors the capability discovery IDM performs with its first plain GET,
/// but avoids downloading a body when the server supports HEAD.
pub async fn probe(client: &reqwest::Client, url: &str, user_agent: &str) -> Result<Probe> {
    probe_with(client, url, user_agent, None).await
}

/// Like [`probe`], with an optional `Cookie` header for browser captures.
pub async fn probe_with(
    client: &reqwest::Client,
    url: &str,
    user_agent: &str,
    cookie: Option<&str>,
) -> Result<Probe> {
    let mut head = client.head(url).header(USER_AGENT, user_agent);
    if let Some(c) = cookie {
        head = head.header(COOKIE, c);
    }
    let head = head.send().await;

    if let Ok(resp) = head {
        if resp.status().is_success() {
            let total = content_length_header(&resp);
            let accept = header_str(&resp, ACCEPT_RANGES)
                .map(|v| v.to_ascii_lowercase().contains("bytes"))
                .unwrap_or(false);
            if total > 0 {
                if accept {
                    let mut range_probe = client
                        .get(url)
                        .header(USER_AGENT, user_agent)
                        .header(RANGE, "bytes=0-0");
                    if let Some(c) = cookie {
                        range_probe = range_probe.header(COOKIE, c);
                    }
                    let range_probe = range_probe.send().await?;
                    if range_probe.status() != StatusCode::PARTIAL_CONTENT {
                        return Ok(Probe {
                            final_url: resp.url().to_string(),
                            total,
                            accept_ranges: false,
                            etag: header_str(&resp, ETAG),
                            last_modified: header_str(&resp, LAST_MODIFIED),
                            content_type: header_str(&resp, reqwest::header::CONTENT_TYPE),
                            content_disposition: header_str(&resp, CONTENT_DISPOSITION),
                        });
                    }
                }
                return Ok(Probe {
                    final_url: resp.url().to_string(),
                    total,
                    accept_ranges: accept,
                    etag: header_str(&resp, ETAG),
                    last_modified: header_str(&resp, LAST_MODIFIED),
                    content_type: header_str(&resp, reqwest::header::CONTENT_TYPE),
                    content_disposition: header_str(&resp, CONTENT_DISPOSITION),
                });
            }
        }
    }

    // Fallback: one-byte ranged GET. Some servers reject HEAD or omit Content-Length.
    let mut get = client
        .get(url)
        .header(USER_AGENT, user_agent)
        .header(RANGE, "bytes=0-0");
    if let Some(c) = cookie {
        get = get.header(COOKIE, c);
    }
    let resp = get.send().await?;

    let status = resp.status();
    if status != StatusCode::PARTIAL_CONTENT && !status.is_success() {
        return Err(Error::BadStatus(status.as_u16()));
    }

    let mut total = 0u64;
    let mut accept = status == StatusCode::PARTIAL_CONTENT;
    if let Some(cr) = header_str(&resp, CONTENT_RANGE) {
        // e.g. "bytes 0-0/123456"
        if let Some(slash) = cr.rfind('/') {
            if let Ok(n) = cr[slash + 1..].trim().parse::<u64>() {
                total = n;
            }
        }
    }
    if total == 0 {
        total = content_length_header(&resp);
    }
    if let Some(ar) = header_str(&resp, ACCEPT_RANGES) {
        accept = ar.to_ascii_lowercase().contains("bytes");
    }

    Ok(Probe {
        final_url: resp.url().to_string(),
        total,
        accept_ranges: accept,
        etag: header_str(&resp, ETAG),
        last_modified: header_str(&resp, LAST_MODIFIED),
        content_type: header_str(&resp, reqwest::header::CONTENT_TYPE),
        content_disposition: header_str(&resp, CONTENT_DISPOSITION),
    })
}
