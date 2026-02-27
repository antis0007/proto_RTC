//! UI layer for desktop GUI: app shell, panels, widgets, themes, and layout primitives.

pub mod app;
pub mod layout;
pub mod panels;
pub mod theme;
pub mod widgets;

pub use app::{AppPaths, DesktopGuiApp, StartupConfig};
