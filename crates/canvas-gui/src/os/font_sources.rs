//! The fonts every OS module names (`crate::os::fonts::sources` of each OS module).

/// The font files and folders the GUI loads, and its preferred families.
pub struct FontSources {
    pub files: Vec<std::path::PathBuf>,
    pub dirs: Vec<std::path::PathBuf>,
    /// Grid font families, in order of preference.
    pub mono: &'static [&'static str],
    /// Label font families, in order of preference.
    pub ui: &'static [&'static str],
}
