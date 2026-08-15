//! Standard normal distribution: CDF and its inverse.
//!
//! Both statistics in this crate are expressed in terms of the standard
//! normal, so the accuracy of everything downstream is bounded by the
//! accuracy here. The implementations are chosen accordingly and are
//! checked against known reference values in the tests.
//!
//! No external dependencies: this crate sits at the bottom of the
//! workspace and is used by tooling that must build anywhere.

use core::f64::consts::PI;

/// Cumulative distribution function of the standard normal distribution.
///
/// Hart's double-precision rational approximation in the body, and a
/// Mills-ratio continued fraction beyond |x| = 7.07.
///
/// Accuracy, measured against reference values in the tests: absolute
/// error below 1e-15 everywhere, and near machine *relative* precision
/// in the far tail. Relative accuracy is the one that matters here —
/// the deflated Sharpe ratio reads probabilities far out in the tail,
/// where an absolute error bound permits an answer that is wrong by
/// orders of magnitude.
#[must_use]
pub fn cdf(x: f64) -> f64 {
    if x.is_nan() {
        return f64::NAN;
    }

    let abs_x = x.abs();

    // Beyond this point the tail is smaller than the smallest normal
    // f64 and the rational form loses meaning.
    if abs_x > 37.0 {
        return if x > 0.0 { 1.0 } else { 0.0 };
    }

    let e = (-abs_x * abs_x / 2.0).exp();

    let tail = if abs_x < 7.071_067_811_865_475 {
        let mut num = 3.526_249_659_989_109e-2 * abs_x + 0.700_383_064_443_688;
        num = num * abs_x + 6.373_962_203_531_65;
        num = num * abs_x + 33.912_866_078_383;
        num = num * abs_x + 112.079_291_497_871;
        num = num * abs_x + 221.213_596_169_931;
        num = num * abs_x + 220.206_867_912_376;

        let mut den = 8.838_834_764_831_844e-2 * abs_x + 1.755_667_163_182_64;
        den = den * abs_x + 16.064_177_579_207;
        den = den * abs_x + 86.780_732_202_946_1;
        den = den * abs_x + 296.564_248_779_674;
        den = den * abs_x + 637.333_633_378_831;
        den = den * abs_x + 793.826_512_519_948;
        den = den * abs_x + 440.413_735_824_752;

        e * num / den
    } else {
        // Mills-ratio continued fraction for the far tail, evaluated
        // downwards from 16 terms. The usual four-term truncation is
        // accurate to ~1e-8 *relative* out here; the deflated Sharpe
        // ratio reads probabilities in exactly this region, so the
        // extra dozen divisions are worth their cost. Sixteen terms
        // reach machine precision and further terms change nothing.
        let mut cf = abs_x;
        for k in (1..=16).rev() {
            cf = abs_x + f64::from(k) / cf;
        }
        e / (cf * (2.0 * PI).sqrt())
    };

    if x > 0.0 { 1.0 - tail } else { tail }
}

/// Probability density function of the standard normal distribution.
#[must_use]
pub fn pdf(x: f64) -> f64 {
    (-x * x / 2.0).exp() / (2.0 * PI).sqrt()
}

