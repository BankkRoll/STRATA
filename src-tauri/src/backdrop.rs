//! Window backdrop selection (Mica on Windows 11, solid elsewhere).

use serde::Serialize;
use tauri::window::{Effect, EffectsBuilder};

/// First Windows build with the Mica material (Windows 11 21H2).
const MICA_MIN_BUILD: u32 = 22000;

/// Which backdrop the main window ended up with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Backdrop {
    /// Windows 11 Mica; the frontend paints a translucent background.
    Mica,
    /// No system material; the frontend paints an opaque background.
    Solid,
}

/// Returns the running Windows build number, or 0 off Windows.
pub fn windows_build() -> u32 {
    #[cfg(windows)]
    {
        windows_version::OsVersion::current().build
    }
    #[cfg(not(windows))]
    {
        0
    }
}

/// Picks the backdrop for a given Windows build.
pub fn choose(build: u32) -> Backdrop {
    if build >= MICA_MIN_BUILD {
        Backdrop::Mica
    } else {
        Backdrop::Solid
    }
}

/// Applies the best available backdrop to `window` and reports what stuck.
///
/// Falls back to [`Backdrop::Solid`] if the effect call fails, so the
/// frontend never leaves a transparent window unpainted.
pub fn apply(window: &tauri::WebviewWindow) -> Backdrop {
    match choose(windows_build()) {
        Backdrop::Mica => {
            let effects = EffectsBuilder::new().effect(Effect::Mica).build();
            match window.set_effects(effects) {
                Ok(()) => Backdrop::Mica,
                Err(_) => Backdrop::Solid,
            }
        }
        Backdrop::Solid => Backdrop::Solid,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_10_gets_solid() {
        assert_eq!(choose(19045), Backdrop::Solid);
    }

    #[test]
    fn windows_11_gets_mica() {
        assert_eq!(choose(22000), Backdrop::Mica);
        assert_eq!(choose(26200), Backdrop::Mica);
    }

    #[test]
    fn serializes_lowercase() {
        assert_eq!(serde_json::to_string(&Backdrop::Mica).unwrap(), "\"mica\"");
    }
}
