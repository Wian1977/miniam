// ============ braille 点阵文字（最小点阵，不用方块） ============
// 终端单行文字只有 2~4px 高，无法点阵化；点阵文字只用于多行区域。
// 用 braille（2x4 像素/格，8 个圆点）而非象限方块：粒度最细、无方块边缘。
// 字模来自 GNU Unifont 原生点阵（见 assets/README-unifont.txt），
// 逐像素对齐 braille 网格，不做任何缩放 / 抗锯齿 / hinting。

use crate::bitmap_font;

/// 原生点阵拼版：逐字符查点阵字模，直接贴到 W×H 的 RGBA 缓冲，
/// **不做任何缩放**。字形宽 6/8/12/16（2 的倍数）、高 12 或 16（4 的倍数），
/// 因此每个点阵像素正好落在一个 braille dot 上；hoff 取偶数、voff 取 4 的倍数
/// 以保持网格对齐，避免点阵行/列跨 braille 格边界产生错位。
/// 12 高字形贴顶，在 16 高的标题带里底部自然留 1 行空白。
/// 标题超出可用宽度时截断，并在末尾补一个省略号（若放得下）。
fn rasterize_bitmap_text(text: &str, max_w: u32, max_h: u32) -> Option<image::RgbaImage> {
    use image::Rgba;
    let mut placed: Vec<(bitmap_font::Glyph, i64)> = Vec::new();
    let mut pen = 0i64;
    let ellipsis = bitmap_font::glyph(0x2026);
    for ch in text.chars() {
        if ch.is_control() {
            continue;
        }
        let g = bitmap_font::glyph(ch as u32);
        if pen + g.w as i64 > max_w as i64 {
            if pen + ellipsis.w as i64 <= max_w as i64 {
                placed.push((ellipsis, pen));
                pen += ellipsis.w as i64;
            }
            break;
        }
        placed.push((g, pen));
        pen += g.w as i64;
    }
    if placed.is_empty() {
        return None;
    }
    // 横向居中：偏移必须是 2 的倍数，否则点阵像素会跨 braille 列边界。
    let hoff = (((max_w as i64 - pen) / 2) / 2 * 2).max(0);
    // 纵向：按实际字形高度贴顶（12 高时在 16 高画布里底部留 1 行空白）。
    // 偏移必须是 4 的倍数才能保持 braille 行对齐。
    let gh = placed.iter().map(|(g, _)| g.h as i64).max().unwrap_or(12);
    let voff = (((max_h as i64 - gh) / 2) / 4 * 4).max(0);
    let mut buf = image::RgbaImage::from_pixel(max_w, max_h, Rgba([0, 0, 0, 0]));
    for (g, x0) in &placed {
        for r in 0..g.h as i64 {
            let bits = g.rows[r as usize];
            for x in 0..g.w as i64 {
                if bits & (1u16 << ((15 - x) as u32)) == 0 {
                    continue;
                }
                let px = hoff + x0 + x;
                let py = voff + r;
                if px >= 0 && py >= 0 && px < max_w as i64 && py < max_h as i64 {
                    buf.put_pixel(px as u32, py as u32, Rgba([255, 255, 255, 255]));
                }
            }
        }
    }
    Some(buf)
}

