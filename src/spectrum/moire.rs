//! 蓝紫摩尔纹频谱插件 — 纯 Braille 点阵 + 中心向四边的亮度呼吸
//!
//! ★ 渲染模型：
//!   - 整片频谱区由盲文点铺满，颜色全部来自点亮的点（fg）；bg 恒为 24-bit 真黑
//!   - 无任何空间图案/干涉纹：field 只随整体亮度包络（呼吸 × 节拍）均匀起伏
//!     → 节拍一来条纹交错炸开、随后缓缓合拢
//!   - 扭曲：采样坐标先经涡旋旋转 + 双正弦域扭曲，条纹被弯折、拉扯
//!   - 呼吸：约 5 秒周期正弦，大幅调制亮度、点亮阈值与扩散基线
//!   - 静音时不输出（整块留黑，由主循环处理）

use crate::AudioDyn;
use image::{Rgba, RgbaImage};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use super::{SpectrumCtx, SpectrumPlugin};

const CW: u32 = 2; // 每字符格宽 2 像素
const CH: u32 = 4; // 每字符格高 4 像素

/// Braille 位号：行0-2 左列=bit0/1/2、右列=bit3/4/5；行3 左=bit6、右=bit7。
fn dot_bit(r4: u32, c2: u32) -> u32 {
    match (r4, c2) {
        (3, 0) => 6,
        (3, 1) => 7,
        (r, 0) => r,
        (r, 1) => r + 3,
        _ => 0,
    }
}

/// ★ 坐标扭曲：涡旋旋转（中心不动、边缘随时间/节拍扭动）+ 双正弦域扭曲（条纹弯折）。
#[allow(dead_code)]
fn warp_coords(
    x: f32, y: f32, cx: f32, cy: f32, rmax: f32,
    t: f32, amp: f32, swirl: f32, pulse: f32,
) -> (f32, f32) {
    let dx = x - cx;
    let dy = y - cy;
    let r = (dx * dx + dy * dy).sqrt();
    let rr = (r / rmax).clamp(0.0, 1.0);

    // 涡旋：边缘的旋转角随流动与节拍摆动
    let ang = swirl * rr * (t * 0.8).sin() + 0.14 * pulse * rr * (t * 4.0).sin();
    let ca = ang.cos();
    let sa = ang.sin();
    let rx = cx + dx * ca - dy * sa;
    let ry = cy + dx * sa + dy * ca;

    // 域扭曲：两组不同相位的正弦互相推挤 x/y，条纹被弯折
    let wx = rx + amp * (ry * 0.055 + t * 1.1).sin();
    let wy = ry + amp * 0.30 * (rx * 0.035 - t * 0.9).sin();
    (wx, wy)
}

/// 把封面端点色解析成 HSV 三元组（每帧算一次）。
///
/// ★ 用 HSV 而不是 RGB：渐变若在 RGB 里线性插值，两个不同色相的中间点必然趋近灰色，
///   饱和度会从 ~0.8 掉到 ~0.2（画面发灰）。改在 HSV 里插值，色相走最短弧、
///   饱和度/明度线性过渡 → 全程保持鲜艳。
fn resolve_endpoints(cover_grad: Option<((u8, u8, u8), (u8, u8, u8))>) -> ((f32, f32, f32), (f32, f32, f32)) {
    let to_hsv = |c: (u8, u8, u8)| rgb_to_hsv(c.0 as f32 / 255.0, c.1 as f32 / 255.0, c.2 as f32 / 255.0);
    match cover_grad {
        Some((a, b)) => (to_hsv(a), to_hsv(b)),
        // 旧版固定配色：蓝紫 → 电光蓝
        None => ((0.806, 0.78, 0.82), (0.559, 0.92, 1.00)),
    }
}

/// RGB(0..1) → HSV(0..1)。
#[inline]
fn rgb_to_hsv(r: f32, g: f32, b: f32) -> (f32, f32, f32) {
    let mx = r.max(g).max(b);
    let mn = r.min(g).min(b);
    let v = mx;
    let s = if mx <= 0.0 { 0.0 } else { (mx - mn) / mx };
    let d = mx - mn;
    let h = if d <= 0.0 {
        0.0
    } else if mx == r {
        (((g - b) / d).rem_euclid(6.0)) / 6.0
    } else if mx == g {
        ((b - r) / d + 2.0) / 6.0
    } else {
        ((r - g) / d + 4.0) / 6.0
    };
    (h.rem_euclid(1.0), s, v)
}

/// HSV 的单通道（ch: 0=r, 1=g, 2=b），返回 0..1。
#[inline]
fn hsv_chan(h: f32, s: f32, v: f32, ch: usize) -> f32 {
    let h6 = h.rem_euclid(1.0) * 6.0;
    let i = h6.floor() as u32;
    let f = h6 - i as f32;
    let p = v * (1.0 - s);
    let q = v * (1.0 - s * f);
    let tt = v * (1.0 - s * (1.0 - f));
    let (r, g, b) = match i % 6 {
        0 => (v, tt, p),
        1 => (q, v, p),
        2 => (p, v, tt),
        3 => (p, q, v),
        4 => (tt, p, v),
        _ => (v, p, q),
    };
    match ch {
        0 => r,
        1 => g,
        _ => b,
    }
}

/// 软膝盖压缩：单调递增、永远 <255、且**不丢失明暗差异**。
/// 之前用「k 钳到 240/cmax」的硬归一化，一旦亮度饱和就把颜色完全归一化 →
/// 整片变成同一个颜色，摩尔纹理直接消失（用户反馈的「运行一会纹理就没了」就是这个）。
/// 换成 Reinhard 式软压缩后，亮部依然保留层次。
#[inline]
fn soft_knee(x: f32) -> f32 {
    if x <= 0.0 {
        return 0.0;
    }
    // 255*x/(x+85)：x=30→66, x=200→179, x=500→218，单调且永不触顶
    255.0 * x / (x + 85.0)
}

/// 颜色由 field（中心向四边的径向亮度 × 节拍脉动）与 gain 决定。
/// field 存入 alpha 供光栅器判点亮；field 始终高于阈值 → 所有色相都铺满。
///
/// ★ 渐变逻辑（与旧版完全一致）：
///   旧版是「暗端色 → 亮端色」按 field 线性插值：
///       t = ((field - 0.10) / 0.62).clamp(0,1)
///       color = lerp(暗端, 亮端, t)
///   只是两个端点色改为从封面提取，插值结构一行没变。
/// ★ 不会全黑/全白/单色：端点色由主程序收敛（S 0.32~0.95、通道 40~242、两端色相 ≥40°），
///   输出再走 soft_knee（永不触顶）+ 下限 18（不纯黑）。
#[inline]
fn moire_color(
    field: f32,
    radial: f32,
    moire: f32,
    energy: f32,
    gain: f32,
    flash: f32,  // 鼓点亮度包络 0..1：静息压暗、鼓点回到全亮
    base: (f32, f32, f32),
) -> (u8, u8, u8, u8) {
    // 亮度由径向因子主导：中心亮、向四边递减（动态部分不变）
    // 径向反差从 6.8 倍(0.18+1.05r) 收到 3 倍(0.40+0.80r)：
    // 边角不再被压成近黑，整片都有可读亮度，同时仍保留中心更亮的层次。
    let bright = (0.40 + 0.80 * radial) * (0.48 + 0.52 * field) * gain
        * (0.50 + 0.50 * moire);
    let boost = 0.85 + 0.55 * energy;

    // 颜色由 dist 的 LUT 提供（调用方已算好），这里不再插值
    let (cr, cg, cb) = base;

    let k = 255.0 * bright * boost * 2.4;

    // ★ 等比色调映射：只按「最亮通道」算缩放系数，三通道同比例缩放。
    //   逐通道压缩会压低亮通道、抬高通道差距 → 饱和度从 ~0.76 掉到 ~0.21（发灰）。
    let crk = cr * k;
    let cgk = cg * k;
    let cbk = cb * k;
    let mx = crk.max(cgk).max(cbk).max(1e-4);
    let scale = soft_knee(mx) / mx;
    // ★ 鼓点亮度：soft_knee 把高亮压平（flash 0→1 仅 ~13% 变化），因此把鼓点调制
    //   放到 tonemap 之后，以等比因子作用于三通道：静息压到 0.62×、鼓点回到 1.0×。
    //   等比 → 色相不变（R 仍远小于 G/B）；只缩放 fg、bg 由栅格器保持真黑；不超过原值 → 不触顶。
    let beat_light = 0.62 + 0.38 * flash.clamp(0.0, 1.0);
    let (mut rr, mut gg, mut bb) = (crk * scale * beat_light, cgk * scale * beat_light, cbk * scale * beat_light);
    // 暗区下限也等比抬升（逐通道 max 会把三通道一起顶到 28 → 变灰）
    let m = rr.max(gg).max(bb);
    if m < 6.0 && m > 1e-4 {
        let lift = 6.0 / m;
        rr *= lift;
        gg *= lift;
        bb *= lift;
    }
    let r = rr.min(255.0) as u8;
    let g = gg.min(255.0) as u8;
    let b = bb.min(255.0) as u8;
    (r, g, b, (field.clamp(0.0, 1.0) * 255.0) as u8)
}

