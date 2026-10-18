use std::sync::LazyLock;

use eyre::{Context, ContextCompat};
use ratatui::{
    style::{Color, Style},
    text::{Line, Span},
};
use syntect::{
    easy::HighlightLines,
    highlighting::{Color as SyntaxColor, StyleModifier, Theme, ThemeItem, ThemeSettings},
    parsing::SyntaxSet,
    util::LinesWithEndings,
};
use unicode_width::UnicodeWidthChar;

static SYNTAXES: LazyLock<SyntaxSet> = LazyLock::new(SyntaxSet::load_defaults_newlines);
static TITANIUM: LazyLock<Theme> = LazyLock::new(titanium_theme);

/// Share syntax data and compiled regexes across the entire review session.
/// Parser state remains fresh for each source file.
pub struct Highlighter {
    syntaxes: &'static SyntaxSet,
    theme: &'static Theme,
}

impl Highlighter {
    pub fn new() -> eyre::Result<Self> {
        Ok(Self {
            syntaxes: &SYNTAXES,
            theme: &TITANIUM,
        })
    }

    pub fn highlight(&self, source: &str) -> eyre::Result<Vec<Line<'static>>> {
        let syntax = self
            .syntaxes
            .find_syntax_by_extension("rs")
            .context("bundled Rust syntax is missing")?;
        let mut highlighter = HighlightLines::new(syntax, self.theme);
        let mut lines = Vec::new();
        // Parse the complete file before selecting viewport lines. In particular,
        // scrolling into a multiline string/comment must retain its context.
        for line in LinesWithEndings::from(source) {
            let tokens = highlighter
                .highlight_line(line, self.syntaxes)
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

/// Inspired by https://github.com/can1357/oh-my-pi/blob/main/packages/tui/src/theme/defaults/titanium.json
fn titanium_theme() -> Theme {
    let rgb = |hex: u32| SyntaxColor {
        r: (hex >> 16) as u8,
        g: (hex >> 8) as u8,
        b: hex as u8,
        a: 255,
    };
    // Titanium syntax colors mapped to Syntect's TextMate scopes.
    let scopes = [
        ("variable", 0xe8ecf4),
        ("keyword, storage, constant.language", 0x00b4ff),
        ("keyword.operator", 0x00b4ff),
        (
            "entity.name.type, entity.name.struct, entity.name.enum, entity.name.trait, entity.name.impl, support.type",
            0x00b4ff,
        ),
        (
            "entity.name.function, variable.function, support.function, support.macro",
            0x00ff88,
        ),
        ("constant.numeric", 0xffb347),
        ("punctuation", 0x9ca3b0),
        (
            "string, string punctuation, string storage, constant.character",
            0xd4c090,
        ),
        ("comment, comment punctuation", 0x6b7280),
    ]
    .into_iter()
    .map(|(scope, foreground)| ThemeItem {
        scope: scope.parse().expect("valid Titanium scope selector"),
        style: StyleModifier {
            foreground: Some(rgb(foreground)),
            ..StyleModifier::default()
        },
    })
    .collect();
    Theme {
        name: Some("Titanium".into()),
        settings: ThemeSettings {
            foreground: Some(rgb(0xe8ecf4)),
            ..ThemeSettings::default()
        },
        scopes,
        ..Theme::default()
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
