//! The HSL color space the palettes derive from. kuma-shell's
//! palette.rs is the original: the wallpaper picker seeds k-means
//! with it, and the apps re-derive their selection tones in the
//! accent's hue with the same two functions.

/// sRGB u32 (0xRRGGBB) to HSL.
pub fn rgb_to_hsl(rgb: u32) -> (f32, f32, f32) {
    let r = ((rgb >> 16) & 0xFF) as f32 / 255.;
    let g = ((rgb >> 8) & 0xFF) as f32 / 255.;
    let b = (rgb & 0xFF) as f32 / 255.;
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) / 2.;
    if max == min {
        return (0., 0., l);
    }
    let d = max - min;
    let s = if l > 0.5 { d / (2. - max - min) } else { d / (max + min) };
    let h = if max == r {
        (g - b) / d + if g < b { 6. } else { 0. }
    } else if max == g {
        (b - r) / d + 2.
    } else {
        (r - g) / d + 4.
    } * 60.;
    (h, s, l)
}

/// HSL to sRGB u32 (0xRRGGBB), clamped.
pub fn hsl_to_rgb(h: f32, s: f32, l: f32) -> u32 {
    let h = h.rem_euclid(360.) / 360.;
    let s = s.clamp(0., 1.);
    let l = l.clamp(0., 1.);
    if s == 0. {
        let v = (l * 255. + 0.5) as u32;
        return (v << 16) | (v << 8) | v;
    }
    let q = if l < 0.5 { l * (1. + s) } else { l + s - l * s };
    let p = 2. * l - q;
    let channel = |mut t: f32| {
        t = t.rem_euclid(1.);
        let v = if t < 1. / 6. {
            p + (q - p) * 6. * t
        } else if t < 0.5 {
            q
        } else if t < 2. / 3. {
            p + (q - p) * (2. / 3. - t) * 6.
        } else {
            p
        };
        ((v * 255.) + 0.5) as u32
    };
    (channel(h + 1. / 3.) << 16) | (channel(h) << 8) | channel(h - 1. / 3.)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primaries_read_as_expected() {
        assert_eq!(rgb_to_hsl(0xff0000), (0., 1., 0.5));
        assert_eq!(rgb_to_hsl(0x00ff00), (120., 1., 0.5));
        assert_eq!(rgb_to_hsl(0x0000ff), (240., 1., 0.5));
        // grays carry no hue
        assert_eq!(rgb_to_hsl(0x808080), (0., 0., 128. / 255.));
    }

    #[test]
    fn the_round_trip_lands_on_the_same_color() {
        for rgb in [0x000000, 0x181b21, 0x4f8cc9, 0x8be4d2, 0xd8dce3, 0xffffff] {
            let (h, s, l) = rgb_to_hsl(rgb);
            let back = hsl_to_rgb(h, s, l);
            // u8 rounding can move a channel by one
            assert!((back as i32 - rgb as i32).abs() <= 1, "{rgb:#x} became {back:#x}");
        }
    }

    #[test]
    fn out_of_range_inputs_clamp() {
        // hue wraps, saturation and lightness clamp: the derive paths
        // rely on this for their scaled coefficients
        assert_eq!(hsl_to_rgb(420., 2., -1.), 0x000000);
        assert_eq!(hsl_to_rgb(-60., -1., 2.), 0xffffff);
        assert_eq!(hsl_to_rgb(60., 1., 0.5), 0xffff00);
    }
}
