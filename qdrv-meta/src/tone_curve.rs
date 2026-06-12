// Author: Michael Lauzon <qdrv2026@gmail.com>
// SPDX-License-Identifier: GPL-2.0-or-later
//! Tone mapping curve types for QDRV dynamic metadata.
//!
//! The per-frame tone mapping curve is the primary mechanism by which a
//! colourist's creative intent is communicated to a decoder. It is based on
//! the SMPTE ST 2094 dynamic metadata framework, extended to IEEE 754
//! Float32 values rather than the integer representations used by ST 2094-40
//! (HDR10+) and ST 2094-10 (Dolby Vision).
//!
//! ## Curve evaluation
//!
//! Linear curves use piecewise linear interpolation between anchor points.
//!
//! Bézier curves use **monotone cubic Hermite spline interpolation**
//! (Fritsch–Carlson, 1980). This algorithm guarantees that the interpolated
//! curve is monotone — that is, the output never decreases as the input
//! increases — which is a physical requirement for any valid tone mapping
//! curve.

use serde::{Deserialize, Serialize};

const MAX_CURVE_ANCHORS: usize = 32;

/// A single anchor point on a tone mapping curve.
///
/// Both `input` and `output` are normalised PQ signal values in `[0.0, 1.0]`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CurveAnchor {
    /// Normalised input PQ signal value in `[0.0, 1.0]`.
    pub input: f32,
    /// Normalised output PQ signal value in `[0.0, 1.0]`.
    pub output: f32,
}

/// The interpolation method used to evaluate a [`ToneMapCurve`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CurveType {
    /// Monotone cubic Hermite spline through the anchor points (Fritsch–Carlson).
    /// Produces a smooth curve that is guaranteed to be monotone between anchors.
    Bezier,
    /// Piecewise linear interpolation between the anchor points.
    Linear,
}

/// A per-frame tone mapping curve based on the SMPTE ST 2094 framework.
///
/// The curve maps a normalised input PQ signal value to a normalised output
/// PQ signal value. It expresses the colourist's creative intent for a
/// specific reference display (typically 1 000 nits peak). Display-side
/// decoders adapt this curve to the actual target display's capabilities at
/// runtime.
///
/// All anchor values are stored as IEEE 754 Float32, extending the ST 2094
/// model beyond its integer constraints.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToneMapCurve {
    /// Interpolation method used to evaluate the curve.
    #[serde(rename = "type")]
    pub curve_type: CurveType,
    /// Anchor points that define the curve shape.
    /// Must contain at least two points, including `(0.0, 0.0)` at the low
    /// end and `(1.0, 1.0)` at the high end. A maximum of 32 anchors is
    /// permitted.
    pub anchors: Vec<CurveAnchor>,
}

