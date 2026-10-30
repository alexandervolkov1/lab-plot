use eframe::egui::{
    Color32, FontId,
    text::{LayoutJob, TextFormat},
};

const TEXT: Color32 = Color32::from_rgb(204, 202, 194);
const COMMENT: Color32 = Color32::from_rgb(112, 122, 140);
const STRING: Color32 = Color32::from_rgb(186, 230, 126);
const NUMBER: Color32 = Color32::from_rgb(212, 191, 255);
const KEYWORD: Color32 = Color32::from_rgb(255, 167, 89);
const FUNCTION: Color32 = Color32::from_rgb(115, 208, 255);
const PUNCTUATION: Color32 = Color32::from_rgb(255, 204, 102);

fn is_punctuation(ch: char) -> bool {
    matches!(ch, '(' | ')' | '[' | ']' | '{' | '}' | '\'' | '`' | ',')
}

fn is_separator(ch: char) -> bool {
    ch.is_whitespace() || is_punctuation(ch) || matches!(ch, '"' | ';')
}

fn atom_color(atom: &str) -> Color32 {
    if matches!(
        atom,
        "define"
            | "define-syntax"
            | "lambda"
            | "let"
            | "let*"
            | "letrec"
            | "if"
            | "else"
            | "cond"
            | "case"
            | "begin"
            | "set!"
            | "and"
            | "or"
            | "when"
            | "unless"
            | "quote"
            | "quasiquote"
            | "syntax-rules"
            | "require"
            | "require-builtin"
            | "provide"
    ) {
        KEYWORD
    } else if atom.starts_with("plot/") || atom.starts_with("lab/") || atom.starts_with("lab-") {
        FUNCTION
    } else if matches!(atom, "#t" | "#f" | "#true" | "#false")
        || atom.starts_with("#\\")
        || atom.parse::<f64>().is_ok()
    {
        NUMBER
    } else {
        TEXT
    }
}

fn string_len(text: &str) -> usize {
    let mut escaped = false;

    for (offset, ch) in text.char_indices().skip(1) {
        if escaped {
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == '"' {
            return offset + 1;
        }
    }

    text.len()
}

fn block_comment_len(text: &str) -> usize {
    let mut offset = 2;
    let mut depth = 1usize;

    while offset < text.len() {
        let rest = &text[offset..];

        if rest.starts_with("#|") {
            depth += 1;
            offset += 2;
        } else if rest.starts_with("|#") {
            depth -= 1;
            offset += 2;

            if depth == 0 {
                break;
            }
        } else {
            offset += rest.chars().next().unwrap().len_utf8();
        }
    }

    offset
}

fn tokenize(source: &str) -> Vec<(&str, Color32)> {
    let mut tokens = Vec::new();
    let mut offset = 0;

    while offset < source.len() {
        let rest = &source[offset..];
        let ch = rest.chars().next().unwrap();

        let (len, color) = if rest.starts_with("#|") {
            (block_comment_len(rest), COMMENT)
        } else if ch == ';' {
            (rest.find('\n').unwrap_or(rest.len()), COMMENT)
        } else if ch == '"' {
            (string_len(rest), STRING)
        } else if ch.is_whitespace() {
            (
                rest.find(|c: char| !c.is_whitespace())
                    .unwrap_or(rest.len()),
                TEXT,
            )
        } else if is_punctuation(ch) {
            (ch.len_utf8(), PUNCTUATION)
        } else {
            let len = rest.find(is_separator).unwrap_or(rest.len());
            (len, atom_color(&rest[..len]))
        };

        tokens.push((&rest[..len], color));
        offset += len;
    }

    tokens
}

pub fn highlight(source: &str) -> LayoutJob {
    let mut job = LayoutJob::default();

    for (token, color) in tokenize(source) {
        job.append(
            token,
            0.0,
            TextFormat {
                font_id: FontId::monospace(16.0),
                color,
                ..Default::default()
            },
        );
    }

    job
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_source_text() {
        for source in [
            "",
            "(define x 42)",
            "(define имя \"температура\")",
            "(plot/point! \"sample\" 1.0 20.5)",
            "; комментарий\n(+ 1 2)",
            "\"unfinished string",
        ] {
            let job = highlight(source);
            assert_eq!(job.text, source);
            job.debug_sanity_check();
        }
    }

    #[test]
    fn highlights_expected_tokens() {
        let tokens = tokenize("(define x 20.5) (plot/point! \"sample\" 1.0 2.0)");

        assert!(tokens.contains(&("define", KEYWORD)));
        assert!(tokens.contains(&("20.5", NUMBER)));
        assert!(tokens.contains(&("plot/point!", FUNCTION)));
        assert!(tokens.contains(&("\"sample\"", STRING)));
    }

    #[test]
    fn handles_escaped_quotes_and_nested_comments() {
        let source = "\"a\\\";b\" #| outer #| inner |# end |#";

        let tokens = tokenize(source);

        assert_eq!(tokens.len(), 3);
        assert_eq!(tokens[0].1, STRING);
        assert_eq!(tokens[1].1, TEXT);
        assert_eq!(tokens[2].1, COMMENT);

        assert_eq!(highlight(source).text, source);
    }

    #[test]
    fn recognizes_lab_functions() {
        assert_eq!(atom_color("lab-get"), FUNCTION);
        assert_eq!(atom_color("lab-set"), FUNCTION);
        assert_eq!(atom_color("lab/connect"), FUNCTION);
    }
}
