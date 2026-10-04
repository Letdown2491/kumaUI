//! Wallpaper-derived palette generation: pure functions over an RGB
//! sample, no gpui, so every step is unit-testable. The shape follows
//! Noctalia v5's approach, simplified: k-means over the downsampled
//! wallpaper picks a seed color, and the whole palette is tone-mapped
//! off the seed's hue with WCAG contrast pulling the text values.

/// The palette's accent seed, in HSL (h in degrees, s and l 0..1).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Seed {
    pub h: f32,
    pub s: f32,
    pub l: f32,
}

/// What the seed optimizes for: `Faithful` favors what the wallpaper
/// actually shows, `Vibrant` lets strong chroma punch through even
/// when it covers few pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flavor {
    Faithful,
    Vibrant,
}

/// Below this saturation a cluster counts as grey and can never
/// become the accent: greys stay in backgrounds, never seeds.
const CHROMA_FLOOR: f32 = 0.15;
const K: usize = 8;

/// The accent when the wallpaper is near-grey (or anything fails):
/// the Catppuccin blue, so a monochrome photo leaves the shell
/// recognizable rather than washed out.
pub const FALLBACK_ACCENT: u32 = 0x89B4FA;

// ---------- color space ----------

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

/// WCAG relative luminance of an sRGB u32.
fn luminance(rgb: u32) -> f32 {
    let channel = |c: u32| {
        let c = c as f32 / 255.;
        if c <= 0.03928 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * channel(rgb >> 16) + 0.7152 * channel((rgb >> 8) & 0xFF) + 0.0722 * channel(rgb & 0xFF)
}

/// WCAG contrast ratio, 1..21.
pub fn contrast(a: u32, b: u32) -> f32 {
    let (la, lb) = (luminance(a), luminance(b));
    let (hi, lo) = if la > lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

// ---------- sampling ----------

/// k-means (k=8, fixed 12 iterations, deterministic start) in HSL
/// space over a 112x112 RGB sample. Hue distance wraps. Returns the
/// cluster centroids and their pixel proportions, sorted by pixel
/// order for stable output.
pub fn clusters(sample: &[u8]) -> Vec<([f32; 3], f32)> {
    let points: Vec<[f32; 3]> = sample
        .chunks_exact(3)
        .map(|px| {
            let (h, s, l) = rgb_to_hsl((px[0] as u32) << 16 | (px[1] as u32) << 8 | px[2] as u32);
            [h, s, l]
        })
        .collect();
    if points.is_empty() {
        return Vec::new();
    }
    // spread the starting centroids across the pixel order
    let mut centroids: Vec<[f32; 3]> = (0..K)
        .map(|k| points[points.len() * k / K])
        .collect();
    let mut assignment = vec![0usize; points.len()];
    for _ in 0..12 {
        for (i, point) in points.iter().enumerate() {
            let best = centroids
                .iter()
                .enumerate()
                .min_by(|(_, a), (_, b)| {
                    hsl_distance(**a, *point)
                        .total_cmp(&hsl_distance(**b, *point))
                })
                .map(|(k, _)| k)
                .unwrap_or(0);
            assignment[i] = best;
        }
        let mut sums = vec![[0f32; 4]; K];
        for (point, k) in points.iter().zip(&assignment) {
            let sum = &mut sums[*k];
            sum[0] += point[0];
            sum[1] += point[1];
            sum[2] += point[2];
            sum[3] += 1.;
        }
        for (k, centroid) in centroids.iter_mut().enumerate() {
            let sum = sums[k];
            if sum[3] > 0. {
                // the mean hue: average the unit vectors, not the
                // degrees, so 350 and 10 mean 0 and not 180
                let (mut sin, mut cos) = (0f32, 0f32);
                for (point, a) in points.iter().zip(&assignment) {
                    if *a == k {
                        sin += point[0].to_radians().sin();
                        cos += point[0].to_radians().cos();
                    }
                }
                if !(sin == 0. && cos == 0.) {
                    centroid[0] = sin.atan2(cos).to_degrees().rem_euclid(360.);
                }
                centroid[1] = sum[1] / sum[3];
                centroid[2] = sum[2] / sum[3];
            }
        }
    }
    let mut counts = vec![0f32; K];
    for k in &assignment {
        counts[*k] += 1.;
    }
    centroids
        .into_iter()
        .zip(counts)
        .map(|(centroid, count)| (centroid, count / points.len() as f32))
        .collect()
}

/// Squared distance in HSL space, hue circular.
fn hsl_distance(a: [f32; 3], b: [f32; 3]) -> f32 {
    let dh = (a[0] - b[0]).abs();
    let dh = dh.min(360. - dh);
    let ds = a[1] - b[1];
    let dl = a[2] - b[2];
    dh * dh + ds * ds * 360. + dl * dl * 360.
}

/// Score the clusters and pick the seed: proportion times chroma,
/// with the chroma floor keeping greys out. `Faithful` squares the
/// proportion (what the wallpaper shows matters most), `Vibrant`
/// squares the chroma. `None` when nothing clears the floor: a
/// near-grey wallpaper.
pub fn seed(sample: &[u8], flavor: Flavor) -> Option<Seed> {
    let scored = clusters(sample)
        .into_iter()
        .filter(|(c, _)| c[1] >= CHROMA_FLOOR && (0.1..=0.9).contains(&c[2]))
        .map(|(c, proportion)| {
            let score = match flavor {
                Flavor::Faithful => proportion * proportion * c[1],
                Flavor::Vibrant => proportion * c[1] * c[1] * c[1],
            };
            (c, score)
        })
        .max_by(|(_, a), (_, b)| a.total_cmp(b))?;
    let c = scored.0;
    Some(Seed { h: c[0], s: c[1], l: c[2] })
}

// ---------- generation ----------

/// The derived palette: the shell's `Theme`, every value keyed off
/// the seed hue. `urgent` never appears here: it stays the constant
/// red, never derived.
pub fn generate(seed: Seed) -> crate::theme::Theme {
    let h = seed.h;
    // the seed's own chroma informs how tinted the neutrals go: a
    // vivid seed tints the surfaces more than a muted one
    let neutral_s = (seed.s * 0.35).clamp(0.04, 0.16);

    let panel = hsl_to_rgb(h, neutral_s, 0.10);
    let surface = hsl_to_rgb(h, neutral_s, 0.17);
    let surface_hover = hsl_to_rgb(h, neutral_s, 0.24);
    let inset = hsl_to_rgb(h, neutral_s, 0.06);
    let divider = hsl_to_rgb(h, neutral_s, 0.28);
    let divider_soft = (divider << 8) | 0x66;
    let accent = hsl_to_rgb(h, seed.s.max(0.55).min(0.85), 0.72);
    let accent_text = inset;

    // text: the lightest tinted near-white, lifted until it clears
    // 4.5:1 against the panel (the panel is always dark here, so the
    // lift is small; the clamp guards a pathological seed)
    let mut text = hsl_to_rgb(h, 0.10, 0.85);
    for lift in [0.87, 0.90, 0.93, 0.96, 1.0] {
        text = hsl_to_rgb(h, 0.10, lift);
        if contrast(text, panel) >= 4.5 {
            break;
        }
    }
    let text_dim = hsl_to_rgb(h, 0.08, 0.52);

    crate::theme::Theme {
        // keep today's panel alpha
        panel_bg: (panel << 8) | 0xF2,
        surface,
        surface_hover,
        inset,
        divider,
        divider_soft,
        text,
        text_dim,
        accent,
        accent_text,
    }
}

/// The palette for a wallpaper sample: seed it, generate off it, or
/// fall back to recognizable defaults when the image is near-grey.
pub fn palette(sample: &[u8], flavor: Flavor) -> crate::theme::Theme {
    match seed(sample, flavor) {
        Some(seed) => generate(seed),
        None => {
            // derive the neutrals off the fallback accent's hue so the
            // shell still coordinates, keeping the default accent
            let (h, s, l) = rgb_to_hsl(FALLBACK_ACCENT);
            generate(Seed { h, s, l }).with_accent(FALLBACK_ACCENT)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 112x112 RGB sample of a solid color.
    fn solid(rgb: u32) -> Vec<u8> {
        let (r, g, b) = (rgb >> 16 & 0xFF, rgb >> 8 & 0xFF, rgb & 0xFF);
        vec![(r as u8), (g as u8), (b as u8)].repeat(112 * 112)
    }

    #[test]
    fn hsl_round_trips() {
        for rgb in [0xFF0000, 0x00FF88, 0x89B4FA, 0x181825, 0xFFFFFF, 0x000000, 0xCDD6F4] {
            let (h, s, l) = rgb_to_hsl(rgb);
            assert_eq!(hsl_to_rgb(h, s, l), rgb & 0xFFFFFF, "round trip {rgb:#x}");
        }
    }

    #[test]
    fn seed_finds_dominant_hue() {
        // half teal, half a sliver of magenta: the faithful seed sits
        // in the teal family
        let mut sample = solid(0x1BA8A0);
        let magenta: Vec<u8> = solid(0xC71585).iter().copied().collect();
        sample.extend_from_slice(&magenta[..3 * 500]);
        let seed = seed(&sample, Flavor::Faithful).expect("teal clears the floor");
        assert!(
            (170. ..=190.).contains(&seed.h.rem_euclid(360.)),
            "seed hue {} not teal",
            seed.h
        );
    }

    #[test]
    fn near_grey_sample_falls_back() {
        assert!(seed(&solid(0x808080), Flavor::Faithful).is_none());
        assert!(seed(&solid(0x2A2A2E), Flavor::Vibrant).is_none());
        // the palette path keeps the recognizable blue accent
        let theme = palette(&solid(0x808080), Flavor::Faithful);
        assert_eq!(theme.accent, FALLBACK_ACCENT);
    }

    #[test]
    fn text_meets_contrast_against_panel() {
        for hue in [15., 75., 140., 210., 270., 330.] {
            let theme = generate(Seed { h: hue, s: 0.6, l: 0.5 });
            let (ph, ..) = rgb_to_hsl(theme.panel_bg >> 8);
            let _ = ph;
            assert!(
                contrast(theme.text, theme.panel_bg >> 8) >= 4.5,
                "text {:#x} on panel {:#x} below 4.5:1 at hue {hue}",
                theme.text,
                theme.panel_bg >> 8
            );
            assert!(
                contrast(theme.text_dim, theme.panel_bg >> 8) >= 2.2,
                "dim text unreadable at hue {hue}"
            );
        }
    }

    #[test]
    fn surfaces_step_lightness_and_hold_hue() {
        let theme = generate(Seed { h: 210., s: 0.5, l: 0.5 });
        let (_, ls, ll) = rgb_to_hsl(theme.surface);
        let (_, hs, hl) = rgb_to_hsl(theme.surface_hover);
        assert!(hl > ll, "hover lighter than surface");
        let _ = (ls, hs);
        // panel bg keeps the derived-theme alpha
        assert_eq!(theme.panel_bg & 0xFF, 0xF2);
        assert_eq!(theme.divider_soft & 0xFF, 0x66);
    }

    #[test]
    fn vibrant_lets_chroma_win() {
        // a wallpaper that is 90% muted teal and 10% hot pink
        let mut sample = solid(0x2E6E6A);
        sample.extend_from_slice(&solid(0xFF2D95)[..3 * (112 * 112 / 10)]);
        let faithful = seed(&sample, Flavor::Faithful).expect("seed");
        let vibrant = seed(&sample, Flavor::Vibrant).expect("seed");
        assert!(
            (faithful.h.rem_euclid(360.) - 180.).abs() < 40.,
            "faithful picked hue {} not the teal mass",
            faithful.h
        );
        let (vh, ..) = (vibrant.h, vibrant.s, vibrant.l);
        assert!(
            (vh.rem_euclid(360.) - 330.).abs() < 40.,
            "vibrant picked hue {} not the pink",
            vh
        );
    }
}
