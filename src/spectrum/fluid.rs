//! Fluid 频谱模式：柔和海浪版 —— 圆缓涌浪 + 沿浪脊溅起的偏白浪花簇。
//! 入口：render_fluid_spectrum(spectrum, cols, rows, phase, state, dyn_state)。
//! 依赖 crate 根的 AudioDyn（节奏/段落）与 raster_fluid（按像素内容生成 braille 点阵）。
//!
//! 【合入说明】整文件覆盖 src/fluid.rs 即可，不改 main.rs / Cargo.toml / 测试。
//! 对外名字不变：pub struct FluidState / pub fn render_fluid_spectrum。
//! 可调常量都在 render_fluid_spectrum 函数体顶部：
//!   WAVE / SOFT / FOAM / WHITE_BASE / WHITE_SPOT / DROP_GATE_EXTRA / WHITE_TARGET
//!
//! 默认 FOAM=0 / SOFT=1 / WAVE=1 / 白飞沫开启为交付值。

use crate::{AudioDyn, raster_fluid};
use super::{SpectrumPlugin, SpectrumCtx};

/// 霓虹蓝水底：R 近 0、G 电光蓝、B 拉满；越靠底越深但始终保持高饱和。
fn water_color(depth: f32) -> (u8, u8, u8) {
    let d = depth.clamp(0.0, 1.0);
    (
        (2.0 + 3.0 * (1.0 - d)) as u8,
        (52.0 + 58.0 * (1.0 - d)) as u8,
        (190.0 + 25.0 * (1.0 - d)) as u8,
    )
}

/// 纯算术静态 hash：只依赖 x / k / j，绝不依赖 t / scroll。
/// rem_euclid(1.0) 保证结果落在 [0,1)，与 hash 的常规定义一致。
#[inline]
fn h1(x: f32, k: f32, j: f32) -> f32 {
    ((x * 12.9898 + k * 78.233 + j * 37.719 + 0.7).sin() * 43758.5453)
        .rem_euclid(1.0)
}

/// 浪脊基形：soft=0 是带尖峰的 Stokes 波；soft=1 是近乎纯正弦的圆缓涌浪。
/// 只依赖 x / k —— 这是无横向漂移的关键。
#[inline]
fn ridge_s(x: f32, k: f32, soft: f32) -> f32 {
    let th = x * (0.035 - 0.010 * soft) + 1.3 + k * 1.7;
    let a2 = 0.34 - 0.22 * soft;
    let a3 = 0.10 - 0.10 * soft;
    th.sin() + a2 * (2.0 * th + 0.7).sin() + a3 * (3.0 * th).sin()
}

/// 静态浪脊高度（像素）。amt=0 → 0，严格退回直带版。
#[inline]
fn crest(x: f32, k: f32, amt: f32, soft: f32) -> f32 {
    let env = (0.70 + 0.12 * soft)
        + (0.30 - 0.12 * soft) * (x * 0.015 + 0.4 + k * 0.9).sin();
    let sharp = match k as usize { 0 => 3.6, 1 => 2.6, _ => 1.9 };
    let mellow = match k as usize { 0 => 2.2, 1 => 1.7, _ => 1.3 };
    amt * (sharp + (mellow - sharp) * soft) * ridge_s(x, k, soft) * env
}

/// 0..1 峰强度：控制浪冠碎沫密度。静态。
#[inline]
fn crest_norm(x: f32, k: f32, soft: f32) -> f32 {
    ((ridge_s(x, k, soft) + 0.55) / 1.75).clamp(0.0, 1.0)
}

/// 流体状态：每列能量 + 惯性速度 + 全局纵向滚动量。
/// scroll 是**单调累加**的绝对量（渲染时 rem_euclid 取模）。
#[derive(Default)]
pub struct FluidState {
    height: Vec<f32>,
    vel: Vec<f32>,
    scroll: f32,
}

impl FluidState {
    fn resize(&mut self, n: usize) {
        if self.height.len() != n {
            self.height = vec![0.12; n];
            self.vel = vec![0.0; n];
            self.scroll = 0.0;
        }
    }
}

