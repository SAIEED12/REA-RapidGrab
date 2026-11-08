//! Proxy configuration and the shared `reqwest` client builder.
//!
//! Every request in RapidGrab goes through a client built here, so the proxy
//! choice made in the GUI or on the command line applies to the capability
//! probe, all range workers, the single-stream fallback and media streams.
//!
//! `reqwest` would otherwise pick up `HTTP_PROXY`/`HTTPS_PROXY` from the
//! environment. That is deliberately disabled: [`ProxyMode::Off`] must really
//! mean "direct", and the user-visible setting must win over a stray variable.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::error::{Error, Result};

/// Where the proxy address comes from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProxyMode {
    /// Never use a proxy, even when the environment defines one.
    #[default]
    Off,
    /// Ask Windows (WinHTTP) for the current user's proxy, including PAC/WPAD.
    System,
    /// Use [`ProxyConfig::url`] exactly as entered.
    Manual,
}

/// Username/password for HTTP Basic authentication (origin or proxy).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Credentials {
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub password: String,
}

impl Credentials {
    pub fn new(username: impl Into<String>, password: impl Into<String>) -> Self {
        Self {
            username: username.into(),
            password: password.into(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.username.trim().is_empty()
    }
}

/// Proxy settings shared by the GUI, CLI and media resolver.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProxyConfig {
    #[serde(default)]
    pub mode: ProxyMode,
    /// Manual endpoint: `host:port`, `http://host:port`, `socks5://host:port`.
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub password: String,
    /// Host suffixes that bypass the proxy, separated by commas, spaces or
    /// semicolons (`localhost, *.internal`).
    #[serde(default)]
    pub no_proxy: String,
    /// Explicit PAC URL. When set it overrides the system `AutoConfigURL`.
    #[serde(default)]
    pub pac_url: String,
}

impl ProxyConfig {
    pub fn is_off(&self) -> bool {
        self.mode == ProxyMode::Off
    }
}

/// Network-layer settings for one download.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkConfig {
    #[serde(default)]
    pub proxy: ProxyConfig,
    /// Origin credentials (HTTP Basic). Empty means "ask the user".
    #[serde(default)]
    pub credentials: Option<Credentials>,
}

impl NetworkConfig {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn proxy(mut self, proxy: ProxyConfig) -> Self {
        self.proxy = proxy;
        self
    }

    pub fn credentials(mut self, credentials: Credentials) -> Self {
        self.credentials = (!credentials.is_empty()).then_some(credentials);
        self
    }

    /// Build the client used for `url`.
    ///
    /// `connections` sizes the idle connection pool, exactly like the previous
    /// `pool_max_idle_per_host` setting in the engine.
    pub async fn client_for(&self, url: &str, connections: usize) -> Result<reqwest::Client> {
        let resolved = self.resolve(url).await?;

        let mut builder = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(20))
            .pool_max_idle_per_host(connections);

        match resolved {
            Some(proxy) => {
                let mut reqwest_proxy =
                    reqwest::Proxy::all(proxy.url.as_str()).map_err(|e| {
                        Error::Proxy(format!("invalid proxy address {:?}: {e}", proxy.url))
                    })?;
                if let Some(user) = proxy.username.as_deref() {
                    reqwest_proxy = reqwest_proxy.basic_auth(user, proxy.password.as_deref().unwrap_or(""));
                }
                let bypass = proxy
                    .no_proxy
                    .as_deref()
                    .and_then(reqwest::NoProxy::from_string);
                reqwest_proxy = reqwest_proxy.no_proxy(bypass);
                debug!(proxy = %proxy.url, bypass = ?proxy.no_proxy, "using proxy");
                builder = builder.proxy(reqwest_proxy);
            }
            None => {
                // Also cancels reqwest's automatic environment-variable proxy.
                builder = builder.no_proxy();
            }
        }