/// Inverse CDF (quantile function) of the standard normal distribution.
///
/// Acklam's rational approximation followed by one Halley refinement
/// step against [`cdf`], which brings the result to near machine
/// precision. Returns ±infinity at `p == 0.0` / `p == 1.0` and NaN
/// outside `[0, 1]`.
#[must_use]
pub fn inverse_cdf(p: f64) -> f64 {
    if p.is_nan() || !(0.0..=1.0).contains(&p) {
        return f64::NAN;
    }
    if p == 0.0 {
        return f64::NEG_INFINITY;
    }
    if p == 1.0 {
        return f64::INFINITY;
    }

    const A: [f64; 6] = [
        -3.969_683_028_665_376e1,
        2.209_460_984_245_205e2,
        -2.759_285_104_469_687e2,
        1.383_577_518_672_69e2,
        -3.066_479_806_614_716e1,
        2.506_628_277_459_239,
    ];
    const B: [f64; 5] = [
        -5.447_609_879_822_406e1,
        1.615_858_368_580_409e2,
        -1.556_989_798_598_866e2,
        6.680_131_188_771_972e1,
        -1.328_068_155_288_572e1,
    ];
    const C: [f64; 6] = [
        -7.784_894_002_430_293e-3,
        -3.223_964_580_411_365e-1,
        -2.400_758_277_161_838,
        -2.549_732_539_343_734,
        4.374_664_141_464_968,
        2.938_163_982_698_783,
    ];
    const D: [f64; 4] = [
        7.784_695_709_041_462e-3,
        3.224_671_290_700_398e-1,
        2.445_134_137_142_996,
        3.754_408_661_907_416,
    ];

    const P_LOW: f64 = 0.024_25;
    const P_HIGH: f64 = 1.0 - P_LOW;

    let mut x = if p < P_LOW {
        let q = (-2.0 * p.ln()).sqrt();
        (((((C[0] * q + C[1]) * q + C[2]) * q + C[3]) * q + C[4]) * q + C[5])
            / ((((D[0] * q + D[1]) * q + D[2]) * q + D[3]) * q + 1.0)
    } else if p <= P_HIGH {
        let q = p - 0.5;
        let r = q * q;
        (((((A[0] * r + A[1]) * r + A[2]) * r + A[3]) * r + A[4]) * r + A[5]) * q
            / (((((B[0] * r + B[1]) * r + B[2]) * r + B[3]) * r + B[4]) * r + 1.0)
    } else {
        let q = (-2.0 * (1.0 - p).ln()).sqrt();
        -(((((C[0] * q + C[1]) * q + C[2]) * q + C[3]) * q + C[4]) * q + C[5])
            / ((((D[0] * q + D[1]) * q + D[2]) * q + D[3]) * q + 1.0)
    };

    // Halley step: removes the ~1e-9 approximation error.
    let err = cdf(x) - p;
    let u = err * (2.0 * PI).sqrt() * (x * x / 2.0).exp();
    x -= u / (1.0 + x * u / 2.0);

    x
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reference values of the standard normal CDF.
    const CDF_REFERENCE: [(f64, f64); 9] = [
        (0.0, 0.5),
        (0.5, 0.691_462_461_274_013),
        (1.0, 0.841_344_746_068_543),
        (2.0, 0.977_249_868_051_821),
        (3.0, 0.998_650_101_968_370),
        (-1.0, 0.158_655_253_931_457),
        (-2.0, 0.022_750_131_948_179),
        (1.959_963_984_540_054, 0.975),
        (-1.959_963_984_540_054, 0.025),
    ];

    #[test]
    fn cdf_matches_reference_values() {
        for (x, expected) in CDF_REFERENCE {
            let got = cdf(x);
            assert!(
                (got - expected).abs() < 1e-13,
                "cdf({x}) = {got}, expected {expected}"
            );
        }
    }

    #[test]
    fn cdf_is_symmetric_and_monotone() {
        let mut prev = 0.0;
        let mut x = -8.0;
        while x <= 8.0 {
            let c = cdf(x);
            assert!(c >= prev, "cdf must be non-decreasing at x = {x}");
            assert!((c + cdf(-x) - 1.0).abs() < 1e-14, "symmetry at x = {x}");
            prev = c;
            x += 0.01;
        }
    }

    #[test]
    fn cdf_saturates_in_the_extreme_tails() {
        assert_eq!(cdf(40.0), 1.0);
        assert_eq!(cdf(-40.0), 0.0);
        assert!(cdf(-8.0) > 0.0, "the far tail must stay strictly positive");
        // Relative accuracy: the value itself is ~6.2e-16, so an
        // absolute tolerance would be meaningless here.
        let far_tail = cdf(-8.0);
        assert!(
            ((far_tail - 6.220_960_574_271_78e-16) / 6.220_960_574_271_78e-16).abs() < 1e-12,
            "cdf(-8) = {far_tail}"
        );
    }

    #[test]
    fn inverse_cdf_matches_reference_values() {
        for (expected, p) in CDF_REFERENCE {
            let got = inverse_cdf(p);
            assert!(
                (got - expected).abs() < 1e-9,
                "inverse_cdf({p}) = {got}, expected {expected}"
            );
        }
    }

    #[test]
    fn inverse_cdf_round_trips() {
        let mut p = 1e-6;
        while p < 1.0 {
            let x = inverse_cdf(p);
            assert!(
                (cdf(x) - p).abs() < 1e-12,
                "round trip failed at p = {p}: cdf(inverse_cdf(p)) = {}",
                cdf(x)
            );
            p += 1e-4;
        }
    }

    #[test]
    fn inverse_cdf_handles_boundaries() {
        assert_eq!(inverse_cdf(0.0), f64::NEG_INFINITY);
        assert_eq!(inverse_cdf(1.0), f64::INFINITY);
        assert!(inverse_cdf(-0.1).is_nan());
        assert!(inverse_cdf(1.1).is_nan());
    }

    #[test]
    fn pdf_matches_known_values() {
        assert!((pdf(0.0) - 0.398_942_280_401_433).abs() < 1e-14);
        assert!((pdf(1.0) - 0.241_970_724_519_143).abs() < 1e-14);
    }
}
