use egui::IconData;

/// Generates a crisp 64x64 RGBA shield + heartbeat pulse icon for the desktop window.
pub fn build_app_icon() -> IconData {
    let size = 64u32;
    let rgba = render_icon_rgba(size);
    IconData {
        rgba,
        width: size,
        height: size,
    }
}

pub fn render_icon_rgba(size: u32) -> Vec<u8> {
    let mut buf = vec![0u8; (size * size * 4) as usize];
    let s = size as f32;
    let cx = s * 0.5;
    let cy = s * 0.5;

    for y in 0..size {
        for x in 0..size {
            let px = x as f32 + 0.5;
            let py = y as f32 + 0.5;
            let nx = (px - cx) / (s * 0.44);
            let ny = (py - cy) / (s * 0.44);

            // Rounded square background tile
            let rx = (px - cx).abs() / (s * 0.46);
            let ry = (py - cy).abs() / (s * 0.46);
            let corner_dist = ((rx - 0.65).max(0.0)).hypot((ry - 0.65).max(0.0));
            let in_tile = rx <= 1.0 && ry <= 1.0 && corner_dist <= 0.35;

            if !in_tile {
                continue;
            }

            // Base dark navy gradient
            let t = py / s;
            let mut r: f32 = 13.0 + t * 10.0;
            let mut g: f32 = 20.0 + t * 14.0;
            let mut b: f32 = 34.0 + t * 20.0;
            let a: f32 = 255.0;

            // Subtle outer rim glow
            let edge_d = (1.0 - rx.max(ry)).min(0.35 - corner_dist);
            if edge_d < 0.05 {
                r += 24.0;
                g += 55.0;
                b += 85.0;
            }

            // Shield SDF in normalized coords
            let shield_w = 0.68 - (ny.max(0.0) * 0.42);
            let in_shield = nx.abs() < shield_w && ny > -0.68 && ny < 0.76 - (nx.abs() * 0.25);
            let on_shield_border = in_shield
                && (nx.abs() > shield_w - 0.09
                    || ny < -0.59
                    || ny > 0.66 - (nx.abs() * 0.25));

            if in_shield {
                r = 16.0;
                g = 34.0;
                b = 54.0;
            }
            if on_shield_border {
                r = 56.0;
                g = 189.0;
                b = 248.0;
            }

            // Heartbeat / CPU pulse line across center
            let pulse_y = if nx < -0.35 {
                0.0
            } else if nx < -0.18 {
                (nx + 0.35) / 0.17 * -0.16
            } else if nx < 0.0 {
                -0.16 + (nx + 0.18) / 0.18 * 0.48
            } else if nx < 0.18 {
                0.32 - nx / 0.18 * 0.56
            } else if nx < 0.35 {
                -0.24 + (nx - 0.18) / 0.17 * 0.24
            } else {
                0.0
            };

            if nx.abs() < 0.52 && (ny - pulse_y).abs() < 0.065 {
                r = 52.0;
                g = 211.0;
                b = 153.0;
            }

            let idx = ((y * size + x) * 4) as usize;
            buf[idx] = r.clamp(0.0, 255.0) as u8;
            buf[idx + 1] = g.clamp(0.0, 255.0) as u8;
            buf[idx + 2] = b.clamp(0.0, 255.0) as u8;
            buf[idx + 3] = a.clamp(0.0, 255.0) as u8;
        }
    }
    buf
}

/// Writes an uncompressed 32-bit BGRA TGA image of the app icon for macOS .icns generation.
pub fn write_icon_tga(path: &std::path::Path, size: u32) -> std::io::Result<()> {
    let rgba = render_icon_rgba(size);
    let mut tga = Vec::with_capacity(18 + rgba.len());
    tga.extend_from_slice(&[
        0, 0, 2, // uncompressed true-color image
        0, 0, 0, 0, 0, // color map spec
        0, 0, 0, 0, // x-origin, y-origin
        (size & 0xff) as u8,
        ((size >> 8) & 0xff) as u8,
        (size & 0xff) as u8,
        ((size >> 8) & 0xff) as u8,
        32,   // 32 bpp
        0x28, // top-left origin + 8-bit alpha
    ]);
    for px in rgba.chunks_exact(4) {
        tga.push(px[2]); // B
        tga.push(px[1]); // G
        tga.push(px[0]); // R
        tga.push(px[3]); // A
    }
    std::fs::write(path, tga)
}