        Ok(builder.build()?)
    }

    /// Resolve the configured mode to a concrete endpoint for `url`.
    pub async fn resolve(&self, url: &str) -> Result<Option<ResolvedProxy>> {
        match self.proxy.mode {
            ProxyMode::Off => Ok(None),
            ProxyMode::Manual => self.manual_proxy().map(Some),
            ProxyMode::System => {
                let pac = non_empty(&self.proxy.pac_url).map(str::to_string);
                let target = url.to_string();
                let resolved = tokio::task::spawn_blocking(move || system_proxy(&target, pac.as_deref()))
                    .await
                    .map_err(|e| Error::Proxy(format!("proxy lookup failed: {e}")))?;
                Ok(resolved.map(|endpoint| ResolvedProxy {
                    username: non_empty(&self.proxy.username).map(str::to_string),
                    password: Some(self.proxy.password.clone()),
                    no_proxy: merge_bypass(endpoint.no_proxy.as_deref(), &self.proxy.no_proxy),
                    url: endpoint.url,
                }))
            }
        }
    }

    fn manual_proxy(&self) -> Result<ResolvedProxy> {
        let raw = self.proxy.url.trim();
        if raw.is_empty() {
            return Err(Error::Proxy(
                "proxy mode is Manual but no proxy address is set".into(),
            ));
        }
        let url = normalize_proxy_url(raw);
        // Parse up front so a typo fails before the download starts.
        reqwest::Url::parse(&url)
            .map_err(|e| Error::Proxy(format!("invalid proxy address {raw:?}: {e}")))?;
        Ok(ResolvedProxy {
            url,
            username: non_empty(&self.proxy.username).map(str::to_string),
            password: Some(self.proxy.password.clone()),
            no_proxy: merge_bypass(None, &self.proxy.no_proxy),
        })
    }
}

/// A proxy endpoint after mode resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedProxy {
    pub url: String,
    pub username: Option<String>,
    pub password: Option<String>,
    pub no_proxy: Option<String>,
}

fn non_empty(value: &str) -> Option<&str> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then_some(trimmed)
}

/// Map a 401/407 response to [`Error::AuthRequired`] so the caller can prompt
/// for credentials instead of showing a bare status code.
pub(crate) fn auth_required(resp: &reqwest::Response, url: &str) -> Option<Error> {
    use reqwest::header::{PROXY_AUTHENTICATE, WWW_AUTHENTICATE};

    let status = resp.status();
    let proxy = match status {
        reqwest::StatusCode::UNAUTHORIZED => false,
        reqwest::StatusCode::PROXY_AUTHENTICATION_REQUIRED => true,
        _ => return None,
    };
    let challenge = if proxy {
        PROXY_AUTHENTICATE
    } else {
        WWW_AUTHENTICATE
    };
    let realm = resp
        .headers()
        .get(challenge)
        .and_then(|value| value.to_str().ok())
        .map(parse_realm)
        .filter(|realm| !realm.is_empty());

    Some(Error::AuthRequired {
        status: status.as_u16(),
        host: host_of(url),
        realm,
        proxy,
    })
}

fn host_of(url: &str) -> String {
    reqwest::Url::parse(url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_string))
        .unwrap_or_else(|| url.to_string())
}

/// `Basic realm="Secure Area", charset="UTF-8"` -> `Secure Area`.
fn parse_realm(challenge: &str) -> String {
    let lower = challenge.to_ascii_lowercase();
    let Some(position) = lower.find("realm=") else {
        return challenge.to_string();
    };
    let rest = challenge[position + "realm=".len()..].trim();
    let rest = rest.strip_prefix('"').unwrap_or(rest);
    rest.split('"').next().unwrap_or(rest).to_string()
}

