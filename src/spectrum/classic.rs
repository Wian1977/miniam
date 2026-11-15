//! 经典绿频谱插件：绿色 braille 栅格底部频谱（替代原 draw_ui 内的 Sparkline）
use crate::{AudioDyn, raster_braille};
use image::RgbaImage;
use image::Rgba;
use ratatui::text::Line;
use super::{SpectrumCtx, SpectrumPlugin};

pub struct ClassicPlugin;
impl Default for ClassicPlugin {
    fn default() -> Self { ClassicPlugin }
}
impl SpectrumPlugin for ClassicPlugin {
    fn name(&self) -> &'static str { "经典绿" }
    fn render(&mut self, spectrum: &[u64], cols: u32, rows: u32, dyn_state: &AudioDyn, _ctx: &SpectrumCtx) -> Vec<Line<'static>> {
        let bw = (cols.max(1) * 2) as u32;
        let bh = (rows.max(1) * 4) as u32;
        let mut buf = RgbaImage::from_pixel(bw, bh, Rgba([0, 0, 0, 255]));
        let n = spectrum.len().max(1);
        let bin_w = ((bw as f32) / n as f32).max(1.0) as u32;
        let dim = dyn_state.passage_gain() < 0.6;
        for (i, &v) in spectrum.iter().enumerate() {
            let e = (v as f32 / 100.0).clamp(0.0, 1.0);
            let h = (e * bh as f32).round().max(0.0) as i64;
            let x0 = (i as f32 * bin_w as f32) as i64;
            let x1 = (((i as f32 + 1.0) * bin_w as f32) as i64).min(bw as i64);
            for x in x0..x1 {
                if x < 0 || x >= bw as i64 { continue; }
                for y in (bh as i64 - h)..bh as i64 {
                    if y < 0 { continue; }
                    let p = buf.get_pixel_mut(x as u32, y as u32);
                    let lum = (55.0 + 200.0 * e) as u8;
                    p[0] = 0;
                    p[1] = if dim { (lum as f32 * 0.6) as u8 } else { lum };
                    p[2] = 20;
                    p[3] = 255;
                }
            }
        }
        raster_braille(&buf, cols, rows, 0, 0)
    }
}
