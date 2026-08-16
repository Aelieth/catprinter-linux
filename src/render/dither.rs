//! Binarization / quantization / filtering kernels (port of catprinter/img.py).
//!
//! All buffers are row-major 8-bit gray, 0 = black, 255 = white, `width * height` bytes.

/// Floyd–Steinberg error diffusion to 1 bit. Returns `true` = black (burn).
/// Standard left-to-right kernel (7/16, 3/16, 5/16, 1/16), threshold at 128 like Pillow's
/// `convert("1", dither=FLOYDSTEINBERG)`.
pub fn floyd_steinberg(gray: &[u8], width: usize, height: usize) -> Vec<bool> {
    let mut work: Vec<f32> = gray.iter().map(|&v| v as f32).collect();
    let mut out = vec![false; width * height];
    for y in 0..height {
        for x in 0..width {
            let i = y * width + x;
            let old = work[i];
            let black = old < 128.0;
            let new = if black { 0.0 } else { 255.0 };
            out[i] = black;
            let err = old - new;
            if x + 1 < width {
                work[i + 1] += err * (7.0 / 16.0);
            }
            if y + 1 < height {
                let below = i + width;
                if x > 0 {
                    work[below - 1] += err * (3.0 / 16.0);
                }
                work[below] += err * (5.0 / 16.0);
                if x + 1 < width {
                    work[below + 1] += err * (1.0 / 16.0);
                }
            }
        }
    }
    out
}

/// Atkinson dithering (img.py `atkinson_dither`): 6 neighbours × 1/8, intermediate values clamped
/// to 0..255 exactly like the reference. Returns `true` = black.
pub fn atkinson(gray: &[u8], width: usize, height: usize) -> Vec<bool> {
    let mut img: Vec<i32> = gray.iter().map(|&v| v as i32).collect();
    let mut out = vec![false; width * height];
    let adjust = |img: &mut Vec<i32>, y: isize, x: isize, delta: i32| {
        if y < 0 || y >= height as isize || x < 0 || x >= width as isize {
            return;
        }
        let i = y as usize * width + x as usize;
        img[i] = (img[i] + delta).clamp(0, 255);
    };
    for y in 0..height {
        for x in 0..width {
            let i = y * width + x;
            let v = img[i];
            let new_val = if v > 127 { 255 } else { 0 };
            let err = v - new_val;
            img[i] = new_val;
            out[i] = new_val == 0;
            // Reference: err * 1/8 in float, stored back into a uint8 array (truncation toward zero).
            let d = err / 8;
            let (yi, xi) = (y as isize, x as isize);
            adjust(&mut img, yi, xi + 1, d);
            adjust(&mut img, yi, xi + 2, d);
            adjust(&mut img, yi + 1, xi - 1, d);
            adjust(&mut img, yi + 1, xi, d);
            adjust(&mut img, yi + 1, xi + 1, d);
            adjust(&mut img, yi + 2, xi, d);
        }
    }
    out
}

/// Fixed threshold: gray < `t` → black.
pub fn threshold(gray: &[u8], t: u8) -> Vec<bool> {
    gray.iter().map(|&v| v < t).collect()
}

/// gray < mean → black (render.py "mean-threshold").
pub fn mean_threshold(gray: &[u8]) -> Vec<bool> {
    if gray.is_empty() {
        return Vec::new();
    }
    let sum: u64 = gray.iter().map(|&v| v as u64).sum();
    let mean = sum as f64 / gray.len() as f64;
    gray.iter().map(|&v| (v as f64) < mean).collect()
}

/// img.py `thermal_curve`: park more of the image in the printable mid-grays.
/// `midtone` > 1 darkens the middle (power curve), `contrast` is the tanh S-curve k (0 = skip).
pub fn thermal_curve(gray: &[u8], contrast: f64, midtone: f64) -> Vec<u8> {
    let denom = if contrast > 0.0 {
        (contrast * 0.5).tanh()
    } else {
        0.0
    };
    gray.iter()
        .map(|&v| {
            let mut x = (v as f64 / 255.0).clamp(0.0, 1.0);
            if midtone > 0.0 && midtone != 1.0 {
                x = x.powf(midtone);
            }
            if contrast > 0.0 && denom != 0.0 {
                x = 0.5 * (1.0 + (contrast * (x - 0.5)).tanh() / denom);
            }
            (x * 255.0).round().clamp(0.0, 255.0) as u8
        })
        .collect()
}