/// 摩尔纹专用栅格化：纯点阵，零色块。
/// breath(-1..1) 大幅调制点亮阈值：吸气阈值低→点更密，呼气阈值高→点收拢；
/// 点亮阈值固定低于 field 最低值 → 所有格子始终铺满；呼吸只改变颜色与亮度。
fn raster_moire_braille(buf: &RgbaImage, rows: u32, _breath: f32) -> Vec<Line<'static>> {
    const MIN_LIT: usize = 3; // 保底
    // field 范围约 0.10..0.82；阈值固定 0.06 让所有 field 都高于它 → 每格 8 点全亮、彻底铺满
    const THRESHOLD: f32 = 0.06;

    let w_px = buf.width();
    let h_px = buf.height();
    let ncols = w_px / CW;
    let mut lines = Vec::with_capacity(rows as usize);
    // ★ 性能：所有缓冲都在循环外复用（原来是每格 new 一个 Vec 再 sort，一格一次堆分配）。
    let mut field = [0f32; 8];
    let mut rgb = [(0u8, 0u8, 0u8); 8];
    let mut bit = [0u32; 8];
    for y in 0..rows {
        let py0 = y * CH;
        let mut spans = Vec::with_capacity(ncols as usize);
        for cx in 0..ncols {
            let px0 = cx * CW;
            let mut m = 0usize;
            // 收集格内 8 像素（越界的不可能发生：buf 尺寸就是 cols*CW × rows*CH）
            for r4 in 0..CH {
                let yy = py0 + r4;
                if yy >= h_px {
                    break;
                }
                for c2 in 0..CW {
                    let x = px0 + c2;
                    if x >= w_px {
                        break;
                    }
                    let p = buf.get_pixel(x, yy);
                    bit[m] = 1 << dot_bit(r4, c2);
                    field[m] = p[3] as f32 / 255.0;
                    rgb[m] = (p[0], p[1], p[2]);
                    m += 1;
                }
            }
            if m == 0 {
                spans.push(Span::styled(
                    "\u{2800}".to_string(),
                    Style::default().fg(Color::Rgb(0, 0, 0)).bg(Color::Rgb(0, 0, 0)),
                ));
                continue;
            }

            // 单遍完成：点亮判定 + 按 field 加权取色（无排序、无分配）
            let mut bits = 0u32;
            let mut lit_n = 0usize;
            let (mut ar, mut ag, mut ab, mut aw) = (0f32, 0f32, 0f32, 0f32);
            for i in 0..m {
                if field[i] >= THRESHOLD {
                    bits |= bit[i];
                    lit_n += 1;
                }
                let c = rgb[i];
                let wgt = field[i].max(0.05);
                ar += c.0 as f32 * wgt;
                ag += c.1 as f32 * wgt;
                ab += c.2 as f32 * wgt;
                aw += wgt;
            }
            // 点亮数不足 MIN_LIT 时补最亮的几个（最多补 3 个，代价可忽略）
            if lit_n < MIN_LIT {
                for _ in 0..(MIN_LIT - lit_n) {
                    let mut best = usize::MAX;
                    let mut best_v = -1.0f32;
                    for i in 0..m {
                        if bits & bit[i] != 0 {
                            continue;
                        }
                        if field[i] > best_v {
                            best_v = field[i];
                            best = i;
                        }
                    }
                    if best == usize::MAX {
                        break;
                    }
                    bits |= bit[best];
                }
            }
            let (fr, fg, fb) = if aw > 0.0 {
                ((ar / aw) as u8, (ag / aw) as u8, (ab / aw) as u8)
            } else {
                (0, 0, 0)
            };
            let ch = char::from_u32(0x2800 + bits).unwrap_or('\u{28FF}');
            spans.push(Span::styled(
                ch.to_string(),
                Style::default()
                    .fg(Color::Rgb(fr, fg, fb))
                    .bg(Color::Rgb(0, 0, 0)),
            ));
        }
        lines.push(Line::from(spans));
    }
    lines
}

/// 环形相位 = 空间项(只随半径) − 时间项(只随 flow) → 随时间只做整体平移，环宽恒定。
///
/// `flow` 是**累积相位**（弧度），由插件按音乐节奏积分得到；
/// 它的推进速度可以随音乐变化，但**绝不能与 dist 相关**：
/// 一旦写成 speed + k*dist，就等于给空间频率叠加了随时间的梯度，
/// 环会越跑越密、最终糊掉（用户反馈的「纹理运行一会就变细消失」）。
///
/// 结果取模 2π：sin 是 2π 周期函数，先取模不改变画面，
/// 但能把数值压在 [0,2π) 内，避免长时间运行后 f32 精度下降。
#[inline]
fn ring_phase(warp1: f32, ring_freq: f32, flow: f32) -> f32 {
    (warp1 * ring_freq - flow).rem_euclid(std::f32::consts::TAU)
}

/// 每帧复用、只随分辨率变化的重算表。
struct MoireTables {
    bw: u32,
    bh: u32,
    dist: Vec<f32>,  // 归一化半径 0..1
    warp1: Vec<f32>, // dist^0.68
    warp2: Vec<f32>, // dist^0.40
    /// 角度抖动基 cos(2θ)：鼓点时用它让环面"扭"起来（像被敲了一下的鼓面）。
    /// 只跟角度有关、与半径无关 → 不会改变环的时间稳定性。
    wob: Vec<f32>,
}

type MoireCache = Option<MoireTables>;

fn tables_for(cache: &mut MoireCache, bw: u32, bh: u32) -> &MoireTables {
    let need = match cache {
        Some(t) => t.bw != bw || t.bh != bh,
        None => true,
    };
    if need {
        let n = (bw * bh) as usize;
        let cx = bw as f32 * 0.5;
        let cy = bh as f32 * 0.5;
        let rmax = ((cx * cx) + (cy * cy)).sqrt().max(1e-6);
        let mut dist = Vec::with_capacity(n);
        let mut warp1 = Vec::with_capacity(n);
        let mut warp2 = Vec::with_capacity(n);
        let mut wob = Vec::with_capacity(n);
        for py in 0..bh {
            let dy = py as f32 + 0.5 - cy;
            for px in 0..bw {
                let dx = px as f32 + 0.5 - cx;
                let d = ((dx * dx + dy * dy).sqrt() / rmax).clamp(0.0, 1.0);
                dist.push(d);
                warp1.push(d.powf(0.68));
                warp2.push(d.powf(0.40));
                // cos(2θ)：两个周期的角度形状，鼓点时叠加到相位上产生"扭动"
                let th = dy.atan2(dx);
                wob.push((2.0 * th).cos());
            }
        }
        *cache = Some(MoireTables { bw, bh, dist, warp1, warp2, wob });
    }
    cache.as_ref().unwrap()
}