/// 流体频谱：铺满整条带的纵向流动水体。
/// 圆缓浪脊横向铺满、整体沿 y 下移；浪尖溅出宽而淡的蓝色光斑；
/// 带底有守恒构造的溅起水花。整幅图只是 y 的平移函数 → 无横向漂移。
pub fn render_fluid_spectrum(
    spectrum: &[u64],
    cols: u32,
    rows: u32,
    phase: f32,
    state: &mut FluidState,
    dyn_state: &AudioDyn,
) -> Vec<ratatui::text::Line<'static>> {
    // 保留签名但刻意不使用：phase 每帧变化，若参与着色会引入非平移的
    // 整体亮度漂移，破坏「每列均亮稳定」的约束。
    let _ = phase;

    // ★ 只改这里就能调效果（其余代码一行都不用动）：
    //   WAVE = 0 → 横平直带；WAVE = 1 → 圆缓涌浪
    //   SOFT = 0 → 合入前的锐利版；SOFT = 1 → 柔和（交付默认）
    //   FOAM = 0 → 关掉旧的青白飞沫层（默认）；FOAM = 0.6 → 完全回到旧代码
    //   想稍微锐一点：SOFT = 0.6~0.8；想留一点青白碎点：FOAM = 0.2~0.3
    //   完全回退旧画面：WAVE=0, SOFT=0, FOAM=0.6
    //
    //   ★ 白色（颜色插值，不是亮度叠加）：
    //   WHITE_BASE / WHITE_SPOT 整条带统一偏白 + 飞沫处额外更白；WHITE_TARGET 是淡青白。
    //
    //   ★ 本轮：把「像下雨的散点」改成「沿浪脊溅起的浪花簇」
    //   CLUSTER_FREQ  浪花簇的横向密度（越小，一朵越大；0.08 → 全带约 2~3 朵）
    //   CROWN_LO/HI   峰区门控范围；CROWN_POW 硬门控指数（越大，簇间空隙越明显）
    //   ARC_H         拱高：浪脊峰顶水花溅得更高，连起来是一条弧
    //   W_BASE        水花宽度（越大越成片、越柔，别调到 1.0 以下）
    //   SAT_*         主水珠上方派生的小水珠（门槛 / 亮度 / 上抛距离）
    //   DROP_GATE_EXTRA 现在只控制小水珠的稀疏度（沿用「飞沫减量」）
    //   想浪花更多：CLUSTER_FREQ→0.12 或 CROWN_LO→0.42；想更少：CROWN_LO→0.56
    //   所有形态量都乘 SOFT（或只在 SOFT>0 分支生效），SOFT=0 严格回退。
    const WAVE: f32 = 1.0;
    const SOFT: f32 = 1.0;
    const FOAM: f32 = 0.0;
    // 两层「白」其实是往高饱和电光蓝里插值，不是往灰白里插值：
    //   WHITE_TARGET 原来 R=205 太高（接近中性白）→ 喷溅被洗成灰蓝；
    //   现在 R 压到 130、G/B 拉满 → 浪脊是电光蓝，最亮处是电光青。
    // WHITE_BASE 也从 0.80 降到 0.50：整条带子少被提白，蓝色保持饱和。
    const WHITE_BASE: f32 = 0.50;
    const WHITE_SPOT: f32 = 1.05;
    const WHITE_TARGET: [f32; 3] = [130.0, 220.0, 255.0];  // 电光青蓝，R 压低、B 拉满
    const CLUSTER_FREQ: f32 = 0.08;
    const CROWN_LO: f32 = 0.50;
    const CROWN_HI: f32 = 0.84;
    const CROWN_POW: f32 = 2.2;
    const ARC_H: f32 = 3.2;
    const W_BASE: f32 = 1.20;
    const SAT_LO: f32 = 0.50;
    const SAT_AMP: f32 = 0.55;
    const SAT_UP: f32 = 1.2;
    const DROP_GATE_EXTRA: f32 = 0.28;
    const NBANDS: usize = 3;

    let bw = (cols.max(1) * 2) as u32;
    let bh = (rows.max(1) * 4) as u32;
    let n = bw as usize;
    let nb = spectrum.len().max(1);
    state.resize(n);
    let bh_f = bh as f32;
    let gain = dyn_state.passage_gain();
    let wave = WAVE;
    let soft = SOFT;

    // ---- 每列目标能量：x 映射到频段，线性插值（原样保留）----
    let mut target = vec![0.0f32; n];
    for x in 0..n {
        let f = (x as f32 / n as f32) * (nb - 1) as f32;
        let i0 = f as usize;
        let i1 = (i0 + 1).min(nb - 1);
        let frac = f - i0 as f32;
        target[x] = ((spectrum[i0] as f32 / 100.0) * (1.0 - frac)
            + (spectrum[i1] as f32 / 100.0) * frac).clamp(0.0, 1.0);
    }
    // ---- 横向平滑（原样保留）----
    let mut smooth = target.clone();
    for x in 0..n {
        let l = if x > 0 { target[x - 1] } else { target[x] };
        let r = if x + 1 < n { target[x + 1] } else { target[x] };
        smooth[x] = (l + 2.0 * target[x] + r) / 4.0;
    }
    // ---- 一阶惯性（原样保留：水位跟踪测试依赖它）----
    for x in 0..n {
        state.vel[x] = (state.vel[x] + 0.06 * (smooth[x] - state.height[x])) * 0.84;
        state.height[x] = (state.height[x] + state.vel[x]).clamp(0.0, 1.0);
    }

    // ---- 三信号纵向流速（原样保留）----
    let avg = state.height.iter().copied().sum::<f32>() / n as f32;
    let speed = (0.7 + avg * 2.0
        + dyn_state.busyness() * 1.4
        + dyn_state.pulse * 3.0).clamp(0.5, 5.0);
    state.scroll += speed;                       // 单调累加，保证 s1 > s0
    let scroll = state.scroll.rem_euclid(bh_f);

    // ---- 底部溅起：全局相位（与 x 无关）----
    let u = scroll / bh_f;
    let env = (1.0 - u).powf(1.4);
    let alpha = 0.88 * env;
    let w_fb = 1.7 + 0.4 * soft;                // 泡沫带拉宽 → 更柔
    let fb: Vec<f32> = (0..bh).map(|y| {
        let t = (bh_f - 1.0 - y as f32) / w_fb;
        (-t * t).exp()
    }).collect();
    let fb_sum: f32 = fb.iter().sum();

    // ---- 柔和参数组（全部是常量，不依赖 y / head）----
    let spray_b = 210.0 - 10.0 * soft;
    let sigma_k = 1.1 + 0.40 * soft;
    let halo_k = 3.0 - 0.40 * soft;
    let tail_w = 0.42 - 0.14 * soft;
    let hi_pow = 0.75 + 0.13 * soft;
    let a0 = 0.38 - 0.08 * soft;
    let a1 = 0.16 - 0.04 * soft;
    let flash_coef = (1.30 + 0.60 * wave) - soft * (0.20 + 0.35 * wave);
    let gate_bias = 0.20 - 0.13 * soft;
    let amp_p0 = 0.40 + 0.42 * soft;
    let amp_p1 = 1.00 - 0.78 * soft;
    let face_k = 0.30 * wave * (1.0 - soft);     // soft=1 → 迎风面消失
    let crown_k = 0.26 * wave * (1.0 - soft);    // soft=1 → 浪冠亮线消失
    let caustic_k = 0.05 - 0.03 * soft;

    const SPRAY_PEAK: [f32; 3] = [55.0, 105.0, 0.0];
    const SPLASH_COL: [f32; 3] = [8.0, 86.0, 220.0];

    let mut buf = image::RgbaImage::from_pixel(bw, bh, image::Rgba([10, 16, 30, 255]));

    for x in 0..bw {
        let xf = x as f32;
        let ex = state.height[x as usize];
        let band = xf / bw as f32;

        // 静态横向起伏：只依赖 x，不含 t / scroll
        let wob = (xf * 0.05).sin() * 0.6 + (xf * 0.013).sin() * 0.3;

        let tau = 2.6 + ex * 2.4 + band * 0.9;
        let core_w = 0.80 + ex * 0.55;
        let sigma = core_w * sigma_k;
        let halo_r = sigma * halo_k;
        let q = (-bh_f / tau).exp();
        let inv_q = 1.0 / (1.0 - q);

        // ---- 浪脊：只随 scroll 平移，形状起伏但不含 t ----
        // 浪脊幅度跟随该列的频谱能量 ex（x 已线性映射到频段，左低右高）：
        //   乐曲在这个频段越强，这一段的浪就越高 → 波浪本身就是频谱。
        // ex 来自 state.height（已做横向平滑 + 一阶惯性），所以跟随是平滑的、
        // 不会引入逐列抖动；给定同一份频谱，整幅图仍只是 y 的平移（无横向漂移）。
        let wave_amp = wave * (0.25 + 1.05 * ex);
        let mut heads = [0.0f32; NBANDS];
        let mut peak = [0.0f32; NBANDS];
        for k in 0..NBANDS {
            let kf = k as f32;
            let base = (kf + 0.5) / NBANDS as f32 * bh_f;
            heads[k] = (base + scroll + wob + crest(xf, kf, wave_amp, soft)).rem_euclid(bh_f);
            peak[k] = crest_norm(xf, kf, soft);
        }

        // ---- 该列溅起水滴 dr(y)（守恒构造）----
        let rnd = h1(xf, 0.0, 0.0);
        let hk = (0.42 + 0.58 * rnd) * (0.60 + 0.40 * (0.5 + 0.5 * (xf * 0.19).sin()));
        let top = (bh_f - 1.0) - (0.6 + 3.6 * env) * hk;
        let w_dr = 1.40 + 0.50 * soft;           // 水滴拉宽
        let dr: Vec<f32> = (0..bh).map(|y| {
            let t = (y as f32 - top) / w_dr;
            (-t * t).exp()
        }).collect();
        let dr_sum: f32 = dr.iter().sum();
        let inv_dr = fb_sum / dr_sum.max(1e-6);   // Σ dr*inv_dr == fb_sum
        let sk = (0.42 + 0.30 * ex) - soft * (0.08 + 0.06 * ex);

        // ---- ★ 浪花簇：沿浪脊峰区横向连续的拱形冠状水花 ----
        // 旧版「每列独立用静态 hash 决定 gate / delta / amp / w」是雨感的根源：
        // 横向完全不相关、谷区也有点、每点随 head 等速下移 → 读作斜雨。
        // 这里改成 x 的连续函数主导（hash 只做 ±0.15 微抖），谷区完全不溅。
        // 所有参数仍只依赖 x / k（静态）→ flash 是 (head-y) 的平移函数，
        // Σ_y 与 head 无关 → 无横向漂移仍是数学保证。
        let mut drops: Vec<(usize, f32, f32, f32)> = Vec::with_capacity(6);
        if soft > 0.0 {
            for k in 0..NBANDS {
                let kf = k as f32;
                // ridge = 浪脊活跃段（长周期，静态）；cluster = 段内分成几朵（中频，静态）
                let ridge = peak[k];
                let cluster = 0.5 + 0.5 * (xf * CLUSTER_FREQ + kf * 2.1 + ridge * 1.3).sin();
                let s = ridge * 0.55 + cluster * 0.45;
                let t = ((s - CROWN_LO) / (CROWN_HI - CROWN_LO)).clamp(0.0, 1.0);
                let mut crown = t * t * (3.0 - 2.0 * t);   // smoothstep
                crown = crown.powf(CROWN_POW);              // 硬门控：簇之间留出空隙
                if crown < 0.03 { continue; }               // 谷区完全不溅 → 不再像雨
                let arc = crown.powf(0.7);
                let jt = h1(xf, kf, 0.0) - 0.5;
                let delta = (0.8 + ARC_H * arc) + 0.30 * jt;  // 峰顶高、两侧低 → 拱形
                let amp = (0.45 + 0.55 * crown) * (0.85 + 0.30 * h1(xf, kf, 7.0));
                let w = W_BASE + 0.50 * arc;                   // 峰顶水花更宽（成片）
                drops.push((k, delta, amp, w));                // 主水珠
                // 卫星小水珠：同一簇派生、向上溅得更高更淡 → 有主次，不是散点
                if crown > SAT_LO + DROP_GATE_EXTRA * 0.30 {
                    drops.push((k, delta + SAT_UP + 0.4 * jt, amp * SAT_AMP, w * 0.70));
                }
            }
        } else {
            // SOFT=0：逐字等同合入前的旧 drops，保证无损回退
            for k in 0..NBANDS {
                let kf = k as f32;
                let gate = 0.34 - gate_bias * wave * peak[k];
                let amp_wave = 1.0 + wave * (amp_p0 + amp_p1 * peak[k] - 1.0);
                for j in 0..3 {
                    let jf = j as f32;
                    if h1(xf, kf, jf + 23.0) < gate { continue; }
                    drops.push((
                        k,
                        0.7 + 4.4 * h1(xf, kf, jf),
                        (0.30 + 0.70 * h1(xf, kf, jf + 7.0)) * amp_wave,
                        0.26 + 0.22 * h1(xf, kf, jf + 13.0),
                    ));
                }
            }
        }

        for y in 0..bh {
            let yy = y as f32;

            // ---- 宽高斯亮核 + 淡光晕 + 单向拖尾（face / crown 在 soft=1 归零）----
            let mut glint = 0.0f32;
            for k in 0..NBANDS {
                let head = heads[k];
                let d = (yy - head).rem_euclid(bh_f);       // 0=浪头，增大=流向(下)
                let d_up = (-d).rem_euclid(bh_f);
                let core = (-(d * d) / (2.0 * sigma * sigma)).exp()
                    * (1.0 - 0.12 * wave * (1.0 - soft));
                let halo = (-(d * d) / (2.0 * halo_r * halo_r)).exp() * 0.28;
                let face = face_k
                    * (-(d_up * d_up) / (2.0 * (sigma * 0.58) * (sigma * 0.58))).exp();
                let tail = ((-d / tau).exp() - q) * inv_q;
                let crown = if crown_k > 0.0 && peak[k] > 0.45 {
                    let t = (d_up - 1.2) / 0.62;
                    (crown_k * (peak[k] - 0.45) / 0.55) * (-t * t).exp()
                } else { 0.0 };
                let g = (core + face + halo + tail * tail_w + crown).min(1.0);
                if g > glint { glint = g; }
            }

            // ---- 深蓝水底 ----
            let depth = (yy / bh_f).clamp(0.0, 1.0);
            let c = water_color(depth);
            let caustic = ((xf * 0.115 + yy * 0.24).sin()
                * (xf * 0.061 - yy * 0.17).sin()).max(0.0).powi(3);
            // 底色只乘「与 y 无关的常系数」+ 静态焦散 → 与高光完全解耦。
            let body_k = (0.54 + 0.20 * ex + caustic * caustic_k) * (0.90 + 0.10 * gain);
            let mut col = [
                c.0 as f32 * body_k,
                c.1 as f32 * body_k,
                c.2 as f32 * body_k,
            ];

            // ---- 飞沫调制量：从浪头往上（逆流向）的环绕距离 ----
            let mut flash = 0.0f32;
            for &(k, delta, amp, w) in &drops {
                let d_up = (heads[k] - yy).rem_euclid(bh_f);
                let t = (d_up - delta) / w;
                let sp = amp * (-t * t).exp();
                if sp > flash { flash = sp; }
            }

            let hi = glint.min(1.0).powf(hi_pow);
            if hi > 0.02 {
                let core3 = hi.powi(3);
                let blue = [
                    10.0 + SPRAY_PEAK[0] * core3,
                    55.0 + SPRAY_PEAK[1] * core3,
                    spray_b,
                ];
                // ★ 两层白（颜色插值，不是亮度叠加）：
                //   base 是常量层，整条水带统一偏白 → 不产生相邻列的横向硬边；
                //   spot 只在 flash（离散光斑）处额外更白 → 少量白水珠。
                //   两者都乘 soft → SOFT=0 时 tw=0，spray 严格等于原来的蓝 spray。
                //   flash 是 F(y-head) 的平移函数 → 守恒与无横向漂移不变。
                let tw = if soft > 0.0 {
                    ((WHITE_BASE + flash * WHITE_SPOT) * soft).min(1.0)
                } else { 0.0 };
                let spray = [
                    blue[0] + (WHITE_TARGET[0] - blue[0]) * tw,
                    blue[1] + (WHITE_TARGET[1] - blue[1]) * tw,
                    blue[2] + (WHITE_TARGET[2] - blue[2]) * tw,
                ];
                // ★ 加法叠加：col(y) = 静态底色(y) + Σ F(y-head_k)，
                //   Σ_y 与 head 无关 → 无横向漂移严格成立。
                let a = ((0.28 + 0.72 * hi)
                    * (a0 + a1 * ex + 0.45 * dyn_state.pulse)
                    * (1.0 + flash_coef * flash)).min(1.0);
                for ch in 0..3 {
                    col[ch] += spray[ch] * a;
                }
                // FOAM：合入前旧代码的青白飞沫层。默认 FOAM=0 关掉（更柔和）；
                // 设 0.6 即逐像素恢复旧效果。这块与旧代码逐字同构，仅把 0.6 抽成常量。
                // 放在 hi 块内、只乘 flash，仍是 F(y-head) 平移函数 → 守恒不变。
                let foam = flash * FOAM;
                col[0] += 15.0 * foam;
                col[1] += 170.0 * foam;
                col[2] += 90.0 * foam;
            }

            // ---- 底部溅起（守恒构造）----
            let s = fb[y as usize] * (1.0 - alpha)
                  + dr[y as usize] * inv_dr * alpha;
            for ch in 0..3 {
                col[ch] += SPLASH_COL[ch] * s * sk;
            }

            let p = buf.get_pixel_mut(x, y);
            p[0] = col[0].clamp(0.0, 255.0) as u8;
            p[1] = col[1].clamp(0.0, 255.0) as u8;
            p[2] = col[2].clamp(0.0, 255.0) as u8;
            p[3] = 255;
        }
    }
    raster_fluid(&buf, cols, rows, 0, 0)
}

