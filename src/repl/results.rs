//! Display layout only: keep Steel's scalar spelling and proper-list syntax.
//! schemat preserves the existing line breaks in these data lists, so formatting
//! its single-line input does not make structured REPL results readable.
use steel::SteelVal;

const INLINE_COLUMNS: usize = 80;
const MAX_DEPTH: usize = 64;

pub(super) fn format(value: &SteelVal) -> String {
    let mut output = String::new();
    if write(value, 0, 0, &mut output).is_some() {
        output
    } else {
        // Opaque values, other containers and very deep lists retain Steel's
        // own representation (including its cycle detection and depth limit).
        value.to_string()
    }
}

fn scalar(value: &SteelVal) -> bool {
    matches!(
        value,
        SteelVal::IntV(_)
            | SteelVal::BigNum(_)
            | SteelVal::NumV(_)
            | SteelVal::BoolV(_)
            | SteelVal::StringV(_)
            | SteelVal::SymbolV(_)
            | SteelVal::CharV(_)
    )
}

fn write(value: &SteelVal, column: usize, depth: usize, output: &mut String) -> Option<()> {
    if scalar(value) {
        output.push_str(&value.to_string());
        return Some(());
    }
    let SteelVal::ListV(items) = value else {
        return None;
    };
    if depth >= MAX_DEPTH {
        return None;
    }
    // Empty lists also fit on a key/value row, e.g. ("binding" ()).
    if items
        .iter()
        .all(|v| scalar(v) || matches!(v, SteelVal::ListV(l) if l.is_empty()))
    {
        let inline = value.to_string();
        if items.is_empty() || column + inline.chars().count() <= INLINE_COLUMNS {
            output.push_str(&inline);
            return Some(());
        }
    }
    output.push('(');
    for (index, item) in items.iter().enumerate() {
        if index > 0 {
            output.push('\n');
            output.extend(std::iter::repeat_n(' ', column + 1));
        }
        write(item, column + 1, depth + 1, output)?;
    }
    output.push(')');
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use steel::steel_vm::engine::Engine;

    fn value(engine: &mut Engine, source: &str) -> SteelVal {
        engine
            .compile_and_run_raw_program(source.to_owned())
            .unwrap()
            .pop()
            .unwrap()
    }

    fn round_trip(engine: &mut Engine, original: &SteelVal) -> String {
        let formatted = format(original);
        // Read the result as data, not as a call. This verifies strings, numeric
        // types and list boundaries rather than comparing whitespace alone.
        let copy = value(engine, &format!("'{formatted}"));
        assert_eq!(&copy, original);
        assert_eq!(copy.to_string(), original.to_string());
        formatted
    }

    #[test]
    fn scalars_empty_and_small_simple_lists_keep_their_spelling() {
        let mut engine = Engine::new();
        for source in [
            "42",
            "3.14",
            "35.0",
            "-0.0",
            "#true",
            "#false",
            r#""ready""#,
            "'ok",
            "'()",
            "'(1 2 3)",
            "'(42 3.14 #true #false ())",
            "9223372036854775808",
        ] {
            let original = value(&mut engine, source);
            assert_eq!(round_trip(&mut engine, &original), original.to_string());
        }
    }

    #[test]
    fn nested_lists_and_association_rows_get_indented_lines() {
        let mut engine = Engine::new();
        for (source, expected) in [
            ("'((1 2) (3 (4 5)))", "((1 2)\n (3\n  (4 5)))"),
            (
                r#"'(("name" "loop") ("parameters" (("setpoint" 35.0) ("kp" 3.0))) ("binding" ()))"#,
                "((\"name\" \"loop\")\n (\"parameters\"\n  ((\"setpoint\" 35.0)\n   (\"kp\" 3.0)))\n (\"binding\" ()))",
            ),
            ("'((a) (b) ())", "((a)\n (b)\n ())"),
        ] {
            let original = value(&mut engine, source);
            assert_eq!(round_trip(&mut engine, &original), expected);
        }
    }

    #[test]
    fn strings_and_characters_are_not_interpreted_as_list_syntax() {
        let mut engine = Engine::new();
        let original = value(
            &mut engine,
            r#"'(("text" "\"quoted\" (parentheses) [brackets] ; #true \\ path\nnext\tcolumn\rreturn 🌡температура") ("chars" (#\( #\) #\newline #\space)))"#,
        );
        let formatted = round_trip(&mut engine, &original);
        assert!(formatted.contains(r#"\"quoted\" (parentheses) [brackets] ; #true \\ path\nnext\tcolumn\rreturn 🌡температура"#));
        assert!(formatted.contains('\n'));
    }

    #[test]
    fn response_shaped_data_is_copyable_and_readable() {
        let mut engine = Engine::new();
        for source in [
            r#"'(("parameters" ((("key" "furnace1/sample_temperature") ("type" "Float") ("unit" "degC") ("writable" #false) ("min" -100.0) ("max" 500.0) ("poll_period_ns" 18446744073709551615)))) ("actions" ((("key" "furnace1/apply") ("description" "Apply model")))))"#,
            r#"'(("revision" 3) ("instances" ((("configuration" (("name" "loop") ("type_id" "pid") ("inputs" (("input" "processing/smooth/value"))) ("parameters" (("setpoint" 35.0) ("kp" 3.0))) ("max_age_ms" 5000))) ("state" "shadow") ("fault" ()) ("binding" ())))) ("total" 1) ("next_offset" ()))"#,
            r#"'((("key" "furnace1/power") ("mode" "manual") ("owner" ()) ("outcome_unknown" #false)))"#,
            r#"'((("type_id" "ema") ("kind" "filter") ("inputs" ("input")) ("outputs" ((("name" "value") ("unit" ())))) ("parameters" ((("name" "tau") ("unit" "s") ("value_type" (("type" "float") ("min" 0.000001) ("max" 86400.0))) ("default" 1.0))))))"#,
        ] {
            let original = value(&mut engine, source);
            let formatted = round_trip(&mut engine, &original);
            assert!(formatted.lines().count() >= 4, "{formatted}");
            assert!(
                formatted.lines().skip(1).all(|line| line.starts_with(' ')),
                "{formatted}"
            );
        }
    }

    #[test]
    fn long_flat_lists_wrap_and_unsupported_or_deep_values_fall_back() {
        let mut engine = Engine::new();
        let original = value(&mut engine, "(range 0 100)");
        assert!(round_trip(&mut engine, &original).contains('\n'));
        for source in ["+", "(list (list 1 2) +)", "(vector 1 2)", "(cons 1 2)"] {
            let original = value(&mut engine, source);
            assert_eq!(format(&original), original.to_string());
        }
        let mut deep = SteelVal::IntV(42);
        for _ in 0..MAX_DEPTH + 1 {
            deep = SteelVal::ListV([deep].into_iter().collect());
        }
        assert_eq!(format(&deep), deep.to_string());
    }
}
