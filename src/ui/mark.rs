//! Hayate mark: an italic H of three polygons (two leaning stems, one rising
//! blade). Geometry is the 512-unit "small cut" of `assets/brand/hayate-mark-small.svg`,
//! heavier than the full mark so it holds up at 16-24 px.

pub type Poly = [(f32, f32); 4];

const SMALL: [Poly; 3] = [
    [(150.8, 88.0), (238.8, 88.0), (161.2, 424.0), (73.2, 424.0)],
    [(350.8, 88.0), (438.8, 88.0), (361.2, 424.0), (273.2, 424.0)],
    [(200.5, 228.0), (346.6, 132.0), (328.2, 212.0), (182.0, 308.0)],
];

/// The three polygons scaled into a `size` x `size` box at (`x`, `y`).
pub fn polys(x: f32, y: f32, size: f32) -> [Poly; 3] {
    let k = size / 512.0;
    SMALL.map(|p| p.map(|(px, py)| (x + px * k, y + py * k)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scales_into_box() {
        let ps = polys(10.0, 20.0, 256.0);
        for p in ps {
            for (x, y) in p {
                assert!((10.0..=266.0).contains(&x) && (20.0..=276.0).contains(&y));
            }
        }
        // first stem top-left: 150.8/2, 88/2 offset
        assert!((ps[0][0].0 - 85.4).abs() < 1e-3 && (ps[0][0].1 - 64.0).abs() < 1e-3);
    }

    #[test]
    fn unit_size_is_identity_scale() {
        let ps = polys(0.0, 0.0, 512.0);
        assert_eq!(ps[1][2], (361.2, 424.0));
    }
}