/// 内容驱动的 braille 光栅化：点亮的像素 -> 对应 braille 位；
/// 整格无内容 -> 裸空格（文字本来就需要空白分隔字形，这不是空洞）。
/// color = None 时用终端默认前景色（Color::Reset），配合「颜色体系统一用终端默认」。
pub fn raster_text_braille(
    buf: &image::RgbaImage,
    color: Option<(u8, u8, u8)>,
) -> Vec<ratatui::text::Line<'static>> {
    use ratatui::style::{Color, Style};
    use ratatui::text::{Line, Span};
    let w_px = buf.width() as i64;
    let h_px = buf.height() as i64;
    let ncols = (w_px / 2).max(0);
    let nrows = (h_px / 4).max(0);
    let fg = match color {
        Some((r, g, b)) => Color::Rgb(r, g, b),
        None => Color::Reset,
    };
    // 铺不透明底(同 render_spectrum：每个 span 都带显式 bg)：空白格是带真黑 bg 的空格。
    // 裸空格 Span::raw(" ") 会把全局黑底冲成 Reset、露出终端默认底色，形成空洞/暗带(空行)；
    // 显式 bg 后整片连续纯黑、无可见点阵图案，也不露底色。
    let black = Color::Rgb(0, 0, 0);
    let void_style = Style::default().bg(black);
    let ink_style = Style::default().fg(fg).bg(black);
    let void_ch = " ".to_string();
    let mut lines = Vec::with_capacity(nrows as usize);
    for ry in 0..nrows {
        let mut spans: Vec<Span> = Vec::with_capacity(ncols as usize);
        for cx in 0..ncols {
            let px0 = cx * 2;
            let py0 = ry * 4;
            let mut bits: u32 = 0;
            for r4 in 0..4i64 {
                for c2 in 0..2i64 {
                    let (x, y) = (px0 + c2, py0 + r4);
                    if x >= w_px || y >= h_px {
                        continue;
                    }
                    let p = buf.get_pixel(x as u32, y as u32);
                    if p[3] > 40 {
                        if c2 == 0 {
                            bits |= 1 << (if r4 < 3 { r4 } else { 6 });
                        } else {
                            bits |= 1 << (if r4 < 3 { r4 + 3 } else { 7 });
                        }
                    }
                }
            }
            if bits == 0 {
                spans.push(Span::styled(void_ch.clone(), void_style));
            } else {
                let ch = char::from_u32(0x2800 + bits).unwrap();
                spans.push(Span::styled(ch.to_string(), ink_style));
            }
        }
        lines.push(Line::from(spans));
    }
    lines
}

/// 便捷封装：文字 -> braille 点阵 Line。走 GNU Unifont 原生点阵字模，
/// 逐像素对齐 braille 2×4 网格，不缩放（因此小字号也锐利）。
/// color = None 用终端默认前景色。cols/rows 是目标字符格数。
pub fn text_to_braille(
    text: &str,
    cols: u32,
    rows: u32,
    color: Option<(u8, u8, u8)>,
) -> Option<Vec<ratatui::text::Line<'static>>> {
    // braille：横向 2px/格、纵向 4px/格
    let max_w = cols.saturating_mul(2).max(2);
    let max_h = rows.saturating_mul(4).max(4);
    let buf = rasterize_bitmap_text(text, max_w, max_h)?;
    // raster_text_braille 输出正好 rows 行(铺满容器)，调用方直接渲染到完整 rect。
    Some(raster_text_braille(&buf, color))
}