impl ToneMapCurve {
    /// Validates a raw anchor slice for the shared QDRV tone-curve shape
    /// rules. This is the single source of truth invoked by
    /// [`ToneMapCurve::from_anchors`], [`crate::DynamicMeta::validate`], and
    /// [`crate::ObjectMeta::validate`] — previously each of those sites
    /// hand-inlined an independent copy of the same checks (audit finding
    /// J-1), so this consolidation prevents the three copies from drifting.
    ///
    /// Rules enforced (`endpoints_required = true`):
    /// - anchor count in `2..=32`
    /// - all `input` / `output` values finite and in `[0.0, 1.0]`
    /// - `anchors[0].input ≈ 0.0` and `anchors[last].input ≈ 1.0`
    /// - inputs strictly increasing
    ///
    /// With `endpoints_required = false`, the endpoint-pinning rule is
    /// skipped so [`ToneMapCurve::from_anchors`] can accept any in-range
    /// anchor set for direct construction (matching the pre-consolidation
    /// behaviour of that constructor).
    pub fn validate_anchors_shape(
        anchors: &[CurveAnchor],
        endpoints_required: bool,
    ) -> Result<(), &'static str> {
        if anchors.len() < 2 || anchors.len() > MAX_CURVE_ANCHORS {
            return Err("tone map curve must contain 2..=32 anchors");
        }
        for a in anchors {
            if !a.input.is_finite() || !a.output.is_finite() {
                return Err("tone map anchor values must be finite");
            }
            if !(0.0..=1.0).contains(&a.input) || !(0.0..=1.0).contains(&a.output) {
                return Err("tone map anchor values must be in [0.0, 1.0]");
            }
        }
        if endpoints_required {
            if anchors[0].input.abs() > f32::EPSILON {
                return Err("tone map curve must start at input=0.0");
            }
            if (anchors[anchors.len() - 1].input - 1.0).abs() > f32::EPSILON {
                return Err("tone map curve must end at input=1.0");
            }
        }
        for pair in anchors.windows(2) {
            if pair[0].input >= pair[1].input {
                return Err("tone map anchor inputs must be strictly increasing");
            }
        }
        Ok(())
    }

    /// Creates a tone map curve from caller-provided anchors after validating
    /// shape and range invariants required by evaluators.
    pub fn from_anchors(
        curve_type: CurveType,
        anchors: Vec<CurveAnchor>,
    ) -> Result<Self, &'static str> {
        // `endpoints_required = false` preserves the original
        // `from_anchors` behaviour, which accepts any well-formed in-range
        // anchor list including ones that do not pin endpoints at 0.0/1.0
        // (e.g. the built-in `linear()` curve constructs via the strict
        // endpoint path below).
        Self::validate_anchors_shape(&anchors, false)?;
        Ok(Self {
            curve_type,
            anchors,
        })
    }

    /// Creates a linear (identity) tone map curve. No tone mapping is applied;
    /// the output equals the input at every point.
    ///
    /// The `.expect` below is on a compile-time-fixed anchor table that
    /// trivially satisfies the `from_anchors` invariants (two anchors,
    /// strictly increasing inputs in [0, 1], finite outputs). It is
    /// considered infallible by construction; the workspace's
    /// `expect_used = "warn"` lint is suppressed here for that reason
    /// (audit L-04).
    #[allow(clippy::expect_used)]
    pub fn linear() -> Self {
        Self::from_anchors(
            CurveType::Linear,
            vec![
                CurveAnchor {
                    input: 0.0,
                    output: 0.0,
                },
                CurveAnchor {
                    input: 1.0,
                    output: 1.0,
                },
            ],
        )
        .expect("built-in linear tone curve anchors must remain valid")
    }

    /// Creates a default Bézier tone map curve targeting a 1 000-nit reference
    /// display. The curve applies a gentle S-shaped roll-off in the upper
    /// highlights to prevent perceptual clipping on real-world displays.
    ///
    /// As with [`ToneMapCurve::linear`], the `.expect` below is on a
    /// compile-time-fixed anchor table that trivially satisfies the
    /// `from_anchors` invariants (five strictly increasing anchors in
    /// `[0, 1]`); the `expect_used` lint is suppressed here on that
    /// basis (audit L-04).
    #[allow(clippy::expect_used)]
    pub fn default_1000nit() -> Self {
        Self::from_anchors(
            CurveType::Bezier,
            vec![
                CurveAnchor {
                    input: 0.00,
                    output: 0.00,
                },
                CurveAnchor {
                    input: 0.25,
                    output: 0.22,
                },
                CurveAnchor {
                    input: 0.50,
                    output: 0.48,
                },
                CurveAnchor {
                    input: 0.75,
                    output: 0.74,
                },
                CurveAnchor {
                    input: 1.00,
                    output: 1.00,
                },
            ],
        )
        .expect("built-in 1000-nit tone curve anchors must remain valid")
    }

    /// Evaluates the tone curve at a given normalised input PQ signal value.
    ///
    /// The evaluation method is determined by [`CurveType`]:
    /// - [`CurveType::Linear`] — piecewise linear interpolation.
    /// - [`CurveType::Bezier`] — monotone cubic Hermite spline (Fritsch–Carlson).
    ///
    /// The input value is clamped to `[0.0, 1.0]` before evaluation. The
    /// output is also clamped to `[0.0, 1.0]` to ensure a valid PQ signal.
    pub fn evaluate(&self, input: f32) -> f32 {
        let input = input.clamp(0.0, 1.0);
        let mut anchors = self.anchors.as_slice();

        if anchors.len() < 2 {
            return input;
        }
        if anchors.len() > MAX_CURVE_ANCHORS {
            // Curves are spec-limited to 32 anchors; clamp evaluation work to
            // that bound so malformed metadata cannot force unbounded CPU work.
            anchors = &anchors[..MAX_CURVE_ANCHORS];
        }

        match self.curve_type {
            CurveType::Linear => Self::eval_linear(anchors, input),
            CurveType::Bezier => Self::eval_monotone_cubic(anchors, input),
        }
    }

    // -------------------------------------------------------------------------
    // Private evaluation helpers
    // -------------------------------------------------------------------------

    /// Evaluates the curve using piecewise linear interpolation.
    ///
    /// The result is clamped to `[0.0, 1.0]` before returning so the
    /// guarantee made by [`ToneMapCurve::evaluate`]'s contract holds for
    /// both interpolation modes. The Bezier path applies its own clamp; the
    /// linear path's clamp here covers the case where a caller has mutated
    /// the public `anchors` field to contain an output outside `[0, 1]`
    /// (audit finding E-1).
    fn eval_linear(anchors: &[CurveAnchor], input: f32) -> f32 {
        for i in 0..anchors.len() - 1 {
            let lo = &anchors[i];
            let hi = &anchors[i + 1];
            if input >= lo.input && input <= hi.input {
                let span = hi.input - lo.input;
                if span < f32::EPSILON {
                    return lo.output.clamp(0.0, 1.0);
                }
                let t = (input - lo.input) / span;
                return (lo.output + t * (hi.output - lo.output)).clamp(0.0, 1.0);
            }
        }
        anchors
            .last()
            .map(|a| a.output.clamp(0.0, 1.0))
            .unwrap_or(1.0)
    }

    /// Evaluates the curve using a monotone cubic Hermite spline.
    ///
    /// This implements the Fritsch–Carlson (1980) algorithm, which constructs
    /// cubic Hermite segments between anchor points with tangents chosen to
    /// guarantee monotonicity. Monotonicity is a hard requirement for a
    /// physically correct tone mapping curve: the output luminance must never
    /// decrease as the input luminance increases.
    ///
    /// ## Algorithm outline
    ///
    /// 1. Compute the chord slope `Δk = (y_{k+1} − y_k) / (x_{k+1} − x_k)`
    ///    for each interval.
    /// 2. Initialise tangents as the average of adjacent chord slopes at
    ///    interior points; use the adjacent chord slope at the endpoints.
    /// 3. Apply the Fritsch–Carlson monotonicity correction: if the ratio
    ///    `α² + β² > 9` (where `α = d_k / Δk` and `β = d_{k+1} / Δk`),
    ///    scale both tangents by `3 / √(α² + β²)`.
    /// 4. Evaluate using the standard cubic Hermite basis polynomials.
    fn eval_monotone_cubic(anchors: &[CurveAnchor], input: f32) -> f32 {
        let n = anchors.len();

        // Handle the two-point degenerate case with direct linear interpolation.
        if n == 2 {
            return Self::eval_linear(anchors, input);
        }
        debug_assert!(n <= MAX_CURVE_ANCHORS);

        // Find the segment that contains the input value.
        let seg = anchors
            .partition_point(|a| a.input < input)
            .saturating_sub(1)
            .min(n - 2);

        let x0 = anchors[seg].input;
        let x1 = anchors[seg + 1].input;
        let y0 = anchors[seg].output;
        let y1 = anchors[seg + 1].output;

        let h = x1 - x0;
        if h < f32::EPSILON {
            return y0;
        }

        // Step 1: Compute chord slopes for every interval.
        let mut slopes = [0.0_f32; MAX_CURVE_ANCHORS - 1];
        for i in 0..n - 1 {
            let dx = anchors[i + 1].input - anchors[i].input;
            if dx > f32::EPSILON {
                slopes[i] = (anchors[i + 1].output - anchors[i].output) / dx;
            }
        }

        // Step 2: Initialise tangents. Endpoints use the adjacent chord slope;
        // interior points use the average of the two neighbouring chord slopes.
        let mut tangents = [0.0_f32; MAX_CURVE_ANCHORS];
        tangents[0] = slopes[0];
        tangents[n - 1] = slopes[n - 2];
        for i in 1..n - 1 {
            tangents[i] = (slopes[i - 1] + slopes[i]) * 0.5;
        }

        // Step 3: Apply the Fritsch–Carlson monotonicity conditions.
        for i in 0..n - 1 {
            if slopes[i].abs() < f32::EPSILON {
                // A zero chord slope requires zero tangents at both endpoints
                // of this interval to prevent the spline from overshooting.
                tangents[i] = 0.0;
                tangents[i + 1] = 0.0;
            } else {
                let alpha = tangents[i] / slopes[i];
                let beta = tangents[i + 1] / slopes[i];
                let sq_norm = alpha * alpha + beta * beta;
                if sq_norm > 9.0 {
                    // Scale both tangents to satisfy the monotonicity bound.
                    let tau = 3.0 / sq_norm.sqrt();
                    tangents[i] *= tau;
                    tangents[i + 1] *= tau;
                }
            }
        }

        // Step 4: Evaluate using the cubic Hermite basis polynomials.
        let t = (input - x0) / h;
        let t2 = t * t;
        let t3 = t2 * t;

        let h00 = 2.0 * t3 - 3.0 * t2 + 1.0; // Basis for p0
        let h10 = t3 - 2.0 * t2 + t; // Basis for m0 (scaled tangent)
        let h01 = -2.0 * t3 + 3.0 * t2; // Basis for p1
        let h11 = t3 - t2; // Basis for m1 (scaled tangent)

        (h00 * y0 + h10 * h * tangents[seg] + h01 * y1 + h11 * h * tangents[seg + 1])
            .clamp(0.0, 1.0)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_linear_identity() {
        // A linear identity curve must map every input to the same output.
        let curve = ToneMapCurve::linear();
        assert!((curve.evaluate(0.0) - 0.0).abs() < 1e-6);
        assert!((curve.evaluate(0.5) - 0.5).abs() < 1e-6);
        assert!((curve.evaluate(1.0) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn test_default_curve_endpoints() {
        // The default 1 000-nit Bézier curve must map 0.0 → 0.0 and 1.0 → 1.0
        // exactly, as required by the anchor constraints.
        let curve = ToneMapCurve::default_1000nit();
        assert!((curve.evaluate(0.0) - 0.0).abs() < 1e-6);
        assert!((curve.evaluate(1.0) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn test_curve_clamps_out_of_range() {
        // Out-of-range inputs must be clamped to the valid signal range before
        // evaluation; the output must remain in [0.0, 1.0].
        let curve = ToneMapCurve::linear();
        assert!((curve.evaluate(-0.5) - 0.0).abs() < 1e-6);
        assert!((curve.evaluate(1.5) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn test_bezier_monotonicity() {
        // The monotone cubic Hermite spline must never produce a lower output
        // for a higher input, even at intermediate points between anchors.
        let curve = ToneMapCurve::default_1000nit();
        let mut prev = -1.0_f32;
        for i in 0..=100 {
            let x = i as f32 / 100.0;
            let val = curve.evaluate(x);
            assert!(
                val >= prev - 1e-6,
                "Curve is not monotone: evaluate({x}) = {val} < previous {prev}"
            );
            prev = val;
        }
    }

    #[test]
    fn test_bezier_smooth_midpoints() {
        // The Bézier (monotone cubic Hermite) curve must produce values that
        // differ from the linear interpolant, confirming that smooth
        // interpolation is being used rather than piecewise linear fallback.
        let bezier = ToneMapCurve::default_1000nit();
        let linear = ToneMapCurve {
            curve_type: CurveType::Linear,
            anchors: bezier.anchors.clone(),
        };
        // At an intermediate point, the smooth curve must differ from the
        // linear interpolant by at least a small amount.
        let x = 0.375_f32;
        let bezier_val = bezier.evaluate(x);
        let linear_val = linear.evaluate(x);
        let diff = (bezier_val - linear_val).abs();
        assert!(
            diff > 1e-4,
            "Bézier and linear curves are identical at x={x}; smooth interpolation \
             may not be active (diff={diff:.6})"
        );
    }

    #[test]
    fn test_evaluate_caps_anchor_count() {
        // Malformed metadata must not force unbounded evaluation work.
        let anchors: Vec<CurveAnchor> = (0..40)
            .map(|i| {
                let x = i as f32 / 39.0;
                CurveAnchor {
                    input: x,
                    output: x,
                }
            })
            .collect();
        let curve = ToneMapCurve {
            curve_type: CurveType::Bezier,
            anchors,
        };

        let y = curve.evaluate(0.5);
        assert!(y.is_finite());
        assert!((0.0..=1.0).contains(&y));
    }

    #[test]
    fn test_from_anchors_rejects_unsorted_inputs() {
        let anchors = vec![
            CurveAnchor {
                input: 0.0,
                output: 0.0,
            },
            CurveAnchor {
                input: 0.7,
                output: 0.7,
            },
            CurveAnchor {
                input: 0.4,
                output: 0.4,
            },
            CurveAnchor {
                input: 1.0,
                output: 1.0,
            },
        ];
        assert!(ToneMapCurve::from_anchors(CurveType::Linear, anchors).is_err());
    }
}
