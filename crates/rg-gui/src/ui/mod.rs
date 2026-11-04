//! Presentation layer: design tokens, theme application and reusable widgets.
//!
//! Nothing in here touches download logic or persisted state.

pub mod theme;
pub mod widgets;

pub use theme::apply_theme;