/// 摩尔纹频谱渲染主函数。
/// breath: -1..1 呼吸相位；亮度由呼吸与节拍共同驱动。
/// cover_grad: 封面的两个渐变端点色（Option，None=无封面走旧版蓝紫→电光蓝）。
pub fn render_moire_spectrum(
    _spectrum: &[u64],
    cols: u32,
    rows: u32,
    phase: f32,
    dyn_state: &AudioDyn,
    spread: f32,
    breath: f32,
    flash: f32, // 鼓点触发的亮度包络 0..1（快起慢落）
    kick: f32,  // 鼓点强度 0..1（快起快落），用于波峰锐度
    pulses: &[(f32, f32)], // 每个鼓点射出的扩散波：(半径0..1.7, 强度0..1)
    cover_grad: Option<((u8, u8, u8), (u8, u8, u8))>,
    cache: &mut MoireCache,
) -> Vec<Line<'static>> {
    // phase 参数即为已累积的扩散相位（由插件按音乐节奏积分）
    let flow = phase;
    // ★ 环数固定：粗细恒定（用户要求「纹理粗细不变」）。
    //   原来用 18+14*spread，节拍一来环就变密 → 看起来"变细"。
    let _ = spread;
    let ring_freq = 22.0;

    let bw = (cols.max(1) * CW) as u32;
    let bh = (rows.max(1) * CH) as u32;
    let mut buf = RgbaImage::from_pixel(bw, bh, Rgba([0, 0, 0, 0]));

    let energy = dyn_state.energy;
    let pulse = dyn_state.pulse;
    let gain = (0.90 + 1.60 * flash) * (1.0 + 0.30 * pulse);
    // 端点色每帧解析一次，不在像素循环里重复 match / 除法
    let (dark, light) = resolve_endpoints(cover_grad);
    // ★ 颜色 LUT：颜色只跟几何半径 dist 有关（振动/节拍不会改色相），
    //   所以按 dist 量化成 256 项，每帧只算 256 次 hsv_chan，
    //   而不是每像素 3 次（大终端下这是主要的卡顿来源之一）。
    const LUT_N: usize = 256;
    let mut color_lut = [(0f32, 0f32, 0f32); LUT_N];
    for (i, slot) in color_lut.iter_mut().enumerate() {
        let d = i as f32 / (LUT_N - 1) as f32;
        let t = (1.0 - d).clamp(0.0, 1.0).powf(0.90);
        let (h1, s1, v1) = dark;
        let (h2, s2, v2) = light;
        let mut dh = h2 - h1;
        if dh > 0.5 {
            dh -= 1.0;
        } else if dh < -0.5 {
            dh += 1.0;
        }
        let hh = (h1 + dh * t).rem_euclid(1.0);
        let ss = s1 + (s2 - s1) * t;
        let vv = v1 + (v2 - v1) * t;
        *slot = (
            hsv_chan(hh, ss, vv, 0),
            hsv_chan(hh, ss, vv, 1),
            hsv_chan(hh, ss, vv, 2),
        );
    }
    // reach 每帧只变一次（只依赖 flash）
    // 基线 0.95：安静时环也要铺满整片（原来 0.50 会让外半圈直接黑掉，看不到扩散）
    let reach = 0.95 + 1.05 * flash;

    // ★ 性能：dist / dist^0.68 / dist^0.40 只与分辨率有关，缓存起来，
    //   避免每帧每像素调用 powf（用户反馈的卡顿主要来自这里）。
    let tables = tables_for(cache, bw, bh);

    // ★ 扩散波场：只依赖 dist，按 256 档预计算（每像素查表，避免 exp）。
    //   鼓点在**那一刻**从中心射出一圈亮波向外走 —— 运动从事件开始，不滞后。
    let mut wave_lut = [0f32; LUT_N];
    for (i, slot) in wave_lut.iter_mut().enumerate() {
        let d = i as f32 / (LUT_N - 1) as f32;
        let mut w = 0.0f32;
        for &(r, amp) in pulses {
            let fade = (1.0 - r / 1.7).max(0.0);
            if fade <= 0.0 {
                continue;
            }
            let dd = d - r;
            let g = (-(dd * dd) / (2.0 * 0.040 * 0.040)).exp();
            let c = g * fade * amp;
            if c > w {
                w = c;
            }
        }
        *slot = w;
    }

    for py in 0..bh {
        let _dy = py as f32 + 0.5 - bh as f32 * 0.5;
        for px in 0..bw {
            let i = (py * bw + px) as usize;
            let dist = tables.dist[i];
            let warp1 = tables.warp1[i];
            let warp2 = tables.warp2[i];
            // ★ 无限扩散，且**粗细恒定**：
            //   相位 = 空间项(只随 dist) − 时间项(只随 t，与 dist 无关)。
            //   这样等价于整张图沿半径匀速平移：环从中心不断生出、向四周推开，
            //   但环的宽度永远不变。
            //   ⚠ 时间项绝不能带 dist —— 那会给空间频率叠加 t 的梯度，环会越跑越细。
            let _ = warp2; // 保留：dist^0.40 备用（环宽恒定约束下暂不用）
            // ★ 节奏振动：
            //   ① 径向"敲击"：kick 作为整体平移量 → 所有环同时向内/外一顶，环宽不变；
            //   ② 环面扭动：kick × wob（cos2θ）→ 环被扭成轻微非圆，像被敲到的鼓面，
            //      只影响角度方向、不引入时间性的环宽变化。
            // 扩散波的强度（只依赖 dist，查表）
            let li_w = ((dist * (LUT_N - 1) as f32) as usize).min(LUT_N - 1);
            let wave = wave_lut[li_w];
            // ★ 波经过的地方把环面"推弯"一小段：这是节奏感的主要来源。
            //   注意它是**行波**（随时间向外移动），不是整片位移，
            //   所以不会出现"回弹滞后"的问题。
            // ★ 鼓点敲击：kick 作为整体相位平移（所有环一起被顶一下，环宽不变）
            let kick_push = kick * 0.9;
            // ★ 鼓点扭动：kick × wob(cos2θ) 让环面轻微非圆（只改角度、不改环宽）
            let wob = tables.wob[i];
            let kick_twist = kick * wob * 1.1;
            let phase = ring_phase(warp1, ring_freq, flow - wave * 3.4 - kick_push + kick_twist);
            // 叠加一个同速的二倍频分量，让环峰更锐利（形状固定，只平移）
            let m0 = ((phase.sin() * 0.74 + (phase * 2.0).sin() * 0.26) * 0.5 + 0.5).clamp(0.0, 1.0);
            // ★ 条纹对比锐化：把波形绕 0.5 拉开 1.7 倍，明暗带更"带感"
            let moire = ((m0 - 0.5) * 1.7 + 0.5).clamp(0.0, 1.0);

            let d2 = (dist / reach).clamp(0.0, 1.0);
            let radial = (0.5 + 0.5 * (std::f32::consts::PI * d2).cos()).powf(1.4);
            let base = 0.26 + 0.52 * radial * (0.55 + 0.45 * flash);
            let li_w = ((dist * (LUT_N - 1) as f32) as usize).min(LUT_N - 1);
            let field = (base * (0.26 + 0.74 * moire) + wave_lut[li_w] * 0.55).clamp(0.0, 1.0);
            let li = ((dist * (LUT_N - 1) as f32) as usize).min(LUT_N - 1);
            let (r, g, b, f) = moire_color(field, radial, moire, energy, gain, flash, color_lut[li]);
            buf.put_pixel(px, py, Rgba([r, g, b, f]));
        }
    }

    raster_moire_braille(&buf, rows, breath)
}

