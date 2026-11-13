#[cfg(test)]
mod bt_probe {
    use crate::braille_text::text_to_braille;

    fn show(tag: &str, text: &str, cols: u32, rows: u32) {
        match text_to_braille(text, cols, rows, Some((180, 230, 255))) {
            Some(lines) => {
                println!("=== {} ({}x{} cells) lines={} ===", tag, cols, rows, lines.len());
                for l in &lines {
                    let s: String = l.spans.iter().map(|x| x.content.as_ref()).collect();
                    println!("  |{}|", s);
                }
            }
            None => println!("=== {} -> None ===", tag),
        }
    }

    #[test]
    fn render_titles() {
        show("夜曲 16x4", "夜曲", 16, 4);
        show("夜曲 24x5", "夜曲", 24, 5);
        show("夜曲 32x6", "夜曲", 32, 6);
        show("A 8x4", "A", 8, 4);
        show("HELLO 24x4", "HELLO", 24, 4);
        show("夜曲 48x4", "夜曲", 48, 4);
        show("周杰伦 40x5", "周杰伦", 40, 5);
        show("강남스타일 40x5 (韩文)", "강남스타일", 40, 5);
        show("안녕 24x5 (韩文)", "안녕", 24, 5);
        show("夜曲+안녕 48x5 (混排)", "夜曲안녕", 48, 5);
    }
}
