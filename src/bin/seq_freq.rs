//! seq_freq — sequence-frequency miner for the WF66 "recognize a common
//! sequence, replace it" optimizer. Measures which adjacent token/instruction
//! sequences are most common, in two domains:
//!
//!   1. High-level Forth — the `:` ... `;` definition bodies in lib/, demos/,
//!      and bench/corpus/. Drives the WF66 reduce-rule catalog (which Forth
//!      idioms to recognize), ordered by real frequency.
//!   2. Kernel MASM — the instruction/macro sequences inside `proc ... endp`.
//!      Drives native peephole opportunities.
//!
//! Run: cargo run --bin seq_freq

use std::collections::HashMap;
use std::fs;
use std::path::Path;

fn main() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));

    // ---- High-level Forth ------------------------------------------------
    let mut forth_files = Vec::new();
    for dir in ["lib", "demos", "bench/corpus"] {
        collect(&root.join(dir), "f", &mut forth_files);
    }
    let mut bodies: Vec<Vec<String>> = Vec::new();
    for f in &forth_files {
        let src = fs::read_to_string(f).unwrap_or_default();
        bodies.extend(forth_bodies(&src));
    }
    let (fb2, fb3) = ngrams(&bodies);
    println!(
        "=== High-level Forth: {} colon-def bodies across {} files ===",
        bodies.len(),
        forth_files.len()
    );
    print_top("bigrams", &fb2, 30);
    print_top("trigrams", &fb3, 20);

    // ---- Kernel MASM -----------------------------------------------------
    let mut masm_files = Vec::new();
    collect(&root.join("kernel"), "masm", &mut masm_files);
    let mut procs: Vec<Vec<String>> = Vec::new();
    for f in &masm_files {
        let src = fs::read_to_string(f).unwrap_or_default();
        procs.extend(masm_procs(&src));
    }
    let (mb2, mb3) = ngrams(&procs);
    println!(
        "\n=== Kernel MASM: {} proc bodies across {} files ===",
        procs.len(),
        masm_files.len()
    );
    print_top("bigrams", &mb2, 30);
    print_top("trigrams", &mb3, 20);
}

fn collect(dir: &Path, ext: &str, out: &mut Vec<std::path::PathBuf>) {
    if let Ok(rd) = fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().map(|x| x == ext).unwrap_or(false) {
                out.push(p);
            }
        }
    }
}

/// Whitespace-tokenize Forth source, honoring `\` line comments and `( ... )`
/// inline comments (which may span lines).
fn forth_tokens(src: &str) -> Vec<String> {
    let mut toks = Vec::new();
    let mut in_paren = false;
    for line in src.lines() {
        for raw in line.split_whitespace() {
            if in_paren {
                if raw == ")" {
                    in_paren = false;
                }
                continue;
            }
            if raw == "\\" {
                break; // rest of line is a comment
            }
            if raw == "(" {
                in_paren = true;
                continue;
            }
            toks.push(raw.to_string());
        }
    }
    toks
}

/// Extract each `:` name ... `;` body (excluding the name and `;`).
fn forth_bodies(src: &str) -> Vec<Vec<String>> {
    let toks = forth_tokens(src);
    let mut bodies = Vec::new();
    let mut i = 0;
    while i < toks.len() {
        if toks[i] == ":" {
            i += 1; // skip ':'
            if i < toks.len() {
                i += 1; // skip the name
            }
            let mut body = Vec::new();
            while i < toks.len() && toks[i] != ";" {
                body.push(toks[i].clone());
                i += 1;
            }
            if !body.is_empty() {
                bodies.push(body);
            }
            if i < toks.len() {
                i += 1; // skip ';'
            }
        } else {
            i += 1;
        }
    }
    bodies
}

/// Extract the instruction/macro mnemonic sequence inside each `proc ... endp`.
fn masm_procs(src: &str) -> Vec<Vec<String>> {
    let mut procs = Vec::new();
    let mut cur: Option<Vec<String>> = None;
    for line in src.lines() {
        let line = match line.find(';') {
            Some(i) => &line[..i],
            None => line,
        };
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        if t.starts_with("proc(") {
            cur = Some(Vec::new());
            continue;
        }
        if t.starts_with("endp(") {
            if let Some(p) = cur.take() {
                procs.push(p);
            }
            continue;
        }
        let Some(p) = cur.as_mut() else { continue };
        if t.ends_with(':') {
            continue; // label
        }
        if t.starts_with('@') || t.starts_with('.') {
            continue; // directive
        }
        // leading identifier (mnemonic, or macro name before its '(')
        let mnem: String = t
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        if !mnem.is_empty() {
            p.push(mnem);
        }
    }
    procs
}

fn ngrams(seqs: &[Vec<String>]) -> (HashMap<String, usize>, HashMap<String, usize>) {
    let mut b2 = HashMap::new();
    let mut b3 = HashMap::new();
    for s in seqs {
        for w in s.windows(2) {
            *b2.entry(format!("{} {}", w[0], w[1])).or_insert(0) += 1;
        }
        for w in s.windows(3) {
            *b3.entry(format!("{} {} {}", w[0], w[1], w[2])).or_insert(0) += 1;
        }
    }
    (b2, b3)
}

fn print_top(label: &str, m: &HashMap<String, usize>, k: usize) {
    let mut v: Vec<_> = m.iter().collect();
    v.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
    println!("  -- top {k} {label} --");
    for (seq, n) in v.into_iter().take(k) {
        println!("  {n:5}  {seq}");
    }
}