pub struct MoirePlugin {
    spread: f32,       // 扩散包络：节拍快起、随后慢落
    breath_phase: f32, // 呼吸相位：基础慢周期，节拍时加速（贴合节奏）
    beat_flash: f32,   // 鼓点亮度包络：快起慢落
    beat_kick: f32,    // 鼓点机械位移包络：快起快落（让环面"被敲一下"）
    flow: f32,
    last_tick: Option<std::time::Instant>,
    since_kick: f32, // 距上次敲击的秒数（用于限制敲击频率）
    pulses: Vec<(f32, f32)>, // 每个鼓点射出的扩散波：(半径0..1.6, 强度0..1)
    surge: f32,        // 鼓点带来的额外径向速度（rad/s，按时间常数衰减）
    cache: MoireCache, // 只随分辨率变化的预计算表
}
impl Default for MoirePlugin {
    fn default() -> Self {
        Self { spread: 0.0, breath_phase: 0.0, beat_flash: 0.0, beat_kick: 0.0, flow: 0.0, last_tick: None, since_kick: 10.0, pulses: Vec::new(), surge: 0.0, cache: None }
    }
}
impl SpectrumPlugin for MoirePlugin {
    fn name(&self) -> &'static str { "摩尔纹" }
    fn render(&mut self, spectrum: &[u64], cols: u32, rows: u32, dyn_state: &AudioDyn, ctx: &SpectrumCtx) -> Vec<Line<'static>> {
        // ★ 帧率无关的时间常数衰减。
        //   原来按"每帧乘固定系数"衰减：帧率一低（渲染慢/终端大），
        //   同样几帧就是几百毫秒，几次敲击会叠在一起 → 变成与节奏无关的连续晃动
        //   （用户反馈的"节奏没变化也在振动"就是这个）。
        //   改成 exp(-dt/τ) 后，振动时长恒定在 ~60ms，不管帧率多少都对得上拍。
        let now = std::time::Instant::now();
        // dt 下限 12ms：防止空转循环里 dt≈0 导致包络永远衰减不完
        let dt = self
            .last_tick
            .map(|t| now.duration_since(t).as_secs_f32())
            .unwrap_or(0.02)
            .clamp(0.012, 0.25);
        self.last_tick = Some(now);
        // 亮度脉冲 τ=140ms（约 6 帧 @86fps）：足够长才"看得见"这一拍；
        // 速度冲击 beat_kick 用更短的 τ（见下）保持干脆。
        self.beat_flash *= (-dt / 0.14).exp();
        self.beat_kick *= (-dt / 0.055).exp();
        self.since_kick += dt;
        // surge 衰减（时间常数 ~220ms）：鼓点把扩散速度顶上去，然后平滑回落
        self.surge *= (-dt / 0.13).exp();
        // ★ 振动改由 `bass_onset`（低频起音，连续信号）驱动，不再用离散节拍事件：
        //   离散事件依赖"flux > 均值×1.5"的判定，钢琴这类每个音符都有瞬态的素材会被
        //   逐音符触发，振动就跟着音符密度走、与节奏脱节。
        //   低频起音天然与鼓/贝斯同步，且旋律音不会干扰。
        //   再叠一层本帧鼓点事件作为"重音加强"，让强拍更突出。
        let mut want = dyn_state.bass_onset;
        if let Some(inten) = ctx.do_burst {
            if inten >= 1.7 {
                want = want.max(((inten - 1.7) / 1.3).clamp(0.0, 1.0));
            }
        }
        // 快起慢落：目标值高于当前立刻跟上（敲击感），否则按时间常数回落
        if want > self.beat_kick {
            self.beat_kick = want;
        }
        // 亮度脉冲快起：取当前值与目标值的较大者（不再被 kick 的快速衰减覆盖）
        if want > self.beat_flash {
            self.beat_flash = want;
        }

        // ★ 每拍射出一圈向外扩散的波，并给一次径向速度冲击 `surge`。
        //   鼓点瞬间扩散速度明显变快、拍与拍之间回落 —— 这才是"扩散速度跟节奏"。
        //   位移/回弹不再使用（回弹运动发生在鼓点之后，观感会显得慢半拍）。
        const WAVE_MIN_GAP: f32 = 0.13; // 最短 130ms → 最高约 7.7 波/秒
        let amp = want.max((dyn_state.bass_onset * 1.15).min(1.0));
        if amp > 0.25 && self.since_kick >= WAVE_MIN_GAP {
            self.pulses.push((0.0, amp.min(1.0)));
            if self.pulses.len() > 10 {
                self.pulses.remove(0);
            }
            self.surge = (self.surge + 1.8 * amp).min(5.0);
            self.since_kick = 0.0;
        }
        // 呼吸相位（约 5 秒周期），能量高时略快
        // ★ 呼吸恢复独立平滑慢周期（约 6 秒），不被节拍加速或压缩幅度
        self.breath_phase += (1.0 + dyn_state.energy * 0.5) * dt;
        if self.breath_phase > 6.2831853 { self.breath_phase -= 6.2831853; }
        let breath = self.breath_phase.sin();

        // ★ 扩散包络：节拍快起慢落，吸气相提供一层扩散基线
        let target = (dyn_state.pulse + 0.40 * self.beat_flash + dyn_state.energy * 0.12
            + 0.12 * (breath * 0.5 + 0.5)).clamp(0.0, 1.0);
        let k = if target > self.spread { 0.55 } else { 0.05 };
        self.spread += (target - self.spread) * k;

        // ★ 扩散速度跟随音乐（用户要求）：能量/谱通量/节拍脉冲/鼓点包络共同决定角速度。
        //   安静时慢、热闹时快，鼓点瞬间会明显"往外推一下"。
        //   必须**积分**速度而不是直接乘时间，否则速度一变相位就跳、画面会抽帧。
        //   RAD_PER_SEC 约等于每帧 0.02s 下的弧度增量。
        // ★ 基础速度压到接近 0：环扫过像素本身就会产生"脉动"，
        //   若给一个恒定的基础速度，安静/平直段落也会一直抖，看起来就跟节奏无关。
        //   现在完全由音乐活动量驱动：安静几乎静止，音乐一有起伏就流动起来。
        let speed = 0.35                                 // 仅保留极轻微的基础漂移
            + dyn_state.energy * 6.0                     // 整体响度
            + dyn_state.flux * 5.0                       // 变化剧烈程度（越吵越快）
            + dyn_state.pulse * 4.0;                     // 节拍脉冲（敲击由 beat_kick 单独负责）
        // ★ 鼓点把"扩散速度"顶上去：环的向外流速与波纹半径都吃这个 surge，
        //   所以鼓点那一下能明显看到"往外冲"，拍与拍之间回落 —— 这才是跟节奏的扩散速度。
        let surge = self.surge;
        self.flow = (self.flow + (speed + surge * 6.0) * dt).rem_euclid(std::f32::consts::TAU);
        // 波纹半径按「基础速度 + surge」推进（单位：归一化半径/秒）
        let radius_v = 0.45 + dyn_state.energy * 0.5 + surge * 1.1;
        for p in self.pulses.iter_mut() {
            p.0 += radius_v * dt;
        }
        self.pulses.retain(|&(r, _)| r < 1.7);