#[cfg(test)]
mod tests {
    use crate::AudioDyn;
    use ratatui::style::Color;
    use super::{FluidState, render_fluid_spectrum};

    /// 铺满、两端有颜色、每列水位带惯性、高能量整体更亮。
    #[test]
    fn fluid_spectrum_fills_band_and_has_inertia() {
        let cols = 100u32;
        let rows = 4u32;
        let mut st = FluidState::default();
        let d = AudioDyn::default();
        let quiet = vec![0u64; 64];
        let loud = vec![85u64; 64];
        for _ in 0..60 {
            let _ = render_fluid_spectrum(&quiet, cols, rows, 0.1, &mut st, &d);
        }
        let h_quiet = st.height.iter().cloned().sum::<f32>() / st.height.len() as f32;
        for _ in 0..60 {
            let _ = render_fluid_spectrum(&loud, cols, rows, 0.1, &mut st, &d);
        }
        let h_loud = st.height.iter().cloned().sum::<f32>() / st.height.len() as f32;
        assert!(h_loud > h_quiet + 0.05, "高能量应让整体水位更高，quiet {} loud {}", h_quiet, h_loud);

        let lines = render_fluid_spectrum(&loud, cols, rows, 0.1, &mut st, &d);
        assert_eq!(lines.len() as u32, rows);
        for l in &lines {
            let width: usize = l.spans.iter().map(|s| s.content.chars().count()).sum();
            assert_eq!(width as u32, cols, "每行必须铺满");
            let raw = l.spans.iter()
                .filter(|s| s.style.fg.is_none() && s.style.bg.is_none())
                .count();
            assert_eq!(raw, 0, "不能有裸空格");
            for sp in [&l.spans[0], &l.spans[l.spans.len() - 1]] {
                for c in [sp.style.fg, sp.style.bg].into_iter().flatten() {
                    if let Color::Rgb(r, g, b) = c {
                        assert!(r as u32 + g as u32 + b as u32 > 40, "两端必须有颜色");
                    }
                }
            }
        }
    }

