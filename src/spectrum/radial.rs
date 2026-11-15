//! 放射状频谱插件（原 render_radial_spectrum）
use crate::*;
use image::RgbaImage;
use image::Rgba;
use ratatui::text::Line;
use super::{SpectrumPlugin, SpectrumCtx};

pub(crate) fn render_radial_internal(
    spectrum: &[u64],
    cols: u32,
    rows: u32,
    particles: &mut Vec<Particle>,
    dyn_state: &AudioDyn,
    phase: f32,
    beat_wave: Option<f32>,
    burst: Option<f32>,
    fade: f32,
) -> Vec<ratatui::text::Line<'static>> {
    use rand::Rng;
    const DT: f32 = 0.05;
    // draw one concentric ring stroke (true circle); pixels outside the buffer are clipped.
    fn draw_ring(buf: &mut image::RgbaImage, cx: f32, cy: f32, r: f32, w: f32, c: (u8, u8, u8), alpha: f32) {
        if r <= 0.0 || w <= 0.0 || alpha <= 0.0 { return; }
        let bw = buf.width() as i64;
        let bh = buf.height() as i64;
        let half = w * 0.5;
        let x0 = ((cx - r - half).floor() as i64).max(0);
        let x1 = ((cx + r + half).ceil() as i64).min(bw - 1);
        let y0 = ((cy - r - half).floor() as i64).max(0);
        let y1 = ((cy + r + half).ceil() as i64).min(bh - 1);
        for yy in y0..=y1 {
            let dy = yy as f32 - cy;
            for xx in x0..=x1 {
                let dx = xx as f32 - cx;
                let d = (dx * dx + dy * dy).sqrt();
                let off = (d - r).abs();
                if off > half { continue; }
                let a = alpha * (1.0 - off / half);
                if a <= 0.003 { continue; }
                let p = buf.get_pixel_mut(xx as u32, yy as u32);
                // 不再把亮部强行压成暗蓝（旧逻辑让波环整体发暗）。
                let (tr, tg, tb) = (c.0 as f32, c.1 as f32, c.2 as f32);
                p[0] = (p[0] as f32 * (1.0 - a) + tr * a).min(255.0) as u8;
                p[1] = (p[1] as f32 * (1.0 - a) + tg * a).min(255.0) as u8;
                p[2] = (p[2] as f32 * (1.0 - a) + tb * a).min(255.0) as u8;
                p[3] = 255;
            }
        }
    }
    let bw = (cols.max(1) * 2) as u32;
    let bh = (rows.max(1) * 4) as u32;
    // 铺满整条频谱带的冷色底晕：两端也有颜色，不再是一片纯黑/露出终端底色。
    // 取配色中段的低亮度版本，射线与波环叠在它上面形成明暗层次。
    let base = radial_color(0.55);
    let mut buf = image::RgbaImage::from_pixel(bw, bh, image::Rgba([
        (base.0 as f32 * 0.32) as u8,
        (base.1 as f32 * 0.32) as u8,
        (base.2 as f32 * 0.32) as u8,
        255,
    ]));
    let cx = bw as f32 / 2.0;
    let cy = bh as f32 / 2.0;
    let n = spectrum.len().max(1) as u32;
    // breathe: slow sine + audio pulse; core pulses with the beat.
    let breathe = 1.0 + 0.08 * (phase * 0.18).sin() + dyn_state.pulse * 0.5;
    // 半径取长边的一半且不留 2% 余量，让圆盘正好铺满水平方向；
    // 上下的溢出仍由边框裁掉（不压成椭圆）。
    let radius = (bw as f32).max(bh as f32) / 2.0;
    let core_r = ((bh as f32) * (0.10 + dyn_state.bass * 0.12 + dyn_state.pulse * 0.12) * breathe).min(bh as f32 * 0.18);
    // 背景：用巨大半径的辉光把整片频谱区均匀铺一层冷蓝底，保证“填满区域”。
    // Base wavefronts: phase is advanced in the main loop at an audio-driven rate,
    // so busier music expands them faster instead of a fixed-speed spin.
    let wave_count = 3i32;
    let mut waves: Vec<f32> = Vec::with_capacity(wave_count as usize);
    for j in 0..wave_count {
        let t = (phase + j as f32 / wave_count as f32) % 1.0;
        waves.push(core_r + t * (radius - core_r));
    }
    let mut ray_waves = waves.clone();
    if let Some(bt) = beat_wave {
        ray_waves.push(core_r + bt * (radius - core_r));
    }
    // 360 spokes from the center; all tips land on one circle, so the outline stays round.
    // brightness floor keeps every direction visible regardless of audio level; overflow is clipped.
    let spokes = 360u32;
    for k in 0..spokes {
        let ang = (k as f32 / spokes as f32) * std::f32::consts::PI * 2.0 - std::f32::consts::FRAC_PI_2;
        let bin = (((ang + std::f32::consts::FRAC_PI_2) / (std::f32::consts::PI * 2.0)) * n as f32) as usize % n as usize;
        let e = ((spectrum[bin] as f32) / 100.0).clamp(0.0, 1.0).powf(0.55).max(0.18);
        // 射线一直画到边界，左右两端不留空白；频谱强度改由亮度/线宽表达。
        let r_tip = radius;
        draw_ray(&mut buf, cx, cy, ang, core_r, core_r, r_tip, r_tip, e, &ray_waves);
    }
    // Ripple rings ride the SAME wavefronts as the spoke bright bands, so the two reinforce
    // into one clearly visible round wave expanding from the center; overflow is clipped.
    for &wr in &waves {
        let t = (wr - core_r) / (radius - core_r).max(1.0);   // 0 = center, 1 = edge
        let env = (1.0 - t).powf(1.1);                           // bright at birth, fades outward
        let a = ((0.5 + dyn_state.energy * 0.3 + dyn_state.pulse * 0.5) * env
            * (0.75 + 0.25 * dyn_state.passage_gain())).min(0.95);
        let c = brighten(radial_color(t), 0.05);
        draw_ring(&mut buf, cx, cy, wr, 3.0 + dyn_state.bass * 3.0, c, a);
    }
    // Beat wavefront: brighter/thicker ring born on each onset, fading with its age.
    if let Some(bt) = beat_wave {
        let wr = core_r + bt * (radius - core_r);
        let env = (1.0 - bt).powf(0.8);
        let a = ((0.65 + dyn_state.pulse * 0.35) * env).min(0.95);
        let c = brighten(radial_color(0.12 + bt * 0.55), 0.15);
        draw_ring(&mut buf, cx, cy, wr, 5.0 + dyn_state.bass * 4.0, c, a);
    }
    // 曲速粒子：强频段从核心沿对应角度向外喷射细长拖尾光线（运动模糊）。
    let mut rng = rand::thread_rng();
    // Onset burst: a short-lived spray of streak particles out from the core.
    if let Some(inten) = burst {
        let count = ((10.0 + inten * 14.0 + dyn_state.energy * 16.0).min(40.0)
            * dyn_state.passage_gain()) as usize;
        for _ in 0..count {
            let a = rng.gen::<f32>() * std::f32::consts::PI * 2.0;
            let speed = (bh as f32) * (1.2 + inten * 1.5 + rng.gen::<f32>() * 1.5);
            let life = 0.25 + rng.gen::<f32>() * (0.4 + dyn_state.treble * 0.4);
            particles.push(Particle {
                x: cx + a.cos() * core_r,
                y: cy + a.sin() * core_r,
                vx: a.cos() * speed,
                vy: a.sin() * speed,
                life, max_life: life,
                color: radial_color(0.05),
            });
        }
    }
    let spawn_scale = (0.3 + dyn_state.energy * 2.2).clamp(0.0, 3.0) * dyn_state.passage_gain();
    for i in 0..n {
        let e = ((spectrum[i as usize] as f32) / 100.0).clamp(0.0, 1.0).powf(0.6);
        if e < 0.05 { continue; }
        let ang = (i as f32 / n as f32) * std::f32::consts::PI * 2.0 - std::f32::consts::FRAC_PI_2;
        let sp = (e * spawn_scale * (0.6 + rng.gen::<f32>() * 0.6)) as usize;
        for _ in 0..sp.min(2) {
            let a = ang + (rng.gen::<f32>() - 0.5) * 0.06;
            let life = 0.45 + rng.gen::<f32>() * (0.7 + dyn_state.treble * 0.6);
            // reach the window edge within its lifetime; flatten vertical speed
            // so particles survive the short band instead of leaving at once
            let speed = (bw as f32 / 2.0) / life * (0.75 + e * 0.5) * (0.85 + dyn_state.busyness() * 0.4);
            let vy_scale = (bh as f32 / bw as f32).max(0.08);
            particles.push(Particle {
                x: cx + a.cos() * core_r,
                y: cy + a.sin() * core_r,
                vx: a.cos() * speed,
                vy: a.sin() * speed * vy_scale,
                life, max_life: life,
                color: radial_color(0.05),
            });
        }
    }
    // 更新并绘制粒子（细长拖尾 = 曲速光线）。
    let mut next: Vec<Particle> = Vec::with_capacity(particles.len());
    for p in particles.drain(..) {
        let mut p = p;
        p.x += p.vx * DT;
        p.y += p.vy * DT;
        p.life -= DT;
        if p.life <= 0.0 || p.x < -4.0 || p.x > bw as f32 + 4.0 || p.y < -4.0 || p.y > bh as f32 + 4.0 {
            continue;
        }
        // 颜色随径向位置连续变化：与所在环带的 radial_color 渐变色一致，
        // 而不是 spawn 时随机取一段 —— 随机会让相邻格色相跳变，看起来粗糙。
        let pd = ((p.x - cx) * (p.x - cx) + (p.y - cy) * (p.y - cy)).sqrt() / radius.max(1.0);
        p.color = radial_color(pd.clamp(0.0, 1.0));
        let lr = (p.life / p.max_life).clamp(0.0, 1.0);
        draw_streak(&mut buf, p.x, p.y, p.vx, p.vy, p.color, lr);
        next.push(p);
    }
    *particles = next;
    // 炽白冷光核 + 光晕（最后画，叠在最上层）。
    draw_core_glow(&mut buf, cx, cy, core_r, core_r, breathe);
    // fade out/in: 只压暗 RGB，不动 alpha。
    // buf 铺的是不透明底，alpha 若跟着缩放，淡出到 0 时所有像素 alpha=0 ->
    // raster_braille 判 bits==0 -> 输出裸空格 -> 整片空行（实测 fade=0 有 400 个空格）。
    // 乐段增益并入 fade：弱段整片压暗，高能段全亮（同样只压 RGB、不动 alpha）。
    let eff_fade = fade * (0.75 + 0.25 * dyn_state.passage_gain());
    if eff_fade < 1.0 {
        for p in buf.pixels_mut() {
            p[0] = (p[0] as f32 * eff_fade) as u8;
            p[1] = (p[1] as f32 * eff_fade) as u8;
            p[2] = (p[2] as f32 * eff_fade) as u8;
        }
    }
    // onset flash: short full-frame brightness lift on each beat, decaying over
    // the first ~0.2 of the beat wavefront life so it lasts more than one frame
    if let Some(bt) = beat_wave {
        let flash = (1.0 - (bt / 0.2).min(1.0)).max(0.0);
        if flash > 0.01 {
            let add = (flash * 50.0) as u32;
            for p in buf.pixels_mut() {
                p[0] = (p[0] as u32 + add).min(255) as u8;
                p[1] = (p[1] as u32 + add).min(255) as u8;
                p[2] = (p[2] as u32 + add).min(255) as u8;
            }
        }
    }
    raster_braille(&buf, cols, rows, 0, 0)
}

pub struct RadialPlugin {
    particles: Vec<Particle>,
}
impl Default for RadialPlugin {
    fn default() -> Self {
        Self { particles: Vec::new() }
    }
}
impl SpectrumPlugin for RadialPlugin {
    fn name(&self) -> &'static str { "放射" }
    fn render(&mut self, spectrum: &[u64], cols: u32, rows: u32, dyn_state: &AudioDyn, ctx: &SpectrumCtx) -> Vec<Line<'static>> {
        render_radial_internal(spectrum, cols, rows, &mut self.particles, dyn_state, ctx.phase, ctx.beat_wave, ctx.do_burst, ctx.fade)
    }
}
