#[cfg(test)]
mod note_probe {
    use crate::braille_text::default_note_lines;
    #[test]
    fn render_note() {
        let lines = default_note_lines(30, 8, Some((200,200,200)), 1.0);
        println!("note lines={}", lines.len());
        for l in &lines {
            let s: String = l.spans.iter().map(|x| x.content.as_ref()).collect();
            println!("  |{}|", s);
        }
        // 至少应有非空格子
        let any = lines.iter().any(|l| l.spans.iter().any(|sp| sp.content.as_ref() != " "));
        assert!(any, "note must render something");
    }
}