    /// 纵向流动：水花随 scroll 沿 y 滚动（运动方向纵向）；列亮度跟随频谱（低频左高/高频右高）。
    #[test]
    fn fluid_spectrum_flows_vertically_and_tracks_spectrum() {
        let cols = 100u32;
        let rows = 4u32;
        let mut st = FluidState::default();
        let d = AudioDyn::default();
        // 低频（左）强 vs 高频（右）强
        let mut low = vec![0u64; 64];
        let mut high = vec![0u64; 64];
        for i in 0..16 { low[i] = 90; }
        for i in 44..64 { high[i] = 90; }
        for _ in 0..60 {
            let _ = render_fluid_spectrum(&low, cols, rows, 0.1, &mut st, &d);
        }
        let left_low = st.height[10];
        let right_low = st.height[st.height.len() - 10];
        assert!(left_low > right_low + 0.05, "低频强时左列应更高，left {} right {}", left_low, right_low);
        for _ in 0..60 {
            let _ = render_fluid_spectrum(&high, cols, rows, 0.1, &mut st, &d);
        }
        let left_high = st.height[10];
        let right_high = st.height[st.height.len() - 10];
        assert!(right_high > left_high + 0.05, "高频强时右列应更高，left {} right {}", left_high, right_high);
        // 纵向流动：scroll 推进（水花上下走），且相邻帧变化分散在垂直方向
        let s0 = st.scroll;
        let l1 = render_fluid_spectrum(&low, cols, rows, 0.40, &mut st, &d);
        let s1 = st.scroll;
        assert!(s1 > s0, "应沿纵向流动（scroll 累加），实际 {} -> {}", s0, s1);
        let l2 = render_fluid_spectrum(&low, cols, rows, 0.41, &mut st, &d);
        let mut rows_changed = vec![0u32; rows as usize];
        for (ri, (a, b)) in l1.iter().zip(l2.iter()).enumerate() {
            for (sa, sb) in a.spans.iter().zip(b.spans.iter()) {
                if let (Some(Color::Rgb(r1, g1, b1)), Some(Color::Rgb(r2, g2, b2))) =
                    (sa.style.fg, sb.style.fg) {
                    let dd = (r1 as i32 - r2 as i32).abs() + (g1 as i32 - g2 as i32).abs() + (b1 as i32 - b2 as i32).abs();
                    if dd > 10 { rows_changed[ri] += 1; }
                }
            }
        }
        let n_changed = rows_changed.iter().filter(|&&c| c > 0).count();
        assert!(n_changed >= 3, "纵向流动应跨多行变化，实际 {} 行有变化", n_changed);
    }