/// Accept `host:port` as a shorthand for an HTTP proxy and normalise
/// case-insensitive SOCKS schemes.
fn normalize_proxy_url(raw: &str) -> String {
    let lower = raw.to_ascii_lowercase();
    if lower.starts_with("socks5h://")
        || lower.starts_with("socks5://")
        || lower.starts_with("socks4://")
        || lower.starts_with("http://")
        || lower.starts_with("https://")
    {
        return raw.to_string();
    }
    if lower.starts_with("socks") {
        // Any other `socks…` spelling is treated as SOCKS5.
        return format!("socks5://{}", raw.split_once("://").map(|(_, rest)| rest).unwrap_or(raw));
    }
    format!("http://{raw}")
}

fn merge_bypass(a: Option<&str>, b: &str) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    for source in [a.unwrap_or(""), b].into_iter() {
        for part in source.split([',', ';', ' ', '\t']) {
            let part = part.trim();
            if !part.is_empty() && !parts.iter().any(|p| p == part) {
                parts.push(part.to_string());
            }
        }
    }
    (!parts.is_empty()).then(|| parts.join(","))
}

/// A WinHTTP result before user credentials are attached.
struct SystemProxy {
    url: String,
    no_proxy: Option<String>,
}

/// Resolve the current user's WinHTTP proxy for `url`.
///
/// Returns `None` when Windows has no proxy for this URL (direct connection).
#[cfg(windows)]
fn system_proxy(url: &str, pac_override: Option<&str>) -> Option<SystemProxy> {
    use windows_sys::Win32::Networking::WinHttp::{
        WinHttpGetIEProxyConfigForCurrentUser, WINHTTP_CURRENT_USER_IE_PROXY_CONFIG,
    };

    unsafe {
        let mut config = WINHTTP_CURRENT_USER_IE_PROXY_CONFIG::default();
        if WinHttpGetIEProxyConfigForCurrentUser(&mut config) == 0 {
            return None;
        }
        let auto_url = take_wide(config.lpszAutoConfigUrl);
        let static_proxy = take_wide(config.lpszProxy);
        let static_bypass = take_wide(config.lpszProxyBypass);
        let auto_detect = config.fAutoDetect != 0;

        let pac = pac_override.map(str::to_string).or(auto_url);
        let (spec, bypass) = if let Some(pac) = pac.as_deref() {
            match winhttp_proxy_for_url(Some(pac), false, url) {
                Some((spec, bypass)) => (Some(spec), bypass.or(static_bypass)),
                None => (static_proxy, static_bypass),
            }
        } else if auto_detect {
            match winhttp_proxy_for_url(None, true, url) {
                Some((spec, bypass)) => (Some(spec), bypass.or(static_bypass)),
                None => (static_proxy, static_bypass),
            }
        } else {
            (static_proxy, static_bypass)
        };

        let spec = spec?;
        let url = proxy_spec_to_url(&spec, reqwest::Url::parse(url).ok()?.scheme())?;
        debug!(proxy = %url, "WinHTTP proxy resolved");
        Some(SystemProxy {
            url,
            no_proxy: bypass,
        })
    }
}

/// Read a WinHTTP-allocated UTF-16 string and free it.
#[cfg(windows)]
unsafe fn take_wide(ptr: windows_sys::core::PWSTR) -> Option<String> {
    use windows_sys::Win32::Foundation::{GlobalFree, HGLOBAL};

    if ptr.is_null() {
        return None;
    }
    let mut len = 0usize;
    while *ptr.add(len) != 0 {
        len += 1;
    }
    let text = String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len));
    GlobalFree(ptr as HGLOBAL);
    (!text.is_empty()).then_some(text)
}

