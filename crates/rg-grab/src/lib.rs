//! Site Grabber crawler (Phase 5).
//!
//! [`scan`] breadth-first walks a site starting from one page, reports every
//! download-looking link it finds ([`GrabFile`]) and every page it fetched
//! ([`GrabEvent::Page`]). It is deliberately independent of the GUI so it can
//! be unit-tested against a local HTTP server.
//!
//! Design notes:
//! * Fetching reuses [`rg_core::NetworkConfig::client_for`], so proxy and
//!   origin-credential settings behave exactly like downloads.
//! * Only `<a>`/`<img>`/`<video>`/… attributes and absolute URLs in inline
//!   script JSON are inspected; there is no JavaScript execution, so SPA
//!   pages that build their links client-side expose nothing (documented
//!   limitation).
//! * Pages are fetched at most four at a time with a small polite delay;
//!   `robots.txt` is honoured when [`GrabRules::respect_robots`] is set.

pub mod crawl;
pub mod extract;

pub use crawl::{
    extensions_from, preset_names, scan, GrabEvent, GrabFile, GrabReport, GrabRules, LinkKind,
    RobotsRules, ScanOptions, PRESETS,
};
pub use extract::extract_urls;
