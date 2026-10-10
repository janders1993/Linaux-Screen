//! Resizing KDE's virtual monitor to match the viewing device, using
//! `kscreen-doctor` (part of KDE Plasma).
//!
//! The portal creates the virtual output at a default size. kscreen-doctor can
//! register a custom mode on it and switch to it, and set the scale.

use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use tokio::process::Command;

#[derive(Debug, Deserialize)]
struct Screen {
    outputs: Vec<Output>,
}

#[derive(Debug, Deserialize)]
struct Output {
    name: String,
    #[serde(default)]
    modes: Vec<Mode>,
}

#[derive(Debug, Deserialize)]
struct Mode {
    id: serde_json::Value,
    size: Size,
    #[serde(rename = "refreshRate", default)]
    refresh_rate: f64,
}

#[derive(Debug, Deserialize)]
struct Size {
    width: u32,
    height: u32,
}

async fn screen() -> Result<Screen> {
    let out = Command::new("kscreen-doctor")
        .arg("-j")
        .output()
        .await
        .context("could not run kscreen-doctor (is this KDE Plasma?)")?;
    if !out.status.success() {
        bail!("kscreen-doctor -j failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    serde_json::from_slice(&out.stdout).context("could not parse kscreen-doctor -j output")
}

/// Names of the current outputs, e.g. `["DP-1", "HDMI-A-1"]`.
pub async fn output_names() -> Result<Vec<String>> {
    Ok(screen().await?.outputs.into_iter().map(|o| o.name).collect())
}

/// Runs kscreen-doctor and returns its combined output.
async fn kscreen_output(args: &[String]) -> Result<String> {
    tracing::debug!("kscreen-doctor {}", args.join(" "));
    let out = Command::new("kscreen-doctor").args(args).output().await?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    if !out.status.success() {
        bail!("kscreen-doctor {} failed: {}", args.join(" "), text.trim());
    }
    Ok(text)
}

async fn kscreen(args: &[String]) -> Result<()> {
    kscreen_output(args).await.map(|_| ())
}

/// Our virtual monitor, as kscreen-doctor knows it.
pub struct VirtualDisplay {
    name: String,
}

impl VirtualDisplay {
    /// Finds the output that appeared since `before` was taken. KWin may take
    /// a moment to publish it, so this polls for a couple of seconds.
    pub async fn find_new(before: &[String]) -> Result<Self> {
        for _ in 0..20 {
            let now = output_names().await?;
            let new: Vec<_> = now.into_iter().filter(|n| !before.contains(n)).collect();
            match new.as_slice() {
                [name] => {
                    tracing::info!("virtual monitor is output {name}");
                    return Ok(Self { name: name.clone() });
                }
                [] => tokio::time::sleep(Duration::from_millis(100)).await,
                many => bail!("several new outputs appeared ({many:?}); not sure which is ours"),
            }
        }
        Err(anyhow!("no new output appeared after starting the virtual monitor"))
    }

    /// Sets the monitor to `width`×`height` pixels at 60 Hz, scaled by
    /// `scale`.
    pub async fn apply(&self, width: u32, height: u32, scale: f64) -> Result<()> {
        let name = &self.name;
        let mut mode = self.find_mode(width, height).await?;
        if mode.is_none() {
            let out = kscreen_output(&[format!(
                "output.{name}.addCustomMode.{width}.{height}.60000.full"
            )])
            .await?;
            if !out.trim().is_empty() {
                tracing::info!("kscreen-doctor addCustomMode said: {}", out.trim());
            }
            // KWin can take a moment to publish the new mode.
            for _ in 0..10 {
                mode = self.find_mode(width, height).await?;
                if mode.is_some() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
        // Fall back to selecting the mode by name if it never showed up in
        // the mode list.
        let mode = mode.unwrap_or_else(|| format!("{width}x{height}@60"));
        if let Err(e) = kscreen(&[format!("output.{name}.mode.{mode}")]).await {
            let available = self.mode_sizes().await.unwrap_or_default();
            bail!("{e:#}. Modes KDE lists for this monitor: {available}");
        }
        kscreen(&[format!("output.{name}.scale.{scale}")]).await?;
        tracing::info!("virtual monitor set to {width}x{height} at scale {scale}");
        Ok(())
    }

    /// The monitor's modes, for error messages.
    async fn mode_sizes(&self) -> Result<String> {
        let screen = screen().await?;
        let Some(output) = screen.outputs.into_iter().find(|o| o.name == self.name) else {
            return Ok("(monitor not found)".into());
        };
        let list: Vec<String> = output
            .modes
            .iter()
            .map(|m| format!("{}x{}@{:.0}", m.size.width, m.size.height, m.refresh_rate))
            .collect();
        Ok(if list.is_empty() { "(none)".into() } else { list.join(", ") })
    }

    /// The id of an existing mode with this size, preferring ~60 Hz.
    async fn find_mode(&self, width: u32, height: u32) -> Result<Option<String>> {
        let screen = screen().await?;
        let output = screen
            .outputs
            .into_iter()
            .find(|o| o.name == self.name)
            .ok_or_else(|| anyhow!("virtual monitor {} has gone", self.name))?;
        let mut matching: Vec<_> = output
            .modes
            .into_iter()
            .filter(|m| m.size.width == width && m.size.height == height)
            .collect();
        matching.sort_by(|a, b| {
            (a.refresh_rate - 60.0)
                .abs()
                .total_cmp(&(b.refresh_rate - 60.0).abs())
        });
        Ok(matching.into_iter().next().map(|m| match m.id {
            serde_json::Value::String(s) => s,
            other => other.to_string(),
        }))
    }
}

/// Picks a size and scale for a device's screen.
///
/// The size is the device's physical pixels, rounded to even numbers (H.264
/// needs them) and capped so the long edge stays within what hardware
/// encoders handle. The scale follows the device's pixel density so text is
/// the size you'd expect on that device, in quarter steps.
pub fn plan(width: u32, height: u32, dpr: f64) -> (u32, u32, f64) {
    const MAX_EDGE: u32 = 4096;
    let long = width.max(height).max(1);
    let shrink = if long > MAX_EDGE { MAX_EDGE as f64 / long as f64 } else { 1.0 };
    let even = |v: u32| (((v as f64 * shrink) as u32) / 2 * 2).max(2);
    let scale = ((dpr * shrink).clamp(1.0, 4.0) * 4.0).round() / 4.0;
    (even(width), even(height), scale)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phone_portrait() {
        // iPhone-ish: 390x844 CSS px at 3x.
        assert_eq!(plan(1170, 2532, 3.0), (1170, 2532, 3.0));
    }

    #[test]
    fn odd_sizes_round_down_to_even() {
        assert_eq!(plan(1081, 2401, 2.6), (1080, 2400, 2.5));
    }

    #[test]
    fn huge_screens_are_capped() {
        let (w, h, s) = plan(2560 * 2, 1600 * 2, 2.0);
        assert!(w <= 4096 && h <= 4096);
        assert_eq!(w % 2, 0);
        assert!(s >= 1.0);
    }

    #[test]
    fn low_density_never_below_scale_1() {
        assert_eq!(plan(1280, 800, 0.75), (1280, 800, 1.0));
    }

    #[test]
    fn parses_kscreen_json() {
        let json = r#"{"outputs":[{"name":"Virtual-1","enabled":true,
            "modes":[{"id":"1","name":"1920x1080@60","refreshRate":60.0,
            "size":{"width":1920,"height":1080}}]}]}"#;
        let s: Screen = serde_json::from_str(json).unwrap();
        assert_eq!(s.outputs[0].modes[0].size.width, 1920);
    }
}
