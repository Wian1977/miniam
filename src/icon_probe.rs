#[cfg(test)]
mod icon_probe {
    use crate::braille_text::icon_lines;
    fn show(tag: &str, kind: u8) {
        let lines = icon_lines(kind, 20, 2, Some((200,200,200)), if kind == 2 { Some(0.0) } else { None });
        println!("=== {} ({}x2) ===", tag, 20);
        for l in &lines {
            let s: String = l.spans.iter().map(|x| x.content.as_ref()).collect();
            println!("  |{}|", s);
        }
    }
    #[test]
    fn render_icons() {
        show("play(▶)", 0);
        show("pause(⏸)", 1);
        show("busy(▶ blink)", 2);
    }
}
