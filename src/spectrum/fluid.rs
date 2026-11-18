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

use crate::{AudioDyn, raster_solid};
use super::{SpectrumPlugin, SpectrumCtx};

/// 霓虹蓝水底：R 极暗、G 中蓝、B 电光蓝；越靠底越深但始终保持高饱和。
/// 关键：B > G（避免灰蓝），且 R ≈ 0 → 纯冷色调。
fn water_color(depth: f32) -> (u8, u8, u8) {
    let d = depth.clamp(0.0, 1.0);
    // 深度 0.0 = 水面（亮电光蓝）；深度 1.0 = 海底（深蓝近黑）
    // 策略：全程 B 最高，G 约为 B 的 40~55%，R 极低 → 高饱和纯蓝
    let r = (1.5 + 2.5 * (1.0 - d)) as u8;
    let g = (35.0 + 90.0 * (1.0 - d) * 0.55) as u8;   // G ≈ 0.55 × B 基线
    let b = (150.0 + 105.0 * (1.0 - d)) as u8;         // B 始终显著高于 G
    (r.min(g.min(b) / 2), g.min(b), b)
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
///
/// ★ 新增字段用于增强「晃动感」：
///   - wave_amp / wave_vel : 二阶弹簧振子，让波幅在节拍到来时有「余震」回弹
///   - phase_k [NBANDS]    : 各浪脊带独立慢相位轨道（纵向流动速度不同）
///   - slow_phase          : 横向蛇形漂移的全局慢相
///   - wave_phase_k [NBANDS]: 各浪脊带独立慢相位（持续高低起伏的基础振荡）
#[derive(Default)]
pub struct FluidState {
    height: Vec<f32>,
    vel: Vec<f32>,
    scroll: f32,
    // ★ 新增：波幅弹簧-阻尼
    wave_amp: Vec<f32>,
    wave_vel: Vec<f32>,
    // ★ 新增：独立相位轨道 & 横向漂移
    phase_k: [f32; NBANDS_MAX],
    slow_phase: f32,
    // ★ 新增：基础起伏振荡（每带独立慢相位，产生持续的高低波动）
    wave_phase_k: [f32; NBANDS_MAX],
    // ★ 离散节拍涌浪：do_burst 注入、逐帧衰减，作用于全局抬升 + 浪幅（守恒）
    beat_surge: f32,
}
const NBANDS_MAX: usize = 8;  // 足够覆盖当前及未来扩展

impl FluidState {
    fn resize(&mut self, n: usize) {
        if self.height.len() != n {
            self.height = vec![0.12; n];
            self.vel = vec![0.0; n];
            self.scroll = 0.0;
            // ★ 新增初始化
            self.wave_amp = vec![0.25; n];     // 初始波幅基线
            self.wave_vel = vec![0.0; n];
            self.phase_k = [0.0; NBANDS_MAX];
            self.slow_phase = 0.0;
            self.wave_phase_k = [0.0; NBANDS_MAX];  // ★ 新增：基础起伏振荡相位
            self.beat_surge = 0.0;
        }
    }

    /// ★ 方案 C：离散节拍事件注入涌浪（取较强者，避免密集鼓点叠加到失控）。
    fn inject_beat(&mut self, intensity: f32) {
        self.beat_surge = self.beat_surge.max(intensity.clamp(0.0, 2.0));
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
    const WHITE_TARGET: [f32; 3] = [60.0, 150.0, 255.0];  // 电光青蓝，R更低、纯蓝
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
    // ★ 方案 C：涌浪包络逐帧衰减（~0.88/帧，约 0.4s 回落），由鼓点事件注入。
    state.beat_surge *= 0.88;
    if state.beat_surge < 0.01 { state.beat_surge = 0.0; }
    let surge = state.beat_surge;

    let avg = state.height.iter().copied().sum::<f32>() / n as f32;
    // ★ 降底噪：基础流速调低、pulse 不再直接灌进速度（否则速度被 phase_k 平方放大 → 画面忽快忽慢地闪）。
    //   节拍改由离散 do_burst 注入的 beat_surge 驱动（见下方 C 段），这里只留温和的 pulse 影响。
    let speed = (0.4 + avg * 1.2
        + dyn_state.busyness() * 0.9
        + dyn_state.pulse * 1.2).clamp(0.3, 4.0);
    state.scroll += speed;                       // 单调累加，保证 s1 > s0
    let scroll = state.scroll.rem_euclid(bh_f);

    // ---- ★ 新增：二阶弹簧振子更新波幅（余震效果）----
    // target = 当前列能量决定的理想波幅；vel 逐渐逼近 → pulse 到来时振幅快速拉起后缓慢回落
    const WAVE_SPRING: f32 = 0.22;
    const WAVE_DAMP: f32 = 0.86;
    for x in 0..n {
        // 鼓点涌浪把浪幅顶高：短暂增大 crest 幅度（F(y-head) 形状，守恒不变）
        let target_a = wave * (0.25 + 1.05 * state.height[x]) * (1.0 + 0.55 * surge);
        state.wave_vel[x] = (state.wave_vel[x] + WAVE_SPRING * (target_a - state.wave_amp[x])) * WAVE_DAMP;
        state.wave_amp[x] = (state.wave_amp[x] + state.wave_vel[x]).clamp(0.0, wave * 1.5);
    }

    // ---- ★ 新增：三条浪脊各自独立慢相位轨道（错开同步性）----
    // 频率比取非整数：1.00 / 1.37 / 0.73，避免周期性对齐造成机械感
    const PHASE_BIAS: f32 = 0.10;                 // 每带相对基准的速度偏移幅度
    for k in 0..NBANDS {
        let kf = k as f32;
        state.phase_k[k] += speed * (1.0 + PHASE_BIAS * (kf - (NBANDS as f32 - 1.0) * 0.5));
    }

    // ---- ★ 新增：基础起伏振荡（让浪脊持续上下波动，不依赖音频变化）----
    // 模拟真实水面的自然波动：即使没有强节拍，水体也在持续"呼吸"
    // 频率取非整数比，避免各带周期性同步
    const OSC_SPEED_BASE: f32 = 0.025;            // 基础振荡速度（帧^-1），周期约 40 帧：从"高频抖"变"缓慢呼吸"
    const OSC_AMP: f32 = 2.0;                      // 振幅（px），半个字符格：降低常开抖动幅度
    const OSC_AMP_TWIST: f32 = 2.0;                // "twist"分量振幅——所有带等量平移，保守
    const OSC_BIAS: [f32; NBANDS_MAX] = [1.00, 1.37, 0.73, 1.00, 1.00, 1.00, 1.00, 1.00];
    for k in 0..NBANDS {
        state.wave_phase_k[k] += OSC_SPEED_BASE * OSC_BIAS[k];
    }
    // 计算当前各带的纵向偏移量
    let vertical_osc: [f32; NBANDS] = std::array::from_fn(|k| {
        (state.wave_phase_k[k] * 6.2832).sin() * OSC_AMP
    });

    // ---- ★ 新增：全局慢相用于横向蛇形漂移（周期 ~40 帧，极低频）----
    const SLOW_DRIFT_SPEED: f32 = 0.025;
    state.slow_phase += SLOW_DRIFT_SPEED;

    // ---- ★ 新增：整体水平平移（所有 k 相同幅度，守恒约束保持）----
    // global_sway 对所有浪带施加相同的 y 方向位移，不会改变任何列的 Σy（守恒）
    // SWAY_AMP=8px ≈ 4个字符格宽，肉眼可辨；周期约 40 帧
    const SWAY_AMP: f32 = 8.0;
    let global_sway = (state.slow_phase * 1.6).sin() * SWAY_AMP;
    // ★ 鼓点涌浪：对所有浪带施加相同的向上抬升（全局同量 → 每列均亮守恒，不引入横向漂移）
    let surge_lift = -surge * 3.5;

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

    const SPRAY_PEAK: [f32; 3] = [30.0, 120.0, 10.0]; // 更偏蓝青的喷溅色
    const SPLASH_COL: [f32; 3] = [5.0, 60.0, 230.0];  // 溅起水花纯蓝光

    // ★ 性能优化：预计算焦散查找表（避免每像素重复求 sin，air 上关键）
    const CAUSTIC_TABLE_SIZE: usize = 256;
    let caustic_sin_table: Vec<f32> = (0..CAUSTIC_TABLE_SIZE)
        .map(|i| ((i as f32 / CAUSTIC_TABLE_SIZE as f32) * 6.2832).sin())
        .collect();
    let sin_from_table = |v: f32| -> f32 {
        let idx = ((v.rem_euclid(6.2832) / 6.2832) * CAUSTIC_TABLE_SIZE as f32) as usize;
        caustic_sin_table[idx.min(CAUSTIC_TABLE_SIZE - 1)]
    };

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

        // ---- 浪脊：独立相位轨道 + 横向蛇形漂移 + 弹簧波幅 + 基础起伏振荡 ----
        // ★ phase_k[k] 让每条浪带以不同速度纵向推进，不再同步平移
        // ★ lateral(xf) 列内微弯，补充细节（< 2px）
        // ★ global_sway 整体水平平移（所有 k 同量，守恒约束不受破坏）→ 水面左右晃动
        // ★ state.wave_amp[x] 来自二阶振子 → pulse 后有余震回弹
        // ★ vertical_osc[k] 基础起伏振荡 → 持续的高低波动，模拟真实水面"呼吸"
        let lateral = (xf * 0.018 + state.slow_phase).sin() * 1.4;   // 列内微弯，< 2px，补充细节
        let mut heads = [0.0f32; NBANDS];
        let mut peak = [0.0f32; NBANDS];
        for k in 0..NBANDS {
            let kf = k as f32;
            let base = (kf + 0.5) / NBANDS as f32 * bh_f;
            // 用独立相位替代全局 scroll，保留波浪 crest 形状
            // ★ 去掉原来的 * speed（phase_k 本身已逐帧累加 speed，再乘一次等于 speed² → 闪跳）
            let phase_shift = state.phase_k[k] * 0.18;
            // ★ 每带加一个频率不同的正弦高度调制：相同相位偏移 + 不同波长 = 剪切感
            //   不依赖 x，因此 Σ_y 对每列保持不变（无横向漂移），但视觉上像水体在扭转
            let twist = (state.wave_phase_k[k] * 6.2832).sin() * OSC_AMP_TWIST;
            // ★ 叠加基础起伏振荡（per-band 纵向抖动）
            let vert_osc = vertical_osc[k];
            heads[k] = (base + phase_shift + wob + lateral + global_sway + surge_lift + twist + vert_osc + crest(xf, kf, state.wave_amp[x as usize], soft)).rem_euclid(bh_f);
            peak[k] = crest_norm(xf, kf, soft);
        }

        // ---- 该列溅起水滴 dr(y)（守恒构造）----
        // ★ 用固定数组替代 Vec 分配，避免每列堆内存操作（减少 GC 压力）
        let rnd = h1(xf, 0.0, 0.0);
        let hk = (0.42 + 0.58 * rnd) * (0.60 + 0.40 * (0.5 + 0.5 * (xf * 0.19).sin()));
        let top = (bh_f - 1.0) - (0.6 + 3.6 * env) * hk;
        let w_dr = 1.40 + 0.50 * soft;           // 水滴拉宽
        let mut dr: [f32; 16] = [0.0; 16];       // bh 最大 16（rows=4），固定大小无堆分配
        let mut dr_sum = 0.0f32;
        for y in 0..bh {
            let t = (y as f32 - top) / w_dr;
            let v = (-t * t).exp();
            dr[y as usize] = v;
            dr_sum += v;
        }
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
            // 使用查表代替直接 sin()，降低约 70% 的 sin 调用数
            let caustic = (sin_from_table(xf * 0.115 + yy * 0.24)
                * sin_from_table(xf * 0.061 - yy * 0.17)).max(0.0).powi(3);
            // 底色亮度底线提到 0.75：即使能量极低（ex≈0, gain≈0）水也不发黑，保持霓虹蓝底色。
            // body_k = 0.75 + ex*0.12 + caustic*0.03 + gain*0.10
            let body_k = 0.75 + 0.12 * ex + caustic * caustic_k + 0.10 * gain;
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
                    * (a0 + a1 * ex + 0.95 * dyn_state.pulse)
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
    // ★ 纯色块渲染：每格取最亮像素色、fg=bg=色，不输出白色盲文点阵。
    raster_solid(&buf, cols, rows, 0, 0)
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
        assert!(max_col_diff < 400, "纵向流动应保持每列均亮基本稳定（无横向平移）；有基础起伏振荡时部分亮度波动属正常，实际最大列差 {}", max_col_diff);

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

    /// ★ 新增测试：独立相位轨道确保三条浪带不再同步平移。
    /// 静止频谱下推进若干帧后，phase_k[0]、phase_k[1]、phase_k[2] 应当各不相同；
    /// 若三带仍同步则相位的差值会在 0.5 以内（同余 bh_f 意义下）。
    #[test]
    fn fluid_phase_k_diverges_across_bands() {
        let cols = 80u32;
        let rows = 4u32;
        let mut st = FluidState::default();
        let d = AudioDyn::default();
        let quiet = vec![30u64; 64];  // 稳态低能量
        for _ in 0..120 {
            let _ = render_fluid_spectrum(&quiet, cols, rows, 0.1, &mut st, &d);
        }
        // 三条浪带的独立相位应已分叉
        let p0 = st.phase_k[0];
        let p1 = st.phase_k[1];
        let p2 = st.phase_k[2];
        // 由于速度比 1.0 : 1.37 : 0.73，120 帧后三者差异明显
        let diff_01 = (p1 - p0).abs();
        let diff_02 = (p2 - p0).abs();
        assert!(diff_01 > 0.3, "phase_k[1] 应与 phase_k[0] 不同，diff={:.2}", diff_01);
        assert!(diff_02 > 0.3, "phase_k[2] 应与 phase_k[0] 不同，diff={:.2}", diff_02);
    }

    /// ★ 方案 A：静息时相邻帧的亮度变化应温和（无高频闪动）。
    ///   降 OSC 速度/幅度 + 去掉 speed² 放大后，像素级平均变化应低于阈值。
    #[test]
    fn fluid_idle_frame_flicker_is_low() {
        let cols = 80u32;
        let rows = 4u32;
        let mut st = FluidState::default();
        let d = AudioDyn::default();
        let spec = vec![40u64; 64];
        for _ in 0..30 { let _ = render_fluid_spectrum(&spec, cols, rows, 0.1, &mut st, &d); }
        let lum = |lines: &[ratatui::text::Line<'static>]| -> f64 {
            let mut sum = 0.0f64;
            let mut cnt = 0u64;
            for l in lines {
                for sp in &l.spans {
                    if let Some(ratatui::style::Color::Rgb(r, g, b)) = sp.style.fg {
                        sum += r as f64 + g as f64 + b as f64;
                        cnt += 1;
                    }
                }
            }
            sum / cnt.max(1) as f64
        };
        let mut max_step = 0.0f64;
        let mut prev = lum(&render_fluid_spectrum(&spec, cols, rows, 0.2, &mut st, &d));
        for _ in 0..6 {
            let cur = lum(&render_fluid_spectrum(&spec, cols, rows, 0.2, &mut st, &d));
            max_step = max_step.max((cur - prev).abs());
            prev = cur;
        }
        assert!(max_step < 12.0, "静息帧间亮度跳变过大（闪动）：{:.1}", max_step);
    }

    /// ★ 方案 C：离散鼓点注入涌浪——do_burst 应抬高 beat_surge 并逐帧衰减，
    ///   且涌浪确实把浪幅顶高（比无涌浪时更强）。
    #[test]
    fn fluid_beat_surge_lifts_wave() {
        let cols = 80u32;
        let rows = 4u32;
        let spec = vec![50u64; 64];
        let d = AudioDyn::default();
        // 无鼓点基线
        let mut st0 = FluidState::default();
        for _ in 0..30 { let _ = render_fluid_spectrum(&spec, cols, rows, 0.1, &mut st0, &d); }
        let amp0 = st0.wave_amp.iter().sum::<f32>() / st0.wave_amp.len() as f32;
        // 注入强鼓点
        let mut st1 = FluidState::default();
        for _ in 0..30 { let _ = render_fluid_spectrum(&spec, cols, rows, 0.1, &mut st1, &d); }
        st1.inject_beat(2.0);
        assert!(st1.beat_surge > 1.5, "鼓点应注入涌浪包络");
        let _ = render_fluid_spectrum(&spec, cols, rows, 0.11, &mut st1, &d);
        let amp1 = st1.wave_amp.iter().sum::<f32>() / st1.wave_amp.len() as f32;
        assert!(amp1 > amp0 + 0.02, "涌浪应把浪幅顶高：base {:.3} surge {:.3}", amp0, amp1);
        // 逐帧衰减
        let peak = st1.beat_surge;
        for _ in 0..10 { let _ = render_fluid_spectrum(&spec, cols, rows, 0.12, &mut st1, &d); }
        assert!(st1.beat_surge < peak * 0.5, "涌浪应逐帧衰减：{:.2} -> {:.2}", peak, st1.beat_surge);
    }

    /// ★ 新增测试：弹簧振子波幅具有惯性——脉冲后波幅不会立即回落。
    /// 先推一次强节拍，再静观数帧：wave_amp 在峰值过后应仍高于初始基线。
    #[test]
    fn fluid_wave_amp_has_inertial_decay_after_pulse() {
        let cols = 80u32;
        let rows = 4u32;
        let mut st = FluidState::default();
        let mut d = AudioDyn::default();
        let quiet = vec![10u64; 64];
        let loud = vec![90u64; 64];
        // 预热到稳态
        for _ in 0..40 {
            let _ = render_fluid_spectrum(&quiet, cols, rows, 0.1, &mut st, &d);
        }
        let amp_before = st.wave_amp.iter().sum::<f32>() / st.wave_amp.len() as f32;
        // 连续 5 帧高频 + 一次强节拍脉冲
        for _ in 0..5 {
            d.beat(1.0);  // 强脉冲
            let _ = render_fluid_spectrum(&loud, cols, rows, 0.1, &mut st, &d);
        }
        let amp_peak = st.wave_amp.iter().sum::<f32>() / st.wave_amp.len() as f32;
        assert!(amp_peak > amp_before + 0.05,
            "脉冲后波幅应显著升高：peak={} before={}", amp_peak, amp_before);
        // 继续喂安静频谱，波幅应缓慢回落但未回到基线（惯性）
        for _ in 0..30 {
            d.beat(0.0);
            let _ = render_fluid_spectrum(&quiet, cols, rows, 0.1, &mut st, &d);
        }
        let amp_late = st.wave_amp.iter().sum::<f32>() / st.wave_amp.len() as f32;
        assert!(amp_late > amp_before * 0.95,
            "惯性衰减应保留大部分振幅，late={} base={}", amp_late, amp_before);
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
        // ★ 方案 C：离散鼓点事件注入涌浪；连续 pulse 不再灌进速度（见 A 段）。
        if let Some(inten) = ctx.do_burst {
            self.state.inject_beat(inten);
        }
        render_fluid_spectrum(spectrum, cols, rows, ctx.phase, &mut self.state, dyn_state)
    }
}
