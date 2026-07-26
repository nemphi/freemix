use core::fmt;

use crate::Rgb;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ToneMapPolicy {
    None,
    Clip,
    /// Luminance-preserving extended Reinhard mapping.
    Reinhard {
        source_peak_nits: f32,
        target_peak_nits: f32,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToneMapError {
    InvalidPeakLuminance,
    NonFiniteSample,
}

impl fmt::Display for ToneMapError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPeakLuminance => {
                formatter.write_str("tone-map peak luminance must be finite and positive")
            }
            Self::NonFiniteSample => formatter.write_str("tone-map sample must be finite"),
        }
    }
}

impl std::error::Error for ToneMapError {}

/// Tone maps linear BT.2020 RGB. Working value `1.0` represents 100 cd/m2.
///
/// # Errors
///
/// Returns an error for a non-finite sample or invalid Reinhard peak value.
pub fn tone_map_rgb(rgb: Rgb, policy: ToneMapPolicy) -> Result<Rgb, ToneMapError> {
    if !rgb.r.is_finite() || !rgb.g.is_finite() || !rgb.b.is_finite() {
        return Err(ToneMapError::NonFiniteSample);
    }
    match policy {
        ToneMapPolicy::None => Ok(rgb),
        ToneMapPolicy::Clip => Ok(rgb.map(|value| value.clamp(0.0, 1.0))),
        ToneMapPolicy::Reinhard {
            source_peak_nits,
            target_peak_nits,
        } => {
            if !source_peak_nits.is_finite()
                || !target_peak_nits.is_finite()
                || source_peak_nits <= 0.0
                || target_peak_nits <= 0.0
            {
                return Err(ToneMapError::InvalidPeakLuminance);
            }
            let rgb = rgb.map(|value| value.max(0.0));
            let luminance = 0.2627 * rgb.r + 0.6780 * rgb.g + 0.0593 * rgb.b;
            if luminance == 0.0 {
                return Ok(Rgb::default());
            }
            let target_scale = target_peak_nits / 100.0;
            let relative = luminance / target_scale;
            let white = source_peak_nits / target_peak_nits;
            let mapped = relative * (1.0 + relative / (white * white)) / (1.0 + relative);
            let scale = mapped * target_scale / luminance;
            Ok(rgb.map(|value| value * scale))
        }
    }
}