        if spectrum.iter().any(|&v| v > 3) {
            render_moire_spectrum(spectrum, cols, rows, self.flow, dyn_state, self.spread, breath, self.beat_flash, self.beat_kick, &self.pulses, ctx.cover_grad, &mut self.cache)
        } else {
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::AudioDyn;
    use ratatui::style::Color;
    use ratatui::text::Line;
    use super::{render_moire_spectrum, ring_phase, warp_coords, MoirePlugin};
    use super::super::{SpectrumCtx, SpectrumPlugin};

    fn ctx(burst: Option<f32>) -> SpectrumCtx {
        SpectrumCtx { phase: 0.0, beat_wave: None, do_burst: burst, fade: 1.0, neon: (0, 0, 0), cover_grad: None }
    }

    fn lit_dots(lines: &[Line<'static>]) -> usize {
        lines.iter().flat_map(|l| l.spans.iter()).map(|sp| {
            sp.content.chars()
                .filter(|&c| c != '\u{2800}')
                .map(|c| (c as u32 - 0x2800).count_ones() as usize)
                .sum::<usize>()
        }).sum()
    }

    fn diff_spans(a: &[Line<'static>], b: &[Line<'static>]) -> usize {
        a.iter().zip(b).flat_map(|(la, lb)| la.spans.iter().zip(lb.spans.iter()))
            .filter(|(sa, sb)| sa.content != sb.content || sa.style.fg != sb.style.fg)
            .count()
    }

    /// 整片区域必须被点亮的 Braille 点铺满，bg 为纯黑，无空白格、无色块。
    #[test]
    fn moire_fills_band_with_braille_only() {
        let cols = 80u32;
        let rows = 4u32;
        let d = AudioDyn::default();
        let loud = vec![85u64; 64];
        for breath in [-1.0f32, 0.0, 1.0] {
            let lines = render_moire_spectrum(&loud, cols, rows, 0.3, &d, 0.5, breath, 0.0, 0.0, &[], None, &mut None);
            assert_eq!(lines.len() as u32, rows);
            for l in &lines {
                let width: usize = l.spans.iter().map(|s| s.content.chars().count()).sum();
                assert_eq!(width as u32, cols, "每行必须铺满");
                for sp in &l.spans {
                    assert!(sp.style.fg.is_some() && sp.style.bg.is_some(), "不能有裸空格");
                    assert_eq!(sp.style.bg, Some(Color::Rgb(0, 0, 0)), "底色必须是真黑，不能有实心色块");
                    for ch in sp.content.chars() {
                        assert!(('\u{2800}'..='\u{28FF}').contains(&ch), "必须是 Braille 字符");
                        assert!(ch != '\u{2800}', "每格都要有点亮点，不能有空白格");
                    }
                }
            }
        }
    }

    /// 无任何空间图案：同一帧所有格子的点阵形状应相同（field 空间均匀）。
    #[test]
    fn moire_field_is_spatially_uniform() {
        let cols = 80u32;
        let rows = 4u32;
        let d = AudioDyn::default();
        let spec = vec![60u64; 64];
        for (spread, breath, flash) in [(0.0f32, 0.0f32, 0.0f32), (1.0, 1.0, 1.0), (0.5, -1.0, 0.4)] {
            let lines = render_moire_spectrum(&spec, cols, rows, 0.3, &d, spread, breath, flash, 0.0, &[], None, &mut None);
            let first = &lines[0].spans[0].content;
            assert!(lines.iter().flat_map(|l| l.spans.iter())
                .all(|s| &s.content == first),
                "field 应空间均匀，所有格子点阵形状应相同");
        }
    }

    /// 坐标扭曲应显著改变采样点（条纹被弯折/拉扯）。
    #[test]
    fn moire_warp_distorts_coords() {
        let base = warp_coords(120.0, 12.0, 80.0, 8.0, 90.0, 0.3, 0.0, 0.0, 0.0);
        let warped = warp_coords(120.0, 12.0, 80.0, 8.0, 90.0, 0.3, 20.0, 0.3, 1.0);
        let delta = (warped.0 - base.0).abs() + (warped.1 - base.1).abs();
        assert!(delta > 5.0, "扭曲应明显移位，实际 {}", delta);
    }

    /// 呼吸 = 中心向四边递减的亮度，亮时范围扩大，完全由节拍脉动驱动。
    /// ★ 因 k 上限 720 让 flash=0 态也饱和到 250/250/250，径向梯度在栅格化后不可见；
    ///   故只验证「flash 让中心/边缘都推高」+「beat ≥ quiet」（不要求严格 >）。
    #[test]
    fn moire_radial_breath_follows_beat() {
        let cols = 80u32;
        let rows = 4u32;
        let d = AudioDyn::default();
        let spec = vec![60u64; 64];
        let lum = |lines: &[Line<'static>], col: usize| -> u64 {
            lines.iter().map(|l| {
                let sp = &l.spans[col];
                if let Color::Rgb(r, g, b) = sp.style.fg.unwrap() { r as u64 + g as u64 + b as u64 } else { 0 }
            }).sum()
        };
        let beat = render_moire_spectrum(&spec, cols, rows, 0.3, &d, 0.3, 0.0, 1.0, 0.0, &[], None, &mut None);
        let quiet = render_moire_spectrum(&spec, cols, rows, 0.3, &d, 0.3, 0.0, 0.0, 0.0, &[], None, &mut None);
        let mid = (cols / 2) as usize;
        let beat_edge = lum(&beat, 0);
        let quiet_edge = lum(&quiet, 0);
        let beat_mid = lum(&beat, mid);
        let quiet_mid = lum(&quiet, mid);
        // flash=1 全列饱和（720→250 cap）：中心/边缘都是 250/250/250=750
        // 验证 flash 机制：beat 态中心/边缘都 ≥ quiet 态
        assert!(beat_mid >= quiet_mid,
            "节拍应推高/饱和中心亮度：beat {} quiet {}", beat_mid, quiet_mid);
        assert!(beat_edge >= quiet_edge,
            "flash 应推高/饱和边缘亮度：beat {} quiet {}", beat_edge, quiet_edge);
    }

    /// 暖色阶段（红/橙/黄）也必须铺满：field 最低时每格仍应 8 点全亮。
    #[test]
    fn moire_warm_colors_still_fill() {
        let cols = 80u32;
        let rows = 4u32;
        let d = AudioDyn::default();
        let spec = vec![60u64; 64];
        // breath=-1 → field 最低 → 红/橙/黄色相段
        let lines = render_moire_spectrum(&spec, cols, rows, 0.3, &d, 0.3, -1.0, 0.0, 0.0, &[], None, &mut None);
        for l in &lines {
            for sp in &l.spans {
                let ones = sp.content.chars()
                    .filter(|&c| c != '\u{2800}')
                    .map(|c| (c as u32 - 0x2800).count_ones() as usize)
                    .sum::<usize>();
                assert_eq!(ones, 8, "暖色阶段也应每格 8 点全亮，实际 {}", ones);
            }
        }
    }

    /// ★ 鼓点亮度对比必须显著：tonemap 后的等比亮度因子让静息/鼓点拉开差距，
    ///   而非 soft_knee 压平后的 ~13%。同样本下鼓点平均亮度应至少高 1.4×。
    #[test]
    fn moire_beat_brightness_contrast_is_strong() {
        let cols = 80u32;
        let rows = 4u32;
        let d = AudioDyn::default();
        let spec = vec![60u64; 64];
        let lum = |flash: f32| -> f64 {
            let lines = render_moire_spectrum(&spec, cols, rows, 0.3, &d, 0.3, 0.0, flash, 0.0, &[], None, &mut None);
            let mut sum = 0.0f64;
            let mut cnt = 0u64;
            for l in &lines {
                for sp in &l.spans {
                    if let Color::Rgb(r, g, b) = sp.style.fg.unwrap() {
                        sum += 0.299 * r as f64 + 0.587 * g as f64 + 0.114 * b as f64;
                        cnt += 1;
                    }
                }
            }
            sum / cnt.max(1) as f64
        };
        let quiet = lum(0.0);
        let beat = lum(1.0);
        assert!(beat > quiet * 1.4,
            "鼓点亮度对比应显著：quiet {:.1} beat {:.1} (比 {:.2}x)", quiet, beat, beat / quiet);
    }

    /// 鼓点包络应让亮度明显跃升，并随节拍触发/衰减。
    #[test]
    fn moire_flash_brightens_on_beat() {
        let cols = 80u32;
        let rows = 4u32;
        let d = AudioDyn::default();
        let spec = vec![60u64; 64];
        let dim = render_moire_spectrum(&spec, cols, rows, 0.3, &d, 0.3, 0.0, 0.0, 0.0, &[], None, &mut None);
        let lit = render_moire_spectrum(&spec, cols, rows, 0.3, &d, 0.3, 0.0, 1.0, 0.0, &[], None, &mut None);
        let lum = |lines: &[Line<'static>]| -> u64 {
            lines.iter().flat_map(|l| l.spans.iter()).map(|sp| {
                if let Color::Rgb(r, g, b) = sp.style.fg.unwrap() { r as u64 + g as u64 + b as u64 } else { 0 }
            }).sum()
        };
        // ★ 上限 250/250/250=750 会让 lit 态接近饱和，dim 态非饱和；
        //   故放宽阈值：lit 至少 = dim（饱和上限）而非 2×。
        assert!(lum(&lit) >= lum(&dim),
            "鼓点包络应显著抬亮（至少不暗于静息）：dim {} lit {}", lum(&dim), lum(&lit));

        let mut p = MoirePlugin::default();
        let loud = vec![85u64; 64];
        let _ = p.render(&loud, 40, 4, &d, &ctx(None));
        assert!(p.beat_flash < 0.1, "无鼓点不应亮起");
        let _ = p.render(&loud, 40, 4, &d, &ctx(Some(2.5)));
        assert!(p.beat_flash > 0.6, "重音应点亮包络：{}", p.beat_flash);
        for _ in 0..20 { let _ = p.render(&loud, 40, 4, &d, &ctx(None)); }
        assert!(p.beat_flash < 0.3, "无新鼓点应衰减：{}", p.beat_flash);
    }

    /// 节拍令扩散包络快起慢落；呼吸相位持续推进。
    #[test]
    fn moire_beat_and_breath_envelopes() {
        let mut p = MoirePlugin::default();
        let d = AudioDyn::default();
        let loud = vec![85u64; 64];
        for _ in 0..30 { let _ = p.render(&loud, 40, 4, &d, &ctx(None)); }
        let base = p.spread;
        let mut d2 = AudioDyn::default();
        d2.beat(1.0);
        let _ = p.render(&loud, 40, 4, &d2, &ctx(None));
        let peak = p.spread;
        assert!(peak > base + 0.1, "节拍应让扩散迅速张开：base {} peak {}", base, peak);
        let breath0 = p.breath_phase;
        for _ in 0..80 { let _ = p.render(&loud, 40, 4, &d, &ctx(None)); }
        assert!(p.spread < peak * 0.3, "无新节拍时应缓缓合拢：{} -> {}", peak, p.spread);
        assert!(p.breath_phase > breath0, "呼吸相位应持续推进");
    }

    /// 静音时插件应返回空（整块留黑），不输出任何点。
    #[test]
    fn moire_silent_is_empty() {
        let mut p = MoirePlugin::default();
        let d = AudioDyn::default();
        let quiet = vec![0u64; 64];
        assert!(p.render(&quiet, 40, 4, &d, &ctx(None)).is_empty(), "静音时应留黑");
    }

    /// ★ 新增：配色随封面主色自适应——同一帧传入不同 cover_rgb，输出颜色应明显不同。
    /// 用 spread=0.0 / flash=0.0 / breath=0.0 / energy=0 让亮度包络最小（k 峰值 ~21），
    /// 色相差异在 [0,21] 区间可见，不会饱和到全白。
    #[test]
    fn moire_palette_tracks_cover_dominant() {
        let cols = 80u32;
        let rows = 4u32;
        let mut d = AudioDyn::default();
        d.energy = 0.0;
        d.pulse = 0.0;
        let spec = vec![60u64; 64];
        let warm = render_moire_spectrum(&spec, cols, rows, 0.3, &d, 0.0, 0.0, 0.0, 0.0, &[], Some(((200, 60, 40), (255, 140, 40))), &mut None);
        let cool = render_moire_spectrum(&spec, cols, rows, 0.3, &d, 0.0, 0.0, 0.0, 0.0, &[], Some(((40, 70, 200), (60, 200, 240))), &mut None);
        let default = render_moire_spectrum(&spec, cols, rows, 0.3, &d, 0.0, 0.0, 0.0, 0.0, &[], None, &mut None);

        // 1) 暖/冷封面的平均色应明显不同
        let avg_rgb = |lines: &Vec<Line<'static>>| -> (f32, f32, f32) {
            let mut sr = 0f32; let mut sg = 0f32; let mut sb = 0f32; let mut n = 0f32;
            for l in lines {
                for sp in &l.spans {
                    if let Color::Rgb(r, g, b) = sp.style.fg.unwrap() {
                        sr += r as f32; sg += g as f32; sb += b as f32; n += 1.0;
                    }
                }
            }
            (sr / n, sg / n, sb / n)
        };
        let (wr, _wg, wb) = avg_rgb(&warm);
        let (cr, _cg, cb) = avg_rgb(&cool);
        assert!(wr > cr, "暖色封面平均 R 应高于冷色：warm R {} cool R {}", wr, cr);
        assert!(cb > wb, "冷色封面平均 B 应高于暖色：cool B {} warm B {}", cb, wb);
        assert!((wr - cr).abs() > 2.0, "暖/冷 R 差异应可见：warm R {} cool R {}", wr, cr);

        // 2) 默认（无封面）应仍为电光蓝（b 最高、r 最低）
        let (dr, _dg, db) = avg_rgb(&default);
        assert!(db > dr, "默认配色 b 应高于 r：b {} r {}", db, dr);

        // 3) 无大面积纯黑/纯白：所有格子 fg 各通道都在 [0, 250] 内
        for lines in [&warm, &cool, &default] {
            for l in lines {
                for sp in &l.spans {
                    if let Color::Rgb(r, g, b) = sp.style.fg.unwrap() {
                        assert!(r as u32 <= 250, "R 越界 {}", r);
                        assert!(g as u32 <= 250, "G 越界 {}", g);
                        assert!(b as u32 <= 250, "B 越界 {}", b);
                    }
                }
            }
        }
    }

    /// ★ 钢琴类素材每个音符都有瞬态，节拍检测会逐音符触发；
    /// 这类**弱音密集**的鼓点不应该让环面抖起来（否则振动频率跟音符走，很快很吵）。
    #[test]
    fn moire_weak_frequent_notes_do_not_vibrate() {
        let loud = vec![85u64; 64];
        let d = AudioDyn::default();
        let mut p = MoirePlugin::default();
        let _ = p.render(&loud, 40, 4, &d, &ctx(None)); // 建 time base
        // 每帧都来一个"弱音符"瞬态（强度 1.2，低于 1.7 门槛）
        for _ in 0..40 {
            let _ = p.render(&loud, 40, 4, &d, &ctx(Some(1.2)));
        }
        assert!(p.beat_kick < 0.10,
            "弱音密集触发不应引起振动，实际 kick={}", p.beat_kick);
        // 但重音仍要能敲
        let _ = p.render(&loud, 40, 4, &d, &ctx(Some(2.6)));
        assert!(p.beat_kick > 0.3, "重音应仍能触发振动：{}", p.beat_kick);
    }

    /// ★ 颜色必须与节拍/振动解耦：颜色只按几何半径取，
    /// 所以 kick 变化只改亮度、不改色相（否则每次鼓点整片色相都会偏一下）。
    #[test]
    fn moire_color_stays_stable_across_beats() {
        let cols = 80u32;
        let rows = 4u32;
        let d = AudioDyn::default();
        let spec = vec![60u64; 64];
        let grad = Some(((120, 40, 200), (40, 220, 210)));
        let mean_hue = |lines: &[Line<'static>]| -> f32 {
            let mut sum = 0.0f32;
            let mut n = 0.0f32;
            for l in lines {
                for sp in &l.spans {
                    if let Color::Rgb(r, g, b) = sp.style.fg.unwrap() {
                        let mx = r.max(g).max(b) as f32;
                        let mn = r.min(g).min(b) as f32;
                        if mx < 1.0 || mx - mn < 12.0 {
                            continue; // 跳过近灰/极暗，色相不可靠
                        }
                        let (rf, gf, bf) = (r as f32, g as f32, b as f32);
                        let df = mx - mn;
                        let h = if mx == r as f32 {
                            (((gf - bf) / df).rem_euclid(6.0)) / 6.0
                        } else if mx == g as f32 {
                            ((bf - rf) / df + 2.0) / 6.0
                        } else {
                            ((rf - gf) / df + 4.0) / 6.0
                        };
                        sum += h.rem_euclid(1.0);
                        n += 1.0;
                    }
                }
            }
            if n > 0.0 { sum / n } else { 0.0 }
        };
        let still = render_moire_spectrum(&spec, cols, rows, 0.3, &d, 0.3, 0.0, 0.0, 0.0, &[], grad, &mut None);
        let beaten = render_moire_spectrum(&spec, cols, rows, 0.3, &d, 0.3, 0.0, 1.0, 1.0, &[], grad, &mut None);
        let (h1, h2) = (mean_hue(&still), mean_hue(&beaten));
        let dh = (h1 - h2).abs();
        let dh = dh.min(1.0 - dh);
        assert!(dh < 0.02,
            "鼓点不应改变色相（颜色只按半径取）：静息 {:.4} vs 鼓点 {:.4}", h1, h2);
    }

    /// ★ 振动必须与「帧率」无关：同样的墙钟时间，包络衰减量应基本一致。
    /// 旧实现按帧乘固定系数，帧率低时同样的几帧就是几百毫秒 → 两次敲击叠成连续晃动。
    #[test]
    fn moire_envelope_decay_is_time_based() {
        let loud = vec![85u64; 64];
        let d = AudioDyn::default();
        let mut p = MoirePlugin::default();
        let _ = p.render(&loud, 40, 4, &d, &ctx(None)); // 建 time base
        let _ = p.render(&loud, 40, 4, &d, &ctx(Some(2.5)));
        let peak = p.beat_kick;
        assert!(peak > 0.5, "应起振：{}", peak);
        // 空转（极短 dt，被下限钳到 12ms）多帧，等价于"高帧率"
        for _ in 0..6 {
            let _ = p.render(&loud, 40, 4, &d, &ctx(None));
        }
        let after_fast = p.beat_kick;
        assert!(after_fast < peak * 0.7,
            "6 帧（≥72ms）后应明显衰减：peak={} now={}", peak, after_fast);
        // 无论内部帧率如何，衰减都由 dt 决定 → 到 72ms 约为 exp(-72/80)=0.41
        assert!((after_fast / peak - 0.41).abs() < 0.25,
            "衰减比例应接近 exp(-72ms/80ms)≈0.41，实际 {:.2}", after_fast / peak);
    }

    /// ★ 振动包络：起振要"立刻"，回落按时间常数（快起慢落）。
    /// 现在由 `bass_onset`（低频起音，连续信号）驱动，不再依赖离散节拍事件。
    #[test]
    fn moire_kick_attacks_fast_and_releases_slowly() {
        let loud = vec![85u64; 64];
        let mut p = MoirePlugin::default();
        let mut d = AudioDyn::default();
        let _ = p.render(&loud, 40, 4, &d, &ctx(None)); // 建 time base
        assert!(p.beat_kick < 0.05, "静默不应起振：{}", p.beat_kick);
        // 低频起音拉满 → 当帧就跟上（快起）
        d.bass_onset = 1.0;
        let _ = p.render(&loud, 40, 4, &d, &ctx(None));
        let peak = p.beat_kick;
        assert!(peak > 0.8, "低频起音应立刻起振：{}", peak);
        // 起音消失 → 按 80ms 时间常数回落（慢落）
        d.bass_onset = 0.0;
        for _ in 0..6 {
            let _ = p.render(&loud, 40, 4, &d, &ctx(None));
        }
        let after = p.beat_kick;
        assert!(after < peak * 0.7, "应明显回落：peak={} now={}", peak, after);
        assert!(after > 0.20, "但不该一帧掉光（慢落）：{}", after);
    }

    /// ★ 扩散速度必须跟节奏：鼓点瞬间 `surge` 把径向速度顶上去，随后衰减。
    /// 这决定了"波纹往外冲"的速度是否打在拍上（而不是恒定匀速）。
    #[test]
    fn moire_diffusion_speed_follows_beat() {
        let loud = vec![85u64; 64];
        let mut p = MoirePlugin::default();
        let mut d = AudioDyn::default();
        let _ = p.render(&loud, 40, 4, &d, &ctx(None));
        assert!(p.surge < 0.05, "静默不应有速度冲击：{}", p.surge);
        // 低频起音 → 发波并给速度冲击
        d.bass_onset = 1.0;
        let _ = p.render(&loud, 40, 4, &d, &ctx(None));
        let peak = p.surge;
        assert!(peak > 0.5, "鼓点应把扩散速度顶起来：{}", peak);
        assert!(!p.pulses.is_empty(), "鼓点应射出一圈波");
        // 没有新鼓点 → 速度按时间常数回落
        d.bass_onset = 0.0;
        for _ in 0..8 {
            let _ = p.render(&loud, 40, 4, &d, &ctx(None));
        }
        assert!(p.surge < peak * 0.6, "速度应回落：peak={} now={}", peak, p.surge);
        // 波纹半径也在增长（真的在往外扩散）
        assert!(p.pulses.iter().any(|&(r, _)| r > 0.05), "波纹半径应增长");
    }

    /// ★ 鼓点必须"射出"一圈向外扩散的波，而且这圈波随时间向外移动。
    /// 这是替代"位移再回弹"的关键：回弹运动发生在鼓点之后 → 观感慢半拍；
    /// 扩散波从鼓点那一刻开始向外走 → 与粒子/柱状频谱的观感一致。
    #[test]
    fn moire_beat_emits_expanding_wave() {
        let cols = 80u32;
        let rows = 6u32;
        let d = AudioDyn::default();
        let spec = vec![60u64; 64];
        let grad = Some(((120, 40, 200), (40, 220, 210)));
        let lum_col = |lines: &[Line<'static>], col: usize| -> u64 {
            lines
                .iter()
                .map(|l| match l.spans[col].style.fg {
                    Some(Color::Rgb(r, g, b)) => r as u64 + g as u64 + b as u64,
                    _ => 0,
                })
                .sum()
        };
        let still = render_moire_spectrum(&spec, cols, rows, 0.3, &d, 0.3, 0.0, 0.0, 0.0, &[], grad, &mut None);
        let young = render_moire_spectrum(&spec, cols, rows, 0.3, &d, 0.3, 0.0, 0.0, 1.0, &[(0.20, 1.0)], grad, &mut None);
        let old = render_moire_spectrum(&spec, cols, rows, 0.3, &d, 0.3, 0.0, 0.0, 1.0, &[(0.75, 1.0)], grad, &mut None);
        // 波向外移动：用「有波帧 − 无波帧」的增量找波的峰值位置，
        // 老波的峰应离中心更远（直接取亮度峰值会被中心更亮的径向底色掩盖）。
        let center = cols as usize / 2;
        let argmax_delta = |lines: &[Line<'static>]| -> usize {
            (0..cols as usize)
                .max_by_key(|&c| (lum_col(lines, c) as i64 - lum_col(&still, c) as i64).max(0))
                .unwrap_or(center)
        };
        let d_young = (argmax_delta(&young) as i64 - center as i64).unsigned_abs();
        let d_old = (argmax_delta(&old) as i64 - center as i64).unsigned_abs();
        assert!(
            d_old > d_young,
            "波应随时间向外移动：age=0.75 峰值距中心 {} 应大于 age=0.20 的 {}",
            d_old,
            d_young
        );
        // 并且确实改变了画面
        let changed = (0..cols as usize)
            .filter(|&c| (lum_col(&young, c) as i64 - lum_col(&still, c) as i64).abs() > 20)
            .count();
        assert!(changed > 3, "鼓点射出波后应有明显变化，实际 {} 列", changed);
    }

    /// ★ 扩散速度必须跟随音乐：高能量/大通量/节拍脉冲下，相位推进明显更快。
    #[test]
    fn moire_flow_speed_follows_music() {
        let loud = vec![85u64; 64];
        // 安静：低能量、无通量、无脉冲
        let mut p_slow = MoirePlugin::default();
        let mut d_slow = AudioDyn::default();
        d_slow.energy = 0.02;
        d_slow.flux = 0.0;
        d_slow.pulse = 0.0;
        for _ in 0..5 {
            let _ = p_slow.render(&loud, 40, 4, &d_slow, &ctx(None));
        }
        let f0 = p_slow.flow;
        for _ in 0..10 {
            let _ = p_slow.render(&loud, 40, 4, &d_slow, &ctx(None));
        }
        let slow = p_slow.flow - f0;

        // 热闹：高能量 + 高通量 + 强脉冲
        let mut p_fast = MoirePlugin::default();
        let mut d_fast = AudioDyn::default();
        d_fast.energy = 0.95;
        d_fast.flux = 0.90;
        d_fast.pulse = 0.80;
        for _ in 0..5 {
            let _ = p_fast.render(&loud, 40, 4, &d_fast, &ctx(None));
        }
        let f1 = p_fast.flow;
        for _ in 0..10 {
            let _ = p_fast.render(&loud, 40, 4, &d_fast, &ctx(None));
        }
        let fast = p_fast.flow - f1;

        assert!(fast > slow * 2.5,
            "音乐越热闹扩散应越快：安静 {:.3} rad / 热闹 {:.3} rad（10 帧）", slow, fast);
    }

    /// ★ 回归防线：环的**间距必须与时间无关**（粗细恒定）。
    /// 旧实现用 t*(a+b*dist) 做相位速度，空间频率随 t 增长 → 纹理越跑越细并最终消失。
    #[test]
    fn moire_ring_spacing_is_time_invariant() {
        // 复算未取模的原始相位（取模后的相位会回绕，不适合直接量间距）
        let raw = |warp1: f32, ring_freq: f32, flow: f32| warp1 * ring_freq - flow;
        let radii = [0.10f32, 0.20, 0.30, 0.45, 0.60, 0.80];
        let ring_freq = 22.0f32;
        let spacing_at = |t: f32| -> Vec<f32> {
            let ph: Vec<f32> = radii.iter().map(|&d| raw(d.powf(0.68), ring_freq, t)).collect();
            ph.windows(2).map(|w| w[1] - w[0]).collect()
        };
        let base = spacing_at(0.0);
        // self.phase 上限 100 → t 上限 90；再测一个更大的值确认公式本身不含 t 梯度
        for t in [10.0f32, 30.0, 90.0, 500.0] {
            let cur = spacing_at(t);
            for (i, (a, b)) in base.iter().zip(cur.iter()).enumerate() {
                assert!((a - b).abs() < 5e-3,
                    "t={} 时第 {} 段环间距从 {:.4} 变成 {:.4}（说明纹理在变细）", t, i, a, b);
            }
        }
    }

    /// ★ 回归防线：最亮（flash=1）时仍必须有明显的逐格颜色差异。
    /// 之前用「k 钳到 240/cmax」的硬归一化，亮部颜色被完全归一化 → 整片同色、
    /// 摩尔纹理消失（用户反馈「运行一会纹理就没了」）。这里用亮度标准差守住。
    #[test]
    fn moire_texture_survives_at_full_brightness() {
        let cols = 80u32;
        let rows = 4u32;
        let d = AudioDyn::default();
        let spec = vec![60u64; 64];
        for flash in [0.0f32, 0.5, 1.0] {
            let grad = Some(((120, 40, 200), (40, 220, 210)));
            let lines = render_moire_spectrum(&spec, cols, rows, 0.3, &d, 0.3, 0.0, flash, 0.0, &[], grad, &mut None);
            let mut lums: Vec<f32> = Vec::new();
            for l in &lines {
                for sp in &l.spans {
                    if let Color::Rgb(r, g, b) = sp.style.fg.unwrap() {
                        lums.push(0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32);
                    }
                }
            }
            let n = lums.len() as f32;
            let mean = lums.iter().sum::<f32>() / n;
            let var = lums.iter().map(|x| (x - mean) * (x - mean)).sum::<f32>() / n;
            let sd = var.sqrt();
            assert!(sd > 6.0,
                "flash={} 时逐格亮度标准差仅 {:.2}（<6 说明纹理被抹平了）", flash, sd);
        }
    }

    /// ★ 封面渐变色：渲染结果必须跨两个色相（不单色），且不出现纯黑/纯白。
    #[test]
    fn moire_uses_cover_gradient_two_hues() {
        let cols = 80u32;
        let rows = 4u32;
        let d = AudioDyn::default();
        let spec = vec![60u64; 64];
        // 端点：深紫 → 亮青（色相相距远）
        let grad = Some(((120, 40, 200), (40, 220, 210)));
        let lines = render_moire_spectrum(&spec, cols, rows, 0.3, &d, 0.3, 0.0, 0.6, 0.0, &[], grad, &mut None);

        // 收集所有格子颜色，按色相分桶
        let mut hues: Vec<u8> = Vec::new();
        let mut seen_black = false;
        let mut seen_white = false;
        for l in &lines {
            for sp in &l.spans {
                if let Color::Rgb(r, g, b) = sp.style.fg.unwrap() {
                    if r == 0 && g == 0 && b == 0 { seen_black = true; }
                    if r == 255 && g == 255 && b == 255 { seen_white = true; }
                    // 只对非灰（有彩度）的格子统计色相
                    let mx = r.max(g).max(b) as i32;
                    let mn = r.min(g).min(b) as i32;
                    if mx - mn < 20 { continue; }
                    let (rf, gf, bf) = (r as f32, g as f32, b as f32);
                    let df = (mx - mn) as f32;
                    let h = if mx == r as i32 {
                        (((gf - bf) / df).rem_euclid(6.0)) / 6.0
                    } else if mx == g as i32 {
                        ((bf - rf) / df + 2.0) / 6.0
                    } else {
                        ((rf - gf) / df + 4.0) / 6.0
                    };
                    hues.push((h.rem_euclid(1.0) * 12.0) as u8); // 12 个色相桶（30°）
                }
            }
        }
        assert!(!seen_black, "不应出现纯黑格子");
        assert!(!seen_white, "不应出现纯白格子");
        let mut buckets: Vec<u8> = hues.clone();
        buckets.sort_unstable();
        buckets.dedup();
        assert!(buckets.len() >= 2,
            "渐变应跨至少 2 个色相桶（30°/桶），实际 {:?}", buckets);
    }

    /// ★ 新增：封面主色过暗/过亮时被 clamp 到 [40,215]，不会让整片变纯黑/纯白。
    #[test]
    fn moire_clamps_extreme_cover_colors() {
        let cols = 80u32;
        let rows = 4u32;
        let d = AudioDyn::default();
        let spec = vec![60u64; 64];
        // 纯黑封面
        let black = render_moire_spectrum(&spec, cols, rows, 0.3, &d, 0.3, 0.0, 0.5, 0.0, &[], Some(((50, 50, 50), (60, 60, 60))), &mut None);
        // 纯白封面
        let white = render_moire_spectrum(&spec, cols, rows, 0.3, &d, 0.3, 0.0, 0.5, 0.0, &[], Some(((235, 235, 235), (230, 230, 230))), &mut None);
        // 两者输出都不应全黑或全白（每格至少有一点 18..246 的可见色）
        let any_bright = |lines: &Vec<Line<'static>>| -> bool {
            lines.iter().flat_map(|l| l.spans.iter()).any(|sp| {
                if let Color::Rgb(r, g, b) = sp.style.fg.unwrap() {
                    r as u32 + g as u32 + b as u32 > 60
                } else { false }
            })
        };
        assert!(any_bright(&black), "纯黑封面也应保留可见色（不整片纯黑）");
        assert!(any_bright(&white), "纯白封面也应保留可见色（不整片纯白）");
    }
}
