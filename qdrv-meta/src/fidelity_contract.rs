// Author: Michael Lauzon <qdrv2026@gmail.com>
// SPDX-License-Identifier: GPL-2.0-or-later
//! Fidelity contract schemas and evaluation helpers.

use serde::{Deserialize, Serialize};

/// Contract thresholds used to gate transcoding quality regressions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FidelityContract {
    /// Minimum PSNR in dB.
    #[serde(default)]
    pub psnr_db_min: Option<f64>,
    /// Minimum SSIM (0..=1).
    #[serde(default)]
    pub ssim_min: Option<f64>,
    /// Maximum CIE DeltaE76.
    #[serde(default)]
    pub delta_e_max: Option<f64>,
    /// Optional minimum VMAF-HDR value.
    #[serde(default)]
    pub vmaf_hdr_min: Option<f64>,
}

/// Measured fidelity metrics for a candidate output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MeasuredFidelity {
    /// Measured PSNR in dB.
    #[serde(default)]
    pub psnr_db: Option<f64>,
    /// Measured SSIM.
    #[serde(default)]
    pub ssim: Option<f64>,
    /// Measured DeltaE76.
    #[serde(default)]
    pub delta_e: Option<f64>,
    /// Measured VMAF-HDR (if available).
    #[serde(default)]
    pub vmaf_hdr: Option<f64>,
}

/// Outcome of evaluating a contract against measured metrics.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FidelityContractResult {
    /// True if all configured thresholds passed.
    pub passed: bool,
    /// Human-readable failures.
    pub failures: Vec<String>,
}

impl FidelityContract {
    /// Evaluates this contract against measured metrics.
    pub fn evaluate(&self, measured: &MeasuredFidelity) -> FidelityContractResult {
        let mut failures = Vec::new();

        if let Some(min_psnr) = self.psnr_db_min {
            match measured.psnr_db {
                Some(actual) if actual >= min_psnr => {}
                Some(actual) => failures.push(format!(
                    "PSNR regression: measured {actual:.4} dB below required {min_psnr:.4} dB"
                )),
                None => {
                    failures.push("PSNR threshold configured but metric unavailable".to_string())
                }
            }
        }

        if let Some(min_ssim) = self.ssim_min {
            match measured.ssim {
                Some(actual) if actual >= min_ssim => {}
                Some(actual) => failures.push(format!(
                    "SSIM regression: measured {actual:.6} below required {min_ssim:.6}"
                )),
                None => {
                    failures.push("SSIM threshold configured but metric unavailable".to_string())
                }
            }
        }

        if let Some(max_delta_e) = self.delta_e_max {
            match measured.delta_e {
                Some(actual) if actual <= max_delta_e => {}
                Some(actual) => failures.push(format!(
                    "DeltaE regression: measured {actual:.6} above allowed {max_delta_e:.6}"
                )),
                None => {
                    failures.push("DeltaE threshold configured but metric unavailable".to_string())
                }
            }
        }

        if let Some(min_vmaf_hdr) = self.vmaf_hdr_min {
            match measured.vmaf_hdr {
                Some(actual) if actual >= min_vmaf_hdr => {}
                Some(actual) => failures.push(format!(
                    "VMAF-HDR regression: measured {actual:.4} below required {min_vmaf_hdr:.4}"
                )),
                None => failures.push(
                    "VMAF-HDR threshold configured but no in-repo evaluator is available"
                        .to_string(),
                ),
            }
        }

        FidelityContractResult {
            passed: failures.is_empty(),
            failures,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contract_reports_missing_vmaf_metric() {
        let contract = FidelityContract {
            psnr_db_min: None,
            ssim_min: None,
            delta_e_max: None,
            vmaf_hdr_min: Some(90.0),
        };
        let measured = MeasuredFidelity {
            psnr_db: Some(45.0),
            ssim: Some(0.99),
            delta_e: Some(1.0),
            vmaf_hdr: None,
        };
        let result = contract.evaluate(&measured);
        assert!(!result.passed);
        assert_eq!(result.failures.len(), 1);
    }
}
