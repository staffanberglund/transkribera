use std::f32::consts::TAU;

pub fn periodic_hann(size: usize) -> Vec<f32> {
    (0..size)
        .map(|index| 0.5 - 0.5 * (TAU * index as f32 / size as f32).cos())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::periodic_hann;

    #[test]
    fn periodic_hann_has_the_expected_shape() {
        let window = periodic_hann(8);
        assert!(window[0].abs() < 1e-6);
        assert!((window[4] - 1.0).abs() < 1e-6);
        assert!(window.iter().all(|sample| (0.0..=1.0).contains(sample)));
    }
}
