//! The existing signal-level gates; passing does not select an app backend.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Gates {
    pub echo_reduction: bool,
    pub near_only: bool,
    pub overlap_gain: bool,
    pub overlap_snr: bool,
}

impl Gates {
    pub(crate) fn pass(&self) -> bool {
        self.echo_reduction && self.near_only && self.overlap_gain && self.overlap_snr
    }
}

pub(crate) fn assess(near: f64, echo: f64, gain: f64, snr: f64, raw_snr: f64) -> Gates {
    let finite = [near, echo, gain, snr, raw_snr]
        .into_iter()
        .all(f64::is_finite);
    Gates {
        echo_reduction: finite && echo >= 15.0,
        near_only: finite && near.abs() <= 0.5,
        overlap_gain: finite && gain.abs() <= 1.0,
        overlap_snr: finite && snr - raw_snr >= 6.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn the_documented_thresholds_are_inclusive_and_near_misses_do_not_pass() {
        assert!(assess(0.5, 15.0, -1.0, 9.0, 3.0).pass());
        assert!(!assess(0.5001, 15.0, -1.0, 9.0, 3.0).pass());
        assert!(!assess(0.5, 14.9999, -1.0, 9.0, 3.0).pass());
        assert!(!assess(0.5, 15.0, -1.0001, 9.0, 3.0).pass());
        assert!(!assess(0.5, 15.0, -1.0, 8.9999, 3.0).pass());
    }
    #[test]
    fn muting_passthrough_and_invalid_measurements_cannot_qualify() {
        assert!(!assess(-120.0, 90.0, -120.0, -120.0, 3.0).pass());
        assert!(!assess(0.0, 0.0, 0.0, 3.0, 3.0).pass());
        assert!(!assess(f64::NAN, 90.0, 0.0, 20.0, 3.0).pass());
        assert!(!assess(0.0, f64::INFINITY, 0.0, 20.0, 3.0).pass());
        assert!(!assess(0.0, 90.0, 0.0, 20.0, f64::NEG_INFINITY).pass());
    }
}
