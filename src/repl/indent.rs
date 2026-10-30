fn char_to_byte(text: &str, index: usize) -> usize {
    text.char_indices()
        .nth(index)
        .map_or(text.len(), |(offset, _)| offset)
}

struct OpenForm<'a> {
    bracket: char,
    column: usize,
    line: usize,
    head: Option<&'a str>,
    count: usize,
    first_arg: Option<(usize, usize)>,
    data: bool,
    clause: bool,
}

fn next_indent(prefix: &str) -> usize {
    let base = prefix
        .rsplit('\n')
        .next()
        .unwrap_or("")
        .chars()
        .take_while(|ch| *ch == ' ')
        .count();
    if super::highlight::literal_continuation(prefix) {
        return base;
    }

    let mut stack: Vec<OpenForm<'_>> = Vec::new();
    let (mut line, mut column, mut offset) = (0, 0, 0);
    let mut quoted = false;
    for (start, token) in super::highlight::expression_spans(prefix) {
        for ch in prefix[offset..start].chars() {
            if ch == '\n' {
                line += 1;
                column = 0;
            } else {
                column += 1;
            }
        }
        offset = start;
        if matches!(token, "'" | "`" | "," | ",@") {
            quoted = token != "," && token != ",@";
            continue;
        }
        if matches!(token, ")" | "]" | "}") {
            if let Some(open) = stack.pop()
                && !matches!((open.bracket, token), ('(', ")") | ('[', "]") | ('{', "}"))
            {
                stack.clear();
            }
            quoted = false;
            continue;
        }
        let is_open = matches!(token, "(" | "[" | "{");
        let mut data = quoted;
        let mut clause = false;
        if let Some(parent) = stack.last_mut() {
            data |= parent.data;
            clause = !parent.data
                && (parent.head == Some("cond")
                    || (parent.head == Some("case") && parent.count >= 2));
            if parent.count == 0 && !is_open {
                parent.head = Some(token);
            }
            if parent.count == 1 {
                parent.first_arg = Some((column, line));
            }
            parent.count += 1;
        }
        if is_open {
            stack.push(OpenForm {
                bracket: token.chars().next().unwrap(),
                column,
                line,
                head: None,
                count: 0,
                first_arg: None,
                data,
                clause,
            });
        }
        quoted = false;
    }
    let Some(form) = stack.last() else {
        return 0;
    };
    if form.clause || form.data || form.head.is_none() {
        return form.column + 1;
    }
    let inline_arg = form
        .first_arg
        .filter(|(_, line)| *line == form.line)
        .map(|(col, _)| col);
    match form.head.unwrap() {
        "if" => form.column + 4,
        "lambda" if form.count == 1 => form.column + 4,
        "define" | "define-syntax" | "lambda" | "let" | "let*" | "letrec" | "cond" | "case"
        | "when" | "unless" | "begin-for-syntax" => form.column + 2,
        "begin" | "hash" => inline_arg.unwrap_or(form.column + 2),
        _ => inline_arg.unwrap_or(form.column + 1),
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
    fn follows_special_forms_and_call_examples_from_schemat() {
        for (prefix, expected) in [
            ("(define (square x)", 2),
            ("(define answer", 2),
            ("(map (lambda (x)", 7),
            ("(lambda", 4),
            ("(let ((x 10)", 6),
            ("(let ((x 10)\n      (y 20))", 2),
            ("(let* ((x 10))", 2),
            ("(letrec ((f 1))", 2),
            ("(let loop ((n 3))", 2),
            ("(if condition", 4),
            ("(if condition\n    then-value", 4),
            ("(cond", 2),
            ("(cond\n  [(positive? x)", 3),
            ("(case x", 2),
            ("(case x\n  [(1 2) 'small", 3),
            ("(begin", 2),
            ("(begin (define x 1)", 7),
            ("(when ready?", 2),
            ("(unless ready?", 2),
            ("(hash \"id\" id", 6),
            ("(hash ; header", 2),
            ("(foo first", 5),
            ("(compute first\n         second", 9),
            ("(long-function", 1),
            ("(ж first", 3),
            ("(f λ (g first", 8),
            ("'(if first", 2),
        ] {
            assert_eq!(next_indent(prefix), expected, "prefix {prefix}");
        }
    }

    #[test]
    fn tracks_all_open_forms_while_ignoring_literals_and_comments() {
        for prefix in [
            "(foo first ; (unclosed ) [\n     second",
            "(foo first #| ) #| [ |# } |#\n     second",
            "(foo \"(text\\\"[\"\n     second",
            "(foo #\\(\n     second",
            "(foo #\\;\n     second",
        ] {
            assert_eq!(next_indent(prefix), 5, "prefix {prefix}");
        }
        assert_eq!(
            next_indent("(define (f x)\n  (let ((x 1))\n    (foo first)"),
            4
        );
        assert_eq!(next_indent("  (display \"unfinished ["), 2);
        assert_eq!(next_indent("  #| unclosed #| inner |#"), 2);
        let mut text = "(define имя 1) trailing".to_owned();
        insert_newline(&mut text, 14, 22);
        assert!(text.contains('\n'));
    }

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
        assert_eq!(next_indent("  (plot/point!"), 3);
        assert_eq!(next_indent("  ; comment ("), 0);
        assert_eq!(next_indent("  (display \"(\")"), 0);
        assert_eq!(next_indent("    (foo))"), 0);
    }

    #[test]
    fn supports_unicode() {
        let mut source = "(define имя 10)".to_owned();
        let cursor = source.chars().count();

        insert_newline(&mut source, cursor, cursor);

        assert_eq!(source, "(define имя 10)\n");
    }
}
