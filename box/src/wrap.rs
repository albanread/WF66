//! Control structures outside a definition.
//!
//! MF66 runs `3 0 do i . loop` or `x if ... then` typed at the prompt or
//! written at a file's top level. WF66's if, do and the rest are compile-only,
//! so a line (or the part of it after the last definition) that uses one
//! outside a definition is given to WF66 as `:noname ... ; execute`.

const CONTROL: &[&str] = &[
    "if", "else", "then", "begin", "until", "again", "while", "repeat", "do", "?do", "loop",
    "+loop", "leave", "unloop", "case", "of", "endof", "endcase", "i", "j",
];

/// Words that take the next token (or text) as their argument.
fn takes_name(w: &str) -> bool {
    matches!(
        w,
        "char" | "[char]" | "'" | "[']" | "postpone" | "variable" | "constant" | "value" | "to"
            | "create" | "include" | "2variable" | "2constant" | "fvariable" | "fconstant"
            | "defer" | "is" | "buffer:" | "forget" | "marker"
    )
}

/// The line with interpreted control structures wrapped; `in_def` is whether
/// a definition is open, before the line and after it.
pub fn wrap_interpreted(line: &str, in_def: &mut bool) -> String {
    let b = line.as_bytes();
    let mut i = 0;
    let mut seg_start: Option<usize> = None; // where the top-level run began
    let mut need = false; // that run has a control word
    let mut out = String::new();
    let mut copied = 0;
    let mut flush = |out: &mut String, copied: &mut usize, start: usize, end: usize, need: bool| {
        if need {
            out.push_str(&line[*copied..start]);
            out.push_str(":noname ");
            out.push_str(&line[start..end]);
            out.push_str(" ; execute ");
            *copied = end;
        }
    };
    let mut pending_name = false;
    while i < b.len() {
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= b.len() {
            break;
        }
        let st = i;
        while i < b.len() && !b[i].is_ascii_whitespace() {
            i += 1;
        }
        let w = line[st..i].to_ascii_lowercase();
        if pending_name {
            pending_name = false;
            continue;
        }
        let skip_to = |i: &mut usize, c: u8| {
            if *i < b.len() {
                *i += 1;
            }
            while *i < b.len() && b[*i] != c {
                *i += 1;
            }
            if *i < b.len() {
                *i += 1;
            }
        };
        match w.as_str() {
            "\\" => {
                i = b.len();
                break;
            }
            "(" | ".(" => {
                skip_to(&mut i, b')');
                continue;
            }
            _ if w.ends_with('"') => {
                skip_to(&mut i, b'"');
                if seg_start.is_none() && !*in_def {
                    seg_start = Some(st);
                }
                continue;
            }
            _ => {}
        }
        if *in_def {
            if w == ";" {
                *in_def = false;
            }
            continue;
        }
        if w == ":" || w == ":noname" {
            if let Some(s0) = seg_start.take() {
                flush(&mut out, &mut copied, s0, st, need);
            }
            need = false;
            *in_def = true;
            pending_name = w == ":";
            continue;
        }
        if seg_start.is_none() {
            seg_start = Some(st);
        }
        if takes_name(&w) {
            pending_name = true;
        } else if CONTROL.contains(&w.as_str()) {
            need = true;
        }
    }
    if let Some(s0) = seg_start {
        let end = line.trim_end().len().max(s0);
        let end = match line[s0..].find(" \\ ") {
            Some(k) if need => s0 + k,
            _ => end,
        };
        flush(&mut out, &mut copied, s0, end, need);
    }
    out.push_str(&line[copied..]);
    out
}

pub fn wrap_source(src: &str, in_def: bool) -> String {
    let mut d = in_def;
    src.split('\n').map(|l| wrap_interpreted(l, &mut d)).collect::<Vec<_>>().join("\n")
}


#[cfg(test)]
mod tests {
    use super::wrap_interpreted;
    fn w(l: &str) -> String {
        let mut d = false;
        wrap_interpreted(l, &mut d)
    }
    #[test]
    fn wraps() {
        assert_eq!(w("3 0 do i . loop"), ":noname 3 0 do i . loop ; execute ");
        assert_eq!(w(": sq dup * ;"), ": sq dup * ;");
        assert_eq!(w(": sq dup * ; 3 0 do i sq . loop"), ": sq dup * ; :noname 3 0 do i sq . loop ; execute ");
        assert_eq!(w(".\" if then\" cr"), ".\" if then\" cr");
        assert_eq!(w("1 if .\" yes\" then \\ if"), ":noname 1 if .\" yes\" then ; execute  \\ if");
        assert_eq!(w("char i emit"), "char i emit");
        let mut d = false;
        assert_eq!(wrap_interpreted(": x", &mut d), ": x");
        assert!(d);
        assert_eq!(wrap_interpreted("  begin 1 until ;", &mut d), "  begin 1 until ;");
        assert!(!d);
    }
}
