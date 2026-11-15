//! neon particle spectrum plugin (was render_spectrum)
use crate::*;
use image::RgbaImage;
use image::Rgba;
use ratatui::text::Line;
use super::{SpectrumPlugin, SpectrumCtx};
pub(crate) fn render_neon(
    spectrum: &[u64],
    cols: u32,
    rows: u32,
    particles: &mut Vec<Particle>,
    neon: (u8, u8, u8),
    peaks: &mut Vec<f32>,
    dyn_state: &AudioDyn,
) -> Vec<ratatui::text::Line<'static>> {
    use rand::Rng;
    const DT: f32 = 0.05;
    // Braille: 2x4 subpixels per cell（8 点/格，纵向分辨率最高 = 最细腻）
    let bw = (cols.max(1) * 2) as u32;
    let bh = (rows.max(1) * 4) as u32;
    // 铺不透明底：每格 8 点全亮 -> 整片 ⣿ 连续点阵，无空格/空行。
    // 颜色仍取格内最亮像素（零稀释），所以铺底不会淹没亮线。
    let mut buf = image::RgbaImage::from_pixel(bw, bh, image::Rgba([0, 0, 0, 255]));
    let n = spectrum.len().max(1) as u32;
    let bin_w = ((bw as f32) / n as f32).max(1.0) as u32;
    let mut rng = rand::thread_rng();
    let busyness = dyn_state.busyness();
    let heaviness = dyn_state.heaviness();
    // 发射率随能量：安静时几乎无粒子，激烈时最多 ~每 bin 3 个（少而精）。
    let spawn_scale = (0.4 + dyn_state.energy * 2.6).clamp(0.0, 3.0);
    if peaks.len() != n as usize {
        *peaks = vec![0.0f32; n as usize];
    }
    for (i, &v) in spectrum.iter().enumerate() {
        let e0 = (v as f32 / 100.0).clamp(0.0, 1.0);
        let e = e0.powf(0.6);
        let t = i as f32 / n as f32;
        let c = neonize(hue_gradient(t));
        if e <= 0.002 {
            continue;
        }
        let cx = (i as f32 + 0.5) * (bw as f32 / n as f32);
        let bar_h = (((e * (bh as f32)).max(1.0)) as u32).min(bh);
        let dx = (cx - bin_w as f32 / 2.0) as i64;
        let dy = (bh - bar_h) as i64;
        let c_bright = brighten(c, 0.08); // 整体提亮，霓虹底色更醒目
        draw_core(&mut buf, dx, dy, bin_w.max(1) as i64, bar_h as i64, c_bright, 0.55 + 0.40 * e);
        // 柱顶亮芯：additive 叠加近白高光，形成霓虹发光边
        let cap = (bin_w.max(2) as i64).min(3);
        draw_core(&mut buf, dx, (bh - bar_h) as i64, cap, 1, brighten(c, 0.4), 1.0);
        let pi = i as usize;
        if (bar_h as f32) > peaks[pi] {
            peaks[pi] = bar_h as f32;
        } else {
            peaks[pi] -= 1.4;
            if peaks[pi] < 0.0 { peaks[pi] = 0.0; }
        }
        if peaks[pi] > 1.0 {
            let py = (bh as f32 - peaks[pi]) as i64;
            draw_core(&mut buf, dx, py, bin_w.max(1) as i64, 1, brighten(c, 0.3), 1.0);
        }
        // 粒子数随能量与频段：高频更多更小的火花，低频少而慢
        let hi = if t > 0.62 { 1.0 } else { 0.55 };
        let spawn = ((e * spawn_scale * hi) as usize).min(3);
        // 速度随歌曲：busyness(谱通量+高频) 与 energy 共同放大整体速率（安静≈0.6x，激烈≈3x）
        let speed = (0.6 + busyness * 1.6 + dyn_state.energy * 0.4).clamp(0.5, 3.0);
        for _ in 0..spawn {
            let px = cx + (rng.gen::<f32>() - 0.5) * bin_w as f32;
            let rise = (15.0 + rng.gen::<f32>() * 30.0) * speed;
            let vy = -rise;
            let vx = (rng.gen::<f32>() - 0.5) * (10.0 + busyness * 40.0) * speed;
            let max_life = 0.45 + rng.gen::<f32>() * (0.55 + dyn_state.treble * 0.4);
            // 颜色更亮：先提亮再掺白做高光核心
            let col = brighten(neonize(c), 0.30);
            particles.push(Particle {
                x: px, y: (bh - bar_h) as f32, vx, vy, life: max_life, max_life,
                color: col,
            });
        }
    }
    // 静音/极弱时给一条暗淡霓虹底线，不再喷粒子
    if dyn_state.energy < 0.01 {
        draw_core(&mut buf, 0, bh as i64 - 2, bw as i64, 2, neonize(neon), 0.12);
    }
    // 粒子动效：向上飘 + 横向湍流 + 低频重力回落 + 高频闪烁，寿命随亮度衰减
    let mut next: Vec<Particle> = Vec::with_capacity(particles.len());
    for p in particles.drain(..) {
        let mut p = p;
        // 速度随歌曲：重力回落也随 busyness 加快
        let speed = (0.6 + dyn_state.busyness() * 1.6 + dyn_state.energy * 0.4).clamp(0.5, 3.0);
        p.vy += (8.0 + heaviness * 46.0) * speed * DT; // 重力：低频/节拍越强回落越快
        p.x += p.vx * DT;
        p.y += p.vy * DT;
        p.life -= DT;
        if p.life <= 0.0 || p.y > bh as f32 + 2.0 || p.x < -2.0 || p.x > bw as f32 + 2.0 {
            continue;
        }
        let lr = (p.life / p.max_life).clamp(0.0, 1.0);
        // 更小：1~2 子像素的亮芯 + 低亮度光晕
        let size = (1.0 + lr).clamp(1.0, 2.0);
        let flick = 1.0 - dyn_state.treble * 0.35 * (1.0 - lr); // 高频轻微闪烁
        let a = lr * flick;
        let (dx, dy) = ((p.x - size / 2.0) as i64, (p.y - size / 2.0) as i64);
        // additive 亮芯：中心近白高光（更亮），外圈同色光晕
        draw_core(&mut buf, dx, dy, size as i64 + 1, size as i64 + 1, p.color, a * 0.5);
        draw_core(&mut buf, dx, dy, 1, 1, brighten(p.color, 0.4), a);
        next.push(p);
    }
    *particles = next;
    raster_braille(&buf, cols, rows, 0, 0)
}

pub struct NeonPlugin {
    particles: Vec<Particle>,
    peaks: Vec<f32>,
}
impl Default for NeonPlugin {
    fn default() -> Self {
        Self { particles: Vec::new(), peaks: Vec::new() }
    }
}
impl SpectrumPlugin for NeonPlugin {
    fn name(&self) -> &'static str { "neon" }
    fn render(&mut self, spectrum: &[u64], cols: u32, rows: u32, dyn_state: &AudioDyn, ctx: &SpectrumCtx) -> Vec<Line<'static>> {
        if let Some(inten) = ctx.do_burst {
            spawn_beat_burst(&mut self.particles, cols as f32 * 2.0, rows as f32 * 4.0, inten, dyn_state, spectrum);
        }
        if spectrum.iter().any(|&v| v > 3) {
            render_neon(spectrum, cols, rows, &mut self.particles, SPEC_NEON, &mut self.peaks, dyn_state)
        } else {
            Vec::new()
        }
    }
}
