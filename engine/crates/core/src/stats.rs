//! Small numeric helpers that ignore NaN values.

pub fn mean(xs: &[f64]) -> Option<f64> {
    let v: Vec<f64> = xs.iter().copied().filter(|x| x.is_finite()).collect();
    if v.is_empty() {
        None
    } else {
        Some(v.iter().sum::<f64>() / v.len() as f64)
    }
}

pub fn median(xs: &[f64]) -> Option<f64> {
    let mut v: Vec<f64> = xs.iter().copied().filter(|x| x.is_finite()).collect();
    if v.is_empty() {
        return None;
    }
    v.sort_by(|a, b| a.partial_cmp(b).expect("finite"));
    let n = v.len();
    Some(if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    })
}

/// Gross profit / gross loss. `None` when there are no losses.
pub fn profit_factor(pnls: &[f64]) -> Option<f64> {
    // fold from +0.0: `Iterator::sum` of nothing is -0.0, which would print as "-0.00"
    let gains: f64 = pnls.iter().filter(|x| **x > 0.0).fold(0.0, |a, b| a + b);
    let losses: f64 = pnls.iter().filter(|x| **x < 0.0).fold(0.0, |a, b| a - b);
    if losses <= 0.0 {
        None
    } else {
        Some(gains / losses)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basics() {
        assert_eq!(median(&[3.0, 1.0, 2.0]), Some(2.0));
        assert_eq!(median(&[4.0, 1.0, 2.0, 3.0]), Some(2.5));
        assert_eq!(median(&[f64::NAN]), None);
        assert_eq!(mean(&[1.0, 3.0]), Some(2.0));
        assert_eq!(profit_factor(&[2.0, -1.0, 1.0]), Some(3.0));
        assert_eq!(profit_factor(&[1.0]), None);
        let all_losers = profit_factor(&[-1.0, -2.0]).unwrap();
        assert!(all_losers == 0.0 && all_losers.is_sign_positive());
    }
}