#[cfg(windows)]
fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Evaluate a PAC URL (or WPAD auto-detection) through WinHTTP.
#[cfg(windows)]
unsafe fn winhttp_proxy_for_url(
    pac_url: Option<&str>,
    auto_detect: bool,
    url: &str,
) -> Option<(String, Option<String>)> {
    use windows_sys::Win32::Networking::WinHttp::{
        WinHttpCloseHandle, WinHttpGetProxyForUrl, WinHttpOpen, WinHttpSetTimeouts,
        WINHTTP_ACCESS_TYPE_NO_PROXY, WINHTTP_AUTOPROXY_AUTO_DETECT, WINHTTP_AUTOPROXY_CONFIG_URL,
        WINHTTP_AUTOPROXY_OPTIONS, WINHTTP_AUTO_DETECT_TYPE_DHCP, WINHTTP_AUTO_DETECT_TYPE_DNS_A,
        WINHTTP_PROXY_INFO,
    };

    if pac_url.is_none() && !auto_detect {
        return None;
    }
    let agent = wide("RapidGrab/1.0");
    let session = WinHttpOpen(
        agent.as_ptr(),
        WINHTTP_ACCESS_TYPE_NO_PROXY,
        std::ptr::null(),
        std::ptr::null(),
        0,
    );
    if session.is_null() {
        return None;
    }
    // Keep a dead WPAD lookup from stalling the download for a minute.
    WinHttpSetTimeouts(session, 5_000, 5_000, 5_000, 5_000);

    let mut options = WINHTTP_AUTOPROXY_OPTIONS::default();
    let pac_wide = pac_url.map(wide);
    if let Some(pac) = pac_wide.as_ref() {
        options.dwFlags = WINHTTP_AUTOPROXY_CONFIG_URL;
        options.lpszAutoConfigUrl = pac.as_ptr();
    } else {
        options.dwFlags = WINHTTP_AUTOPROXY_AUTO_DETECT;
        options.dwAutoDetectFlags = WINHTTP_AUTO_DETECT_TYPE_DHCP | WINHTTP_AUTO_DETECT_TYPE_DNS_A;
    }
    options.fAutoLogonIfChallenged = 1;

    let target = wide(url);
    let mut info = WINHTTP_PROXY_INFO::default();
    let ok = WinHttpGetProxyForUrl(session, target.as_ptr(), &mut options, &mut info);
    let proxy = take_wide(info.lpszProxy);
    let bypass = take_wide(info.lpszProxyBypass);
    let _ = WinHttpCloseHandle(session);

    if ok == 0 {
        return None;
    }
    proxy.map(|proxy| (proxy, bypass))
}

#[cfg(not(windows))]
fn system_proxy(_url: &str, _pac_override: Option<&str>) -> Option<SystemProxy> {
    // Only Windows system proxy configuration is supported.
    None
}

/// Turn a WinHTTP proxy string into a reqwest proxy URL.
///
/// The string is either a single `host:port` or a per-scheme list such as
/// `http=proxy:8080;https=proxy:8443`. PAC results may also contain a
/// `DIRECT`/`PROXY host:port` form; only a plain endpoint is accepted.
fn proxy_spec_to_url(spec: &str, scheme: &str) -> Option<String> {
    let spec = spec.trim();
    if spec.is_empty() {
        return None;
    }
    if spec.contains('=') {
        let mut http = None;
        let mut https = None;
        for part in spec.split(';') {
            let (key, value) = part.split_once('=')?;
            let value = value.trim();
            if value.is_empty() {
                continue;
            }
            match key.trim().to_ascii_lowercase().as_str() {
                "http" => http = Some(value),
                "https" => https = Some(value),
                _ => {}
            }
        }
        let chosen = if scheme.eq_ignore_ascii_case("https") {
            https.or(http)?
        } else {
            http.or(https)?
        };
        return Some(with_http_scheme(chosen));
    }
    // PAC files and the IE config may include a `PROXY` prefix.
    let spec = spec
        .strip_prefix("PROXY ")
        .or_else(|| spec.strip_prefix("proxy "))
        .unwrap_or(spec);
    if spec.eq_ignore_ascii_case("DIRECT") {
        return None;
    }
    Some(with_http_scheme(spec))
}

fn with_http_scheme(endpoint: &str) -> String {
    let endpoint = endpoint.trim();
    if endpoint.contains("://") {
        endpoint.to_string()
    } else {
        format!("http://{endpoint}")
    }
}