    /// 纵向流动不横向漂移：连续两帧每列均亮应稳定（水花上下流，不左右平移），
    /// 同时整帧确实有变化（水真的在动）。
    #[test]
    fn fluid_spectrum_undulates_without_horizontal_drift() {
        let cols = 100u32;
        let rows = 4u32;
        let mut st = FluidState::default();
        let d = AudioDyn::default();
        let loud = vec![85u64; 64];
        for k in 0..40 {
            let _ = render_fluid_spectrum(&loud, cols, rows, 0.1 + k as f32 * 0.01, &mut st, &d);
        }
        let l1 = render_fluid_spectrum(&loud, cols, rows, 0.5, &mut st, &d);
        let l2 = render_fluid_spectrum(&loud, cols, rows, 0.51, &mut st, &d);

        // 每列均亮：纵向流动只在水里上下移动，列均亮应保持稳定（否则就是横向平移）
        let col_lum = |lines: &[ratatui::text::Line<'static>], col: usize| -> u32 {
            let mut s = 0u32;
            let mut c = 0u32;
            for l in lines {
                let mut x = 0usize;
                for sp in &l.spans {
                    let w = sp.content.chars().count();
                    if x <= col && col < x + w {
                        if let Some(Color::Rgb(r, g, b)) = sp.style.fg {
                            s += r as u32 + g as u32 + b as u32;
                            c += 1;
                        }
                    }
                    x += w;
                }
            }
            if c > 0 { s / c } else { 0 }
        };
        let mut max_col_diff = 0u32;
        for c in 0..cols as usize {
            let a = col_lum(&l1, c);
            let b = col_lum(&l2, c);
            max_col_diff = max_col_diff.max((a as i32 - b as i32).abs() as u32);
        }
        assert!(max_col_diff < 90, "纵向流动应保持每列均亮稳定（无横向平移），实际最大列差 {}", max_col_diff);

        // 整帧确实在动（水真的在流），且变化分散在垂直方向上
        let mut changed = 0usize;
        let mut rows_changed = vec![0u32; rows as usize];
        for (ri, (a, b)) in l1.iter().zip(l2.iter()).enumerate() {
            for (sa, sb) in a.spans.iter().zip(b.spans.iter()) {
                if let (Some(Color::Rgb(r1, g1, b1)), Some(Color::Rgb(r2, g2, b2))) =
                    (sa.style.fg, sb.style.fg) {
                    let dd = (r1 as i32 - r2 as i32).abs() + (g1 as i32 - g2 as i32).abs() + (b1 as i32 - b2 as i32).abs();
                    if dd > 10 { changed += 1; rows_changed[ri] += 1; }
                }
            }
        }
        assert!(changed > 20, "纵向流动应让画面有变化，实际变化 {}", changed);
        let n_rows = rows_changed.iter().filter(|&&c| c > 0).count();
        assert!(n_rows >= 3, "纵向流动应跨多行变化，实际 {} 行有变化", n_rows);
    }
}

pub struct FluidPlugin {
    state: FluidState,
}
impl Default for FluidPlugin {
    fn default() -> Self {
        Self { state: FluidState::default() }
    }
}
impl SpectrumPlugin for FluidPlugin {
    fn name(&self) -> &'static str { "流体" }
    fn render(&mut self, spectrum: &[u64], cols: u32, rows: u32, dyn_state: &AudioDyn, ctx: &SpectrumCtx) -> Vec<ratatui::text::Line<'static>> {
        render_fluid_spectrum(spectrum, cols, rows, ctx.phase, &mut self.state, dyn_state)
    }
}
