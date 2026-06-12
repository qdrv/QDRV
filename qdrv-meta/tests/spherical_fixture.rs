// Author: Michael Lauzon <qdrv2026@gmail.com>
// SPDX-License-Identifier: GPL-2.0-only
#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Checked-in spherical (360°/immersive) metadata fixture for roadmap item 1.
//!
//! Validates that the canonical spherical `ObjectMeta` fixture parses, passes
//! `validate()`, round-trips losslessly through JSON, and resolves a
//! forward-facing coordinate to its region — without depending on runtime
//! generation.

use qdrv_meta::ObjectMeta;

/// The fixture is embedded at compile time, so a missing or moved file is a
/// build error rather than a silent skip.
const FIXTURE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../test-vectors/spherical-region.objectmeta.json"
));

#[test]
fn spherical_fixture_parses_validates_and_resolves() {
    let meta: ObjectMeta = qdrv_meta::from_json(FIXTURE).expect("fixture must parse");

    // One spherical region, no flat regions.
    assert!(meta.regions.is_empty());
    assert_eq!(meta.spherical_regions.len(), 1);
    assert_eq!(meta.spherical_regions[0].id, 1);

    // The fixture is a valid ObjectMeta.
    meta.validate().expect("fixture must validate");

    // JSON round-trip is lossless.
    let json = qdrv_meta::to_json(&meta).expect("serialise");
    let recovered: ObjectMeta = qdrv_meta::from_json(&json).expect("re-parse");
    assert_eq!(meta, recovered);

    // The region is centred frame-forward (azimuth 0, elevation 0): a forward
    // coordinate resolves to it; the antipodal coordinate does not.
    assert!(meta.resolve_spherical_curve_at(0.0, 0.0).is_some());
    assert!(
        meta.resolve_spherical_curve_at(std::f32::consts::PI, 0.0)
            .is_none()
    );
}
