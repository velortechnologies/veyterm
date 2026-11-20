use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};
use unicode_width::UnicodeWidthStr;
pub fn bold(text: String, style: Style) -> Line<'static> {
    let mut strong = false;
    Line::from(
        text.split("**")
            .map(|part| {
                let st = if strong {
                    style.add_modifier(Modifier::BOLD)
                } else {
                    style
                };
                strong = !strong;
                Span::styled(part.to_owned(), st)
            })
            .collect::<Vec<_>>(),
    )
}
pub fn tables(text: &str) -> String {
    let lines: Vec<_> = text.lines().collect();
    let mut out = Vec::new();
    let mut i = 0;
    let mut fenced = false;
    while i < lines.len() {
        if lines[i].trim_start().starts_with("```") {
            fenced = !fenced;
        }
        if !fenced && i + 1 < lines.len() && lines[i].contains('|') && separator(lines[i + 1]) {
            let mut rows = vec![cells(lines[i])];
            i += 2;
            while i < lines.len() && lines[i].contains('|') && !lines[i].trim().is_empty() {
                rows.push(cells(lines[i]));
                i += 1;
            }
            let n = rows[0].len();
            let mut widths = vec![0; n];
            for row in &rows {
                for (j, c) in row.iter().take(n).enumerate() {
                    widths[j] = widths[j].max(c.replace("**", "").width());
                }
            }
            for (r, row) in rows.iter().enumerate() {
                let parts: Vec<_> = (0..n)
                    .map(|j| {
                        let c = row.get(j).map(String::as_str).unwrap_or("");
                        format!(
                            " {}{} ",
                            c,
                            " ".repeat(widths[j].saturating_sub(c.replace("**", "").width()))
                        )
                    })
                    .collect();
                out.push(format!("│{}│", parts.join("│")));
                if r == 0 {
                    out.push(format!(
                        "├{}┤",
                        widths
                            .iter()
                            .map(|w| "─".repeat(w + 2))
                            .collect::<Vec<_>>()
                            .join("┼")
                    ));
                }
            }
        } else {
            out.push(lines[i].to_owned());
            i += 1;
        }
    }
    out.join("\n")
}
fn cells(s: &str) -> Vec<String> {
    s.trim()
        .trim_matches('|')
        .split('|')
        .map(|v| v.trim().to_owned())
        .collect()
}
fn separator(s: &str) -> bool {
    let c = cells(s);
    s.contains('|')
        && !c.is_empty()
        && c.iter().all(|v| {
            v.contains('-') && v.chars().all(|c| c == '-' || c == ':' || c.is_whitespace())
        })
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn table_unicode() {
        let t = tables("| ім'я | n |\n|---|---:|\n| Ki | 2 |");
        assert!(t.contains('┼'));
        let widths: Vec<_> = t.lines().map(|v| v.width()).collect();
        assert!(widths.iter().all(|w| *w == widths[0]));
    }
    #[test]
    fn bold_style() {
        let l = bold("hi **Ki**".into(), Style::default());
        assert!(l.spans[1].style.add_modifier.contains(Modifier::BOLD));
    }
}
