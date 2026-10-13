//! Per-device stream quality, picked in the viewer's menu. Each device has
//! its own encoder, so one device's choice never affects another's. Every
//! new connection starts on Smooth.

/// A quality preset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Preset {
    /// 30 fps at half the bitrate: easier on the device's battery and Wi-Fi.
    Saver,
    /// 60 fps at the automatic bitrate (the default).
    Smooth,
    /// 60 fps at 1.5× the bitrate, for text-heavy work on good Wi-Fi.
    Sharp,
    /// 60 fps at 3× the bitrate, for a strong Wi-Fi 6 or wired connection.
    Ultra,
}

/// The highest bitrate any preset asks for (kbit/s).
const MAX_KBPS: u32 = 80_000;
const MIN_KBPS: u32 = 2_000;

impl Preset {
    pub fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "saver" => Self::Saver,
            "smooth" => Self::Smooth,
            "sharp" => Self::Sharp,
            "ultra" => Self::Ultra,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Saver => "saver",
            Self::Smooth => "smooth",
            Self::Sharp => "sharp",
            Self::Ultra => "ultra",
        }
    }

    /// Frame-rate cap, if the preset has one.
    pub fn max_fps(self) -> Option<u32> {
        match self {
            Self::Saver => Some(30),
            _ => None,
        }
    }

    /// The bitrate for this preset, given the automatic (Smooth) one.
    pub fn kbps(self, base: u32) -> u32 {
        let factor = match self {
            Self::Saver => 0.5,
            Self::Smooth => 1.0,
            Self::Sharp => 1.5,
            Self::Ultra => 3.0,
        };
        ((base as f64 * factor) as u32).clamp(MIN_KBPS, MAX_KBPS.max(base))
    }

    /// What to set on the encoder to get `kbps(base)` on the wire. Encoders
    /// budget per frame from the capture's nominal 60 fps, so with every other
    /// frame dropped the encoder needs twice the target.
    pub fn encoder_kbps(self, base: u32) -> u32 {
        match self.max_fps() {
            Some(fps) if fps < 60 => self.kbps(base) * 60 / fps,
            _ => self.kbps(base),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_at_1080p() {
        let base = 12_000;
        assert_eq!(Preset::Saver.kbps(base), 6_000);
        assert_eq!(Preset::Smooth.kbps(base), 12_000);
        assert_eq!(Preset::Sharp.kbps(base), 18_000);
        assert_eq!(Preset::Ultra.kbps(base), 36_000);
        assert_eq!(Preset::Saver.max_fps(), Some(30));
        assert_eq!(Preset::Ultra.max_fps(), None);
        assert_eq!(Preset::Saver.encoder_kbps(base), 12_000);
        assert_eq!(Preset::Sharp.encoder_kbps(base), 18_000);
    }

    #[test]
    fn limits() {
        // 4K: Ultra stops at the cap.
        assert_eq!(Preset::Ultra.kbps(34_000), 80_000);
        // A fixed --bitrate above the cap is left alone on Smooth.
        assert_eq!(Preset::Smooth.kbps(100_000), 100_000);
        assert_eq!(Preset::Saver.kbps(3_000), 2_000);
        for p in ["saver", "smooth", "sharp", "ultra"] {
            assert_eq!(Preset::parse(p).unwrap().name(), p);
        }
        assert_eq!(Preset::parse("4k"), None);
    }
}
