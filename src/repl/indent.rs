fn char_to_byte(text: &str, index: usize) -> usize {
    text.char_indices()
        .nth(index)
        .map_or(text.len(), |(offset, _)| offset)
}

fn next_indent(prefix: &str) -> usize {
    let line = prefix.rsplit('\n').next().unwrap_or("");

    let base = line.chars().take_while(|ch| *ch == ' ').count();

    let mut balance: i32 = 0;
    let mut in_string = false;
    let mut escaped = false;

    for ch in line.chars() {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }

        match ch {
            ';' => break,
            '"' => in_string = true,
            '(' | '[' | '{' => balance += 1,
            ')' | ']' | '}' => balance -= 1,
            _ => {}
        }
    }

    if balance > 0 {
        base + 2
    } else if balance < 0 {
        base.saturating_sub(2)
    } else {
        base
    }
}

pub(super) fn insert_newline(source: &mut String, start_char: usize, end_char: usize) -> usize {
    let start = char_to_byte(source, start_char);
    let end = char_to_byte(source, end_char);

    let indent = next_indent(&source[..start]);
    let insertion = format!("\n{}", " ".repeat(indent));

    source.replace_range(start..end, &insertion);

    start_char + 1 + indent
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indents_scheme_body() {
        let mut source = "(define (square x)".to_owned();
        let cursor = source.chars().count();

        let new_cursor = insert_newline(&mut source, cursor, cursor);

        assert_eq!(source, "(define (square x)\n  ");
        assert_eq!(new_cursor, cursor + 3);
    }

    #[test]
    fn preserves_existing_indentation() {
        assert_eq!(next_indent("  (plot/point!"), 4);
        assert_eq!(next_indent("  ; comment ("), 2);
        assert_eq!(next_indent("  (display \"(\")"), 2);
        assert_eq!(next_indent("    (foo))"), 2);
    }

    #[test]
    fn supports_unicode() {
        let mut source = "(define имя 10)".to_owned();
        let cursor = source.chars().count();

        insert_newline(&mut source, cursor, cursor);

        assert_eq!(source, "(define имя 10)\n");
    }
}