/// img.py `quantize_16`: straight 16-bin quantize, 0 = white .. 15 = black.
pub fn quantize_16(gray: &[u8]) -> Vec<u8> {
    gray.iter()
        .map(|&v| ((255u16 - v as u16) >> 4) as u8)
        .collect()
}

/// img.py `quantize_16_serpentine_fs`: 16-level serpentine Floyd–Steinberg onto the 4 bpp bins,
/// reconstructing with each bin's centre (247 − 16·level). 0 = white .. 15 = black.
pub fn quantize_16_serpentine_fs(gray: &[u8], width: usize, height: usize) -> Vec<u8> {
    let mut work: Vec<f64> = gray.iter().map(|&v| v as f64).collect();
    let mut out = vec![0u8; width * height];
    for y in 0..height {
        let ltr = y % 2 == 0;
        let step: isize = if ltr { 1 } else { -1 };
        for k in 0..width {
            let x = if ltr { k } else { width - 1 - k };
            let i = y * width + x;
            let val = work[i].clamp(0.0, 255.0);
            // int(255 - val) truncates toward zero for non-negative values.
            let level = (((255.0 - val) as i32) >> 4).clamp(0, 15);
            out[i] = level as u8;
            let recon = 247.0 - 16.0 * level as f64;
            let err = val - recon;
            let nx = x as isize + step;
            let nx_ok = nx >= 0 && (nx as usize) < width;
            if nx_ok {
                work[y * width + nx as usize] += err * (7.0 / 16.0);
            }
            let ny = y + 1;
            if ny < height {
                let px = x as isize - step;
                if px >= 0 && (px as usize) < width {
                    work[ny * width + px as usize] += err * (3.0 / 16.0);
                }
                work[ny * width + x] += err * (5.0 / 16.0);
                if nx_ok {
                    work[ny * width + nx as usize] += err * (1.0 / 16.0);
                }
            }
        }
    }
    out
}

/// Separable Gaussian blur (σ = `sigma`, kernel radius = ceil(3σ)), edges clamped.
pub fn gaussian_blur(gray: &[u8], width: usize, height: usize, sigma: f32) -> Vec<f32> {
    let radius = (sigma * 3.0).ceil().max(1.0) as isize;
    let kernel: Vec<f32> = (-radius..=radius)
        .map(|i| (-((i * i) as f32) / (2.0 * sigma * sigma)).exp())
        .collect();
    let ksum: f32 = kernel.iter().sum();
    let kernel: Vec<f32> = kernel.iter().map(|k| k / ksum).collect();

    let mut tmp = vec![0f32; width * height];
    for y in 0..height {
        let row = &gray[y * width..(y + 1) * width];
        for x in 0..width {
            let mut acc = 0f32;
            for (ki, k) in kernel.iter().enumerate() {
                let sx = (x as isize + ki as isize - radius).clamp(0, width as isize - 1) as usize;
                acc += row[sx] as f32 * k;
            }
            tmp[y * width + x] = acc;
        }
    }
    let mut out = vec![0f32; width * height];
    for x in 0..width {
        for y in 0..height {
            let mut acc = 0f32;
            for (ki, k) in kernel.iter().enumerate() {
                let sy = (y as isize + ki as isize - radius).clamp(0, height as isize - 1) as usize;
                acc += tmp[sy * width + x] * k;
            }
            out[y * width + x] = acc;
        }
    }
    out
}

