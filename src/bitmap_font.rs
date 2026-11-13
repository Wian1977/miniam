//! GNU Unifont 原生点阵字库（见 assets/README-unifont.txt）。
//!
//! 与「矢量字体缩小」不同：这里直接查表得到 8x16 / 16x16 的 1px 点阵。
//! 宽是 2 的倍数、高固定 16 = 4 行 braille x 4，因此每个点阵像素正好
//! 落在一个 braille dot 上，无需缩放 / 抗锯齿 / hinting —— 这是极低
//! 分辨率下最清晰的做法。

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::OnceLock;

/// 编译期内置混合点阵字库（assets/font_iosevka.hex）：ASCII/标点用 Iosevka 8×16（4×4 braille 格），
/// 汉字等仍用文泉驿 12×12/16×16。半角 8×16、全角 12×12 或 16×16，均为 2/4 的倍数，对齐网格。
/// 在 16 像素高的标题带里贴顶，底部自然留 1 行空白。
/// 来源与许可见 assets/README-unifont.txt。
/// 旧 16×16 粗体字库 assets/unifont_bold.hex 仍保留，可用 MINIAM_FONT_HEX 切回。
const BUILTIN_HEX: &str = include_str!("../assets/font_iosevka.hex");

/// 一个点阵字形：宽 w（6/8/12/16）、高 h（12 或 16）像素。
/// rows[r] 的 bit(15 - x) 为 1 表示第 r 行、第 x 列点亮（MSB = 最左像素）。
#[derive(Clone, Copy)]
pub struct Glyph {
    pub w: u8,
    pub h: u8,
    pub rows: [u16; 16],
}

fn blank() -> Glyph {
    Glyph { w: 6, h: 12, rows: [0; 16] }
}

fn hex_nib(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// 解析一行 "码点:HEXDATA"。四种格式（宽 x 高）：
///   24 hex -> 12 行 x 1 字节 -> 6x12   半角（12x12 细体字库）
///   48 hex -> 12 行 x 2 字节 -> 12x12  全角（12x12 细体字库）
///   32 hex -> 16 行 x 1 字节 -> 8x16   半角（旧 Unifont 粗体字库）
///   64 hex -> 16 行 x 2 字节 -> 16x16  全角（旧 Unifont 粗体字库）
fn parse_line(line: &str) -> Option<(u32, Glyph)> {
    let (code, hx) = line.split_once(':')?;
    let (h, w, step) = match hx.len() {
        24 => (12usize, 6u8, 2usize),   // 12x12 字库半角：每行高 6 位有效
        48 => (12, 12, 4),
        32 => (16, 8, 2),              // 旧 Unifont 半角：8 位全有效
        64 => (16, 16, 4),
        _ => return None,
    };
    let cp = u32::from_str_radix(code.trim(), 16).ok()?;
    let b = hx.as_bytes();
    let mut rows = [0u16; 16];
    for r in 0..h {
        if step == 2 {
            // 半角：每行 1 字节，放到 u16 高 8 位，bit15 对应 x=0。
            let byte = (hex_nib(b[2 * r])? << 4) | hex_nib(b[2 * r + 1])?;
            rows[r] = (byte as u16) << 8;
        } else {
            // 全角：每行 2 字节，高字节在左。
            let hi = (hex_nib(b[4 * r])? << 4) | hex_nib(b[4 * r + 1])?;
            let lo = (hex_nib(b[4 * r + 2])? << 4) | hex_nib(b[4 * r + 3])?;
            rows[r] = ((hi as u16) << 8) | lo as u16;
        }
    }
    Some((cp, Glyph { w, h: h as u8, rows }))
}

static FONT: OnceLock<HashMap<u32, Glyph>> = OnceLock::new();

fn load() -> HashMap<u32, Glyph> {
    // MINIAM_FONT_HEX 可指向另一份同格式 .hex 用于临时试别的点阵字库；
    // 读不到就回退内置字库，保证开箱即用。
    let text: Cow<str> = match std::env::var("MINIAM_FONT_HEX") {
        Ok(p) => std::fs::read_to_string(&p)
            .map(Cow::Owned)
            .unwrap_or(Cow::Borrowed(BUILTIN_HEX)),
        Err(_) => Cow::Borrowed(BUILTIN_HEX),
    };
    let mut map = HashMap::with_capacity(42_000);
    for line in text.lines() {
        if let Some((cp, g)) = parse_line(line) {
            map.insert(cp, g);
        }
    }
    map
}

/// 查字形；缺字回退 '?'，再缺则返回 8x16 空白。
pub fn glyph(cp: u32) -> Glyph {
    let f = FONT.get_or_init(load);
    f.get(&cp)
        .copied()
        .or_else(|| f.get(&(b'?' as u32)).copied())
        .unwrap_or_else(blank)
}
