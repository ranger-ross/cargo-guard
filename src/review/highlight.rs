use eyre::{Context, ContextCompat};
use ratatui::{
    style::{Color, Style},
    text::{Line, Span},
};
use syntect::{
    easy::HighlightLines,
    highlighting::{Theme, ThemeSet},
    parsing::SyntaxSet,
    util::LinesWithEndings,
};
use unicode_width::UnicodeWidthChar;

/// Syntax/theme data is shared across files; parser state is fresh per file.
pub struct Highlighter {
    syntaxes: SyntaxSet,
    theme: Theme,
}

impl Highlighter {
    pub fn new() -> eyre::Result<Self> {
        let syntaxes = SyntaxSet::load_defaults_newlines();
        let theme = ThemeSet::load_defaults()
            .themes
            .remove("base16-eighties.dark")
            .context("bundled syntax theme is missing")?;
        Ok(Self { syntaxes, theme })
    }

    pub fn highlight(&self, source: &str) -> eyre::Result<Vec<Line<'static>>> {
        let syntax = self
            .syntaxes
            .find_syntax_by_extension("rs")
            .context("bundled Rust syntax is missing")?;
        let mut highlighter = HighlightLines::new(syntax, &self.theme);
        let mut lines = Vec::new();
        // Parse the complete file before selecting viewport lines. In particular,
        // scrolling into a multiline string/comment must retain its context.
        for line in LinesWithEndings::from(source) {
            let tokens = highlighter
                .highlight_line(line, &self.syntaxes)
                .context("could not highlight Rust source")?;
            let mut column = 0;
            let spans = tokens
                .into_iter()
                .map(|(style, text)| {
                    let text = expand_tabs(text.trim_end_matches(['\r', '\n']), &mut column);
                    let foreground = style.foreground;
                    // Deliberately omit the theme's background: later diff rendering
                    // can apply addition/deletion backgrounds to the entire line.
                    Span::styled(
                        text,
                        Style::default().fg(Color::Rgb(foreground.r, foreground.g, foreground.b)),
                    )
                })
                .collect::<Vec<_>>();
            lines.push(Line::from(spans));
        }
        Ok(lines)
    }
}

fn expand_tabs(text: &str, column: &mut usize) -> String {
    let mut result = String::new();
    for c in text.chars() {
        if c == '\t' {
            let spaces = 4 - *column % 4;
            result.extend(std::iter::repeat_n(' ', spaces));
            *column += spaces;
        } else {
            result.push(c);
            *column += c.width().unwrap_or(0);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    fn color_at(line: &Line<'_>, needle: &str) -> Option<Color> {
        let index = text(line).find(needle).unwrap();
        let mut offset = 0;
        for span in &line.spans {
            offset += span.content.len();
            if index < offset {
                return span.style.fg;
            }
        }
        panic!("missing span for {needle}")
    }

    #[test]
    fn preserves_multiline_comment_and_raw_string_context() {
        let highlighter = Highlighter::new().unwrap();
        let lines = highlighter
            .highlight(concat!(
                "/* opening\n",
                "fn inside_comment() {}\n",
                "*/\n",
                "fn main() {\n",
                "    let text = r#\"opening\n",
                "fn inside_string() {}\n",
                "\"#;\n",
                "}\n",
            ))
            .unwrap();
        assert_eq!(lines.len(), 8);
        let keyword = color_at(&lines[3], "fn");
        assert!(keyword.is_some());
        assert_ne!(color_at(&lines[1], "fn"), keyword);
        assert_ne!(color_at(&lines[5], "fn"), keyword);
        let fresh = highlighter.highlight("fn other() {}").unwrap();
        assert_eq!(color_at(&fresh[0], "fn"), keyword);
        for line in &lines {
            assert!(line.style.bg.is_none());
            assert!(line.spans.iter().all(|span| span.style.bg.is_none()));
        }
    }

    #[test]
    fn preserves_text_unicode_line_endings_and_tabs() {
        let highlighter = Highlighter::new().unwrap();
        let lines = highlighter
            .highlight("\tlet x = \"界\";\t// 🦀\r\n\r\nlast")
            .unwrap();
        assert_eq!(lines.len(), 3);
        assert_eq!(text(&lines[0]), "    let x = \"界\";   // 🦀");
        assert_eq!(text(&lines[1]), "");
        assert_eq!(text(&lines[2]), "last");
        assert!(highlighter.highlight("").unwrap().is_empty());
        assert_eq!(highlighter.highlight("\n").unwrap().len(), 1);
    }
}