/// img.py `unsharp_gray` (Pillow UnsharpMask semantics): radius σ, `percent` amount, `threshold`
/// on |orig − blurred|.
pub fn unsharp(
    gray: &[u8],
    width: usize,
    height: usize,
    sigma: f32,
    percent: u32,
    threshold: u8,
) -> Vec<u8> {
    if width == 0 || height == 0 {
        return Vec::new();
    }
    let blurred = gaussian_blur(gray, width, height, sigma);
    gray.iter()
        .zip(blurred.iter())
        .map(|(&o, &b)| {
            let diff = o as f32 - b;
            if diff.abs() > threshold as f32 {
                (o as f32 + diff * percent as f32 / 100.0)
                    .round()
                    .clamp(0.0, 255.0) as u8
            } else {
                o
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn threshold_and_mean() {
        let g = [0u8, 100, 179, 180, 255];
        assert_eq!(threshold(&g, 180), vec![true, true, true, false, false]);
        // mean = 142.8 → 0,100 black
        assert_eq!(mean_threshold(&g), vec![true, true, false, false, false]);
    }

    #[test]
    fn quantize_bins_match_maikel() {
        assert_eq!(quantize_16(&[0, 15, 16, 255]), vec![15, 15, 14, 0]);
    }

    #[test]
    fn thermal_curve_darkens_midtones() {
        let out = thermal_curve(&[160; 16], 1.6, 1.22);
        let mean = out.iter().map(|&v| v as u32).sum::<u32>() / 16;
        assert!(mean < 160, "mean {mean}");
        // extremes stay put
        assert_eq!(thermal_curve(&[0, 255], 1.6, 1.22), vec![0, 255]);
    }

    #[test]
    fn serpentine_ramp_uses_many_levels() {
        let w = 384;
        let h = 32;
        let mut ramp = vec![0u8; w * h];
        for y in 0..h {
            for x in 0..w {
                ramp[y * w + x] = (x as f64 * 255.0 / (w - 1) as f64).round() as u8;
            }
        }
        let levels = quantize_16_serpentine_fs(&ramp, w, h);
        let mut used: Vec<u8> = levels.clone();
        used.sort_unstable();
        used.dedup();
        assert!(used.len() >= 12, "used {used:?}");
        assert!(used.contains(&0));
        assert!(used.contains(&15));
    }

    #[test]
    fn floyd_steinberg_solid_blocks_are_solid() {
        let w = 16;
        let h = 4;
        let mut g = vec![255u8; w * h];
        for v in g.iter_mut().take(w * 2) {
            *v = 0;
        }
        let bits = floyd_steinberg(&g, w, h);
        assert!(bits[..w * 2].iter().all(|&b| b));
        assert!(bits[w * 2..].iter().all(|&b| !b));
        // mid gray produces a mix
        let mid = vec![128u8; 64 * 8];
        let bits = floyd_steinberg(&mid, 64, 8);
        let n = bits.iter().filter(|&&b| b).count();
        assert!(n > 100 && n < 412, "n {n}");
    }

    #[test]
    fn atkinson_solid_blocks_are_solid() {
        let w = 16;
        let h = 4;
        let mut g = vec![255u8; w * h];
        for v in g.iter_mut().take(w) {
            *v = 0;
        }
        let bits = atkinson(&g, w, h);
        assert!(bits[..w].iter().all(|&b| b));
        assert!(bits[w..].iter().all(|&b| !b));
    }

    #[test]
    fn unsharp_keeps_flat_and_sharpens_edges() {
        let w = 32;
        let h = 8;
        let flat = vec![200u8; w * h];
        assert_eq!(unsharp(&flat, w, h, 1.0, 55, 2), flat);
        let mut edge = vec![255u8; w * h];
        for y in 0..h {
            for x in 0..w / 2 {
                edge[y * w + x] = 0;
            }
        }
        let out = unsharp(&edge, w, h, 1.0, 55, 2);
        // A hard 0|255 edge: unsharp overshoots on both sides but clamps back to 0 / 255.
        assert_eq!(out[w / 2 - 1], 0);
        assert_eq!(out[w / 2], 255);
        // A symmetric soft ramp gets steeper: the dark side goes darker, the light side lighter,
        // the centre is untouched.
        let mut ramp = vec![0u8; 9 * 3];
        for y in 0..3 {
            for x in 0..9 {
                ramp[y * 9 + x] = [0, 0, 0, 64, 128, 192, 255, 255, 255][x];
            }
        }
        let out = unsharp(&ramp, 9, 3, 1.0, 55, 2);
        assert!(out[9 + 3] < 64, "{}", out[9 + 3]);
        assert_eq!(out[9 + 4], 128);
        assert!(out[9 + 5] > 192, "{}", out[9 + 5]);
    }
}
