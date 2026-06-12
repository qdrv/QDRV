// Author: Michael Lauzon <qdrv2026@gmail.com>
// SPDX-License-Identifier: GPL-2.0-or-later
//! Error types for all `qdrv-core` operations.
//!
//! Every fallible function in `qdrv-core` returns a [`Result`] using the
//! [`QdrvError`] enumeration defined here. Callers that require more granular
//! handling should match on the individual variants.

use thiserror::Error;

/// All errors that can be produced by `qdrv-core` operations.
#[derive(Debug, Error)]
pub enum QdrvError {
    /// A luminance value expressed in nits was outside the valid SMPTE ST 2084
    /// PQ range of `[0.0, 10 000.0]` cd/m².
    #[error("luminance {0} nits is outside the valid ST 2084 PQ range [0.0, 10000.0]")]
    LuminanceOutOfRange(f64),

    /// A normalised PQ signal value was outside the valid range of `[0.0, 1.0]`.
    #[error("PQ signal value {0} is outside the normalised range [0.0, 1.0]")]
    PqSignalOutOfRange(f64),

    /// A pixel channel contained a non-finite value (NaN or infinity).
    /// QDRV pixel data must be finite at all stages of the pipeline.
    #[error("pixel channel value {0} is not finite (NaN or Inf)")]
    NonFiniteValue(f64),
}

/// Convenience `Result` alias for all `qdrv-core` operations.
pub type Result<T> = std::result::Result<T, QdrvError>;
