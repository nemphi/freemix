use core::fmt;

use fm_types::TransferFunction;

const PQ_M1: f32 = 0.159_301_76;
const PQ_M2: f32 = 78.843_75;
const PQ_C1: f32 = 0.835_937_5;
const PQ_C2: f32 = 18.851_563;
const PQ_C3: f32 = 18.687_5;
const HLG_A: f32 = 0.178_832_77;
const HLG_B: f32 = 0.284_668_92;
const HLG_C: f32 = 0.559_910_7;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransferError {
    NonFiniteSample,
}

impl fmt::Display for TransferError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NonFiniteSample => formatter.write_str("transfer sample must be finite"),
        }
    }
}

impl std::error::Error for TransferError {}

/// Decodes a transfer-encoded sample to linear light.
///
/// PQ output is absolute luminance normalized to 10,000 cd/m2. HLG output is
/// scene-linear relative light. Other curves return reference-white-relative
/// linear light.
///
/// # Errors
///
/// Returns [`TransferError::NonFiniteSample`] for a NaN or infinite sample.
pub fn decode_transfer(transfer: TransferFunction, encoded: f32) -> Result<f32, TransferError> {
    finite(encoded)?;
    let encoded = encoded.clamp(0.0, 1.0);
    Ok(match transfer {
        TransferFunction::Linear => encoded,
        TransferFunction::Srgb => srgb_to_linear(encoded),
        TransferFunction::Bt709 => bt709_to_linear(encoded),
        TransferFunction::Bt1886 => encoded.powf(2.4),
        TransferFunction::Hlg => hlg_to_linear(encoded),
        TransferFunction::Pq => pq_to_linear(encoded),
    })
}

/// Encodes linear light using the requested transfer function.
///
/// PQ input uses the same normalized absolute-luminance representation as
/// [`decode_transfer`]. Values outside the representable signal are clipped.
///
/// # Errors
///
/// Returns [`TransferError::NonFiniteSample`] for a NaN or infinite sample.
pub fn encode_transfer(transfer: TransferFunction, linear: f32) -> Result<f32, TransferError> {
    finite(linear)?;
    let linear = linear.clamp(0.0, 1.0);
    Ok(match transfer {
        TransferFunction::Linear => linear,
        TransferFunction::Srgb => srgb_from_linear(linear),
        TransferFunction::Bt709 => bt709_from_linear(linear),
        TransferFunction::Bt1886 => linear.powf(1.0 / 2.4),
        TransferFunction::Hlg => hlg_from_linear(linear),
        TransferFunction::Pq => pq_from_linear(linear),
    })
}

#[must_use]
pub fn srgb_to_linear(encoded: f32) -> f32 {
    if encoded <= 0.040_45 {
        encoded / 12.92
    } else {
        ((encoded + 0.055) / 1.055).powf(2.4)
    }
}

#[must_use]
pub fn srgb_from_linear(linear: f32) -> f32 {
    if linear <= 0.003_130_8 {
        12.92 * linear
    } else {
        1.055 * linear.powf(1.0 / 2.4) - 0.055
    }
}

/// Inverse BT.709 OETF.
#[must_use]
pub fn bt709_to_linear(encoded: f32) -> f32 {
    if encoded <= 0.081 {
        encoded / 4.5
    } else {
        ((encoded + 0.099) / 1.099).powf(1.0 / 0.45)
    }
}

/// BT.709 OETF.
#[must_use]
pub fn bt709_from_linear(linear: f32) -> f32 {
    if linear < 0.018 {
        4.5 * linear
    } else {
        1.099 * linear.powf(0.45) - 0.099
    }
}

/// SMPTE ST 2084 EOTF, returning luminance divided by 10,000 cd/m2.
#[must_use]
pub fn pq_to_linear(encoded: f32) -> f32 {
    let power = encoded.max(0.0).powf(1.0 / PQ_M2);
    let numerator = (power - PQ_C1).max(0.0);
    let denominator = PQ_C2 - PQ_C3 * power;
    (numerator / denominator).powf(1.0 / PQ_M1)
}

/// SMPTE ST 2084 inverse EOTF, taking luminance divided by 10,000 cd/m2.
#[must_use]
pub fn pq_from_linear(linear: f32) -> f32 {
    let power = linear.max(0.0).powf(PQ_M1);
    ((PQ_C1 + PQ_C2 * power) / (1.0 + PQ_C3 * power)).powf(PQ_M2)
}

/// ARIB STD-B67 inverse OETF, returning scene-linear relative light.
#[must_use]
pub fn hlg_to_linear(encoded: f32) -> f32 {
    if encoded <= 0.5 {
        encoded * encoded / 3.0
    } else {
        (((encoded - HLG_C) / HLG_A).exp() + HLG_B) / 12.0
    }
}

/// ARIB STD-B67 OETF, taking scene-linear relative light.
#[must_use]
pub fn hlg_from_linear(linear: f32) -> f32 {
    if linear <= 1.0 / 12.0 {
        (3.0 * linear.max(0.0)).sqrt()
    } else {
        HLG_A * (12.0 * linear - HLG_B).ln() + HLG_C
    }
}

fn finite(value: f32) -> Result<(), TransferError> {
    if value.is_finite() {
        Ok(())
    } else {
        Err(TransferError::NonFiniteSample)
    }
}
