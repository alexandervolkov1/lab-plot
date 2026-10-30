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
const MATCH_BACKGROUND: Color32 = Color32::from_rgb(57, 68, 88);

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

fn quoted_len(text: &str, delimiter: char) -> usize {
    let mut escaped = false;

    for (offset, ch) in text.char_indices().skip(1) {
        if escaped {
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == delimiter {
            return offset + 1;
        }
    }

    text.len()
}

fn block_comment_end(text: &str) -> (usize, bool) {
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

    (offset, depth == 0)
}

fn tokenize(source: &str) -> Vec<(&str, Color32)> {
    let mut tokens = Vec::new();
    let mut offset = 0;

    while offset < source.len() {
        let rest = &source[offset..];
        let ch = rest.chars().next().unwrap();

        let (len, color) = if rest.starts_with("#|") {
            (block_comment_end(rest).0, COMMENT)
        } else if ch == ';' {
            (rest.find('\n').unwrap_or(rest.len()), COMMENT)
        } else if ch == '"' {
            (quoted_len(rest, '"'), STRING)
        } else if ch == '|' {
            (quoted_len(rest, '|'), TEXT)
        } else if let Some(character) = rest.strip_prefix("#\\") {
            // The first character is literal even when it is a delimiter: #\(, #\;.
            let first = character.chars().next().map_or(0, char::len_utf8);
            let tail = &character[first..];
            (
                2 + first
                    + if character.chars().next().is_some_and(char::is_alphabetic) {
                        tail.find(is_separator).unwrap_or(tail.len())
                    } else {
                        0
                    },
                NUMBER,
            )
        } else if ch.is_whitespace() {
            (
                rest.find(|c: char| !c.is_whitespace())
                    .unwrap_or(rest.len()),
                TEXT,
            )
        } else if is_punctuation(ch) {
            (ch.len_utf8(), PUNCTUATION)
        } else {
            let len = rest
                .find(is_separator)
                .unwrap_or(rest.len())
                .min(rest.find("#|").unwrap_or(rest.len()));
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

/// Lexical code only, with byte offsets; shared with incomplete-input indentation.
pub(super) fn expression_spans(source: &str) -> Vec<(usize, &str)> {
    let mut offset = 0;
    tokenize(source)
        .into_iter()
        .filter_map(|(token, color)| {
            let start = offset;
            offset += token.len();
            (color != COMMENT && !token.trim().is_empty()).then_some((start, token))
        })
        .collect()
}

fn code_spans(source: &str) -> Vec<(usize, &str)> {
    expression_spans(source)
        .into_iter()
        .filter(|(_, token)| {
            !token.starts_with('"') && !token.starts_with('|') && !token.starts_with("#\\")
        })
        .collect()
}

pub(super) fn literal_continuation(source: &str) -> bool {
    let Some((last, _)) = tokenize(source).last().copied() else {
        return false;
    };
    if last.starts_with("#|") {
        return !block_comment_end(last).1;
    }
    if last.starts_with('"') || last.starts_with('|') {
        let delimiter = last.chars().next().unwrap();
        let mut escaped = false;
        for ch in last.chars().skip(1) {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == delimiter {
                return false;
            }
        }
        return true;
    }
    false
}

pub(super) fn matching_pair(source: &str, cursor: usize) -> Option<(usize, usize)> {
    let caret = source
        .char_indices()
        .nth(cursor)
        .map_or(source.len(), |(byte, _)| byte);
    let previous = source[..caret]
        .char_indices()
        .next_back()
        .map(|(byte, _)| byte);
    let brackets: Vec<_> = code_spans(source)
        .into_iter()
        .filter(|(_, token)| token.len() == 1 && "()[]{}".contains(*token))
        .map(|(byte, token)| (byte, token.chars().next().unwrap()))
        .collect();
    let target = brackets
        .iter()
        .find(|(byte, _)| *byte == caret)
        .or_else(|| brackets.iter().find(|(byte, _)| Some(*byte) == previous))?
        .0;
    let mut stack = Vec::new();
    for (byte, ch) in brackets {
        if "([{".contains(ch) {
            stack.push((byte, ch));
        } else if let Some((open, left)) = stack.pop() {
            if matches!((left, ch), ('(', ')') | ('[', ']') | ('{', '}')) {
                if open == target || byte == target {
                    return Some((open, byte));
                }
            } else {
                stack.clear();
            }
        }
    }
    None
}

pub fn highlight_with_cursor(source: &str, cursor: Option<usize>) -> LayoutJob {
    let mut job = highlight(source);
    if let Some((left, right)) = cursor.and_then(|cursor| matching_pair(source, cursor)) {
        for section in &mut job.sections {
            if section.byte_range.start.0 == left || section.byte_range.start.0 == right {
                section.format.background = MATCH_BACKGROUND;
            }
        }
    }
    job
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_only_the_pair_at_the_unicode_cursor() {
        let source = "(ж [x {y}])";
        for (cursor, expected) in [
            (0, Some((0, 11))),
            (1, Some((0, 11))),
            (3, Some((4, 10))),
            (6, Some((7, 9))),
            (9, Some((4, 10))),
            (11, Some((0, 11))),
            (2, None),
        ] {
            assert_eq!(matching_pair(source, cursor), expected, "cursor {cursor}");
        }
        assert_eq!(matching_pair("([)]", 0), None);
        assert_eq!(matching_pair("(unfinished", 0), None);
    }

    #[test]
    fn ignores_strings_comments_character_literals_and_quoted_symbols() {
        let source = "(list \"(\\\"[\" #\\( #\\) #\\; |(name)| ; )\n #| [ #| } |# ] |# x)";
        assert_eq!(matching_pair(source, 0), Some((0, source.len() - 1)));
        for (index, ch) in source.chars().enumerate() {
            if "()[]{}".contains(ch) && index > 1 && index + 1 < source.chars().count() {
                assert_eq!(matching_pair(source, index), None, "cursor {index}");
            }
        }
        let job = highlight_with_cursor(source, Some(0));
        assert_eq!(job.text, source);
        job.debug_sanity_check();
        assert_eq!(
            job.sections
                .iter()
                .filter(|s| s.format.background == MATCH_BACKGROUND)
                .count(),
            2
        );
        assert!(
            highlight_with_cursor("(", Some(0))
                .sections
                .iter()
                .all(|s| s.format.background != MATCH_BACKGROUND)
        );
    }

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