/// 缺省图：八分音符（符头实心圆 + 符干 + 旗），纯点阵、不依赖字体是否有 ♪ 字形。
/// 用 braille 渲染，color = None 时为终端默认前景色。居中于给定的字符格。
/// breath 为呼吸缩放系数（1.0 不变；>1 放大、<1 缩小），用于启动中动效。
pub fn default_note_lines(
    cols: u32,
    rows: u32,
    color: Option<(u8, u8, u8)>,
    breath: f32,
) -> Vec<ratatui::text::Line<'static>> {
    use image::Rgba;
    use image::imageops::FilterType;
    let w = (cols.saturating_mul(2)).max(10) as u32;
    let h = (rows.saturating_mul(4)).max(10) as u32;
    // 先在固定分辨率画八分音符，再按比例缩放到目标（留 80% 余量）。
    // 余量需保证 0.80*(1+最大呼吸幅度) < 1，否则放大到最大时超封面高度被截断、底部裁切。
    let nw = 20u32;
    let nh = 30u32;
    let mut note = image::RgbaImage::from_pixel(nw, nh, Rgba([0, 0, 0, 0]));
    let (cx, cy, rx, ry) = (9i64, 19i64, 8i64, 6i64);
    for y in 0..nh as i64 {
        for x in 0..nw as i64 {
            let nx = (x - cx) as f64 / rx as f64;
            let ny = (y - cy) as f64 / ry as f64;
            // superellipse p=3, flat-ish bottom: drop the |ny|=1 rows so the head ends
            // on a wide row instead of a single-pixel tip (which looked detached in braille)
            if ny.abs() < 1.0 && nx.abs().powf(3.0) + ny.abs().powf(3.0) <= 1.0 {
                note.put_pixel(x as u32, y as u32, Rgba([255, 255, 255, 255]));
            }
        }
    }
    // 符干（符头右侧往上）
    let stem_x = cx + rx - 2;
    for y in 3..cy {
        let x = stem_x;
        if x >= 0 && x < nw as i64 {
            note.put_pixel(x as u32, y as u32, Rgba([255, 255, 255, 255]));
        }
    }
    // 旗（符干顶端往右下的小三角）
    for y in 3..12 {
        for x in stem_x..(stem_x + 6).min(nw as i64) {
            if (x - stem_x) + (y - 3) <= 7 {
                note.put_pixel(x as u32, y as u32, Rgba([255, 255, 255, 255]));
            }
        }
    }
    let scale = ((w as f64 / nw as f64).min(h as f64 / nh as f64) * 0.80 * breath as f64).max(0.1);
    let rw = ((nw as f64 * scale) as u32).min(w).max(1);
    let rh = ((nh as f64 * scale) as u32).min(h).max(1);
    let resized = image::imageops::resize(&note, rw, rh, FilterType::Nearest);
    // 底部对齐：让音符贴着封面区下沿（紧挨点阵标题），空白留在顶部而非中段，
    // 避免「点阵文字 ↔ 滚动文字」之间出现一整片空行。
    let offx = ((w - rw) / 2) as i64;
    let offy = (h as i64 - rh as i64).max(0);
    let mut buf = image::RgbaImage::from_pixel(w, h, Rgba([0, 0, 0, 0]));
    for y in 0..rh as i64 {
        for x in 0..rw as i64 {
            let p = resized.get_pixel(x as u32, y as u32);
            if p[3] > 40 {
                let bx = (offx + x) as u32;
                let by = (offy + y) as u32;
                if bx < w && by < h {
                    buf.put_pixel(bx, by, *p);
                }
            }
        }
    }
    raster_text_braille(&buf, color)
}

/// 播放态图标：用点阵（braille）画「播放▶ / 暂停⏸」，居中铺满给定字符格、可放大。
/// kind: 0=播放, 1=暂停。blink_phase=None 时恒亮；Some(p) 时按 sin(p)>=0 隐现
/// （调用方传入慢相位=起播中忙闪，或快相位=按键反馈快闪 2 次）。
/// color = None 用终端默认前景色。
pub fn icon_lines(kn: u8, cols: u32, rows: u32, color: Option<(u8, u8, u8)>, blink_phase: Option<f64>) -> Vec<ratatui::text::Line<'static>> {
    use image::Rgba;
    let w = (cols.saturating_mul(2)).max(4) as i64;
    let h = (rows.saturating_mul(4)).max(4) as i64;
    let mut img = image::RgbaImage::from_pixel(w as u32, h as u32, Rgba([0, 0, 0, 0]));
    let cx = w / 2;
    let cy = h / 2;
    // 以高度为基准取方形尺寸并水平居中，避免宽屏下被拉成扁片；且严格按传入 rows 决定高度（不强行放大导致被裁切）
    let s = ((h as f64) * 0.42).max(2.0);
    let sx = s;
    let sy = s;
    // blink_phase=None 恒亮；Some(p): sin(p)>=0 为亮、否则灭。
    let visible = blink_phase.map_or(true, |p| p >= 0.0);
    for y in 0..h {
        for x in 0..w {
            let dx = x as f64 - cx as f64;
            let dy = y as f64 - cy as f64;
            let on = match kn {
                1 => {
                    // 双竖条（暂停）
                    let bw = sx as f64 * 0.45;
                    let gap = sx as f64 * 0.55;
                    dy.abs() <= sy as f64
                        && ((dx + gap / 2.0).abs() <= bw / 2.0 || (dx - gap / 2.0).abs() <= bw / 2.0)
                }
                _ => {
                    // 右指三角（▶）：底边在左、顶点在右
                    let t = (dx + sx as f64) / (2.0 * sx as f64);
                    t >= 0.0 && t <= 1.0 && dy.abs() <= sy as f64 * (1.0 - t)
                }
            };
            if on && visible {
                img.put_pixel(x as u32, y as u32, Rgba([255, 255, 255, 255]));
            }
        }
    }
    raster_text_braille(&img, color)
}
